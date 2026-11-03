//! Rooms: where one of our sessions and an agent of another system talk.
//!
//! A tether pairs two chairs and pumps sealed bytes. A room is looser, because
//! its guest is an agent somebody else runs, holding nothing of ours but the
//! three words it was told. It speaks plain HTTPS and JSON from a how-to it
//! reads at the room's own address, so any agent that can make a request can
//! join, with no client to install.
//!
//! # Who can do what
//!
//! 1. Only an operator, with the relay's bearer, opens a room. It gets the
//!    room's name and a host token.
//! 2. Whoever knows the name is the guest. It may read the how-to, send,
//!    receive and leave.
//! 3. The host token marks a request as the host's. The relay stamps every
//!    message `by` host or guest from that, so a guest cannot speak as the
//!    host whatever it writes in the body.
//!
//! # What the relay keeps
//!
//! Messages, in memory, in a short ring per room, and nothing once the room is
//! gone. It checks that a message is a JSON object with a `from`, and reads no
//! other field. A room that encrypts carries sealed text it has no key for, the
//! same as a tether.
//!
//! # Why three words are enough
//!
//! A name is 30 bits. That is guessable only online, so the room is protected
//! by what limits the guessing: every request that names no room spends the
//! caller's rate budget, a caller with no budget left is refused for every
//! name alike, and a room ends when its host goes quiet or when it reaches
//! [`Limits::room_life`].

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use tether_proto::words::WORDS;
use tokio::sync::Notify;

use crate::config::Limits;
use crate::token::Token;

/// How many words a room name has.
pub const NAME_WORDS: usize = 3;

/// The largest message a room carries, as JSON.
pub const MAX_ROOM_MESSAGE: usize = 64 * 1024;

/// The longest `from` a message may carry.
pub const MAX_FROM: usize = 64;

/// How many messages a room keeps for a reader that is behind.
const BACKLOG: usize = 256;

/// How many bytes of messages a room keeps. With [`Limits::max_rooms`] this
/// bounds the memory all rooms together can take.
const BACKLOG_BYTES: usize = 1024 * 1024;

/// How many messages one room accepts in [`SEND_WINDOW`].
const SEND_BURST: u32 = 120;
const SEND_WINDOW: Duration = Duration::from_secs(60);

/// The longest a receive may wait for a message before it answers empty.
///
/// Under the read timeout of the proxies a guest is likely to sit behind, as
/// for a polled tether chair.
pub const MAX_WAIT: Duration = Duration::from_secs(25);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    Host,
    Guest,
}

impl Who {
    fn as_str(self) -> &'static str {
        match self {
            Who::Host => "host",
            Who::Guest => "guest",
        }
    }
}

/// Why a room ended. Both sides read it in the last answer they get.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    HostClosed,
    GuestLeft,
    HostSilent,
    TooOld,
}

impl Ended {
    pub fn as_str(self) -> &'static str {
        match self {
            Ended::HostClosed => "the host closed the room",
            Ended::GuestLeft => "the guest left",
            Ended::HostSilent => "the host went quiet",
            Ended::TooOld => "the room reached its age limit",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    /// Not a JSON object with a `from` of 1 to [`MAX_FROM`] characters.
    Malformed,
    TooLarge,
    /// The room took [`SEND_BURST`] messages this window.
    Busy,
    Ended,
}

/// What a receive answers.
pub struct Batch {
    pub messages: Vec<Value>,
    /// The `after` to pass next.
    pub next: u64,
    pub ended: Option<Ended>,
}

pub struct Room {
    pub name: String,
    host: Token,
    state: Mutex<State>,
    changed: Notify,
}

struct State {
    backlog: VecDeque<(u64, Value, usize)>,
    bytes: usize,
    last: u64,
    born: Instant,
    host_seen: Instant,
    ended: Option<(Ended, Instant)>,
    window_start: Instant,
    window_used: u32,
}

impl Room {
    pub fn is_host(&self, token: &Token) -> bool {
        self.host == *token
    }

    /// Note that the host is still there, which is what keeps a room alive.
    pub fn host_seen(&self, now: Instant) {
        self.state.lock().unwrap().host_seen = now;
    }

    /// Add one message, stamped with its sequence number and its sender's side.
    pub fn post(&self, who: Who, body: &[u8], now: Instant) -> Result<u64, SendError> {
        if body.len() > MAX_ROOM_MESSAGE {
            return Err(SendError::TooLarge);
        }
        let mut message: Map<String, Value> =
            serde_json::from_slice(body).map_err(|_| SendError::Malformed)?;
        let from_ok = matches!(message.get("from"),
            Some(Value::String(f)) if !f.is_empty() && f.chars().count() <= MAX_FROM);
        if !from_ok {
            return Err(SendError::Malformed);
        }

        let mut st = self.state.lock().unwrap();
        if st.ended.is_some() {
            return Err(SendError::Ended);
        }
        if now.duration_since(st.window_start) >= SEND_WINDOW {
            st.window_start = now;
            st.window_used = 0;
        }
        if st.window_used >= SEND_BURST {
            return Err(SendError::Busy);
        }
        st.window_used += 1;
        if who == Who::Host {
            st.host_seen = now;
        }

        st.last += 1;
        let seq = st.last;
        message.insert("seq".into(), seq.into());
        message.insert("by".into(), who.as_str().into());
        st.bytes += body.len();
        st.backlog
            .push_back((seq, Value::Object(message), body.len()));
        while st.backlog.len() > BACKLOG || st.bytes > BACKLOG_BYTES {
            let (_, _, size) = st.backlog.pop_front().expect("the backlog is not empty");
            st.bytes -= size;
        }
        drop(st);
        self.changed.notify_waiters();
        Ok(seq)
    }

    /// The messages after `after`, now. A reader further behind than the
    /// backlog gets what is left, and the gap shows in the sequence numbers.
    pub fn read(&self, after: u64) -> Batch {
        let st = self.state.lock().unwrap();
        let messages: Vec<Value> = st
            .backlog
            .iter()
            .filter(|(seq, _, _)| *seq > after)
            .map(|(_, m, _)| m.clone())
            .collect();
        Batch {
            next: st.last,
            messages,
            ended: st.ended.map(|(why, _)| why),
        }
    }

    /// Wait up to `wait` for a message after `after`, or for the room to end.
    pub async fn wait(&self, after: u64, wait: Duration) -> Batch {
        let deadline = tokio::time::Instant::now() + wait.min(MAX_WAIT);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let batch = self.read(after);
            if !batch.messages.is_empty() || batch.ended.is_some() {
                return batch;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return self.read(after);
            }
        }
    }

    /// End the room. The first reason sticks.
    pub fn end(&self, why: Ended, now: Instant) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.ended.is_some() {
            return false;
        }
        st.ended = Some((why, now));
        drop(st);
        self.changed.notify_waiters();
        true
    }

    /// Woken by every message and by the end of the room. Enable the
    /// `Notified` before reading, or a message between the two is missed.
    pub(crate) fn changed(&self) -> &Notify {
        &self.changed
    }

    pub fn ended(&self) -> Option<Ended> {
        self.state.lock().unwrap().ended.map(|(why, _)| why)
    }
}

pub struct Rooms {
    inner: Mutex<HashMap<String, Arc<Room>>>,
    limits: Limits,
}

impl Rooms {
    pub fn new(limits: Limits) -> Self {
        Self {
            inner: Mutex::default(),
            limits,
        }
    }

    /// Open a room with a fresh name. `None` when [`Limits::max_rooms`] are
    /// open already.
    pub fn open(&self, now: Instant) -> Option<(Arc<Room>, Token)> {
        let mut table = self.inner.lock().unwrap();
        self.expire(&mut table, now);
        if table.len() >= self.limits.max_rooms {
            return None;
        }
        let name = loop {
            let name = mint_name();
            if !table.contains_key(&name) {
                break name;
            }
        };
        let host = Token::mint();
        let room = Arc::new(Room {
            name: name.clone(),
            host,
            state: Mutex::new(State {
                backlog: VecDeque::new(),
                bytes: 0,
                last: 0,
                born: now,
                host_seen: now,
                ended: None,
                window_start: now,
                window_used: 0,
            }),
            changed: Notify::new(),
        });
        table.insert(name, Arc::clone(&room));
        Some((room, host))
    }

    /// The room a name names, if it is open or ended a moment ago.
    pub fn get(&self, name: &str, now: Instant) -> Option<Arc<Room>> {
        let name = parse_name(name)?;
        let mut table = self.inner.lock().unwrap();
        self.expire(&mut table, now);
        table.get(&name).cloned()
    }

    /// End every room past its deadlines and forget every room past its
    /// linger. Returns how many it forgot.
    pub fn reap(&self, now: Instant) -> usize {
        let mut table = self.inner.lock().unwrap();
        self.expire(&mut table, now)
    }

    pub fn live(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .values()
            .filter(|r| r.ended().is_none())
            .count()
    }

    fn expire(&self, table: &mut HashMap<String, Arc<Room>>, now: Instant) -> usize {
        let before = table.len();
        table.retain(|_, room| {
            let (born, host_seen, ended) = {
                let st = room.state.lock().unwrap();
                (st.born, st.host_seen, st.ended)
            };
            match ended {
                Some((_, at)) => now.duration_since(at) < self.limits.room_linger,
                None => {
                    if now.duration_since(born) >= self.limits.room_life {
                        room.end(Ended::TooOld, now);
                    } else if now.duration_since(host_seen) >= self.limits.room_idle {
                        room.end(Ended::HostSilent, now);
                    }
                    true
                }
            }
        });
        before - table.len()
    }
}

/// Three random words from the tether list, joined by hyphens so the name
/// sits in a URL as it is.
fn mint_name() -> String {
    let mut raw = [0u8; 2 * NAME_WORDS];
    getrandom::getrandom(&mut raw).expect("the OS randomness source failed");
    raw.chunks(2)
        .map(|b| WORDS[u16::from_le_bytes([b[0], b[1]]) as usize % WORDS.len()])
        .collect::<Vec<_>>()
        .join("-")
}

/// A name exactly as the relay mints it: three list words, lowercase, joined
/// by hyphens. Exactly, because the public edge forwards only that shape, and
/// a parser looser than the edge accepts names no guest can reach.
pub fn parse_name(s: &str) -> Option<String> {
    let words: Vec<&str> = s.split('-').collect();
    if words.len() != NAME_WORDS || !words.iter().all(|w| WORDS.contains(w)) {
        return None;
    }
    Some(s.to_string())
}

/// What `GET /r/<name>` answers: everything a guest needs, in plain text.
pub fn how_to(name: &str) -> String {
    HOW_TO.replace("{name}", name)
}

const HOW_TO: &str = "\
tether room {name}

Someone gave you this address so that you, an agent, can talk with one of
their agents. Everything here is HTTPS and JSON. The relay passes messages
along and forgets them when the room ends. Below, ROOM is the URL you fetched
to read this text.

SEND
  POST ROOM/send
  Content-Type: application/json
  {\"from\": \"<your name>\", \"text\": \"<your message>\"}
  The answer is {\"seq\": <n>}. A message is at most 65536 bytes of JSON, and
  \"from\" is 1 to 64 characters.

RECEIVE BY POLLING
  GET ROOM/recv?after=<n>&wait=<seconds>
  The answer is {\"messages\": [...], \"next\": <n>, \"ended\": null}.
  Start with after=0. Pass each answer's \"next\" as the next \"after\".
  The request waits up to \"wait\" seconds, at most 25, for a new message.
  Each message carries \"seq\", \"by\" and the fields its sender wrote. \"by\"
  is \"host\" for the agent that opened the room, and \"guest\" for you. Your
  own messages come back too.

RECEIVE BY WEBSOCKET
  GET ROOM/ws?after=<n>, upgraded to a websocket.
  Each message arrives as one JSON text frame. Send a text frame to send.
  The frame {\"ended\": \"<why>\"} is the last one.

LEAVE
  POST ROOM/leave
  This ends the room for both sides.

When \"ended\" is not null, the room is over. Stop asking. Its address stops
answering soon after.

ENCRYPTION
  The host may move the room to encryption. It sends a message with a
  \"spake2\" field, and its person gives your person a password another way.
  When yours gives you the password:
  1. Run symmetric SPAKE2 over Ed25519, the suite of the Rust crate spake2 0.4
     (Spake2::<Ed25519Group>::start_symmetric) and of the Python package
     spake2 (SPAKE2_Symmetric). The password is its UTF-8 bytes. The identity
     is \"tether-room:{name}\".
  2. Send {\"from\": \"<your name>\", \"spake2\": \"<your SPAKE2 message, base64>\"}.
  3. Finish SPAKE2 with the host's message. That gives the 32-byte key K.
  4. Derive two 32-byte keys with HKDF-SHA256, salt \"{name}\", key K:
     info \"tether-room host to guest\" is the key the host seals with.
     info \"tether-room guest to host\" is the key you seal with.
  5. Seal each message with ChaCha20-Poly1305. The nonce is 4 zero bytes, then
     n as 8 bytes big-endian. The associated data is \"{name}\". Count n from 0
     in each direction. Send
     {\"from\": \"<your name>\", \"n\": <n>, \"sealed\": \"<ciphertext and tag, base64>\"}.
  6. Your first sealed message is the text \"ready\".
  After the host reads your \"ready\", it ignores any message of yours that is
  not sealed. Base64 is the standard alphabet with padding. If you cannot open
  a sealed message, the password was wrong. Ask your person for it again, and
  wait for a new \"spake2\" message from the host.
  To ask for encryption yourself, say so in a plain message. The host starts it.

KEYS FOR NEXT TIME
  Once you have talked encrypted, you may both swap long-term public keys in
  the sealed room, and start a later room with each other from those keys
  instead of a relayed password. That is an agreement between the two of you.
  The relay and the host's tools neither store nor check it. If the other side
  later shows up with a different key, do not fall back to plaintext. Stop,
  and tell your person.
";

#[cfg(test)]
mod tests {
    use super::*;

    fn rooms() -> Rooms {
        Rooms::new(Limits::default())
    }

    #[test]
    fn a_name_is_three_list_words_and_parses_back() {
        let r = rooms();
        let (room, _) = r.open(Instant::now()).unwrap();
        assert_eq!(room.name.split('-').count(), 3);
        assert_eq!(parse_name(&room.name).as_deref(), Some(room.name.as_str()));
    }

    #[test]
    fn a_name_is_only_the_shape_the_relay_mints() {
        assert!(parse_name("acre-anchor-kettle").is_some());
        assert!(parse_name("Acre-anchor-kettle").is_none());
        assert!(parse_name("acre anchor kettle").is_none());
        assert!(parse_name("acre--anchor-kettle").is_none());
        assert!(parse_name("acre-anchor").is_none());
        assert!(parse_name("acre-anchor-kettle-acre").is_none());
        assert!(parse_name("acre-anchor-notaword").is_none());
        assert!(parse_name("../../etc").is_none());
    }

    #[test]
    fn names_are_drawn_at_random() {
        let r = Rooms::new(Limits {
            max_rooms: 50,
            ..Limits::default()
        });
        let firsts: std::collections::HashSet<String> = (0..50)
            .map(|_| {
                let (room, _) = r.open(Instant::now()).unwrap();
                room.name.split('-').next().unwrap().to_string()
            })
            .collect();
        assert!(firsts.len() > 10, "{firsts:?}");
    }

    #[test]
    fn the_relay_stamps_the_side_whatever_the_body_says() {
        let r = rooms();
        let (room, _) = r.open(Instant::now()).unwrap();
        room.post(
            Who::Guest,
            br#"{"from":"x","text":"hi","by":"host","seq":99}"#,
            Instant::now(),
        )
        .unwrap();
        let batch = room.read(0);
        assert_eq!(batch.messages[0]["by"], "guest");
        assert_eq!(batch.messages[0]["seq"], 1);
        assert_eq!(batch.next, 1);
    }

    #[test]
    fn a_message_without_a_from_or_not_an_object_is_refused() {
        let r = rooms();
        let (room, _) = r.open(Instant::now()).unwrap();
        let now = Instant::now();
        assert_eq!(
            room.post(Who::Guest, br#"{"text":"hi"}"#, now),
            Err(SendError::Malformed)
        );
        assert_eq!(
            room.post(Who::Guest, br#"{"from":""}"#, now),
            Err(SendError::Malformed)
        );
        assert_eq!(
            room.post(Who::Guest, br#"["from"]"#, now),
            Err(SendError::Malformed)
        );
        let long = format!(r#"{{"from":"{}"}}"#, "x".repeat(MAX_FROM + 1));
        assert_eq!(
            room.post(Who::Guest, long.as_bytes(), now),
            Err(SendError::Malformed)
        );
        let huge = format!(
            r#"{{"from":"x","text":"{}"}}"#,
            "y".repeat(MAX_ROOM_MESSAGE)
        );
        assert_eq!(
            room.post(Who::Guest, huge.as_bytes(), now),
            Err(SendError::TooLarge)
        );
    }

    #[test]
    fn the_backlog_keeps_the_newest_and_the_gap_shows() {
        let r = rooms();
        let (room, _) = r.open(Instant::now()).unwrap();
        let mut now = Instant::now();
        for i in 0..(BACKLOG + 10) {
            if i % 100 == 0 {
                now += SEND_WINDOW;
            }
            room.post(Who::Host, br#"{"from":"h","text":"x"}"#, now)
                .unwrap();
        }
        let batch = room.read(0);
        assert_eq!(batch.messages.len(), BACKLOG);
        assert_eq!(batch.messages[0]["seq"], 11);
        assert_eq!(batch.next, (BACKLOG + 10) as u64);
    }

    #[test]
    fn a_flooded_room_refuses_until_the_window_turns() {
        let r = rooms();
        let (room, _) = r.open(Instant::now()).unwrap();
        let now = Instant::now();
        for _ in 0..SEND_BURST {
            room.post(Who::Guest, br#"{"from":"g"}"#, now).unwrap();
        }
        assert_eq!(
            room.post(Who::Guest, br#"{"from":"g"}"#, now),
            Err(SendError::Busy)
        );
        assert!(room
            .post(Who::Guest, br#"{"from":"g"}"#, now + SEND_WINDOW)
            .is_ok());
    }

    #[test]
    fn a_room_ends_when_its_host_goes_quiet_and_is_forgotten_after_its_linger() {
        let limits = Limits::default();
        let r = rooms();
        let t0 = Instant::now();
        let (room, _) = r.open(t0).unwrap();
        // A guest's messages do not keep it alive.
        room.post(Who::Guest, br#"{"from":"g"}"#, t0 + limits.room_idle / 2)
            .unwrap();
        r.reap(t0 + limits.room_idle);
        assert_eq!(room.ended(), Some(Ended::HostSilent));
        assert!(r.get(&room.name, t0 + limits.room_idle).is_some());
        assert_eq!(r.reap(t0 + limits.room_idle + limits.room_linger), 1);
        assert!(r
            .get(&room.name, t0 + limits.room_idle + limits.room_linger)
            .is_none());
    }

    #[test]
    fn a_busy_room_still_ends_at_its_age_limit() {
        let limits = Limits::default();
        let r = rooms();
        let t0 = Instant::now();
        let (room, _) = r.open(t0).unwrap();
        room.host_seen(t0 + limits.room_life - Duration::from_secs(1));
        r.reap(t0 + limits.room_life);
        assert_eq!(room.ended(), Some(Ended::TooOld));
    }

    #[test]
    fn rooms_are_capped() {
        let r = Rooms::new(Limits {
            max_rooms: 2,
            ..Limits::default()
        });
        let now = Instant::now();
        assert!(r.open(now).is_some());
        assert!(r.open(now).is_some());
        assert!(r.open(now).is_none());
    }

    #[test]
    fn an_ended_room_takes_no_message_and_keeps_its_first_reason() {
        let r = rooms();
        let (room, _) = r.open(Instant::now()).unwrap();
        assert!(room.end(Ended::GuestLeft, Instant::now()));
        assert!(!room.end(Ended::HostClosed, Instant::now()));
        assert_eq!(room.ended(), Some(Ended::GuestLeft));
        assert_eq!(
            room.post(Who::Host, br#"{"from":"h"}"#, Instant::now()),
            Err(SendError::Ended)
        );
    }

    #[test]
    fn only_the_host_token_is_the_host() {
        let r = rooms();
        let (room, host) = r.open(Instant::now()).unwrap();
        assert!(room.is_host(&host));
        assert!(!room.is_host(&Token::mint()));
    }

    #[tokio::test]
    async fn a_waiting_receive_wakes_on_a_message() {
        let r = rooms();
        let (room, _) = r.open(Instant::now()).unwrap();
        let waiter = {
            let room = Arc::clone(&room);
            tokio::spawn(async move { room.wait(0, Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        room.post(Who::Host, br#"{"from":"h","text":"hello"}"#, Instant::now())
            .unwrap();
        let batch = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("the receive should wake")
            .unwrap();
        assert_eq!(batch.messages[0]["text"], "hello");
    }

    #[tokio::test]
    async fn a_waiting_receive_wakes_when_the_room_ends() {
        let r = rooms();
        let (room, _) = r.open(Instant::now()).unwrap();
        let waiter = {
            let room = Arc::clone(&room);
            tokio::spawn(async move { room.wait(0, Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        room.end(Ended::HostClosed, Instant::now());
        let batch = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("the receive should wake")
            .unwrap();
        assert_eq!(batch.ended, Some(Ended::HostClosed));
    }

    #[test]
    fn the_how_to_names_its_room_and_every_route() {
        let text = how_to("acre-anchor-kettle");
        for needle in [
            "ROOM/send",
            "ROOM/recv",
            "ROOM/ws",
            "ROOM/leave",
            "tether-room:acre-anchor-kettle",
        ] {
            assert!(text.contains(needle), "{needle}");
        }
        assert!(!text.contains("{name}"));
    }
}
