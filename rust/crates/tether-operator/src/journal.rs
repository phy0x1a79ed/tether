//! Everything that happened, in order, for anyone who asks.
//!
//! A session used to be observable only by the caller that started it: `run`
//! blocked, collected the output into a buffer, and handed it back down the
//! socket to exactly one listener. Nothing else could watch, nothing could
//! catch up afterwards, and a second caller saw nothing at all. This module is
//! the replacement. Facts are appended here as they happen, and readers arrive,
//! leave and come back without the daemon caring how many there are.
//!
//! # The phrase cannot get in here
//!
//! This side of the tool holds the one thing that must never reach a file or a
//! subscriber: the phrase. `log.rs` keeps it out of the daemon's own log by
//! being hand-rolled, so that no formatter can ever print a struct that happens
//! to contain it. This module needs the same property and takes the same
//! approach one step further — it will not name the types that carry the
//! secret. There is no function here that accepts a session's state, an invite
//! code or a phrase, so a caller with a secret in hand has nothing to pass it
//! to. Events are addressed by **slot**, which is public by design and already
//! travels in a URL.
//!
//! That matters most for the event announcing a new invite. The line the owner
//! is told to run contains the phrase in plain text, and these events fan out
//! to every subscriber of a topic. The slot goes in; the code and the command
//! do not.
//!
//! # One sequence for the whole daemon, and why it needs an epoch
//!
//! Events are numbered across the daemon rather than per session, so a reader
//! holds one cursor rather than one per slot, and so facts that belong to no
//! session yet still have a place in the order.
//!
//! A sequence number means nothing across a restart. A reader holding cursor
//! 400 that reconnects to a daemon which has just started from zero would
//! either wait forever for a number that is not coming or replay a stranger's
//! history as if it were its own. So a daemon stamps every stream and every
//! read with the epoch it minted at startup, and a reader whose epoch has
//! changed knows to begin again rather than guess.
//!
//! # Losing history is allowed; losing it silently is not
//!
//! The ring is bounded in bytes rather than in events, because command output
//! dominates and one chunk is already thirty-two kilobytes. When a reader asks
//! for a cursor the ring has moved past, it is told so as an event of its own
//! rather than handed the remainder as though nothing were missing.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::broadcast;

/// A position in this daemon's stream of events.
pub type Seq = u64;

/// How many events the ring holds before the oldest is dropped.
const MAX_EVENTS: usize = 8192;

/// How many bytes of event payload it holds, which is the binding limit in
/// practice. Deliberately larger than the adapter's own buffer: the adapter
/// refills from here after it restarts, and a smaller ring upstream would turn
/// a gap that should have been transient into a permanent one.
const MAX_BYTES: usize = 16 * 1024 * 1024;

/// The most output one event carries. A larger chunk is split across several,
/// in order, so no single record can dominate the ring.
pub const MAX_EVENT_DATA: usize = 32 * 1024;

/// What an event costs the ring when it carries no payload.
const OVERHEAD: usize = 256;

/// How many events one read returns at most.
pub const BATCH: usize = 512;

/// One thing that happened.
#[derive(Debug)]
pub struct Record {
    pub seq: Seq,
    /// Wall clock, in milliseconds. The daemon measures durations on a
    /// monotonic clock, but a reader writing a log wants a date.
    pub at_ms: u64,
    /// The session this belongs to, by its public slot. `None` for facts about
    /// the daemon itself.
    pub slot: Option<u32>,
    pub kind: String,
    body: Value,
    weight: usize,
}

impl Record {
    /// The whole event, as a reader receives it.
    pub fn to_json(&self) -> Value {
        let mut out = self.body.clone();
        if let Some(map) = out.as_object_mut() {
            map.insert("type".into(), json!(self.kind));
            map.insert("seq".into(), json!(self.seq));
            map.insert("at".into(), json!(self.at_ms));
            if let Some(slot) = self.slot {
                map.insert("slot".into(), json!(slot));
            }
        }
        out
    }
}

/// What a reader is told when its cursor has fallen off the back of the ring.
#[derive(Debug, Clone, Copy)]
pub struct Gap {
    pub from: Seq,
    pub to: Seq,
}

impl Gap {
    pub fn to_json(&self) -> Value {
        json!({
            "type": "gap",
            "seq": self.to,
            "from": self.from,
            "to": self.to,
            "lost": self.to.saturating_sub(self.from),
            "note": "the buffer moved past this cursor; these events are gone",
        })
    }
}

/// Where the stream currently stands.
#[derive(Debug, Clone, Copy)]
pub struct Head {
    pub epoch: u64,
    pub next: Seq,
    pub first_kept: Seq,
    pub evicted: u64,
}

impl Head {
    pub fn to_json(&self) -> Value {
        json!({
            "epoch": self.epoch,
            "next": self.next,
            "first_kept": self.first_kept,
            "evicted": self.evicted,
        })
    }
}

struct Ring {
    next: Seq,
    first_kept: Seq,
    events: VecDeque<std::sync::Arc<Record>>,
    bytes: usize,
    evicted: u64,
}

pub struct Journal {
    inner: Mutex<Ring>,
    /// A nudge, not a delivery path. A reader that falls behind this re-reads
    /// from the ring, which is why losing a wakeup costs nothing.
    wake: broadcast::Sender<()>,
    epoch: u64,
}

impl Default for Journal {
    fn default() -> Self {
        Self::new()
    }
}

impl Journal {
    pub fn new() -> Self {
        let (wake, _) = broadcast::channel(64);
        Self {
            inner: Mutex::new(Ring {
                next: 0,
                first_kept: 0,
                events: VecDeque::new(),
                bytes: 0,
                evicted: 0,
            }),
            wake,
            // Unique per process, and larger for a later start, which makes it
            // readable in a log. Nothing depends on it being unpredictable: it
            // says which daemon a cursor belongs to, and authorises nothing.
            epoch: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(1),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Record one event. Never blocks, never fails, never awaits.
    ///
    /// Called from the middle of a live session, so it must not be able to
    /// stall one. The lock is held for a push and a few pops and is never held
    /// across an await.
    pub fn append(&self, slot: Option<u32>, kind: &str, body: Value) -> Seq {
        let payload = body
            .get("data")
            .and_then(|d| d.as_str())
            .map(|s| s.len())
            .unwrap_or(0);
        let mut ring = match self.inner.lock() {
            Ok(ring) => ring,
            Err(poisoned) => poisoned.into_inner(),
        };
        let seq = ring.next;
        ring.next += 1;
        let record = std::sync::Arc::new(Record {
            seq,
            at_ms: now_ms(),
            slot,
            kind: kind.to_string(),
            body,
            weight: payload + OVERHEAD,
        });
        ring.bytes += record.weight;
        ring.events.push_back(record);
        while ring.events.len() > MAX_EVENTS || ring.bytes > MAX_BYTES {
            let Some(dropped) = ring.events.pop_front() else {
                break;
            };
            ring.bytes = ring.bytes.saturating_sub(dropped.weight);
            ring.first_kept = dropped.seq + 1;
            ring.evicted += 1;
        }
        // Sent while the lock is held, so a wakeup can never arrive before the
        // event that caused it is readable.
        let _ = self.wake.send(());
        seq
    }

    /// Everything from `cursor` onwards, and where to ask from next.
    ///
    /// The gap is `Some` when `cursor` named an event the ring no longer holds.
    pub fn since(&self, cursor: Seq, limit: usize) -> (Vec<Value>, Seq, Option<Gap>) {
        let ring = match self.inner.lock() {
            Ok(ring) => ring,
            Err(poisoned) => poisoned.into_inner(),
        };
        let gap = (cursor < ring.first_kept).then_some(Gap {
            from: cursor,
            to: ring.first_kept,
        });
        let from = cursor.max(ring.first_kept);
        let mut out = Vec::new();
        let mut next = from.max(ring.next.min(from));
        for record in ring.events.iter().filter(|r| r.seq >= from) {
            if out.len() >= limit {
                break;
            }
            out.push(record.to_json());
            next = record.seq + 1;
        }
        if out.is_empty() {
            next = ring.next.max(from);
        }
        (out, next, gap)
    }

    pub fn head(&self) -> Head {
        let ring = match self.inner.lock() {
            Ok(ring) => ring,
            Err(poisoned) => poisoned.into_inner(),
        };
        Head {
            epoch: self.epoch,
            next: ring.next,
            first_kept: ring.first_kept,
            evicted: ring.evicted,
        }
    }

    /// A wakeup channel for one reader.
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.wake.subscribe()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// -- what an event is ---------------------------------------------------------
//
// One constructor per kind, so the vocabulary is a list rather than a
// convention, and so the slot-not-code rule is enforced at the only place an
// event can be made.

pub fn daemon_started(build: &str, relay: &str, can_invite: bool) -> (&'static str, Value) {
    (
        "daemon.started",
        json!({"build": build, "relay": relay, "can_invite": can_invite}),
    )
}

/// A slot was issued and a phrase minted for it.
///
/// Carries neither. The command the owner runs embeds the phrase, and the
/// caller that asked for the invite already has it over a private socket;
/// putting it here would hand it to every subscriber instead.
pub fn session_minted(words: usize, expires_in_s: u64) -> (&'static str, Value) {
    (
        "session.minted",
        json!({"words": words, "expires_in_s": expires_in_s}),
    )
}

pub fn session_phase(phase: &str, owner: Option<Value>, ended: Option<&str>) -> (&'static str, Value) {
    (
        "session.phase",
        json!({"phase": phase, "owner": owner, "ended": ended}),
    )
}

/// Somebody arrived with the wrong words.
///
/// The count only. A near miss of a twenty-bit phrase is still information
/// about that phrase, so what was attempted is not recorded.
pub fn session_miss(misses: u32) -> (&'static str, Value) {
    ("session.miss", json!({"misses": misses}))
}

pub fn task_started(task: u32, kind: &str, command: Option<&str>, cols: u16, rows: u16) -> (&'static str, Value) {
    (
        "task.started",
        json!({"task": task, "kind": kind, "command": command, "cols": cols, "rows": rows}),
    )
}

/// A chunk of what a task wrote.
///
/// The data is base64 rather than text on purpose. A terminal's output is
/// escape sequences, and a command's output can split a multi-byte character
/// across a chunk boundary, so decoding here would corrupt both. It is decoded
/// where it is presented instead.
pub fn task_output(task: u32, stream: &str, data: &[u8]) -> (&'static str, Value) {
    (
        "task.output",
        json!({"task": task, "stream": stream, "bytes": data.len(), "data": b64(data)}),
    )
}

/// Exactly one of these follows every `task.started`, and it is always last for
/// that task. A task ends with a status or with a signal, never both.
pub fn task_exited(task: u32, exit_code: Option<i32>, signal: Option<i32>) -> (&'static str, Value) {
    (
        "task.exited",
        json!({"task": task, "exit_code": exit_code, "signal": signal}),
    )
}

/// A line the owner typed to the operator.
pub fn owner_said(text: &str) -> (&'static str, Value) {
    ("owner.said", json!({"text": text}))
}

/// A line the operator spoke to the owner, echoed so a drained record reads as
/// a conversation rather than half of one.
pub fn operator_said(text: &str) -> (&'static str, Value) {
    ("operator.said", json!({"text": text}))
}

fn b64(data: &[u8]) -> String {
    const SET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(SET[(n >> 18) as usize & 63] as char);
        out.push(SET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            SET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            SET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(j: &Journal, n: usize) {
        for i in 0..n {
            let (kind, body) = owner_said(&format!("line {i}"));
            j.append(Some(7), kind, body);
        }
    }

    #[test]
    fn every_event_has_the_next_number_and_carries_its_slot() {
        let j = Journal::new();
        let (kind, body) = owner_said("hello");
        assert_eq!(j.append(Some(7), kind, body), 0);
        let (kind, body) = owner_said("again");
        assert_eq!(j.append(Some(7), kind, body), 1);

        let (events, next, gap) = j.since(0, BATCH);
        assert!(gap.is_none());
        assert_eq!(next, 2);
        assert_eq!(events[0]["seq"], 0);
        assert_eq!(events[0]["slot"], 7);
        assert_eq!(events[0]["type"], "owner.said");
        assert_eq!(events[1]["text"], "again");
    }

    #[test]
    fn a_reader_that_asks_from_the_end_gets_nothing_and_stays_there() {
        let j = Journal::new();
        put(&j, 3);
        let (events, next, gap) = j.since(3, BATCH);
        assert!(events.is_empty());
        assert!(gap.is_none());
        assert_eq!(next, 3, "and does not go backwards");
    }

    #[test]
    fn a_cursor_the_ring_has_moved_past_is_told_so_rather_than_quietly_shortened() {
        let j = Journal::new();
        put(&j, MAX_EVENTS + 10);
        let head = j.head();
        assert!(head.first_kept > 0, "the ring evicted something");

        let (events, _, gap) = j.since(0, BATCH);
        let gap = gap.expect("asking from zero must report the loss");
        assert_eq!(gap.from, 0);
        assert_eq!(gap.to, head.first_kept);
        assert_eq!(gap.to_json()["lost"], head.first_kept);
        assert_eq!(events[0]["seq"], head.first_kept);
    }

    #[test]
    fn one_read_is_bounded_even_when_the_ring_is_full() {
        let j = Journal::new();
        put(&j, 2000);
        let (events, next, _) = j.since(0, BATCH);
        assert_eq!(events.len(), BATCH);
        assert_eq!(next, BATCH as u64, "and says where to carry on from");
    }

    /// The property the module doc claims, checked rather than promised.
    ///
    /// Every constructor is driven with words that would be a phrase, and the
    /// whole serialized stream is searched for them. A constructor added later
    /// that took a code would have to be added here to compile, which is the
    /// point: the list is the vocabulary.
    #[test]
    fn no_event_can_carry_the_phrase() {
        let j = Journal::new();
        let secret_a = "anchor";
        let secret_b = "kettle";

        let (k, b) = daemon_started("9fda69b", "nexus.example/tether", true);
        j.append(None, k, b);
        let (k, b) = session_minted(2, 300);
        j.append(Some(7), k, b);
        let (k, b) = session_phase("open", Some(json!({"who": "tony"})), None);
        j.append(Some(7), k, b);
        let (k, b) = session_miss(1);
        j.append(Some(7), k, b);
        let (k, b) = task_started(1, "command", Some("df -h"), 120, 40);
        j.append(Some(7), k, b);
        let (k, b) = task_output(1, "stdout", b"filesystem size used");
        j.append(Some(7), k, b);
        let (k, b) = task_exited(1, Some(0), None);
        j.append(Some(7), k, b);
        let (k, b) = owner_said("thanks");
        j.append(Some(7), k, b);
        let (k, b) = operator_said("checking the disk");
        j.append(Some(7), k, b);

        let (events, _, _) = j.since(0, BATCH);
        let whole = serde_json::to_string(&events).unwrap();
        assert!(!whole.contains(secret_a), "{whole}");
        assert!(!whole.contains(secret_b), "{whole}");
        assert!(
            !whole.contains("bash -s"),
            "the line the owner runs embeds the phrase and must never be an event"
        );
    }

    #[test]
    fn the_epoch_says_which_daemon_a_cursor_belongs_to() {
        let a = Journal::new();
        let b = Journal::new();
        assert_ne!(a.epoch(), b.epoch());
        assert_eq!(a.head().epoch, a.epoch());
    }

    #[test]
    fn output_is_base64_so_a_split_character_is_not_corrupted_in_transit() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
        // The first two bytes of a three-byte character, which is exactly what
        // a chunk boundary produces and what text would have destroyed.
        assert_eq!(b64(&[0xe2, 0x82]), "4oI=");
    }
}
