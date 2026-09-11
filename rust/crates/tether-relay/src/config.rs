//! The relay's tunables, and the reasoning behind the numbers.
//!
//! Every limit here exists because the join path is public. Read them together
//! with [`crate::session`]: the interesting ones are not independent knobs but
//! the three properties that make a two-word phrase safe to say out loud — a
//! session is one-shot, it expires in minutes, and it dies after a few
//! attempts.
//!
//! The durations live in a struct rather than in constants so a test can drive
//! an expiry in milliseconds. Nothing reads them from the environment: these
//! are design decisions, not deployment settings, and a host that could shorten
//! the failure limit could undo the reason the phrase is safe.

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use tether_proto::invite::MAX_SLOT;

/// The relay's loopback port.
///
/// Nothing reaches this directly. nginx terminates TLS, the public edge decides
/// what the internet may ask for, and only then does a request arrive here.
pub const DEFAULT_PORT: u16 = 12520;

/// The largest WebSocket message the relay will carry.
///
/// A sealed frame is one plaintext frame plus a 16-byte tag. The slack above
/// that is for the seal's framing, not for a caller to grow into.
pub const MAX_MESSAGE: usize = tether_proto::frame::MAX_FRAME + 1024;

/// How many addresses the per-address limiter will track at once.
///
/// Past this it refuses rather than growing, because a limiter that allocates
/// per source address is itself a flood target. Failing closed is the right
/// direction here: the cost is that a genuine owner retries.
pub const RATE_TABLE_CAP: usize = 4096;

/// The deadlines and counts that bound a session's life.
#[derive(Debug, Clone)]
pub struct Limits {
    /// From issuing a slot to the two ends finding each other.
    ///
    /// Short because an unredeemed slot is an open door, and long enough that
    /// the operator can read the code out and the owner can paste one command.
    pub issue_ttl: Duration,

    /// How long an owner's join ticket is good for. The launcher claims and
    /// upgrades in the same breath, so this only has to cover a slow network.
    pub ticket_ttl: Duration,

    /// How long a socket that has taken a seat may wait for the other end.
    ///
    /// Normally equal to `issue_ttl`: the operator's socket is what waits, and
    /// it should live exactly as long as the slot it is waiting on.
    pub unpaired_timeout: Duration,

    /// After pairing, how long a socket may stay silent before it is dropped.
    ///
    /// This is the "passed a real frame" deadline. Both ends send their half of
    /// the handshake the instant they are paired, so a socket that pairs and
    /// then says nothing is not a client of this service. A keepalive does not
    /// satisfy it; only a data message does.
    pub first_frame_timeout: Duration,

    /// After the first frame, how long a live session may go quiet.
    ///
    /// Well under nginx's one-hour cut and Cloudflare's shorter one, because
    /// the relay would rather end a dead session itself than have an
    /// intermediary do it at a moment neither end can explain.
    pub idle_timeout: Duration,

    /// How often the relay sends its own WebSocket ping on each leg.
    ///
    /// A session can be legitimately idle for a long time — the owner watching
    /// a long copy, the operator reading. Nothing else on the path will keep
    /// the socket open, so the relay does.
    pub keepalive: Duration,

    /// How long the relay waits for a socket's writer to drain after its reader
    /// has ended, before abandoning it.
    pub drain_grace: Duration,

    /// How many times one slot may pair before it is destroyed.
    ///
    /// This is the failure limit, and it is counted in *pairings* rather than
    /// in failed handshakes because the relay cannot see a handshake — it pumps
    /// sealed bytes it has no key for. A good session needs one pairing. A
    /// wrong phrase costs one. So a handful leaves room for a fumbled code and
    /// still closes the door long before guessing twenty bits is worth trying.
    pub max_pairings: u32,

    /// The per-address window for public requests, and what fits in one.
    pub rate_window: Duration,
    pub rate_burst: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            issue_ttl: Duration::from_secs(300),
            ticket_ttl: Duration::from_secs(60),
            unpaired_timeout: Duration::from_secs(300),
            first_frame_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(120),
            keepalive: Duration::from_secs(30),
            drain_grace: Duration::from_secs(5),
            max_pairings: 5,
            rate_window: Duration::from_secs(60),
            rate_burst: 30,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    /// The bearer that `/issue` and `/status` require. There is no unprotected
    /// mode: session creation is the one thing that must never be reachable by
    /// a caller who merely got as far as this process.
    pub issue_token: String,
    /// Where the launcher script and the client binaries live, if this host has
    /// them. Absent means those routes report that they do not exist, which is
    /// the honest answer on a host that has not been shipped to yet.
    pub assets: Option<PathBuf>,
    pub max_sessions: usize,
    /// The header carrying the real client address. Whatever sets it must be
    /// upstream of anything a caller controls; Cloudflare overwrites its own on
    /// every request, which is why that is the default. When the header is
    /// absent every such request shares one bucket, so the per-address limit
    /// degrades to a global one rather than to no limit at all.
    pub client_ip_header: String,
    pub build: String,
    pub limits: Limits,
}

#[derive(Debug)]
pub enum ConfigError {
    MissingIssueToken,
    BadPort(String),
    BadMaxSessions(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingIssueToken => write!(
                f,
                "AWM_TETHER_ISSUE_TOKEN is not set; the relay will not start without it"
            ),
            Self::BadPort(v) => write!(f, "AWM_TETHER_PORT is not a port number: {v}"),
            Self::BadMaxSessions(v) => write!(f, "AWM_TETHER_MAX_SESSIONS is not a number: {v}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let port = match env::var("AWM_TETHER_PORT") {
            Ok(v) => v.parse().map_err(|_| ConfigError::BadPort(v))?,
            Err(_) => DEFAULT_PORT,
        };
        let issue_token = env::var("AWM_TETHER_ISSUE_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .ok_or(ConfigError::MissingIssueToken)?;
        let max_sessions = match env::var("AWM_TETHER_MAX_SESSIONS") {
            Ok(v) => v.parse().map_err(|_| ConfigError::BadMaxSessions(v))?,
            Err(_) => 64usize,
        };
        Ok(Self {
            port,
            issue_token,
            assets: env::var_os("AWM_TETHER_ASSETS").map(PathBuf::from),
            // A slot is read aloud, so the namespace is three digits and the
            // session cap can never usefully exceed it.
            max_sessions: max_sessions.clamp(1, MAX_SLOT as usize),
            client_ip_header: env::var("AWM_TETHER_CLIENT_IP_HEADER")
                .unwrap_or_else(|_| "cf-connecting-ip".into())
                .to_ascii_lowercase(),
            build: option_env!("TETHER_BUILD").unwrap_or("unstamped").into(),
            limits: Limits::default(),
        })
    }
}
