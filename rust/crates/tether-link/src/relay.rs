//! Where the relay is, and the two URLs the owner's side ever builds.
//!
//! Compiled in rather than asked for. The owner already trusted this host far
//! enough to pipe a script from it into a shell, so a prompt asking them to
//! confirm the same name again would be ceremony. What matters is that the
//! address is *shown* before consent, which is [`crate::consent`]'s job.
//!
//! The base is overridable from the environment because a test needs a relay on
//! loopback and because a second deployment should not need a code change. It
//! is deliberately not a command-line argument: the code the owner types is the
//! code they were read, and an invite that could also carry a hostname is an
//! invite that could carry somebody else's.

use std::env;
use std::fmt;

use tether_proto::invite::Slot;

/// The relay this build points at when nothing says otherwise.
pub const DEFAULT_BASE: &str = "https://nexus.tony-xy-liu.com/tether";

/// The environment variable that overrides it.
pub const BASE_ENV: &str = "TETHER_RELAY";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relay {
    tls: bool,
    /// Host, with the port attached when there is one. Kept joined because
    /// every use is either a URL or a line on the owner's screen.
    authority: String,
    /// The mount the relay sits under, with a leading slash and no trailing
    /// one, or empty when it is at the root.
    prefix: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RelayError {
    NoScheme(String),
    NoHost(String),
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoScheme(s) => write!(f, "`{s}` does not start with http:// or https://"),
            Self::NoHost(s) => write!(f, "`{s}` names no host"),
        }
    }
}

impl std::error::Error for RelayError {}

impl Relay {
    /// Read the relay this run should use.
    pub fn from_env() -> Result<Self, RelayError> {
        let base = env::var(BASE_ENV).unwrap_or_else(|_| DEFAULT_BASE.to_string());
        Self::parse(&base)
    }

    /// Parse a base URL by hand.
    ///
    /// Hand-rolled because this accepts exactly one shape — scheme, authority,
    /// optional path prefix — and a general URL parser would accept a dozen
    /// more, each of which is a way for an address to be one thing on the
    /// owner's screen and another on the wire.
    pub fn parse(base: &str) -> Result<Self, RelayError> {
        let base = base.trim();
        let (tls, rest) = match base.split_once("://") {
            Some(("https", rest)) => (true, rest),
            Some(("http", rest)) => (false, rest),
            _ => return Err(RelayError::NoScheme(base.to_string())),
        };
        // Trailing slashes go after the scheme is off, not before: trimming
        // first turns `https://` into `https:`, which is then refused for
        // having no scheme — true in the end, and a misleading way to say it.
        let rest = rest.trim_end_matches('/');
        let (authority, path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, path),
            None => (rest, ""),
        };
        if authority.is_empty() || authority.contains(['@', '?', '#']) {
            return Err(RelayError::NoHost(base.to_string()));
        }
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("/{}", path.trim_matches('/'))
        };
        Ok(Self {
            tls,
            authority: authority.to_ascii_lowercase(),
            prefix,
        })
    }

    pub fn tls(&self) -> bool {
        self.tls
    }

    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// The host alone, for the TLS handshake and for the owner's screen.
    pub fn host(&self) -> &str {
        // An IPv6 authority keeps its brackets; nothing here builds one, and
        // splitting on the last colon would corrupt it if something did.
        match self.authority.rsplit_once(':') {
            Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
            _ => &self.authority,
        }
    }

    pub fn port(&self) -> u16 {
        match self.authority.rsplit_once(':') {
            Some((_, port)) => port.parse().unwrap_or(if self.tls { 443 } else { 80 }),
            None => {
                if self.tls {
                    443
                } else {
                    80
                }
            }
        }
    }

    /// The path the owner's claim goes to. A path, not a URL, because the
    /// request is written onto a socket this crate opened itself.
    pub fn claim_path(&self, slot: Slot) -> String {
        format!("{}/claim/{}", self.prefix, slot)
    }

    /// The socket URL, with the ticket in the path.
    ///
    /// The path is where a ticket has to ride: the edge forwards only three
    /// headers across a WebSocket upgrade, so a header would simply not arrive.
    /// Nothing secret is in here — the ticket is one attempt at one chair, and
    /// the phrase never leaves this machine.
    pub fn join_url(&self, slot: Slot, ticket: &str) -> String {
        let scheme = if self.tls { "wss" } else { "ws" };
        format!(
            "{scheme}://{}{}/join/{}/{}",
            self.authority, self.prefix, slot, ticket
        )
    }
}

impl fmt::Display for Relay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.authority, self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(n: u32) -> Slot {
        Slot::new(n).unwrap()
    }

    #[test]
    fn the_shipped_default_parses() {
        let r = Relay::parse(DEFAULT_BASE).unwrap();
        assert!(r.tls());
        assert_eq!(r.host(), "nexus.tony-xy-liu.com");
        assert_eq!(r.port(), 443);
        assert_eq!(r.claim_path(slot(7)), "/tether/claim/7");
        assert_eq!(
            r.join_url(slot(7), "abc"),
            "wss://nexus.tony-xy-liu.com/tether/join/7/abc"
        );
    }

    #[test]
    fn a_loopback_relay_with_a_port_and_no_prefix_works() {
        let r = Relay::parse("http://127.0.0.1:12520").unwrap();
        assert!(!r.tls());
        assert_eq!(r.host(), "127.0.0.1");
        assert_eq!(r.port(), 12520);
        assert_eq!(r.claim_path(slot(1)), "/claim/1");
        assert_eq!(r.join_url(slot(1), "t"), "ws://127.0.0.1:12520/join/1/t");
    }

    #[test]
    fn a_trailing_slash_does_not_double_up() {
        let a = Relay::parse("https://example.test/tether/").unwrap();
        let b = Relay::parse("https://example.test/tether").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.claim_path(slot(9)), "/tether/claim/9");
    }

    #[test]
    fn a_deep_prefix_is_kept_whole() {
        let r = Relay::parse("https://example.test/a/b/c").unwrap();
        assert_eq!(r.claim_path(slot(2)), "/a/b/c/claim/2");
    }

    #[test]
    fn something_that_is_not_a_base_url_is_refused() {
        assert!(matches!(
            Relay::parse("nexus.tony-xy-liu.com"),
            Err(RelayError::NoScheme(_))
        ));
        assert!(matches!(
            Relay::parse("ftp://example.test"),
            Err(RelayError::NoScheme(_))
        ));
        assert!(matches!(
            Relay::parse("https://"),
            Err(RelayError::NoHost(_))
        ));
        assert!(matches!(
            Relay::parse("https://someone@example.test"),
            Err(RelayError::NoHost(_))
        ));
    }

    #[test]
    fn the_display_is_what_the_owner_is_asked_to_trust() {
        let r = Relay::parse(DEFAULT_BASE).unwrap();
        assert_eq!(r.to_string(), "nexus.tony-xy-liu.com/tether");
    }
}
