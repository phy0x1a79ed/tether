//! The session carried over plain HTTPS requests, for a path that will not
//! carry a WebSocket.
//!
//! The relay's side, and the numbering that lets any request here be repeated,
//! are in `tether_relay::poll`. This side is two loops: one keeps a `recv`
//! waiting at the relay at all times and hands what comes back to [`Polled::next`],
//! the other drains what [`Polled::send`] queued into `send` requests. Each
//! holds its own connection, so a keystroke does not wait behind a `recv` that
//! has nothing to say for another twenty seconds.
//!
//! Either loop rides out a broken request by asking again, from where the
//! relay last confirmed. It gives up after [`GIVE_UP`] of nothing but failures,
//! which is inside the relay's own idle deadline, so the two ends agree that
//! the session is over rather than one of them waiting on a chair already gone.

use std::collections::VecDeque;
use std::time::Duration;

use tether_proto::invite::Slot;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, Instant};

use crate::http::{Conn, HttpError};
use crate::net::Route;
use crate::relay::Relay;
use crate::LinkError;

/// Longer than the relay holds a `recv` open, so an empty answer arrives
/// before this gives up on it.
const RECV_TIMEOUT: Duration = Duration::from_secs(40);

/// Longer than the relay waits on a peer that is not draining.
const SEND_TIMEOUT: Duration = Duration::from_secs(35);

/// How long either loop keeps trying through failures before calling the
/// session lost.
const GIVE_UP: Duration = Duration::from_secs(60);

/// How many bytes of frames one `send` carries, past the first frame.
const BATCH: usize = 256 * 1024;

/// How many frames may wait to be sent before [`Polled::send`] waits too.
const QUEUE: usize = 32;

pub struct Polled {
    out: Option<mpsc::Sender<Vec<u8>>>,
    inbox: mpsc::Receiver<Vec<u8>>,
    receiver: JoinHandle<()>,
    sender: Option<JoinHandle<()>>,
}

impl Polled {
    /// Take the chair the ticket names, and start both loops.
    pub async fn open(relay: &Relay, slot: Slot, ticket: &str) -> Result<Self, LinkError> {
        let route = Route::for_relay(relay)?;
        // A fresh connection, because taking a chair spends the ticket and so
        // must not be the request [`Conn`] repeats on a stale one.
        let mut conn = Conn::new(relay.clone(), route.clone());
        let (status, body) = conn
            .request("POST", &relay.poll_path(slot, ticket), b"")
            .await?;
        match status {
            200 => {}
            404 => {
                return Err(LinkError::Socket(
                    "there is no such session — the code may have expired".into(),
                ))
            }
            409 => return Err(LinkError::Taken),
            other => return Err(HttpError::Status(other).into()),
        }
        let id = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("id")?.as_str().map(str::to_string))
            .ok_or_else(|| HttpError::Malformed("no poll id in the answer".into()))?;

        let (out, queued) = mpsc::channel(QUEUE);
        let (arrived, inbox) = mpsc::channel(QUEUE);
        let receiver = tokio::spawn(receive(
            Conn::new(relay.clone(), route),
            relay.clone(),
            id.clone(),
            arrived,
        ));
        let sender = tokio::spawn(deliver(conn, relay.clone(), id, queued));
        Ok(Self {
            out: Some(out),
            inbox,
            receiver,
            sender: Some(sender),
        })
    }

    pub async fn send(&mut self, frame: Vec<u8>) -> Result<(), LinkError> {
        match &self.out {
            Some(out) => out.send(frame).await.map_err(|_| LinkError::Closed),
            None => Err(LinkError::Closed),
        }
    }

    pub async fn next(&mut self) -> Result<Vec<u8>, LinkError> {
        self.inbox.recv().await.ok_or(LinkError::Closed)
    }

    /// Send what is queued, then give the chair up.
    ///
    /// Waits a little for that, because the frame that matters most is usually
    /// the last one: the reason the session is ending.
    pub async fn close(&mut self) {
        self.out = None;
        if let Some(sender) = self.sender.take() {
            let _ = timeout(Duration::from_secs(5), sender).await;
        }
        self.receiver.abort();
    }
}

impl Drop for Polled {
    fn drop(&mut self) {
        // The sender is left to finish: with its queue closed it sends what
        // is left and then gives the chair up, which is the departure the
        // relay should hear about.
        self.receiver.abort();
    }
}

/// Keep a `recv` waiting at the relay, and pass on what it brings, in order.
async fn receive(mut conn: Conn, relay: Relay, id: String, arrived: mpsc::Sender<Vec<u8>>) {
    let mut ack: u64 = 0;
    let mut failing = Failing::default();
    loop {
        let path = format!("{}?ack={ack}", relay.seat_path(&id, "recv"));
        let answer = timeout(RECV_TIMEOUT, conn.request("GET", &path, b"")).await;
        match answer {
            Ok(Ok((200, body))) => {
                let Some((first, frames)) = unframe(&body) else {
                    conn.reset();
                    if failing.again().await {
                        return;
                    }
                    continue;
                };
                failing = Failing::default();
                for (n, frame) in (first..).zip(frames) {
                    // Already handed on from an earlier answer to a request
                    // that is being repeated.
                    if n < ack {
                        continue;
                    }
                    if n > ack || arrived.send(frame).await.is_err() {
                        return;
                    }
                    ack += 1;
                }
            }
            // The chair is gone: the other end left, or the relay dropped it.
            Ok(Ok((404, _))) => return,
            _ => {
                conn.reset();
                if failing.again().await {
                    return;
                }
            }
        }
    }
}

/// Send what is queued, in batches, until the queue closes; then give the
/// chair up.
async fn deliver(mut conn: Conn, relay: Relay, id: String, mut queued: mpsc::Receiver<Vec<u8>>) {
    // `pending[0]` is frame number `seq`.
    let mut seq: u64 = 0;
    let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
    let mut open = true;
    let mut failing = Failing::default();
    loop {
        if pending.is_empty() {
            if !open {
                break;
            }
            match queued.recv().await {
                Some(frame) => pending.push_back(frame),
                None => break,
            }
        }
        let mut size: usize = pending.iter().map(Vec::len).sum();
        while open && size < BATCH {
            match queued.try_recv() {
                Ok(frame) => {
                    size += frame.len();
                    pending.push_back(frame);
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => open = false,
            }
        }

        let mut body = Vec::new();
        for (i, frame) in pending.iter().enumerate() {
            if i > 0 && body.len() + frame.len() > BATCH {
                break;
            }
            body.extend_from_slice(&(frame.len() as u32).to_be_bytes());
            body.extend_from_slice(frame);
        }
        let path = format!("{}?seq={seq}", relay.seat_path(&id, "send"));
        match timeout(SEND_TIMEOUT, conn.request("POST", &path, &body)).await {
            Ok(Ok((200, answer))) => {
                let next = serde_json::from_slice::<serde_json::Value>(&answer)
                    .ok()
                    .and_then(|v| v.get("next")?.as_u64());
                match next {
                    Some(next) => {
                        failing = Failing::default();
                        while seq < next && pending.pop_front().is_some() {
                            seq += 1;
                        }
                    }
                    None => {
                        if failing.again().await {
                            return;
                        }
                    }
                }
            }
            Ok(Ok((404, _))) => return,
            _ => {
                conn.reset();
                if failing.again().await {
                    return;
                }
            }
        }
    }
    let close = relay.seat_path(&id, "close");
    let _ = timeout(Duration::from_secs(5), conn.request("POST", &close, b"")).await;
}

/// A run of failed requests: how long to wait before the next, and when to
/// stop.
#[derive(Default)]
struct Failing {
    since: Option<Instant>,
    tries: u32,
}

impl Failing {
    /// Wait before trying again. True once the run has lasted [`GIVE_UP`].
    async fn again(&mut self) -> bool {
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() >= GIVE_UP {
            return true;
        }
        // The first retry is immediate: most failures are a connection the
        // path closed while it sat idle, and the fresh one will simply work.
        let wait = match self.tries {
            0 => Duration::ZERO,
            n => Duration::from_millis(250 << n.min(4)),
        };
        self.tries += 1;
        sleep(wait).await;
        false
    }
}

/// A `recv` answer: the number of its first frame, and the frames.
fn unframe(body: &[u8]) -> Option<(u64, Vec<Vec<u8>>)> {
    let (first, mut rest) = body.split_first_chunk::<8>()?;
    let mut frames = Vec::new();
    while !rest.is_empty() {
        let (len, tail) = rest.split_first_chunk::<4>()?;
        let len = u32::from_be_bytes(*len) as usize;
        if len > tail.len() {
            return None;
        }
        frames.push(tail[..len].to_vec());
        rest = &tail[len..];
    }
    Some((u64::from_be_bytes(*first), frames))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_unframes_into_its_number_and_frames() {
        let mut body = 7u64.to_be_bytes().to_vec();
        for frame in [&b"ab"[..], b""] {
            body.extend_from_slice(&(frame.len() as u32).to_be_bytes());
            body.extend_from_slice(frame);
        }
        assert_eq!(unframe(&body), Some((7, vec![b"ab".to_vec(), vec![]])));
        assert_eq!(unframe(&body[..body.len() - 1]), None);
        assert_eq!(unframe(&[0; 7]), None);
        assert_eq!(unframe(&[0; 8]), Some((0, vec![])));
    }
}
