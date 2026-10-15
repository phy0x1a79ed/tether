//! A per-address budget for the public requests.
//!
//! This lives on the plain request leg and nowhere else, and the reason is a
//! property of the path rather than a preference. The edge forwards only three
//! headers across a WebSocket upgrade — cookie, authorization and origin — so
//! there is no forwarded client address on the socket at all. An address-keyed
//! limit applied there would be keyed on the same proxy every time.
//!
//! So the public flow is arranged to put a plain request in front of the
//! upgrade: the owner's launcher claims a ticket, and that claim is what is
//! counted here. A caller cannot skip it, because the socket will not open
//! without the ticket the claim returns.
//!
//! A fixed window rather than a token bucket: the numbers involved are a
//! handful of requests a minute from one person running one command, and a
//! window that a determined caller can straddle is not the threat. What the
//! threat is, is volume, and a window bounds volume.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::RATE_TABLE_CAP;

pub struct Limiter {
    inner: Mutex<HashMap<String, Window>>,
    window: Duration,
    burst: u32,
}

struct Window {
    started: Instant,
    used: u32,
}

impl Limiter {
    pub fn new(window: Duration, burst: u32) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            window,
            burst,
        }
    }

    /// Spend one request against `key`. False means refuse the request.
    pub fn allow(&self, key: &str, now: Instant) -> bool {
        let mut table = self.inner.lock().unwrap();
        table.retain(|_, w| now.duration_since(w.started) < self.window);

        match table.get_mut(key) {
            Some(window) => {
                if window.used >= self.burst {
                    return false;
                }
                window.used += 1;
                true
            }
            None => {
                // A limiter that allocates per source address is itself a flood
                // target, so past the cap it refuses rather than grows. Failing
                // closed costs a genuine owner a retry; failing open costs the
                // host its memory.
                if table.len() >= RATE_TABLE_CAP {
                    return false;
                }
                table.insert(
                    key.to_string(),
                    Window {
                        started: now,
                        used: 1,
                    },
                );
                true
            }
        }
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_is_allowed_and_then_refused() {
        let l = Limiter::new(Duration::from_secs(60), 3);
        let now = Instant::now();
        for _ in 0..3 {
            assert!(l.allow("1.2.3.4", now));
        }
        assert!(!l.allow("1.2.3.4", now));
    }

    #[test]
    fn one_address_running_out_does_not_touch_another() {
        let l = Limiter::new(Duration::from_secs(60), 1);
        let now = Instant::now();
        assert!(l.allow("1.2.3.4", now));
        assert!(!l.allow("1.2.3.4", now));
        assert!(l.allow("5.6.7.8", now));
    }

    #[test]
    fn the_budget_comes_back_with_the_next_window() {
        let l = Limiter::new(Duration::from_secs(60), 1);
        let now = Instant::now();
        assert!(l.allow("1.2.3.4", now));
        assert!(!l.allow("1.2.3.4", now));
        assert!(l.allow("1.2.3.4", now + Duration::from_secs(61)));
    }

    #[test]
    fn an_expired_window_stops_being_tracked() {
        let l = Limiter::new(Duration::from_secs(60), 5);
        let now = Instant::now();
        l.allow("1.2.3.4", now);
        assert_eq!(l.tracked(), 1);
        l.allow("5.6.7.8", now + Duration::from_secs(61));
        assert_eq!(l.tracked(), 1);
    }

    #[test]
    fn the_table_refuses_rather_than_growing_without_bound() {
        let l = Limiter::new(Duration::from_secs(60), 5);
        let now = Instant::now();
        for i in 0..RATE_TABLE_CAP {
            assert!(l.allow(&format!("addr-{i}"), now));
        }
        assert!(!l.allow("one-too-many", now));
        // An address already being tracked still gets its own budget.
        assert!(l.allow("addr-0", now));
    }
}
