//! Getting the two ends onto one sealed channel, and nothing beyond that.
//!
//! Both binaries dial the same relay and run the same handshake, so this is one
//! crate rather than two implementations that agree until they do not. What
//! differs between the ends is only which chair they take and how they got the
//! token for it: the operator asks the relay for a slot on an authenticated
//! path, and the owner claims a ticket on the public one.
//!
//! # What travels, and what does not
//!
//! The slot travels: it is in a URL, it lands in every access log on the path,
//! and that is fine because it authenticates nobody. The **phrase never leaves
//! the machine it was typed on.** It is consumed here by
//! [`tether_proto::handshake`] and what goes on the wire is a
//! password-authenticated exchange, which is what lets twenty bits of spoken
//! secret be safe against a recorded conversation.
//!
//! # The opening exchange
//!
//! Four messages, then the channel:
//!
//! 1. each end sends its exchange message, without waiting for the other;
//! 2. each end derives a key from what it got back;
//! 3. each end sends a tag proving which key it derived;
//! 4. each end checks the other's tag.
//!
//! A wrong phrase gets as far as step 4 and fails there — at the **peer**, not
//! at the relay, which has no opinion because it never knew the phrase.
//!
//! # What carries it
//!
//! A WebSocket, when the path allows one. The owner's machine may sit behind a
//! proxy that refuses or breaks the upgrade, so the owner falls back to plain
//! requests ([`poll`]) when the upgrade fails for any reason other than the
//! relay's own refusal. `TETHER_TRANSPORT=poll` or `=socket` takes the choice
//! away, for testing either path on its own. The operator always holds a
//! socket: it runs where its operator put it, not behind someone else's proxy.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tether_proto::frame::{Frame, Role, MAX_FRAME};
use tether_proto::handshake::{self, Channel, HandshakeError};
use tether_proto::invite::{InviteCode, Phrase, Slot};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::WebSocketStream;

pub mod http;
pub mod net;
pub mod poll;
pub mod relay;
pub mod resume;

pub use net::{DialError, Route};
pub use relay::{Relay, RelayError};
pub use resume::{Incoming, Resumable};

/// How long the opening exchange may take.
///
/// Generous, because the relay opens a socket the moment a chair is taken and
/// the other end may not be there yet: the owner can be reading the code out
/// while the operator's side waits. Bounded, because a socket that pairs and
/// then says nothing is not somebody having trouble typing.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(90);

type Socket = WebSocketStream<net::Stream>;

#[derive(Debug)]
pub enum LinkError {
    Relay(RelayError),
    Dial(DialError),
    Http(http::HttpError),
    /// The socket would not open. Carries the status when the relay answered
    /// with one: `404` is a slot that is not there, `409` a chair already taken.
    Socket(String),
    /// The chair is occupied, or the other one is still unwinding a pairing.
    Taken,
    /// The relay has no such slot. To an owner who has just run the launcher,
    /// the code expired or is in use. To an end redialling a held session, the
    /// relay restarted.
    NoSuchSession,
    /// The relay says this held session was ended on purpose. Redialling is
    /// pointless.
    Ended,
    /// Nobody completed the exchange from the other side in time.
    NobodyThere,
    /// The peer's half of the exchange did not check out. In practice this is
    /// a mistyped phrase, and it is worth saying so rather than reporting a
    /// cryptographic failure to somebody trying to get help with a backup.
    Handshake(HandshakeError),
    Closed,
    /// A held session's link stayed down for the whole resume window.
    GaveUp,
    /// `TETHER_TRANSPORT` names neither transport.
    Transport(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Relay(e) => write!(f, "{e}"),
            Self::Dial(e) => write!(f, "{e}"),
            Self::Http(e) => write!(f, "{e}"),
            Self::Socket(e) => write!(f, "the relay would not open a socket: {e}"),
            Self::Taken => write!(f, "somebody is already in that seat"),
            Self::NoSuchSession => write!(
                f,
                "there is no such session — the code may have expired or already be in use; \
                 ask the operator for a new code"
            ),
            Self::Ended => write!(f, "the session was ended"),
            Self::NobodyThere => write!(
                f,
                "nobody answered on the other side of the session in time"
            ),
            Self::Handshake(HandshakeError::WrongCode) => write!(
                f,
                "the other end has a different invite code — check the words and try again"
            ),
            Self::Handshake(e) => write!(f, "the session could not be secured: {e}"),
            Self::Closed => write!(f, "the session ended"),
            Self::GaveUp => write!(
                f,
                "the link did not come back within {} minutes",
                tether_proto::RESUME_WINDOW.as_secs() / 60
            ),
            Self::Transport(value) => write!(
                f,
                "TETHER_TRANSPORT is `{value}`, but it takes `socket` or `poll`"
            ),
        }
    }
}

impl std::error::Error for LinkError {}

impl From<HandshakeError> for LinkError {
    fn from(e: HandshakeError) -> Self {
        Self::Handshake(e)
    }
}

impl From<DialError> for LinkError {
    fn from(e: DialError) -> Self {
        Self::Dial(e)
    }
}

impl From<http::HttpError> for LinkError {
    fn from(e: http::HttpError) -> Self {
        Self::Http(e)
    }
}

/// A sealed session with the other end.
pub struct Link {
    transport: Transport,
    channel: Channel,
    /// Why this session is not on a WebSocket, when it is not.
    polled: Option<String>,
}

enum Transport {
    Socket(Box<Socket>),
    Poll(poll::Polled),
}

impl Transport {
    async fn send(&mut self, bytes: Vec<u8>) -> Result<(), LinkError> {
        match self {
            Self::Socket(socket) => socket
                .send(Message::Binary(bytes.into()))
                .await
                .map_err(|_| LinkError::Closed),
            Self::Poll(polled) => polled.send(bytes).await,
        }
    }

    /// The next thing the peer actually said, skipping keepalives.
    async fn next(&mut self) -> Result<Vec<u8>, LinkError> {
        match self {
            Self::Socket(socket) => next_binary(socket).await,
            Self::Poll(polled) => polled.next().await,
        }
    }

    async fn close(&mut self) {
        match self {
            Self::Socket(socket) => {
                let _ = socket.close(None).await;
            }
            Self::Poll(polled) => polled.close().await,
        }
    }
}

/// Which transport the owner may use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Carry {
    /// A socket, or plain requests if the path will not carry one.
    Either,
    Socket,
    Poll,
}

impl Carry {
    /// What `TETHER_TRANSPORT` asks for.
    pub fn from_env() -> Result<Self, LinkError> {
        match std::env::var("TETHER_TRANSPORT") {
            Err(_) => Ok(Self::Either),
            Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
                "" => Ok(Self::Either),
                "socket" | "websocket" => Ok(Self::Socket),
                "poll" => Ok(Self::Poll),
                _ => Err(LinkError::Transport(v)),
            },
        }
    }
}

/// Why a socket did not open, sorted by whether plain requests could do better.
enum Upgrade {
    /// The relay itself refused: no such slot, or the chair is taken. Polling
    /// would be refused the same way.
    Refused(LinkError),
    /// Something between here and the relay would not carry the upgrade.
    Failed(String),
}

/// Tell the relay a slot is finished with, so it stops existing now.
///
/// Not a method on [`Link`], because the moment this matters is after the link
/// is gone. The relay cannot work out on its own that a session is over — a
/// refusal and a mistyped phrase look the same through sealed bytes, and one
/// must end the slot while the other must leave it open for a retry. The
/// operator is the only party that knows, so it says so here.
///
/// Best effort. The relay expires an operator-less slot on its own shortly
/// after, so a failure here costs a few seconds of a code that no longer works,
/// not a leaked session.
pub async fn release(relay: &Relay, slot: Slot, seat_token: &str) -> Result<(), LinkError> {
    http::post_json(relay, &relay.release_path(slot, seat_token), None).await?;
    Ok(())
}

/// Turn a slot into a resumable session for the owner whose token this is.
///
/// The operator calls this once, after consent and before it sends the owner
/// `Frame::Hold`. An older relay answers `404`, and the session then runs as
/// it always did, without resuming.
pub async fn hold(
    relay: &Relay,
    slot: Slot,
    seat_token: &str,
    owner_token: &str,
) -> Result<(), LinkError> {
    let body = serde_json::json!({ "owner": owner_token }).to_string();
    http::post_json_body(relay, &relay.hold_path(slot, seat_token), None, &body).await?;
    Ok(())
}

/// Put a held slot back on a relay that restarted and forgot it.
///
/// `Ok` also when the slot is already back. A `409` means the number went to
/// another session first, and a `410` that the session was ended.
pub async fn restore(
    relay: &Relay,
    bearer: &str,
    slot: Slot,
    seat_token: &str,
    owner_token: &str,
) -> Result<(), LinkError> {
    let body = serde_json::json!({
        "slot": slot.to_string(),
        "token": seat_token,
        "owner": owner_token,
    })
    .to_string();
    match http::post_json_body(relay, &relay.issue_path(), Some(bearer), &body).await {
        Ok(_) => Ok(()),
        Err(http::HttpError::Status(409)) => Err(LinkError::Taken),
        Err(http::HttpError::Status(410)) => Err(LinkError::Ended),
        Err(e) => Err(e.into()),
    }
}

impl Link {
    /// The owner's way back into a held session, with the token and secret
    /// from `Frame::Hold`.
    ///
    /// The owner token takes the chair directly: a held slot mints no tickets.
    /// A session that fell back to plain requests the first time goes straight
    /// to them again.
    pub async fn resume_owner(
        relay: &Relay,
        slot: Slot,
        owner_token: &str,
        secret: &[u8; 32],
        choice: Carry,
    ) -> Result<Self, LinkError> {
        if choice != Carry::Poll {
            match open_socket(relay, slot, owner_token).await {
                Ok(socket) => {
                    let transport = Transport::Socket(Box::new(socket));
                    return Self::secure_resume(transport, slot, secret, Role::Owner).await;
                }
                Err(Upgrade::Refused(e)) => return Err(e),
                Err(Upgrade::Failed(why)) if choice == Carry::Socket => {
                    return Err(LinkError::Socket(why))
                }
                Err(Upgrade::Failed(_)) => {}
            }
        }
        let polled = poll::Polled::open(relay, slot, owner_token).await?;
        let mut link =
            Self::secure_resume(Transport::Poll(polled), slot, secret, Role::Owner).await?;
        link.polled = Some("the WebSocket did not open".into());
        Ok(link)
    }

    /// The operator's way back into a held session.
    pub async fn resume_operator(
        relay: &Relay,
        slot: Slot,
        seat_token: &str,
        secret: &[u8; 32],
    ) -> Result<Self, LinkError> {
        let socket = open_socket(relay, slot, seat_token)
            .await
            .map_err(|e| match e {
                Upgrade::Refused(e) => e,
                Upgrade::Failed(why) => LinkError::Socket(why),
            })?;
        Self::secure_resume(
            Transport::Socket(Box::new(socket)),
            slot,
            secret,
            Role::Operator,
        )
        .await
    }

    async fn secure_resume(
        mut transport: Transport,
        slot: Slot,
        secret: &[u8; 32],
        role: Role,
    ) -> Result<Self, LinkError> {
        let (pending, ours) = handshake::start_resume(role, slot, secret);
        let channel = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            finish_exchange(&mut transport, pending, ours),
        )
        .await
        .map_err(|_| LinkError::NobodyThere)??;
        Ok(Self {
            transport,
            channel,
            polled: None,
        })
    }

    /// The owner's way in: claim a ticket, take the owner's chair, handshake.
    ///
    /// The claim is a separate plain request on purpose and must stay one. It
    /// is the only point on the path where the relay can still see the client's
    /// address, because the edge forwards none of that across an upgrade, so
    /// the per-address limit lives there or nowhere.
    ///
    /// Each attempt claims its own ticket, because a ticket is spent by the
    /// attempt whether or not the chair was taken.
    pub async fn dial_owner(relay: &Relay, code: &InviteCode) -> Result<Self, LinkError> {
        Self::dial_owner_via(relay, code, Carry::from_env()?).await
    }

    /// [`Link::dial_owner`] with the transport chosen here rather than by the
    /// environment.
    pub async fn dial_owner_via(
        relay: &Relay,
        code: &InviteCode,
        choice: Carry,
    ) -> Result<Self, LinkError> {
        let why = if choice == Carry::Poll {
            "TETHER_TRANSPORT=poll asks for that".to_string()
        } else {
            let ticket = claim(relay, code.slot).await?;
            match open_socket(relay, code.slot, &ticket).await {
                Ok(socket) => {
                    let transport = Transport::Socket(Box::new(socket));
                    return Self::secure(transport, code.slot, &code.phrase, Role::Owner).await;
                }
                Err(Upgrade::Refused(e)) => return Err(e),
                Err(Upgrade::Failed(why)) if choice == Carry::Socket => {
                    return Err(LinkError::Socket(why))
                }
                Err(Upgrade::Failed(why)) => format!("the WebSocket did not open ({why})"),
            }
        };

        let polled = poll_owner(relay, code.slot).await?;
        let mut link = Self::secure(
            Transport::Poll(polled),
            code.slot,
            &code.phrase,
            Role::Owner,
        )
        .await?;
        link.polled = Some(why);
        Ok(link)
    }

    /// The operator's way in: take the operator's chair with the seat token the
    /// relay handed back when it issued the slot.
    pub async fn dial_operator(
        relay: &Relay,
        slot: Slot,
        seat_token: &str,
        phrase: &Phrase,
    ) -> Result<Self, LinkError> {
        Self::open(relay, slot, seat_token, phrase, Role::Operator).await
    }

    async fn open(
        relay: &Relay,
        slot: Slot,
        token: &str,
        phrase: &Phrase,
        role: Role,
    ) -> Result<Self, LinkError> {
        let socket = open_socket(relay, slot, token).await.map_err(|e| match e {
            Upgrade::Refused(e) => e,
            Upgrade::Failed(why) => LinkError::Socket(why),
        })?;
        Self::secure(Transport::Socket(Box::new(socket)), slot, phrase, role).await
    }

    async fn secure(
        mut transport: Transport,
        slot: Slot,
        phrase: &Phrase,
        role: Role,
    ) -> Result<Self, LinkError> {
        let channel = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            exchange(&mut transport, slot, phrase, role),
        )
        .await
        .map_err(|_| LinkError::NobodyThere)??;
        Ok(Self {
            transport,
            channel,
            polled: None,
        })
    }

    pub fn role(&self) -> Role {
        self.channel.role()
    }

    /// Why this session runs over plain requests rather than a WebSocket, as
    /// the end of a sentence starting "because", or `None` when it does not.
    pub fn polled(&self) -> Option<&str> {
        self.polled.as_deref()
    }

    pub async fn send(&mut self, frame: &Frame) -> Result<(), LinkError> {
        let sealed = self.channel.seal(frame)?;
        self.transport.send(sealed).await
    }

    /// The next frame the peer sent.
    ///
    /// A frame that will not open is the end of the session and not something
    /// to skip past: the channel counts every frame in each direction, so one
    /// that fails to open means the stream is out of step and everything after
    /// it is untrustworthy.
    pub async fn recv(&mut self) -> Result<Frame, LinkError> {
        let sealed = self.transport.next().await?;
        Ok(self.channel.open(&sealed)?)
    }

    /// Say why, then go.
    ///
    /// Best-effort by design: this runs on paths where the socket may already
    /// be gone, and failing to announce a departure must not stop the departure.
    pub async fn cut(&mut self, reason: &str) {
        self.leave(&Frame::Cut {
            reason: reason.to_string(),
        })
        .await;
    }

    /// Send a last frame, then close.
    pub async fn leave(&mut self, last: &Frame) {
        let _ = self.send(last).await;
        self.close().await;
    }

    /// Close, and wait a moment for the far end to close too.
    ///
    /// Dropping a socket with unread bytes in its receive buffer makes the
    /// kernel reset the connection rather than end it, and a reset lets the
    /// relay's kernel discard what this end sent last and the relay had not
    /// yet read: the reason a session ended, most often. Reading until the
    /// far end's close arrives empties the buffer first.
    pub async fn close(&mut self) {
        self.transport.close().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while self.transport.next().await.is_ok() {}
        })
        .await;
    }
}

async fn claim(relay: &Relay, slot: Slot) -> Result<String, LinkError> {
    // A held slot answers 404 here as well, so a second run of the launcher
    // during a session is told to ask for a new code rather than let in.
    let answer = match http::post_json(relay, &relay.claim_path(slot), None).await {
        Err(http::HttpError::Status(404)) => return Err(LinkError::NoSuchSession),
        other => other?,
    };
    answer
        .get("token")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| http::HttpError::Malformed("no ticket in the claim".into()).into())
}

async fn open_socket(relay: &Relay, slot: Slot, token: &str) -> Result<Socket, Upgrade> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME + 1024))
        .max_frame_size(Some(MAX_FRAME + 1024));
    // A route that cannot be dialled at all would fail plain requests the same
    // way, so it is a refusal rather than a reason to poll.
    let route = Route::for_relay(relay).map_err(|e| Upgrade::Refused(e.into()))?;
    let stream = net::dial(relay, &route)
        .await
        .map_err(|e| Upgrade::Refused(e.into()))?;
    match tokio_tungstenite::client_async_with_config(
        relay.join_url(slot, token),
        stream,
        Some(config),
    )
    .await
    {
        Ok((socket, _)) => Ok(socket),
        Err(WsError::Http(response)) => match response.status().as_u16() {
            404 => Err(Upgrade::Refused(LinkError::NoSuchSession)),
            409 => Err(Upgrade::Refused(LinkError::Taken)),
            410 => Err(Upgrade::Refused(LinkError::Ended)),
            other => Err(Upgrade::Failed(format!("the path answered {other}"))),
        },
        Err(other) => Err(Upgrade::Failed(other.to_string())),
    }
}

/// Take the owner's chair for polling.
///
/// A socket that did open and was then cut by the path may have paired before
/// it died. The operator's chair is then still unwinding for a moment, and the
/// relay answers that as a taken seat — so that answer is tried again, briefly,
/// on a fresh ticket each time.
async fn poll_owner(relay: &Relay, slot: Slot) -> Result<poll::Polled, LinkError> {
    let mut tries = 0;
    loop {
        let ticket = claim(relay, slot).await?;
        match poll::Polled::open(relay, slot, &ticket).await {
            Err(LinkError::Taken) if tries < 4 => {
                tries += 1;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            other => return other,
        }
    }
}

/// The four messages, in order. See the module docs.
async fn exchange(
    transport: &mut Transport,
    slot: Slot,
    phrase: &Phrase,
    role: Role,
) -> Result<Channel, LinkError> {
    let (pending, ours) = handshake::start(role, slot, phrase);
    finish_exchange(transport, pending, ours).await
}

async fn finish_exchange(
    transport: &mut Transport,
    pending: handshake::Pending,
    ours: Vec<u8>,
) -> Result<Channel, LinkError> {
    transport.send(ours).await?;

    let theirs = transport.next().await?;
    let (confirming, our_tag) = pending.finish(&theirs)?;

    transport.send(our_tag.to_vec()).await?;

    let their_tag = transport.next().await?;
    Ok(confirming.confirm(&their_tag)?)
}

/// Read the next thing the peer actually said, skipping keepalives.
async fn next_binary(socket: &mut Socket) -> Result<Vec<u8>, LinkError> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Binary(bytes))) => return Ok(bytes.to_vec()),
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
            Some(Ok(Message::Frame(_))) | Some(Ok(Message::Text(_))) => {
                return Err(LinkError::Closed)
            }
            Some(Ok(Message::Close(_))) | None => return Err(LinkError::Closed),
            Some(Err(_)) => return Err(LinkError::Closed),
        }
    }
}
