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

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tether_proto::frame::{Frame, Role, MAX_FRAME};
use tether_proto::handshake::{self, Channel, HandshakeError};
use tether_proto::invite::{InviteCode, Phrase, Slot};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub mod http;
pub mod relay;

pub use relay::{Relay, RelayError};

/// How long the opening exchange may take.
///
/// Generous, because the relay opens a socket the moment a chair is taken and
/// the other end may not be there yet: the owner can be reading the code out
/// while the operator's side waits. Bounded, because a socket that pairs and
/// then says nothing is not somebody having trouble typing.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(90);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug)]
pub enum LinkError {
    Relay(RelayError),
    Http(http::HttpError),
    /// The socket would not open. Carries the status when the relay answered
    /// with one: `404` is a slot that is not there, `409` a chair already taken.
    Socket(String),
    /// Nobody completed the exchange from the other side in time.
    NobodyThere,
    /// The peer's half of the exchange did not check out. In practice this is
    /// a mistyped phrase, and it is worth saying so rather than reporting a
    /// cryptographic failure to somebody trying to get help with a backup.
    Handshake(HandshakeError),
    Closed,
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Relay(e) => write!(f, "{e}"),
            Self::Http(e) => write!(f, "{e}"),
            Self::Socket(e) => write!(f, "the relay would not open a socket: {e}"),
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
        }
    }
}

impl std::error::Error for LinkError {}

impl From<HandshakeError> for LinkError {
    fn from(e: HandshakeError) -> Self {
        Self::Handshake(e)
    }
}

impl From<http::HttpError> for LinkError {
    fn from(e: http::HttpError) -> Self {
        Self::Http(e)
    }
}

/// A sealed session with the other end.
pub struct Link {
    socket: Socket,
    channel: Channel,
}

impl Link {
    /// The owner's way in: claim a ticket, take the owner's chair, handshake.
    ///
    /// The claim is a separate plain request on purpose and must stay one. It
    /// is the only point on the path where the relay can still see the client's
    /// address, because the edge forwards none of that across an upgrade, so
    /// the per-address limit lives there or nowhere.
    pub async fn dial_owner(relay: &Relay, code: &InviteCode) -> Result<Self, LinkError> {
        let answer = http::post_json(relay, &relay.claim_path(code.slot), None).await?;
        let ticket = answer
            .get("token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| http::HttpError::Malformed("no ticket in the claim".into()))?;
        Self::open(relay, code.slot, ticket, &code.phrase, Role::Owner).await
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
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME + 1024))
            .max_frame_size(Some(MAX_FRAME + 1024));
        let (mut socket, _) = tokio_tungstenite::connect_async_with_config(
            relay.join_url(slot, token),
            Some(config),
            false,
        )
        .await
        .map_err(|e| LinkError::Socket(describe(e)))?;

        let channel =
            tokio::time::timeout(HANDSHAKE_TIMEOUT, exchange(&mut socket, slot, phrase, role))
                .await
                .map_err(|_| LinkError::NobodyThere)??;

        Ok(Self { socket, channel })
    }

    pub fn role(&self) -> Role {
        self.channel.role()
    }

    pub async fn send(&mut self, frame: &Frame) -> Result<(), LinkError> {
        let sealed = self.channel.seal(frame)?;
        self.socket
            .send(Message::Binary(sealed.into()))
            .await
            .map_err(|_| LinkError::Closed)
    }

    /// The next frame the peer sent.
    ///
    /// A frame that will not open is the end of the session and not something
    /// to skip past: the channel counts every frame in each direction, so one
    /// that fails to open means the stream is out of step and everything after
    /// it is untrustworthy.
    pub async fn recv(&mut self) -> Result<Frame, LinkError> {
        let sealed = next_binary(&mut self.socket).await?;
        Ok(self.channel.open(&sealed)?)
    }

    /// Say why, then go.
    ///
    /// Best-effort by design: this runs on paths where the socket may already
    /// be gone, and failing to announce a departure must not stop the departure.
    pub async fn cut(&mut self, reason: &str) {
        let _ = self
            .send(&Frame::Cut {
                reason: reason.to_string(),
            })
            .await;
        let _ = self.socket.close(None).await;
    }
}

/// The four messages, in order. See the module docs.
async fn exchange(
    socket: &mut Socket,
    slot: Slot,
    phrase: &Phrase,
    role: Role,
) -> Result<Channel, LinkError> {
    let (pending, ours) = handshake::start(role, slot, phrase);
    socket
        .send(Message::Binary(ours.into()))
        .await
        .map_err(|_| LinkError::Closed)?;

    let theirs = next_binary(socket).await?;
    let (confirming, our_tag) = pending.finish(&theirs)?;

    socket
        .send(Message::Binary(our_tag.to_vec().into()))
        .await
        .map_err(|_| LinkError::Closed)?;

    let their_tag = next_binary(socket).await?;
    Ok(confirming.confirm(&their_tag)?)
}

fn describe(e: WsError) -> String {
    match e {
        WsError::Http(response) => match response.status().as_u16() {
            404 => "there is no such session — the code may have expired".into(),
            409 => "somebody is already in that seat".into(),
            other => format!("the relay answered {other}"),
        },
        other => other.to_string(),
    }
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
