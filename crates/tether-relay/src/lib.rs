//! tether's relay: the public box in the middle, and the least of the three.
//!
//! It does one thing. It pairs two sockets that name the same slot and moves
//! bytes between them. It parses nothing about a session, holds no key, and
//! stores no content, so it could not read what it carries even under an order
//! to do so. That is the point of putting it on a machine exposed to the
//! internet: the exposed component is the one with nothing to take.
//!
//! # Why a stranger cannot make it do anything
//!
//! A session exists only because an authenticated operator asked for one. The
//! relay issues the slot, holds it in memory with a short expiry, and accepts a
//! socket only for a slot it issued. An unauthenticated caller cannot bring a
//! session into existence at all, so the flood surface is closed rather than
//! throttled — which is a different and much better thing than a rate limit.
//!
//! The limits that remain are for the join path, which is genuinely public:
//! a cap on concurrent sessions, a per-address budget on the plain leg, a
//! deadline on a socket that pairs and then says nothing, an expiry on a slot
//! nobody redeems, and a small budget of pairings after which the slot is
//! destroyed. That last one is the failure limit, and it is what makes a phrase
//! short enough to say aloud safe to use. See [`config::Limits`].
//!
//! # The shape of a refusal
//!
//! Everything the relay declines to do answers `404`, including a valid request
//! with the wrong bearer. A caller must not be able to map this service by the
//! shape of its refusals: a live slot and a slot that never existed are the same
//! answer, and so are an authenticated route and a path that is not there.
//!
//! The one exception is `410`, and only an owner can see it: a join that names
//! a held slot the operator ended, with that slot's owner token. It tells the
//! owner to stop redialling, and it tells anybody else nothing.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tether_proto::invite::Slot;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub mod assets;
pub mod config;
pub mod log;
pub mod poll;
pub mod pump;
pub mod rate;
pub mod room;
pub mod session;
pub mod token;

use crate::config::{Config, MAX_MESSAGE};
use crate::poll::{Polls, Recv, Sent};
use crate::rate::Limiter;
use crate::room::{Room, Rooms, SendError, Who, MAX_ROOM_MESSAGE};
use crate::session::{IssueError, JoinError, RestoreError, Sessions};
use crate::token::{secret_eq, Token};

/// How often expired slots are swept.
///
/// The sweep is a backstop, not the mechanism: every path that reads the table
/// expires what it walks past first, so this only matters for a slot nobody
/// ever asks about again.
const REAP_INTERVAL: Duration = Duration::from_secs(5);

pub struct Relay {
    pub sessions: Arc<Sessions>,
    pub polls: Arc<Polls>,
    pub rooms: Arc<Rooms>,
    limiter: Limiter,
    config: Config,
    started: Instant,
}

impl Relay {
    pub fn new(config: Config) -> Self {
        let sessions = Arc::new(Sessions::new(config.limits.clone(), config.max_sessions));
        let limiter = Limiter::new(config.limits.rate_window, config.limits.rate_burst);
        Self {
            sessions,
            polls: Arc::default(),
            rooms: Arc::new(Rooms::new(config.limits.clone())),
            limiter,
            config,
            started: Instant::now(),
        }
    }

    /// The bearer check for the two operator-only routes.
    fn operator(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|got| secret_eq(&self.config.issue_token, got))
            .unwrap_or(false)
    }

    /// The key a public request is counted against.
    ///
    /// When the configured header is absent every such request shares one
    /// bucket. That is a deliberate degradation to a global limit rather than
    /// to no limit: a misconfigured deployment should throttle everybody, not
    /// nobody.
    fn rate_key(&self, headers: &HeaderMap) -> String {
        headers
            .get(&self.config.client_ip_header)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty() && v.len() <= 64)
            .unwrap_or_else(|| "unattributed".into())
    }
}

/// Every refusal this service makes, so none of them can be told apart.
fn nothing_here() -> Response {
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

pub fn router(relay: Arc<Relay>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/issue", post(issue))
        .route("/claim/{slot}", post(claim))
        .route("/release/{slot}/{token}", post(release))
        .route("/hold/{slot}/{token}", post(hold))
        .route("/join/{slot}/{token}", get(join))
        // The same chair as `/join`, held through plain requests instead of a
        // socket. See [`poll`].
        .route("/poll/{slot}/{token}", post(poll_open))
        .route("/seat/{id}/recv", get(poll_recv))
        .route(
            "/seat/{id}/send",
            post(poll_send).layer(DefaultBodyLimit::max(poll::MAX_SEND)),
        )
        .route("/seat/{id}/close", post(poll_close))
        // Twice on purpose. The address the owner is read out is the mount
        // itself — `https://…/tether` — and whether the edge hands that to
        // this router as `/` or as `/tether` is a property of how the mount
        // strips its prefix, not something the owner's line should depend on.
        .route("/", get(launcher))
        .route("/tether", get(launcher))
        // Windows has no shell that can run the launcher above, so it gets its
        // own script at its own address. A route rather than a name under
        // `/bin`, because PowerShell is about to interpret what comes back and
        // `/bin` serves opaque bytes.
        .route("/win", get(launcher_windows))
        .route("/bin/{name}", get(binary))
        // Rooms, for an agent of another system. See [`room`].
        .route("/room", post(room_open))
        .route("/r/{name}", get(room_how_to))
        .route(
            "/r/{name}/send",
            post(room_send).layer(DefaultBodyLimit::max(MAX_ROOM_MESSAGE)),
        )
        .route("/r/{name}/recv", get(room_recv))
        .route("/r/{name}/ws", get(room_ws))
        .route("/r/{name}/leave", post(room_leave))
        // A path that is not here answers exactly as a refusal does.
        .fallback(|| async { nothing_here() })
        .with_state(relay)
}

/// Liveness only. It says nothing about sessions, so it is safe to leave
/// reachable by whatever supervises this process.
async fn health() -> &'static str {
    "ok\n"
}

async fn status(State(relay): State<Arc<Relay>>, headers: HeaderMap) -> Response {
    if !relay.operator(&headers) {
        return nothing_here();
    }
    Json(json!({
        "ok": true,
        "build": relay.config.build,
        "sessions": relay.sessions.live(),
        "polled": relay.polls.live(),
        "rooms": relay.rooms.live(),
        "max_sessions": relay.config.max_sessions,
        "uptime_s": relay.started.elapsed().as_secs(),
        "assets": relay.config.assets.is_some(),
    }))
    .into_response()
}

/// What an operator sends to `/issue` to put back a held slot after a restart.
#[derive(serde::Deserialize)]
struct Restore {
    slot: String,
    token: String,
    owner: String,
}

/// Create a session. The one action that allocates anything, and the one action
/// no unauthenticated caller can reach.
///
/// With a body, this restores a held slot the relay forgot instead. See
/// [`Sessions::restore`].
async fn issue(State(relay): State<Arc<Relay>>, headers: HeaderMap, body: Bytes) -> Response {
    if !relay.operator(&headers) {
        return nothing_here();
    }
    if !body.is_empty() {
        return restore(&relay, &body);
    }
    match relay.sessions.issue(Instant::now()) {
        Ok(issued) => {
            log::info(format_args!("issued slot {}", issued.slot));
            Json(json!({
                "slot": issued.slot.get(),
                "token": issued.token.to_hex(),
                "expires_in": issued.expires_in.as_secs(),
            }))
            .into_response()
        }
        Err(IssueError::Full) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "no slot is free right now\n",
        )
            .into_response(),
    }
}

fn restore(relay: &Relay, body: &[u8]) -> Response {
    let Ok(asked) = serde_json::from_slice::<Restore>(body) else {
        return (StatusCode::BAD_REQUEST, "not a restore request\n").into_response();
    };
    let (Ok(slot), Some(token), Some(owner)) = (
        Slot::parse(&asked.slot),
        Token::parse(&asked.token),
        Token::parse(&asked.owner),
    ) else {
        return (StatusCode::BAD_REQUEST, "not a restore request\n").into_response();
    };
    match relay.sessions.restore(slot, token, owner, Instant::now()) {
        Ok(()) => {
            log::info(format_args!("restored held slot {slot}"));
            Json(json!({
                "slot": slot.get(),
                "expires_in": relay.config.limits.resume_grace.as_secs(),
            }))
            .into_response()
        }
        Err(RestoreError::Taken) => (
            StatusCode::CONFLICT,
            "that slot belongs to another session\n",
        )
            .into_response(),
        Err(RestoreError::Ended) => (StatusCode::GONE, "that session ended\n").into_response(),
        Err(RestoreError::Full) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "no slot is free right now\n",
        )
            .into_response(),
    }
}

/// Take a ticket for the owner's chair at an existing slot.
///
/// This is the plain request that has to happen before the upgrade, and the
/// reason it exists is [`crate::rate`]: it is the last point on the path where
/// the real client address is still a thing the relay can see.
async fn claim(
    State(relay): State<Arc<Relay>>,
    Path(slot): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !relay
        .limiter
        .allow(&relay.rate_key(&headers), Instant::now())
    {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests\n").into_response();
    }
    let Ok(slot) = Slot::parse(&slot) else {
        return nothing_here();
    };
    match relay.sessions.claim(slot, Instant::now()) {
        Some((ticket, ttl)) => Json(json!({
            "token": ticket.to_hex(),
            "expires_in": ttl.as_secs(),
        }))
        .into_response(),
        None => nothing_here(),
    }
}

/// End a slot, because the operator that owns it is finished with it, or
/// because the owner of a held slot is.
///
/// The seat token in the path is the authorisation, exactly as it is on
/// `/join`: it is what this relay handed back when it issued the slot, and
/// nobody else has it. There is no bearer check on top, because possession of
/// that token already permits occupying the chair, which ends the session by
/// another route.
///
/// Rate-limited like `/claim`, so a caller guessing tokens is spending its
/// address's budget rather than scanning freely.
async fn release(
    State(relay): State<Arc<Relay>>,
    Path((slot, token)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if !relay
        .limiter
        .allow(&relay.rate_key(&headers), Instant::now())
    {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests\n").into_response();
    }
    let (Ok(slot), Some(token)) = (Slot::parse(&slot), Token::parse(&token)) else {
        return nothing_here();
    };
    // A wrong token and an unknown slot answer identically, like every other
    // refusal on this surface: the shape of a decline must not map it.
    if !relay.sessions.destroy(slot, token, Instant::now()) {
        return nothing_here();
    }
    log::info(format_args!("released slot {slot}"));
    Json(json!({"released": slot.get()})).into_response()
}

/// What the operator sends to `/hold`.
#[derive(serde::Deserialize)]
struct Hold {
    owner: String,
}

/// Hold a slot for its consented owner. See [`Sessions::hold`].
///
/// Authorised by the seat token in the path, as `/release` is. The owner token
/// rides in the body, because it is what lets the owner back in and a path
/// lands in every access log on the way here.
async fn hold(
    State(relay): State<Arc<Relay>>,
    Path((slot, token)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !relay
        .limiter
        .allow(&relay.rate_key(&headers), Instant::now())
    {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests\n").into_response();
    }
    let (Ok(slot), Some(token)) = (Slot::parse(&slot), Token::parse(&token)) else {
        return nothing_here();
    };
    let Some(owner) = serde_json::from_slice::<Hold>(&body)
        .ok()
        .and_then(|h| Token::parse(&h.owner))
    else {
        return nothing_here();
    };
    if !relay.sessions.hold(slot, token, owner, Instant::now()) {
        return nothing_here();
    }
    log::info(format_args!("holding slot {slot} for its owner"));
    Json(json!({
        "held": slot.get(),
        "grace": relay.config.limits.resume_grace.as_secs(),
    }))
    .into_response()
}

fn ended() -> Response {
    (StatusCode::GONE, "that session ended\n").into_response()
}

/// Take a chair and start carrying bytes.
///
/// The chair is taken *before* the upgrade, so a slot the relay never issued is
/// refused as a status code and never becomes a socket at all.
async fn join(
    State(relay): State<Arc<Relay>>,
    Path((slot, token)): Path<(String, String)>,
    ws: WebSocketUpgrade,
) -> Response {
    let (Ok(slot), Some(token)) = (Slot::parse(&slot), Token::parse(&token)) else {
        return nothing_here();
    };
    let seated = match pump::seat(Arc::clone(&relay.sessions), slot, token) {
        Ok(seated) => seated,
        Err(JoinError::Unknown) => return nothing_here(),
        Err(JoinError::Ended) => return ended(),
        Err(JoinError::Taken) | Err(JoinError::Busy) => {
            return (StatusCode::CONFLICT, "that seat is taken\n").into_response()
        }
    };
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| pump::run(seated, socket))
}

/// Take a chair for a client that will poll for it rather than hold a socket.
///
/// Refused exactly as `/join` refuses, and for the same reasons.
async fn poll_open(
    State(relay): State<Arc<Relay>>,
    Path((slot, token)): Path<(String, String)>,
) -> Response {
    let (Ok(slot), Some(token)) = (Slot::parse(&slot), Token::parse(&token)) else {
        return nothing_here();
    };
    match pump::seat(Arc::clone(&relay.sessions), slot, token) {
        Ok(seated) => Json(json!({ "id": relay.polls.open(seated) })).into_response(),
        Err(JoinError::Unknown) => nothing_here(),
        Err(JoinError::Ended) => ended(),
        Err(JoinError::Taken) | Err(JoinError::Busy) => {
            (StatusCode::CONFLICT, "that seat is taken\n").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct Ack {
    #[serde(default)]
    ack: u64,
}

#[derive(serde::Deserialize)]
struct Seq {
    #[serde(default)]
    seq: u64,
}

async fn poll_recv(
    State(relay): State<Arc<Relay>>,
    Path(id): Path<String>,
    Query(Ack { ack }): Query<Ack>,
) -> Response {
    match relay.polls.recv(&id, ack).await {
        Recv::Frames(body) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            body,
        )
            .into_response(),
        Recv::Gone => nothing_here(),
    }
}

async fn poll_send(
    State(relay): State<Arc<Relay>>,
    Path(id): Path<String>,
    Query(Seq { seq }): Query<Seq>,
    body: Bytes,
) -> Response {
    match relay.polls.send(&id, seq, &body).await {
        Sent::Next(next) => Json(json!({ "next": next })).into_response(),
        Sent::Gone => nothing_here(),
        Sent::Malformed => (StatusCode::BAD_REQUEST, "not a run of frames\n").into_response(),
    }
}

async fn poll_close(State(relay): State<Arc<Relay>>, Path(id): Path<String>) -> Response {
    if relay.polls.close(&id) {
        Json(json!({ "closed": true })).into_response()
    } else {
        nothing_here()
    }
}

async fn launcher(State(relay): State<Arc<Relay>>, headers: HeaderMap) -> Response {
    serve_asset(
        &relay,
        &headers,
        &["tether"],
        "text/x-shellscript; charset=utf-8",
    )
}

async fn launcher_windows(State(relay): State<Arc<Relay>>, headers: HeaderMap) -> Response {
    serve_asset(
        &relay,
        &headers,
        &["tether.ps1"],
        "text/plain; charset=utf-8",
    )
}

async fn binary(
    State(relay): State<Arc<Relay>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    serve_asset(
        &relay,
        &headers,
        &["bin", name.as_str()],
        "application/octet-stream",
    )
}

fn serve_asset(
    relay: &Relay,
    headers: &HeaderMap,
    segments: &[&str],
    content_type: &'static str,
) -> Response {
    if !relay
        .limiter
        .allow(&relay.rate_key(headers), Instant::now())
    {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests\n").into_response();
    }
    match assets::read(relay.config.assets.as_deref(), segments) {
        Ok(body) => (
            [
                (header::CONTENT_TYPE, content_type),
                // The owner runs whatever comes back, once. A cached copy of
                // last month's build is worse than the bytes it saves.
                (header::CACHE_CONTROL, "no-store"),
            ],
            Bytes::from(body),
        )
            .into_response(),
        Err(_) => nothing_here(),
    }
}

/// Open a room. Operator only, like `/issue`.
async fn room_open(State(relay): State<Arc<Relay>>, headers: HeaderMap) -> Response {
    if !relay.operator(&headers) {
        return nothing_here();
    }
    match relay.rooms.open(Instant::now()) {
        Some((room, host)) => {
            // The name is a capability, like a seat token, so it is not logged.
            log::info(format_args!("opened a room"));
            Json(json!({
                "name": room.name,
                "host": host.to_hex(),
                "idle_s": relay.config.limits.room_idle.as_secs(),
                "life_s": relay.config.limits.room_life.as_secs(),
            }))
            .into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "no room is free right now\n",
        )
            .into_response(),
    }
}

/// The room a request names, and which side is asking.
///
/// A name that is no room spends the caller's rate budget. A caller with no
/// budget left is refused for every name, a live one included, or running out
/// of budget would leave guessing free and the answers still telling. A host
/// token that is not this room's is the same miss as a name that is no room.
fn room_for(
    relay: &Relay,
    headers: &HeaderMap,
    name: &str,
) -> Result<(Arc<Room>, Who), Box<Response>> {
    let now = Instant::now();
    let key = relay.rate_key(headers);
    if relay.limiter.spent(&key, now) {
        return Err(Box::new(
            (StatusCode::TOO_MANY_REQUESTS, "too many requests\n").into_response(),
        ));
    }
    let miss = || {
        relay.limiter.allow(&key, now);
        Box::new(nothing_here())
    };
    let Some(room) = relay.rooms.get(name, now) else {
        return Err(miss());
    };
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match bearer {
        None => Ok((room, Who::Guest)),
        Some(got) => match Token::parse(got) {
            Some(token) if room.is_host(&token) => {
                room.host_seen(now);
                Ok((room, Who::Host))
            }
            _ => Err(miss()),
        },
    }
}

fn room_ended(why: room::Ended) -> Response {
    (StatusCode::GONE, Json(json!({ "ended": why.as_str() }))).into_response()
}

async fn room_how_to(
    State(relay): State<Arc<Relay>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (room, _) = match room_for(&relay, &headers, &name) {
        Ok(found) => found,
        Err(refused) => return *refused,
    };
    // Reading the how-to is a guest's first contact, so it is counted as the
    // claim of a tether chair is.
    if !relay
        .limiter
        .allow(&relay.rate_key(&headers), Instant::now())
    {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests\n").into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        room::how_to(&room.name),
    )
        .into_response()
}

async fn room_send(
    State(relay): State<Arc<Relay>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (room, who) = match room_for(&relay, &headers, &name) {
        Ok(found) => found,
        Err(refused) => return *refused,
    };
    match room.post(who, &body, Instant::now()) {
        Ok(seq) => Json(json!({ "seq": seq })).into_response(),
        Err(SendError::Malformed) => (
            StatusCode::BAD_REQUEST,
            "send one JSON object with a \"from\" of 1 to 64 characters\n",
        )
            .into_response(),
        Err(SendError::TooLarge) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "a message is at most 65536 bytes\n",
        )
            .into_response(),
        Err(SendError::Busy) => (
            StatusCode::TOO_MANY_REQUESTS,
            "this room is taking too many messages\n",
        )
            .into_response(),
        Err(SendError::Ended) => room_ended(room.ended().expect("an ended room has a reason")),
    }
}

#[derive(serde::Deserialize)]
struct RoomRecv {
    #[serde(default)]
    after: u64,
    #[serde(default)]
    wait: u64,
}

async fn room_recv(
    State(relay): State<Arc<Relay>>,
    Path(name): Path<String>,
    Query(RoomRecv { after, wait }): Query<RoomRecv>,
    headers: HeaderMap,
) -> Response {
    let (room, who) = match room_for(&relay, &headers, &name) {
        Ok(found) => found,
        Err(refused) => return *refused,
    };
    let batch = room.wait(after, Duration::from_secs(wait)).await;
    if who == Who::Host {
        room.host_seen(Instant::now());
    }
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "messages": batch.messages,
            "next": batch.next,
            "ended": batch.ended.map(|why| why.as_str()),
        })),
    )
        .into_response()
}

async fn room_leave(
    State(relay): State<Arc<Relay>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (room, who) = match room_for(&relay, &headers, &name) {
        Ok(found) => found,
        Err(refused) => return *refused,
    };
    let why = match who {
        Who::Host => room::Ended::HostClosed,
        Who::Guest => room::Ended::GuestLeft,
    };
    room.end(why, Instant::now());
    Json(json!({ "ended": room.ended().unwrap_or(why).as_str() })).into_response()
}

#[derive(serde::Deserialize)]
struct After {
    #[serde(default)]
    after: u64,
}

async fn room_ws(
    State(relay): State<Arc<Relay>>,
    Path(name): Path<String>,
    Query(After { after }): Query<After>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let (room, who) = match room_for(&relay, &headers, &name) {
        Ok(found) => found,
        Err(refused) => return *refused,
    };
    let keepalive = relay.config.limits.keepalive;
    ws.max_message_size(MAX_ROOM_MESSAGE)
        .max_frame_size(MAX_ROOM_MESSAGE)
        .on_upgrade(move |socket| room_socket(room, who, after, socket, keepalive))
}

/// One side of a room held over a websocket: each message out as a text frame,
/// each text frame in as a send.
async fn room_socket(
    room: Arc<Room>,
    who: Who,
    mut after: u64,
    socket: axum::extract::ws::WebSocket,
    keepalive: Duration,
) {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};

    let (mut sink, mut stream) = socket.split();
    let mut tick = tokio::time::interval(keepalive);
    tick.tick().await;
    loop {
        let changed = room.changed().notified();
        tokio::pin!(changed);
        changed.as_mut().enable();

        let batch = room.read(after);
        for message in batch.messages {
            if sink
                .send(Message::Text(message.to_string().into()))
                .await
                .is_err()
            {
                return;
            }
        }
        after = batch.next;
        if let Some(why) = batch.ended {
            let last = json!({ "ended": why.as_str() }).to_string();
            let _ = sink.send(Message::Text(last.into())).await;
            let _ = sink.send(Message::Close(None)).await;
            return;
        }

        tokio::select! {
            _ = &mut changed => {}
            _ = tick.tick() => {
                if who == Who::Host {
                    room.host_seen(Instant::now());
                }
                if sink.send(Message::Ping(Bytes::new())).await.is_err() {
                    return;
                }
            }
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if let Err(e) = room.post(who, text.as_bytes(), Instant::now()) {
                        let why = match e {
                            SendError::Malformed => "send one JSON object with a \"from\" of 1 to 64 characters",
                            SendError::TooLarge => "a message is at most 65536 bytes",
                            SendError::Busy => "this room is taking too many messages",
                            SendError::Ended => continue,
                        };
                        let refused = json!({ "refused": why }).to_string();
                        if sink.send(Message::Text(refused.into())).await.is_err() {
                            return;
                        }
                    }
                }
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Binary(_))) => {
                    let refused = json!({ "refused": "send text frames of JSON" }).to_string();
                    if sink.send(Message::Text(refused.into())).await.is_err() {
                        return;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
            },
        }
    }
}

/// A bound relay, and the task serving it.
pub struct Running {
    pub addr: SocketAddr,
    pub relay: Arc<Relay>,
    pub server: JoinHandle<()>,
    pub reaper: JoinHandle<()>,
}

impl Running {
    pub fn stop(self) {
        self.server.abort();
        self.reaper.abort();
    }
}

/// Bind the loopback listener and start serving.
///
/// Loopback only, and not configurable. Everything that reaches this process
/// has already been through nginx and the public edge's allow-list, and a relay
/// that could be told to bind an external address is one environment variable
/// away from skipping both.
pub async fn start(config: Config) -> std::io::Result<Running> {
    let listener = TcpListener::bind(("127.0.0.1", config.port)).await?;
    let addr = listener.local_addr()?;
    let relay = Arc::new(Relay::new(config));
    let app = router(Arc::clone(&relay));

    let sessions = Arc::clone(&relay.sessions);
    let rooms = Arc::clone(&relay.rooms);
    let reaper = tokio::spawn(async move {
        let mut tick = tokio::time::interval(REAP_INTERVAL);
        loop {
            tick.tick().await;
            let gone = sessions.reap(Instant::now());
            if gone > 0 {
                log::info(format_args!("{gone} slot(s) expired unredeemed"));
            }
            let gone = rooms.reap(Instant::now());
            if gone > 0 {
                log::info(format_args!("{gone} room(s) ended and forgotten"));
            }
        }
    });

    let server = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            log::warn(format_args!("the listener stopped: {e}"));
        }
    });

    Ok(Running {
        addr,
        relay,
        server,
        reaper,
    })
}
