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
//!
//! The cascade does not end a held session. Both ends redial a held slot after
//! a drop, so the cascade only makes sure neither of them sits on a socket
//! whose far half is gone. Each leg logs how it ended, and the first such line
//! for a slot names the side that dropped.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Notify;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
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
pub(crate) const QUEUE: usize = 32;

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
    id: u64,
}

impl Drop for SeatGuard {
    fn drop(&mut self) {
        if self
            .sessions
            .leave(self.slot, self.seat, self.id, Instant::now())
        {
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
    /// Notified when a newer socket takes this chair.
    pub kick: Arc<Notify>,
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
    let (seat, id, kick) = match &outcome {
        Join::Paired { seat, id, kick, .. } | Join::Waiting { seat, id, kick, .. } => {
            (*seat, *id, Arc::clone(kick))
        }
    };
    Ok(Seated {
        guard: SeatGuard {
            sessions: Arc::clone(&sessions),
            slot,
            seat,
            id,
        },
        seat,
        slot,
        outcome,
        rx,
        sessions,
        kick,
    })
}

/// Wait for the other end, then carry bytes until either side stops.
pub async fn run(seated: Seated, socket: WebSocket) {
    let (sink, stream) = socket.split();
    carry(seated, sink, stream).await
}

/// [`run`] for anything shaped like a socket.
///
/// A WebSocket is one such thing and a polled seat is the other: see
/// [`crate::poll`]. Both go through this one loop, so the deadlines and the
/// teardown cascade above hold for both without being written twice.
pub async fn carry<Si, St, E>(seated: Seated, sink: Si, mut stream: St)
where
    Si: Sink<Message> + Unpin + Send + 'static,
    St: Stream<Item = Result<Message, E>> + Unpin,
{
    let Seated {
        guard,
        seat,
        slot,
        outcome,
        rx,
        sessions,
        kick,
    } = seated;
    let limits = sessions.limits().clone();

    // The writer starts before the pairing resolves, because an operator's
    // socket may wait minutes for the owner to run the launcher and nothing
    // else on the path will keep it open that long.
    let mut writer = tokio::spawn(write_leg(sink, rx, limits.keepalive));
    let mut written: Option<Written> = None;

    let leg = lead(
        &mut stream,
        outcome,
        &limits,
        slot,
        seat,
        &mut writer,
        &mut written,
    );
    tokio::select! {
        _ = leg => {}
        _ = kick.notified() => log::info(format_args!(
            "slot {slot} {} leg ended: a new connection took its chair",
            seat.as_str()
        )),
    }

    // Release the chair before waiting on the writer: the writer cannot finish
    // until every sender for its channel is gone, and the table holds one.
    drop(guard);
    if written.is_none() && timeout(limits.drain_grace, &mut writer).await.is_err() {
        writer.abort();
    }
}

/// Pair this socket, then carry what it says, and log how its leg ended.
async fn lead<St, E>(
    stream: &mut St,
    outcome: Join,
    limits: &crate::config::Limits,
    slot: Slot,
    seat: Seat,
    writer: &mut tokio::task::JoinHandle<Written>,
    written: &mut Option<Written>,
) where
    St: Stream<Item = Result<Message, E>> + Unpin,
{
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
            match wait_for_peer(stream, peer, limits.unpaired_timeout).await {
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
        let ended = read_leg(stream, peer, limits, early, writer, written).await;
        log::info(format_args!(
            "slot {slot} {} leg ended: {}",
            seat.as_str(),
            ended.as_str()
        ));
    }
}

/// Why a paired leg stopped carrying.
enum LegEnd {
    /// The client sent a close, or its stream ended.
    Closed,
    /// Reading the socket failed.
    Broken,
    /// Writing to the socket failed.
    Unwritable,
    /// The socket said nothing within its deadline.
    Silent,
    /// The client sent something that is not a sealed frame.
    Stray,
    /// The other leg ended first, and the cascade reached this one.
    PeerGone,
}

impl LegEnd {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Closed => "the client closed it",
            Self::Broken => "the socket failed",
            Self::Unwritable => "the socket stopped taking writes",
            Self::Silent => "silent for too long",
            Self::Stray => "the client sent a message that is not a frame",
            Self::PeerGone => "the other leg ended first",
        }
    }
}

/// How a socket's writer finished.
enum Written {
    /// Its channel closed: the peer's reader is gone.
    PeerGone,
    /// A write to the socket failed.
    Failed,
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
async fn wait_for_peer<St, E>(
    stream: &mut St,
    mut peer: tokio::sync::oneshot::Receiver<Outbound>,
    budget: std::time::Duration,
) -> Wait
where
    St: Stream<Item = Result<Message, E>> + Unpin,
{
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

async fn write_leg<Si>(
    mut sink: Si,
    mut rx: mpsc::Receiver<Bytes>,
    keepalive: std::time::Duration,
) -> Written
where
    Si: Sink<Message> + Unpin,
{
    let mut tick = tokio::time::interval(keepalive);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tick.tick().await; // the first tick fires immediately; skip it

    let written = loop {
        let sent = tokio::select! {
            message = rx.recv() => match message {
                Some(bytes) => sink.send(Message::Binary(bytes)).await,
                None => break Written::PeerGone,
            },
            _ = tick.tick() => sink.send(Message::Ping(Bytes::new())).await,
        };
        if sent.is_err() {
            break Written::Failed;
        }
    };

    // A clean close is what lets the far end say "the other side left" instead
    // of reporting a network error nobody can act on.
    let _ = sink.send(Message::Close(None)).await;
    let _ = sink.close().await;
    written
}

/// Forward what this socket says to its peer until one of them stops.
///
/// The writer is watched here as well, so a leg the cascade ends says so rather
/// than reporting the close it then hears from its own client.
async fn read_leg<St, E>(
    stream: &mut St,
    peer: Outbound,
    limits: &crate::config::Limits,
    early: Vec<Bytes>,
    writer: &mut tokio::task::JoinHandle<Written>,
    written: &mut Option<Written>,
) -> LegEnd
where
    St: Stream<Item = Result<Message, E>> + Unpin,
{
    let mut spoken = false;
    // Whatever this socket said before its peer existed, in the order it said
    // it and ahead of anything it says next.
    for bytes in early {
        spoken = true;
        if peer.send(bytes).await.is_err() {
            return LegEnd::PeerGone;
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
        let next = tokio::select! {
            next = timeout(budget, stream.next()) => next,
            done = &mut *writer => {
                let done = done.unwrap_or(Written::Failed);
                let end = match done {
                    Written::PeerGone => LegEnd::PeerGone,
                    Written::Failed => LegEnd::Unwritable,
                };
                *written = Some(done);
                return end;
            }
        };
        let message = match next {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(_))) => return LegEnd::Broken,
            Ok(None) => return LegEnd::Closed,
            Err(_) => return LegEnd::Silent,
        };
        match message {
            Message::Binary(bytes) => {
                spoken = true;
                if peer.send(bytes).await.is_err() {
                    return LegEnd::PeerGone;
                }
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => return LegEnd::Closed,
            // The two ends speak sealed frames, which are binary. Anything else
            // is a caller this service does not have.
            Message::Text(_) => return LegEnd::Stray,
        }
    }
}
