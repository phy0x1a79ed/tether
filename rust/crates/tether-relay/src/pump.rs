//! Pairing two sockets and moving bytes between them.
//!
//! This is the only part of the relay that touches session traffic, and the
//! whole design of it is about how little it does. It reads a message, it
//! writes that message to the other socket, and it has no way to do anything
//! else: it holds no key, it never calls a decoder, and `tether_proto` is
//! linked here only for the *shape of a slot number*.
//!
//! # The teardown cascade
//!
//! Each socket is two tasks — a reader that forwards into the peer's channel,
//! and a writer that drains its own. Ending one leg ends the other without any
//! shared flag, because the channels themselves carry the signal:
//!
//! 1. A's reader ends and drops the sender it holds for B.
//! 2. B's channel closes, so B's writer closes B's socket.
//! 3. B's reader sees the close and drops the sender it holds for A.
//! 4. A's channel closes and A's writer closes A's socket.
//!
//! That is the property the owner's screen depends on: whichever side cuts, the
//! other notices rather than sitting on a socket that will never speak again.
//! Adding a second path between the two sockets would break it silently, so
//! there is exactly one.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tether_proto::invite::Slot;
use tokio::sync::mpsc;
use tokio::time::{timeout, MissedTickBehavior};

use crate::log;
use crate::session::{Join, Outbound, Seat, Sessions};

/// How many messages may queue for one socket before its peer's reader blocks.
///
/// Small on purpose. A deep queue would let a fast operator buffer megabytes
/// inside the relay for a slow owner; a shallow one turns that into
/// backpressure on the socket, which is where it belongs.
const QUEUE: usize = 32;

/// How many messages a socket may say before its peer exists.
///
/// The operator's client sends its half of the handshake the instant the socket
/// opens, which is usually minutes before anybody redeems the invite, so those
/// bytes are held here until there is somewhere to put them. The protocol sends
/// one. Anything past this allowance is a caller this service does not have.
const EARLY: usize = 4;

/// Frees a chair however this socket ends — including when the WebSocket
/// upgrade itself fails and the handler body never runs.
///
/// Without it, an aborted upgrade would leave a chair occupied by nobody until
/// the session expired, which is a denial of service anyone could perform by
/// opening a connection and dropping it.
pub struct SeatGuard {
    sessions: Arc<Sessions>,
    slot: Slot,
    seat: Seat,
}

impl Drop for SeatGuard {
    fn drop(&mut self) {
        if self.sessions.leave(self.slot, self.seat, Instant::now()) {
            log::info(format_args!("slot {} is spent and gone", self.slot));
        }
    }
}

/// Everything the upgrade handler needs, decided before the upgrade happened.
pub struct Seated {
    pub guard: SeatGuard,
    pub seat: Seat,
    pub slot: Slot,
    pub outcome: Join,
    pub rx: mpsc::Receiver<Bytes>,
    pub sessions: Arc<Sessions>,
}

/// Take a chair for a socket that has not been upgraded yet.
///
/// Deliberately separate from [`run`] so an unknown slot or a token that names
/// no chair is refused as an HTTP status, before any socket exists. A refusal
/// that costs a WebSocket handshake is a refusal that costs the relay more than
/// it costs the caller.
pub fn seat(
    sessions: Arc<Sessions>,
    slot: Slot,
    token: crate::token::Token,
) -> Result<Seated, crate::session::JoinError> {
    let (tx, rx) = mpsc::channel::<Bytes>(QUEUE);
    let outcome = sessions.join(slot, token, tx, Instant::now())?;
    let seat = match &outcome {
        Join::Paired { seat, .. } | Join::Waiting { seat, .. } => *seat,
    };
    Ok(Seated {
        guard: SeatGuard {
            sessions: Arc::clone(&sessions),
            slot,
            seat,
        },
        seat,
        slot,
        outcome,
        rx,
        sessions,
    })
}

/// Wait for the other end, then carry bytes until either side stops.
pub async fn run(seated: Seated, socket: WebSocket) {
    let Seated {
        guard,
        seat,
        slot,
        outcome,
        rx,
        sessions,
    } = seated;
    let limits = sessions.limits().clone();
    let (sink, mut stream) = socket.split();

    // The writer starts before the pairing resolves, because an operator's
    // socket may wait minutes for the owner to run the launcher and nothing
    // else on the path will keep it open that long.
    let mut writer = tokio::spawn(write_leg(sink, rx, limits.keepalive));

    let (peer, early) = match outcome {
        Join::Paired { peer, .. } => {
            log::info(format_args!(
                "slot {slot} paired, {} joined last",
                seat.as_str()
            ));
            (Some(peer), Vec::new())
        }
        Join::Waiting { peer, .. } => {
            log::info(format_args!(
                "slot {slot} waiting, {} is seated",
                seat.as_str()
            ));
            match wait_for_peer(&mut stream, peer, limits.unpaired_timeout).await {
                Wait::Paired(peer, early) => (Some(peer), early),
                Wait::Nobody => {
                    log::info(format_args!(
                        "slot {slot} dropped the {} leg: nobody arrived",
                        seat.as_str()
                    ));
                    (None, Vec::new())
                }
                Wait::Gone => {
                    log::info(format_args!(
                        "slot {slot} lost the {} leg before pairing",
                        seat.as_str()
                    ));
                    (None, Vec::new())
                }
            }
        }
    };

    if let Some(peer) = peer {
        read_leg(stream, peer, &limits, slot, seat, early).await;
    }

    // Release the chair before waiting on the writer: the writer cannot finish
    // until every sender for its channel is gone, and the table holds one.
    drop(guard);
    if timeout(limits.drain_grace, &mut writer).await.is_err() {
        writer.abort();
    }
}

/// How a socket's wait for its peer finished.
enum Wait {
    Paired(Outbound, Vec<Bytes>),
    /// The invite's whole unpaired budget passed with the other chair empty.
    Nobody,
    /// The socket closed, failed, or said more than a client of ours says.
    Gone,
}

/// Hold a seated socket until its peer arrives, reading it while it waits.
///
/// Reading during the wait is not an optimisation and the frames are not the
/// reason for it. A socket nobody reads cannot answer a keepalive ping, and the
/// edge in front of this relay closes a leg whose pings go unanswered — which
/// cut every invite short at around forty seconds, against the five minutes the
/// slot is good for. Reading is what makes the automatic pong happen.
///
/// Holding what the socket says is then forced: the operator speaks first, so
/// dropping those bytes would break the handshake the pairing exists to carry.
async fn wait_for_peer(
    stream: &mut SplitStream<WebSocket>,
    mut peer: tokio::sync::oneshot::Receiver<Outbound>,
    budget: std::time::Duration,
) -> Wait {
    let mut early: Vec<Bytes> = Vec::new();
    let deadline = tokio::time::sleep(budget);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            paired = &mut peer => return match paired {
                Ok(peer) => Wait::Paired(peer, early),
                Err(_) => Wait::Gone,
            },
            _ = &mut deadline => return Wait::Nobody,
            message = stream.next() => match message {
                Some(Ok(Message::Binary(bytes))) => {
                    if early.len() >= EARLY {
                        return Wait::Gone;
                    }
                    early.push(bytes);
                }
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                _ => return Wait::Gone,
            },
        }
    }
}

async fn write_leg(
    mut sink: SplitSink<WebSocket, Message>,
    mut rx: mpsc::Receiver<Bytes>,
    keepalive: std::time::Duration,
) {
    let mut tick = tokio::time::interval(keepalive);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tick.tick().await; // the first tick fires immediately; skip it

    loop {
        let sent = tokio::select! {
            message = rx.recv() => match message {
                Some(bytes) => sink.send(Message::Binary(bytes)).await,
                None => break,
            },
            _ = tick.tick() => sink.send(Message::Ping(Bytes::new())).await,
        };
        if sent.is_err() {
            break;
        }
    }

    // A clean close is what lets the far end say "the other side left" instead
    // of reporting a network error nobody can act on.
    let _ = sink.send(Message::Close(None)).await;
    let _ = sink.close().await;
}

async fn read_leg(
    mut stream: SplitStream<WebSocket>,
    peer: Outbound,
    limits: &crate::config::Limits,
    slot: Slot,
    seat: Seat,
    early: Vec<Bytes>,
) {
    let mut spoken = false;
    // Whatever this socket said before its peer existed, in the order it said
    // it and ahead of anything it says next.
    for bytes in early {
        spoken = true;
        if peer.send(bytes).await.is_err() {
            return;
        }
    }
    loop {
        // Until this socket has passed a real message it is on the short
        // deadline. A keepalive does not satisfy it: a socket that pairs and
        // then only pings is not a client of this service.
        let budget = if spoken {
            limits.idle_timeout
        } else {
            limits.first_frame_timeout
        };
        let message = match timeout(budget, stream.next()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => {
                log::info(format_args!(
                    "slot {slot} dropped the {} leg: silent for too long",
                    seat.as_str()
                ));
                break;
            }
        };
        match message {
            Message::Binary(bytes) => {
                spoken = true;
                if peer.send(bytes).await.is_err() {
                    break;
                }
            }
            Message::Ping(_) | Message::Pong(_) => {}
            // The two ends speak sealed frames, which are binary. Anything else
            // is a caller this service does not have.
            Message::Text(_) | Message::Close(_) => break,
        }
    }
}
