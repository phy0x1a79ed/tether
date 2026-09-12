//! One session, from the moment the slot is minted to the moment it is cut.
//!
//! Each session is a task that owns its [`Link`] and is spoken to over a
//! channel. Nothing else may touch the socket, which is what keeps the protocol
//! honest: there is one writer, so the ordering the frame protocol guarantees is
//! a property of this file rather than of everyone who might send something.
//!
//! # The operator's half of the consent rule
//!
//! The owner's side ends the session if a task-shaped frame arrives before the
//! person at that keyboard has answered. That is the enforcement. This file is
//! the *cooperation*: [`Phase::Open`] is the only phase in which a `Run`
//! command can reach the wire, and the phase advances only on the owner's
//! `Hello`. An operator cannot ask for work early by accident, and asking for it
//! on purpose would mean deleting this.
//!
//! # Why the session waits by redialling
//!
//! The invite lives for minutes, and a socket that has paired with nobody gives
//! up in ninety seconds — the owner may still be walking to the machine. So the
//! dial is a loop against the invite's own deadline rather than one attempt. A
//! socket that waits and times out costs the slot nothing; only a *pairing*
//! spends from the relay's budget, and that budget is the failure limit that
//! makes a spoken phrase safe.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tether_link::http::HttpError;
use tether_link::{release, Link, LinkError};
use tether_proto::frame::{Ended, Frame, Hello, Kind, Stream, TaskId};
use tether_proto::invite::{InviteCode, Phrase, Slot};
use tokio::sync::{mpsc, oneshot};

use crate::config::{Config, RUN_TIMEOUT};
use crate::journal::{self, Journal};

/// How long the daemon waits for consent once the two ends have paired.
///
/// Long, because what is being waited on is a person reading a prompt and
/// deciding. It is bounded only so a session nobody answers is eventually a
/// reported fact rather than a task sitting there forever.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(300);

/// How often an open session proves the path still works.
const KEEPALIVE: Duration = Duration::from_secs(45);

/// How long an ended session stays visible in `status` before it is forgotten.
pub const LINGER: Duration = Duration::from_secs(600);

/// Where a verb's answer goes.
pub type Ack = oneshot::Sender<Result<Value, String>>;

/// One thing the control socket asked a live session to do.
///
/// The reply channel is beside the request rather than inside each variant, so
/// refusing a verb does not need to know which verb it was. That mattered once
/// there were more than three of them.
pub struct Command {
    pub what: What,
    pub reply: Ack,
}

/// What was asked for.
pub enum What {
    /// Run a command to completion. Its output and its ending arrive on the
    /// stream; what comes back here is the task's number.
    Run {
        command: String,
        limit_s: Option<u64>,
    },
    /// Open a terminal. The owner watches it render as a screen rather than as
    /// a block of text, which is the only way a full-screen program is visible
    /// to them at all.
    Shell {
        command: Option<String>,
        cols: u16,
        rows: u16,
    },
    /// Type at a task. Talks to the program, not to the person — see `Say`.
    Keys { task: TaskId, data: Vec<u8> },
    Resize { task: TaskId, cols: u16, rows: u16 },
    /// Stop a task. The protocol is careful that this kills rather than
    /// signalling end of input.
    Close { task: TaskId },
    Tasks,
    /// Say a line to the person at the other keyboard. Talks to the person, not
    /// to the program — see `Keys`.
    Say { text: String },
    Cut { reason: String },
}

impl Command {
    fn refuse(self, why: &str) {
        let _ = self.reply.send(Err(why.to_string()));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Minted, and nobody has run the launcher yet.
    Waiting,
    /// Both ends are on one sealed channel; the owner is being asked.
    Greeting,
    /// The owner said yes. The only phase in which work may be asked for.
    Open,
    Ended,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Waiting => "waiting",
            Phase::Greeting => "greeting",
            Phase::Open => "open",
            Phase::Ended => "ended",
        }
    }
}

/// What `status` reads. Kept beside the channel rather than behind it so that
/// reporting on a session never queues behind the ten-minute command it is
/// reporting on.
pub struct State {
    pub slot: Slot,
    /// The whole invite code, so the operator can read it out again when the
    /// owner mistypes. It stays on this machine: the relay is told the slot.
    pub code: String,
    pub phase: Phase,
    pub owner: Option<Hello>,
    pub minted: Instant,
    pub opened: Option<Instant>,
    pub tasks: u32,
    /// Handshakes that did not check out — in practice, a mistyped phrase.
    /// Each one spends from the relay's pairing budget for this slot.
    pub misses: u32,
    /// Lines the owner has said, and the most recent one. A summary only: the
    /// conversation itself lives on the stream. This exists so that `status`,
    /// the verb that answers when everything else is broken, can still show
    /// that the person at the other end is trying to say something.
    pub heard: u32,
    pub last_said: Option<String>,
    pub ended: Option<String>,
    pub ended_at: Option<Instant>,
}

impl State {
    fn new(code: &InviteCode) -> Self {
        Self {
            slot: code.slot,
            code: code.to_string(),
            phase: Phase::Waiting,
            owner: None,
            minted: Instant::now(),
            opened: None,
            tasks: 0,
            misses: 0,
            heard: 0,
            last_said: None,
            ended: None,
            ended_at: None,
        }
    }

    pub fn report(&self) -> Value {
        json!({
            "slot": self.slot.get(),
            "code": self.code,
            "phase": self.phase.as_str(),
            "age_s": self.minted.elapsed().as_secs(),
            "open_s": self.opened.map(|t| t.elapsed().as_secs()),
            "tasks": self.tasks,
            "misses": self.misses,
            "heard": self.heard,
            "last_said": self.last_said,
            "ended": self.ended,
            "owner": self.owner.as_ref().map(|h| json!({
                "who": h.who,
                "host": h.host,
                "os": h.os,
                "build": h.build,
            })),
        })
    }

    pub fn live(&self) -> bool {
        self.phase != Phase::Ended
    }

    /// Whether an ended session has been ended long enough to forget.
    pub fn stale(&self, now: Instant) -> bool {
        match self.ended_at {
            Some(at) => now.duration_since(at) >= LINGER,
            None => false,
        }
    }
}

/// A handle on one running session.
pub struct Session {
    pub tx: mpsc::Sender<Command>,
    pub state: Arc<Mutex<State>>,
}

/// One session's two records, carried together.
///
/// `status` reads the state; everyone else reads the journal. Bundling them is
/// what stops the two drifting: a phase change writes to both or to neither,
/// because there is no way to reach one of them from in here without the other.
#[derive(Clone)]
pub struct Track {
    slot: u32,
    pub state: Arc<Mutex<State>>,
    journal: Arc<Journal>,
}

impl Track {
    /// Put one event on the stream, already addressed to this session.
    ///
    /// Takes the pair a constructor in [`journal`] returns, so the only way to
    /// record something is to have named it there.
    fn note(&self, event: (&str, Value)) -> crate::journal::Seq {
        self.journal.append(Some(self.slot), event.0, event.1)
    }
}

/// Mint a session task for a slot the relay has already issued.
pub fn spawn(
    cfg: Arc<Config>,
    code: InviteCode,
    seat: String,
    lifetime: Duration,
    journal: Arc<Journal>,
) -> Session {
    let state = Arc::new(Mutex::new(State::new(&code)));
    let track = Track {
        slot: code.slot.get(),
        state: Arc::clone(&state),
        journal,
    };
    let (tx, rx) = mpsc::channel::<Command>(8);
    tokio::spawn(serve(cfg, code, seat, lifetime, track, rx));
    Session { tx, state }
}

fn set_phase(track: &Track, phase: Phase) {
    let owner = {
        let Ok(mut s) = track.state.lock() else {
            return;
        };
        s.phase = phase;
        if phase == Phase::Open {
            s.opened = Some(Instant::now());
        }
        s.owner.as_ref().map(|h| {
            json!({"who": h.who, "host": h.host, "os": h.os, "build": h.build})
        })
    };
    track.note(journal::session_phase(phase.as_str(), owner, None));
}

fn finish(track: &Track, why: impl Into<String>) {
    let why = why.into();
    let owner = {
        let Ok(mut s) = track.state.lock() else {
            return;
        };
        // Only the first ending is the ending. Several paths can reach here as
        // a session comes apart, and the first one is the one that explains it.
        if s.phase == Phase::Ended {
            return;
        }
        s.phase = Phase::Ended;
        s.ended = Some(why.clone());
        s.ended_at = Some(Instant::now());
        s.owner.as_ref().map(|h| {
            json!({"who": h.who, "host": h.host, "os": h.os, "build": h.build})
        })
    };
    track.note(journal::session_phase("ended", owner, Some(&why)));
}

async fn serve(
    cfg: Arc<Config>,
    code: InviteCode,
    seat: String,
    lifetime: Duration,
    track: Track,
    mut rx: mpsc::Receiver<Command>,
) {
    if let Some(mut link) = connect(&cfg, &code, &seat, lifetime, &track, &mut rx).await {
        if consent(&cfg, &mut link, &track, &mut rx).await {
            carry(&mut link, &track, &mut rx).await;
        }
    }
    // Every way a session can end arrives here, and only here. The redial loop
    // inside `connect` drops and retakes the chair many times without reaching
    // it, which is exactly the distinction the relay cannot make for itself: an
    // operator between dials is still attached, an operator that has got this
    // far is not, and the slot should stop existing rather than seat the next
    // person opposite nobody.
    match release(&cfg.relay, code.slot, &seat).await {
        Ok(()) => {}
        // The slot was already gone: an invite nobody redeemed had expired
        // before we got here, which is the ordinary ending rather than a fault.
        Err(LinkError::Http(HttpError::Status(404))) => {}
        Err(e) => crate::log::info(format_args!(
            "slot {} could not be released ({e}); it expires on its own shortly",
            code.slot
        )),
    }
}

/// Hold the operator's chair until the owner arrives, or the invite expires.
async fn connect(
    cfg: &Config,
    code: &InviteCode,
    seat: &str,
    lifetime: Duration,
    track: &Track,
    rx: &mut mpsc::Receiver<Command>,
) -> Option<Link> {
    let deadline = Instant::now() + lifetime;
    while Instant::now() < deadline {
        // The dial gets whatever is left of the invite, never more. Its own
        // ninety-second patience is the right budget for one attempt and the
        // wrong one for the last attempt: without this, an invite nobody
        // redeems is declared expired a minute and a half after it was.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let dial = tokio::time::timeout(
            remaining,
            Link::dial_operator(&cfg.relay, code.slot, seat, &code.phrase),
        );
        tokio::pin!(dial);

        loop {
            tokio::select! {
                dialled = &mut dial => match dialled {
                    Err(_) => break,
                    Ok(Ok(link)) => return Some(link),
                    // Nobody has run the launcher yet. Take the chair again.
                    Ok(Err(LinkError::NobodyThere)) => break,
                    // Somebody arrived with the wrong words. They will very
                    // likely try again, so wait for them — and count it,
                    // because the relay is counting it too.
                    Ok(Err(LinkError::Handshake(_))) => {
                        let misses = match track.state.lock() {
                            Ok(mut s) => {
                                s.misses += 1;
                                s.misses
                            }
                            Err(_) => 0,
                        };
                        track.note(journal::session_miss(misses));
                        break;
                    }
                    Ok(Err(e)) => {
                        finish(track, format!("the session could not be held open: {e}"));
                        return None;
                    }
                },
                command = rx.recv() => match command {
                    None => {
                        finish(track, "the daemon is shutting down");
                        return None;
                    }
                    Some(Command { what: What::Cut { reason }, reply }) => {
                        let _ = reply.send(Ok(json!({"ok": true, "cut": reason})));
                        finish(track, reason);
                        return None;
                    }
                    Some(other) => other.refuse(
                        "nobody has redeemed this invite yet — read them the code, \
                         then try again",
                    ),
                },
            }
        }
    }
    finish(track, "nobody redeemed the invite before it expired");
    None
}

/// Say who we are, and wait for the owner's `Hello`, which *is* their consent.
async fn consent(
    cfg: &Config,
    link: &mut Link,
    track: &Track,
    rx: &mut mpsc::Receiver<Command>,
) -> bool {
    set_phase(track, Phase::Greeting);
    if let Err(e) = link.send(&Frame::Hello(cfg.hello())).await {
        finish(track, format!("could not greet the owner: {e}"));
        return false;
    }

    let waiting = tokio::time::sleep(CONSENT_TIMEOUT);
    tokio::pin!(waiting);

    loop {
        tokio::select! {
            incoming = link.recv() => match incoming {
                Ok(Frame::Hello(owner)) => {
                    if let Ok(mut s) = track.state.lock() {
                        s.owner = Some(owner);
                    }
                    set_phase(track, Phase::Open);
                    return true;
                }
                Ok(Frame::Cut { reason }) => {
                    finish(track, reason);
                    return false;
                }
                Ok(Frame::Say { .. }) | Ok(Frame::Ping { .. }) | Ok(Frame::Pong { .. }) => {}
                Ok(_) => {
                    let why = "the owner's end sent session traffic before consenting";
                    link.cut(why).await;
                    finish(track, why);
                    return false;
                }
                Err(e) => {
                    finish(track, format!("the owner's end went away while deciding: {e}"));
                    return false;
                }
            },
            command = rx.recv() => match command {
                None => {
                    link.cut("the operator's daemon is shutting down").await;
                    finish(track, "the daemon is shutting down");
                    return false;
                }
                Some(Command { what: What::Cut { reason }, reply }) => {
                    link.cut(&reason).await;
                    let _ = reply.send(Ok(json!({"ok": true, "cut": reason})));
                    finish(track, reason);
                    return false;
                }
                // Worth allowing, and the reason the owner's side accepts it
                // before consent: it is how the operator says what they are
                // about to do, to somebody deciding whether to let them.
                Some(Command { what: What::Say { text }, reply }) => {
                    track.note(journal::operator_said(&text));
                    let sent = link.send(&Frame::Say { text }).await;
                    let _ = reply.send(match sent {
                        Ok(()) => Ok(json!({"ok": true, "said": true})),
                        Err(e) => Err(e.to_string()),
                    });
                }
                Some(other) => other.refuse(
                    "the owner has not answered the prompt on their machine yet",
                ),
            },
            _ = &mut waiting => {
                let why = "nobody answered the prompt at the owner's keyboard";
                link.cut(why).await;
                finish(track, why);
                return false;
            }
        }
    }
}

/// One task open at the owner's end.
///
/// What this holds is bookkeeping, not content. Output goes straight onto the
/// stream as it arrives, so nothing accumulates here however much a command
/// prints — which is the whole reason the old single-command buffer is gone.
struct Task {
    kind: Kind,
    command: Option<String>,
    opened: tokio::time::Instant,
    bytes_out: u64,
    /// A `Close` has gone out and the `Exit` that must follow has not.
    closing: bool,
    /// Only a command has one. A terminal has no natural length and is bounded
    /// by the session it lives in.
    deadline: Option<tokio::time::Instant>,
}

impl Task {
    fn report(&self, id: TaskId) -> Value {
        json!({
            "task": id,
            "kind": if self.kind == Kind::Shell { "shell" } else { "command" },
            "command": self.command,
            "age_s": self.opened.elapsed().as_secs(),
            "bytes_out": self.bytes_out,
            "closing": self.closing,
        })
    }
}

/// How many tasks one session may have open at once.
///
/// A resource bound rather than a rule about ordering. The old limit of one was
/// a consequence of the reply being the result, and it went with it.
const MAX_OPEN_TASKS: usize = 8;

/// How long a session may hear nothing at all before it is assumed gone.
///
/// This is the daemon's only liveness check, and it has to exist here because
/// the thing that used to serve as one — a command's deadline firing twice —
/// stopped meaning the peer was unresponsive the moment a session could hold
/// several tasks and a terminal. A task that never reports its ending is not a
/// peer that has gone away.
const SILENCE: Duration = Duration::from_secs(3 * 45 + 30);

/// Carry the open session until either side ends it.
async fn carry(link: &mut Link, track: &Track, rx: &mut mpsc::Receiver<Command>) {
    let mut tasks: BTreeMap<TaskId, Task> = BTreeMap::new();
    let mut next_task: TaskId = 1;
    let mut nonce: u64 = 0;
    let mut heard = tokio::time::Instant::now();
    let mut beat = tokio::time::interval(KEEPALIVE);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    beat.tick().await; // the first tick is immediate; the socket is fresh

    loop {
        // The earliest deadline among whatever is in flight, rebuilt each turn
        // because the set changes. A sleep is cheap and the loop only turns
        // when something happened.
        let overrun = match tasks.values().filter_map(|t| t.deadline).min() {
            Some(at) => tokio::time::sleep_until(at),
            None => tokio::time::sleep(Duration::from_secs(3600)),
        };
        tokio::pin!(overrun);

        tokio::select! {
            incoming = link.recv() => {
                heard = tokio::time::Instant::now();
                match incoming {
                    Ok(Frame::Output { task, stream, data }) => {
                        let name = match stream {
                            Stream::Stdout => "stdout",
                            Stream::Stderr => "stderr",
                            Stream::Screen => "screen",
                        };
                        if let Some(t) = tasks.get_mut(&task) {
                            t.bytes_out += data.len() as u64;
                        }
                        // One frame can be larger than one event, so a big
                        // chunk becomes several in order rather than a single
                        // record that would dominate the ring on its own.
                        for piece in data.chunks(journal::MAX_EVENT_DATA).filter(|c| !c.is_empty()) {
                            track.note(journal::task_output(task, name, piece));
                        }
                    }
                    Ok(Frame::Exit { task, ended }) => {
                        let (code, signal) = match ended {
                            Ended::Code(c) => (Some(c), None),
                            Ended::Signal(n) => (None, Some(n)),
                        };
                        track.note(journal::task_exited(task, code, signal));
                        tasks.remove(&task);
                    }
                    Ok(Frame::Cut { reason }) => {
                        finish(track, reason);
                        return;
                    }
                    // The owner said something. This used to fall into the
                    // empty arm below and be discarded without trace, which
                    // made the frame's own definition — a line from one person
                    // to the other — true in one direction only.
                    Ok(Frame::Say { text }) => {
                        track.note(journal::owner_said(&text));
                        if let Ok(mut st) = track.state.lock() {
                            st.heard += 1;
                            st.last_said = Some(text);
                        }
                    }
                    Ok(Frame::Ping { nonce }) => {
                        if link.send(&Frame::Pong { nonce }).await.is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        finish(track, format!("the owner's end went away: {e}"));
                        return;
                    }
                }
            }

            command = rx.recv() => match command {
                None => {
                    link.cut("the operator's daemon is shutting down").await;
                    finish(track, "the daemon is shutting down");
                    return;
                }
                Some(Command { what, reply }) => {
                    match run_verb(link, track, &mut tasks, &mut next_task, what, reply).await {
                        Carry::On => {}
                        Carry::Ended(why) => {
                            finish(track, why);
                            return;
                        }
                    }
                }
            },

            _ = &mut overrun => {
                let now = tokio::time::Instant::now();
                let due: Vec<TaskId> = tasks
                    .iter()
                    .filter(|(_, t)| t.deadline.is_some_and(|at| at <= now))
                    .map(|(id, _)| *id)
                    .collect();
                for id in due {
                    let Some(task) = tasks.get_mut(&id) else { continue };
                    if task.closing {
                        // Asked to stop and never said whether it did. That is
                        // one task giving up, not a peer that has gone: the
                        // session carries on and the silence check above is
                        // what decides whether anybody is still there.
                        track.note(journal::task_exited(id, None, None));
                        tasks.remove(&id);
                        continue;
                    }
                    task.closing = true;
                    task.deadline = Some(now + Duration::from_secs(30));
                    if link.send(&Frame::Close { task: id }).await.is_err() {
                        finish(track, "the session ended while stopping a command");
                        return;
                    }
                }
            }

            _ = beat.tick() => {
                if heard.elapsed() > SILENCE {
                    let why = "the owner's end stopped answering";
                    link.cut(why).await;
                    finish(track, why);
                    return;
                }
                nonce += 1;
                if link.send(&Frame::Ping { nonce }).await.is_err() {
                    break;
                }
            }
        }
    }

    finish(track, "the session ended");
}

/// Whether the session survived a verb.
enum Carry {
    On,
    Ended(String),
}

/// Do one thing the operator asked for.
///
/// Split out of [`carry`] because the match got longer than the loop it lives
/// in, and because every arm here has the same shape: put a frame on the wire,
/// tell the caller what it now knows, and say whether the session is still
/// there.
async fn run_verb(
    link: &mut Link,
    track: &Track,
    tasks: &mut BTreeMap<TaskId, Task>,
    next_task: &mut TaskId,
    what: What,
    reply: Ack,
) -> Carry {
    match what {
        What::Cut { reason } => {
            link.cut(&reason).await;
            let _ = reply.send(Ok(json!({"ok": true, "cut": reason})));
            Carry::Ended(reason)
        }

        What::Say { text } => {
            track.note(journal::operator_said(&text));
            let sent = link.send(&Frame::Say { text }).await;
            let _ = reply.send(match sent {
                Ok(()) => Ok(json!({"ok": true, "said": true})),
                Err(e) => Err(e.to_string()),
            });
            Carry::On
        }

        What::Tasks => {
            let open: Vec<Value> = tasks.iter().map(|(id, t)| t.report(*id)).collect();
            let _ = reply.send(Ok(json!({"ok": true, "tasks": open})));
            Carry::On
        }

        What::Run { command, limit_s } => {
            if tasks.len() >= MAX_OPEN_TASKS {
                let _ = reply.send(Err(format!(
                    "this session already has {MAX_OPEN_TASKS} tasks open; \
                     let one finish, or close one"
                )));
                return Carry::On;
            }
            let task = *next_task;
            *next_task += 1;
            let seq = track.note(journal::task_started(task, "command", Some(&command), 120, 40));
            let opened = link
                .send(&Frame::Open {
                    task,
                    kind: Kind::Command,
                    command: Some(command.clone()),
                    cols: 120,
                    rows: 40,
                })
                .await
                // End-of-input immediately, because nothing will ever type at a
                // command. Without this one that reads until its input runs out
                // — `cat`, `sort`, anything in a pipeline — waits for a keyboard
                // that does not exist. A terminal gets no such frame: it has no
                // end of input to be told about.
                .and(link.send(&Frame::Input { task, data: Vec::new() }).await);
            match opened {
                Ok(()) => {
                    if let Ok(mut s) = track.state.lock() {
                        s.tasks += 1;
                    }
                    let limit = limit_s.map(Duration::from_secs).unwrap_or(RUN_TIMEOUT);
                    tasks.insert(
                        task,
                        Task {
                            kind: Kind::Command,
                            command: Some(command),
                            opened: tokio::time::Instant::now(),
                            bytes_out: 0,
                            closing: false,
                            deadline: Some(tokio::time::Instant::now() + limit),
                        },
                    );
                    let _ = reply.send(Ok(json!({
                        "ok": true, "task": task, "kind": "command", "cursor": seq,
                    })));
                    Carry::On
                }
                Err(e) => {
                    let _ = reply.send(Err(e.to_string()));
                    Carry::Ended(format!("the session ended: {e}"))
                }
            }
        }

        What::Shell { command, cols, rows } => {
            // One terminal per session, and this is policy rather than
            // protocol. The owner has one screen, and a second terminal is a
            // second thing happening on their machine that they cannot watch.
            if let Some((id, _)) = tasks.iter().find(|(_, t)| t.kind == Kind::Shell) {
                let _ = reply.send(Err(format!(
                    "this session already has a terminal open as task {id}; \
                     close it before opening another"
                )));
                return Carry::On;
            }
            if tasks.len() >= MAX_OPEN_TASKS {
                let _ = reply.send(Err(format!(
                    "this session already has {MAX_OPEN_TASKS} tasks open"
                )));
                return Carry::On;
            }
            let task = *next_task;
            *next_task += 1;
            let seq = track.note(journal::task_started(
                task,
                "shell",
                command.as_deref(),
                cols,
                rows,
            ));
            match link
                .send(&Frame::Open {
                    task,
                    kind: Kind::Shell,
                    command: command.clone(),
                    cols,
                    rows,
                })
                .await
            {
                Ok(()) => {
                    if let Ok(mut s) = track.state.lock() {
                        s.tasks += 1;
                    }
                    tasks.insert(
                        task,
                        Task {
                            kind: Kind::Shell,
                            command,
                            opened: tokio::time::Instant::now(),
                            bytes_out: 0,
                            closing: false,
                            deadline: None,
                        },
                    );
                    let _ = reply.send(Ok(json!({
                        "ok": true, "task": task, "kind": "shell",
                        "cols": cols, "rows": rows, "cursor": seq,
                    })));
                    Carry::On
                }
                Err(e) => {
                    let _ = reply.send(Err(e.to_string()));
                    Carry::Ended(format!("the session ended: {e}"))
                }
            }
        }

        What::Keys { task, data } => {
            if !tasks.contains_key(&task) {
                let _ = reply.send(Err(format!("task {task} is not open in this session")));
                return Carry::On;
            }
            let bytes = data.len();
            match link.send(&Frame::Input { task, data }).await {
                Ok(()) => {
                    let _ = reply.send(Ok(json!({"ok": true, "task": task, "bytes": bytes})));
                    Carry::On
                }
                Err(e) => {
                    let _ = reply.send(Err(e.to_string()));
                    Carry::Ended(format!("the session ended: {e}"))
                }
            }
        }

        What::Resize { task, cols, rows } => {
            if !tasks.contains_key(&task) {
                let _ = reply.send(Err(format!("task {task} is not open in this session")));
                return Carry::On;
            }
            match link.send(&Frame::Resize { task, cols, rows }).await {
                Ok(()) => {
                    let _ = reply.send(Ok(json!({
                        "ok": true, "task": task, "cols": cols, "rows": rows,
                    })));
                    Carry::On
                }
                Err(e) => {
                    let _ = reply.send(Err(e.to_string()));
                    Carry::Ended(format!("the session ended: {e}"))
                }
            }
        }

        What::Close { task } => {
            let Some(open) = tasks.get_mut(&task) else {
                let _ = reply.send(Err(format!("task {task} is not open in this session")));
                return Carry::On;
            };
            open.closing = true;
            open.deadline = Some(tokio::time::Instant::now() + Duration::from_secs(30));
            match link.send(&Frame::Close { task }).await {
                Ok(()) => {
                    let _ = reply.send(Ok(json!({"ok": true, "task": task})));
                    Carry::On
                }
                Err(e) => {
                    let _ = reply.send(Err(e.to_string()));
                    Carry::Ended(format!("the session ended: {e}"))
                }
            }
        }
    }
}

/// The one-line command the owner runs. Assembled here because the two halves
/// of it — where the launcher lives and what to say to it — are only both
/// known on this side.
pub fn bootstrap(relay: &tether_link::Relay, slot: Slot, phrase: &Phrase) -> String {
    format!(
        "curl -fsSL {} | bash -s {} {}",
        relay.launcher_url(),
        slot,
        phrase
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owners_line_has_nothing_to_quote_and_nothing_to_punctuate() {
        let relay = tether_link::Relay::parse("https://nexus.tony-xy-liu.com/tether").unwrap();
        let phrase = Phrase::parse(&["anchor", "kettle"]).unwrap();
        let line = bootstrap(&relay, Slot::new(7).unwrap(), &phrase);
        assert_eq!(
            line,
            "curl -fsSL https://nexus.tony-xy-liu.com/tether | bash -s 7 anchor kettle"
        );
        // The code's own tokens carry no shell metacharacters, which is the
        // property that lets it be read aloud and typed without quoting.
        let code = line.split(" -s ").nth(1).unwrap();
        assert!(
            !code.contains(['"', '\'', '\\', '-', '$', '`']),
            "the invite code needs quoting: {code}"
        );
    }

}
