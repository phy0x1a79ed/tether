//! Seat tokens: which of the two chairs at a slot a socket is allowed to take.
//!
//! A seat token is not what keeps the session private — the handshake does
//! that, and it would do it even if every token leaked. A token decides
//! something smaller and still worth deciding: that the operator's chair can
//! only be taken by whoever asked for the slot, and that the owner's chair can
//! only be taken by a caller who came through the rate-limited plain request
//! that precedes the upgrade.
//!
//! So these are 128 random bits, compared in constant time, and never logged.

use std::fmt;

use subtle::ConstantTimeEq;

const LEN: usize = 16;

#[derive(Clone, Copy)]
pub struct Token([u8; LEN]);

impl Token {
    pub fn mint() -> Self {
        let mut bytes = [0u8; LEN];
        getrandom::getrandom(&mut bytes).expect("the OS randomness source failed");
        Self(bytes)
    }

    /// Read a token out of a URL path segment.
    ///
    /// Strict about length and alphabet before it touches the value, so a
    /// malformed segment is refused by shape rather than by comparison.
    pub fn parse(s: &str) -> Option<Self> {
        if s.len() != LEN * 2 {
            return None;
        }
        let mut bytes = [0u8; LEN];
        hex::decode_to_slice(s, &mut bytes).ok()?;
        Some(Self(bytes))
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

impl PartialEq for Token {
    fn eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl Eq for Token {}

/// Redacted, because a token in a log line is a token in a log file.
impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(redacted)")
    }
}

/// Constant-time comparison of the configured bearer against what a caller
/// sent, including when the two differ in length.
pub fn secret_eq(expected: &str, got: &str) -> bool {
    let (a, b) = (expected.as_bytes(), got.as_bytes());
    // ct_eq is only defined for equal lengths, so fold the length into the
    // answer rather than returning early on it.
    let same_len = a.len() == b.len();
    let mut diff = 0u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0xff);
    }
    same_len && diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_token_survives_a_round_trip_through_a_url() {
        let t = Token::mint();
        assert_eq!(Token::parse(&t.to_hex()), Some(t));
    }

    #[test]
    fn two_minted_tokens_differ() {
        assert_ne!(Token::mint(), Token::mint());
    }

    #[test]
    fn a_malformed_segment_is_refused_by_shape() {
        assert!(Token::parse("").is_none());
        assert!(Token::parse("deadbeef").is_none());
        assert!(Token::parse(&"z".repeat(32)).is_none());
        assert!(Token::parse(&"0".repeat(33)).is_none());
        assert!(Token::parse("../../etc/passwd").is_none());
    }

    #[test]
    fn debug_does_not_print_the_token() {
        let t = Token::mint();
        assert!(!format!("{t:?}").contains(&t.to_hex()[..8]));
    }

    #[test]
    fn the_bearer_comparison_agrees_with_the_obvious_one() {
        assert!(secret_eq("hunter2", "hunter2"));
        assert!(!secret_eq("hunter2", "hunter3"));
        assert!(!secret_eq("hunter2", "hunter2 "));
        assert!(!secret_eq("hunter2", ""));
        assert!(secret_eq("", ""));
    }
}
