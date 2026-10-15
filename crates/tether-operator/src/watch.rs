//! The one connection that does not close.
//!
//! Every other verb on the control socket is a line in and a line out, which is
//! the right shape for asking a question and the wrong one for being told what
//! happens next. `watch` is the exception: the request line arrives as usual,
//! and then the daemon writes events as they happen, one JSON object per line,
//! until the reader goes away.
//!
//! # A reader can fall behind without losing anything
//!
//! Two mechanisms, and the seam between them is the point. A broadcast wakes
//! every watcher when something is appended, and the journal's ring is what is
//! actually read. So a watcher that misses a wakeup — because it was busy, or
//! because more arrived than the wakeup channel would hold — loses nothing at
//! all: it goes back to the ring and reads from its own cursor. The fast path
//! is allowed to be lossy precisely because it carries no data.
//!
//! What a reader can genuinely lose is history the ring has dropped, and that
//! is reported as an event rather than passed over in silence.
//!
//! # A slow reader must never stall a session
//!
//! Appending is synchronous and takes a lock for a moment. Writing to a watcher
//! is not, and a reader that has stopped draining its socket would otherwise
//! hold the daemon's attention while an owner waits on the other side of the
//! world. So every write is bounded, and a watcher that cannot keep up is
//! dropped. Its cursor survives in whatever asked for it, and the ring covers
//! the interval, so reconnecting costs nothing.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::unix::OwnedWriteHalf;

use crate::journal::{Seq, BATCH};
use crate::operator::Operator;

/// How long one write to a watcher may take before it is considered gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a silent stream waits before saying it is still there.
///
/// A reader needs to tell "nothing is happening" from "this socket died", and
/// on a Unix socket a dead peer is not always noticed by a task that is only
/// reading. This is the heartbeat that makes the difference visible.
const DEFAULT_IDLE_PING: Duration = Duration::from_secs(30);

/// Stream events to one reader until it goes away.
///
/// Never returns a reply line: the caller must not try to answer afterwards.
pub async fn stream(operator: Arc<Operator>, args: &Value, mut write: OwnedWriteHalf) {
    let journal = Arc::clone(operator.journal());
    let head = journal.head();

    // `since` absent means "from now", because the common reader is a fresh
    // adapter that wants what happens next rather than the whole ring. An
    // explicit 0 asks for everything still held.
    let mut cursor: Seq = args
        .get("since")
        .and_then(|v| v.as_u64())
        .unwrap_or(head.next);
    let idle = args
        .get("idle_ping_s")
        .and_then(|v| v.as_u64())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_IDLE_PING);

    let mut opening = head.to_json();
    if let Some(map) = opening.as_object_mut() {
        map.insert("type".into(), json!("watch.open"));
        map.insert("cursor".into(), json!(cursor));
    }
    // Written before anything else, so a reader learns whether its cursor is
    // too old and whether this is the daemon it was talking to last time,
    // before a single event arrives to confuse the question.
    if !line(&mut write, &opening).await {
        return;
    }

    let mut wake = journal.subscribe();
    loop {
        let (events, next, gap) = journal.since(cursor, BATCH);
        if let Some(gap) = gap {
            if !line(&mut write, &gap.to_json()).await {
                return;
            }
        }
        let caught_up = events.is_empty();
        for event in events {
            if !line(&mut write, &event).await {
                return;
            }
        }
        cursor = next;

        if !caught_up {
            // More may already be waiting; read again before sleeping.
            continue;
        }
        match tokio::time::timeout(idle, wake.recv()).await {
            // Something arrived, or we missed the news and will find it in the
            // ring anyway. Both mean: go and look.
            Ok(Ok(())) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            // The daemon is going away.
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return,
            Err(_) => {
                if !line(&mut write, &json!({"type": "watch.idle", "next": cursor})).await {
                    return;
                }
            }
        }
    }
}

/// Write one JSON line. `false` means the reader is gone or too slow.
async fn line(write: &mut OwnedWriteHalf, value: &Value) -> bool {
    let Ok(mut bytes) = serde_json::to_vec(value) else {
        return true;
    };
    bytes.push(b'\n');
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, write.write_all(&bytes)).await,
        Ok(Ok(()))
    )
}
