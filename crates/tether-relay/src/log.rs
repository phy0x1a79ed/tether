//! Log lines, hand-rolled and few.
//!
//! There is no logging framework here, and that is a security decision rather
//! than a taste one. A framework invites `#[instrument]` and `{:?}` on whatever
//! is in scope, and what is in scope on this path includes seat tokens. Writing
//! every line by hand means the set of things that can reach a log file is the
//! set of things spelled out at a call site.
//!
//! A slot number may be logged: it is public by design. Nothing else about a
//! session ever is, and there is nothing else to log — the relay does not hold
//! a phrase, a key, or a byte of content.

use std::fmt::Arguments;
use std::time::{SystemTime, UNIX_EPOCH};

fn stamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn info(args: Arguments<'_>) {
    eprintln!("[{}] tether-relay: {}", stamp(), args);
}

pub fn warn(args: Arguments<'_>) {
    eprintln!("[{}] tether-relay: WARN {}", stamp(), args);
}
