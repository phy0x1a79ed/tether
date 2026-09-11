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

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, State};
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
pub mod pump;
pub mod rate;
pub mod session;
pub mod token;

use crate::config::{Config, MAX_MESSAGE};
use crate::rate::Limiter;
use crate::session::{IssueError, JoinError, Sessions};
use crate::token::{secret_eq, Token};

/// How often expired slots are swept.
///
/// The sweep is a backstop, not the mechanism: every path that reads the table
/// expires what it walks past first, so this only matters for a slot nobody
/// ever asks about again.
const REAP_INTERVAL: Duration = Duration::from_secs(5);

pub struct Relay {
    pub sessions: Arc<Sessions>,
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
        .route("/join/{slot}/{token}", get(join))
        // Twice on purpose. The address the owner is read out is the mount
        // itself — `https://…/tether` — and whether the edge hands that to
        // this router as `/` or as `/tether` is a property of how the mount
        // strips its prefix, not something the owner's line should depend on.
        .route("/", get(launcher))
        .route("/tether", get(launcher))
        .route("/bin/{name}", get(binary))
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
        "max_sessions": relay.config.max_sessions,
        "uptime_s": relay.started.elapsed().as_secs(),
        "assets": relay.config.assets.is_some(),
    }))
    .into_response()
}

/// Create a session. The one action that allocates anything, and the one action
/// no unauthenticated caller can reach.
async fn issue(State(relay): State<Arc<Relay>>, headers: HeaderMap) -> Response {
    if !relay.operator(&headers) {
        return nothing_here();
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
        Err(JoinError::Taken) | Err(JoinError::Busy) => {
            return (StatusCode::CONFLICT, "that seat is taken\n").into_response()
        }
    };
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| pump::run(seated, socket))
}

async fn launcher(State(relay): State<Arc<Relay>>, headers: HeaderMap) -> Response {
    serve_asset(
        &relay,
        &headers,
        &["tether"],
        "text/x-shellscript; charset=utf-8",
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
    let reaper = tokio::spawn(async move {
        let mut tick = tokio::time::interval(REAP_INTERVAL);
        loop {
            tick.tick().await;
            let gone = sessions.reap(Instant::now());
            if gone > 0 {
                log::info(format_args!("{gone} slot(s) expired unredeemed"));
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
