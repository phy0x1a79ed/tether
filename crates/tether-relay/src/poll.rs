//! A chair held through plain HTTP requests, for a client whose path to the
//! relay cannot carry a WebSocket.
//!
//! Some networks reach the internet only through a proxy that answers an
//! upgrade with an error, or forwards it and then cuts the connection. The
//! relay pairs chairs, not sockets, so a client on such a network takes its
//! chair here instead and then trades frames over ordinary requests:
//!
//! 1. `POST /poll/{slot}/{token}` takes the chair, with the same ticket or seat
//!    token `/join` takes, and answers with a poll id.
//! 2. `GET /seat/{id}/recv?ack=N` waits up to [`WAIT`] for frames.
//! 3. `POST /seat/{id}/send?seq=N` hands frames over.
//! 4. `POST /seat/{id}/close` gives the chair up.
//!
//! The poll id is a capability exactly as the seat token is: random, unguessable,
//! and the only thing that names the chair. Unlike `/claim` these requests are
//! not counted against the caller's address. A busy screen makes several a
//! second, and the limiter is for the requests that could create something.
//!
//! # One pump
//!
//! A polled chair is a socket made of two channels, handed to
//! [`pump::carry`] like any other. So the deadlines, the queue depth and the
//! teardown cascade are the ones a WebSocket gets, and every request counts as
//! the keepalive a socket would have answered. A client that stops asking is
//! dropped on the same idle deadline as a socket that stops answering pings.
//!
//! # Nothing is lost to a dropped response
//!
//! A proxy is exactly the kind of path that cuts a long request now and then,
//! and the sealed channel counts every frame, so one lost frame is a lost
//! session. Each direction is therefore numbered. A frame handed down stays
//! here until a later `recv` acknowledges it, and a frame handed up that the
//! relay has already taken is skipped when it is sent again. A client can
//! retry any request it did not see answered.
//!
//! # The body
//!
//! A `recv` answer is the sequence number of its first frame, as eight bytes
//! big-endian, then each frame as a four-byte big-endian length and the bytes.
//! A `send` body is the frames alone, numbered from `seq`. Its answer is the
//! next number the relay expects, so a client whose send was cut short knows
//! where to start again.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::Message;
use tokio::sync::{mpsc, Notify};
use tokio::time::Instant;

use crate::config::MAX_MESSAGE;
use crate::pump::{self, Seated, QUEUE};
use crate::token::Token;

/// How long a `recv` waits for something to say before answering empty.
///
/// Under the read timeout of every proxy this is likely to meet, which is what
/// decides it: a request the proxy cuts before the relay answers costs a
/// retry and a fresh connection.
pub const WAIT: Duration = Duration::from_secs(25);

/// How long a `send` may wait on a peer that is not draining.
///
/// That wait is backpressure, the same a socket feels. Past it the relay says
/// how far it got and the client sends the rest again.
const SEND_WAIT: Duration = Duration::from_secs(20);

/// How many bytes of frames one `recv` answer carries. Always at least one
/// frame, so a frame larger than this still goes.
const BATCH: usize = 256 * 1024;

/// The largest `send` body: two of the largest messages, and their lengths.
pub const MAX_SEND: usize = 2 * (MAX_MESSAGE + 4);

/// How long a finished chair stays answerable, so the client can collect the
/// last frames and the close before its id stops existing.
const LINGER: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Polls {
    inner: Mutex<HashMap<String, Arc<Polled>>>,
}

struct Polled {
    down: tokio::sync::Mutex<Down>,
    up: tokio::sync::Mutex<Up>,
    /// A clone of the up channel for the keepalive each request stands for. It
    /// is sent without waiting, so it never queues behind a stalled `send`.
    nudge: mpsc::Sender<Message>,
    /// Woken by every new `recv`, so one the client gave up on stops holding
    /// the queue rather than making the retry wait out its whole [`WAIT`].
    superseded: Notify,
}

struct Down {
    rx: mpsc::Receiver<Message>,
    /// Handed to a `recv` and not yet acknowledged by a later one.
    unacked: VecDeque<Bytes>,
    /// The number of `unacked[0]`, or of the next frame when it is empty.
    first: u64,
    ended: bool,
}

struct Up {
    tx: mpsc::Sender<Message>,
    next: u64,
}

pub enum Recv {
    Frames(Vec<u8>),
    /// The chair is gone, or never existed. One answer for both.
    Gone,
}

pub enum Sent {
    Next(u64),
    Gone,
    Malformed,
}

impl Polls {
    /// Seat a polled client, and start carrying for it.
    pub fn open(self: &Arc<Self>, seated: Seated) -> String {
        let (up_tx, up_rx) = mpsc::channel::<Message>(QUEUE);
        let (down_tx, down_rx) = mpsc::channel::<Message>(QUEUE);
        let id = Token::mint().to_hex();
        let polled = Polled {
            down: tokio::sync::Mutex::new(Down {
                rx: down_rx,
                unacked: VecDeque::new(),
                first: 0,
                ended: false,
            }),
            nudge: up_tx.clone(),
            up: tokio::sync::Mutex::new(Up { tx: up_tx, next: 0 }),
            superseded: Notify::new(),
        };
        self.inner
            .lock()
            .unwrap()
            .insert(id.clone(), Arc::new(polled));

        let sink = Box::pin(futures_util::sink::unfold(
            down_tx,
            |tx, message: Message| async move { tx.send(message).await.map(|()| tx) },
        ));
        let stream = Box::pin(futures_util::stream::unfold(up_rx, |mut rx| async move {
            rx.recv().await.map(|m| (Ok::<_, Infallible>(m), rx))
        }));
        let polls = Arc::clone(self);
        let key = id.clone();
        tokio::spawn(async move {
            pump::carry(seated, sink, stream).await;
            tokio::time::sleep(LINGER).await;
            polls.inner.lock().unwrap().remove(&key);
        });
        id
    }

    fn get(&self, id: &str) -> Option<Arc<Polled>> {
        let id = Token::parse(id)?.to_hex();
        self.inner.lock().unwrap().get(&id).cloned()
    }

    pub fn live(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// Everything after `ack`, waiting up to [`WAIT`] if there is nothing yet.
    pub async fn recv(&self, id: &str, ack: u64) -> Recv {
        let Some(polled) = self.get(id) else {
            return Recv::Gone;
        };
        let _ = polled.nudge.try_send(Message::Pong(Bytes::new()));
        polled.superseded.notify_waiters();
        let superseded = polled.superseded.notified();
        tokio::pin!(superseded);
        superseded.as_mut().enable();

        let mut down = polled.down.lock().await;
        let Down {
            rx,
            unacked,
            first,
            ended,
        } = &mut *down;
        while *first < ack && !unacked.is_empty() {
            unacked.pop_front();
            *first += 1;
        }

        if unacked.is_empty() && !*ended {
            let deadline = tokio::time::sleep(WAIT);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    message = rx.recv() => match message {
                        Some(Message::Binary(bytes)) => {
                            unacked.push_back(bytes);
                            break;
                        }
                        Some(Message::Ping(_)) | Some(Message::Pong(_)) => {}
                        Some(_) | None => {
                            *ended = true;
                            break;
                        }
                    },
                    _ = &mut deadline => break,
                    _ = &mut superseded => break,
                }
            }
        }

        let mut size: usize = unacked.iter().map(Bytes::len).sum();
        while size < BATCH && !*ended {
            match rx.try_recv() {
                Ok(Message::Binary(bytes)) => {
                    size += bytes.len();
                    unacked.push_back(bytes);
                }
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                Err(mpsc::error::TryRecvError::Empty) => break,
                Ok(_) | Err(mpsc::error::TryRecvError::Disconnected) => *ended = true,
            }
        }

        if unacked.is_empty() && *ended {
            drop(down);
            self.close(id);
            return Recv::Gone;
        }
        let mut body = Vec::with_capacity(8 + size + 4 * unacked.len());
        body.extend_from_slice(&first.to_be_bytes());
        for frame in unacked.iter() {
            body.extend_from_slice(&(frame.len() as u32).to_be_bytes());
            body.extend_from_slice(frame);
        }
        Recv::Frames(body)
    }

    /// Take frames numbered from `seq`, skipping any already taken.
    pub async fn send(&self, id: &str, seq: u64, body: &[u8]) -> Sent {
        let Some(polled) = self.get(id) else {
            return Sent::Gone;
        };
        let _ = polled.nudge.try_send(Message::Pong(Bytes::new()));
        let Some(frames) = split(body) else {
            return Sent::Malformed;
        };

        let deadline = Instant::now() + SEND_WAIT;
        let mut up = polled.up.lock().await;
        for (n, frame) in (seq..).zip(frames) {
            if n < up.next {
                continue;
            }
            // A gap means a send the client thinks landed did not. Stop here
            // and say where the relay is, rather than carry frames out of order.
            if n > up.next {
                break;
            }
            let message = Message::Binary(Bytes::copy_from_slice(frame));
            match tokio::time::timeout_at(deadline, up.tx.send(message)).await {
                Ok(Ok(())) => up.next += 1,
                Ok(Err(_)) => return Sent::Gone,
                Err(_) => break,
            }
        }
        Sent::Next(up.next)
    }

    /// Give up a chair. Dropping its channels is what tells the pump.
    pub fn close(&self, id: &str) -> bool {
        let Some(id) = Token::parse(id).map(Token::to_hex) else {
            return false;
        };
        self.inner.lock().unwrap().remove(&id).is_some()
    }
}

/// The frames in a `send` body, or `None` if it is not a whole number of them.
fn split(mut body: &[u8]) -> Option<Vec<&[u8]>> {
    let mut frames = Vec::new();
    while !body.is_empty() {
        let (len, rest) = body.split_first_chunk::<4>()?;
        let len = u32::from_be_bytes(*len) as usize;
        if len > MAX_MESSAGE || len > rest.len() {
            return None;
        }
        let (frame, rest) = rest.split_at(len);
        frames.push(frame);
        body = rest;
    }
    Some(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_splits_into_its_frames_or_not_at_all() {
        let mut body = Vec::new();
        for frame in [&b"one"[..], b"", b"three"] {
            body.extend_from_slice(&(frame.len() as u32).to_be_bytes());
            body.extend_from_slice(frame);
        }
        assert_eq!(
            split(&body).unwrap(),
            vec![&b"one"[..], &b""[..], &b"three"[..]]
        );
        assert!(split(&body[..body.len() - 1]).is_none());
        assert!(split(&[0, 0, 1]).is_none());
        assert!(split(&u32::MAX.to_be_bytes()).is_none());
        assert_eq!(split(&[]).unwrap(), Vec::<&[u8]>::new());
    }
}
