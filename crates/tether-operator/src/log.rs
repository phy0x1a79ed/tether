//! Log lines, hand-rolled and few — for the same reason the relay's are.
//!
//! This side holds the thing that must never reach a file: the phrase. It is
//! the whole credential, and a logging framework is an invitation to `{:?}` a
//! struct that contains one. Writing every line by hand means the set of things
//! that can reach this daemon's log is the set of things spelled out at a call
//! site, and a slot number is the only session fact any of them names.
//!
//! The adapter redirects this process's standard error to the service's log
//! file, so there is no file handling here either.

use std::fmt::Arguments;
use std::time::{SystemTime, UNIX_EPOCH};

fn stamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn info(args: Arguments<'_>) {
    eprintln!("[{}] tether-operator: {}", stamp(), args);
}

pub fn warn(args: Arguments<'_>) {
    eprintln!("[{}] tether-operator: WARN {}", stamp(), args);
}
