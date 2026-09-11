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

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tether_link::{Link, LinkError};
use tether_proto::frame::{Ended, Frame, Hello, Kind, Stream, TaskId};
use tether_proto::invite::{InviteCode, Phrase, Slot};
use tokio::sync::{mpsc, oneshot};

use crate::config::{Config, MAX_CAPTURE, RUN_TIMEOUT};

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

/// Commands the daemon's control socket can put to a live session.
pub enum Command {
    Run {
        command: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Say {
        text: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Cut {
        reason: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
}

impl Command {
    fn refuse(self, why: &str) {
        let sender = match self {
            Command::Run { reply, .. }
            | Command::Say { reply, .. }
            | Command::Cut { reply, .. } => reply,
        };
        let _ = sender.send(Err(why.to_string()));
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

/// Mint a session task for a slot the relay has already issued.
pub fn spawn(cfg: Arc<Config>, code: InviteCode, seat: String, lifetime: Duration) -> Session {
    let state = Arc::new(Mutex::new(State::new(&code)));
    let (tx, rx) = mpsc::channel::<Command>(8);
    tokio::spawn(serve(cfg, code, seat, lifetime, Arc::clone(&state), rx));
    Session { tx, state }
}

fn set_phase(state: &Arc<Mutex<State>>, phase: Phase) {
    if let Ok(mut s) = state.lock() {
        s.phase = phase;
        if phase == Phase::Open {
            s.opened = Some(Instant::now());
        }
    }
}

fn finish(state: &Arc<Mutex<State>>, why: impl Into<String>) {
    if let Ok(mut s) = state.lock() {
        s.phase = Phase::Ended;
        s.ended = Some(why.into());
        s.ended_at = Some(Instant::now());
    }
}

async fn serve(
    cfg: Arc<Config>,
    code: InviteCode,
    seat: String,
    lifetime: Duration,
    state: Arc<Mutex<State>>,
    mut rx: mpsc::Receiver<Command>,
) {
    let Some(mut link) = connect(&cfg, &code, &seat, lifetime, &state, &mut rx).await else {
        return;
    };
    if !consent(&cfg, &mut link, &state, &mut rx).await {
        return;
    }
    carry(&mut link, &state, &mut rx).await;
}

/// Hold the operator's chair until the owner arrives, or the invite expires.
async fn connect(
    cfg: &Config,
    code: &InviteCode,
    seat: &str,
    lifetime: Duration,
    state: &Arc<Mutex<State>>,
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
                        if let Ok(mut s) = state.lock() {
                            s.misses += 1;
                        }
                        break;
                    }
                    Ok(Err(e)) => {
                        finish(state, format!("the session could not be held open: {e}"));
                        return None;
                    }
                },
                command = rx.recv() => match command {
                    None => {
                        finish(state, "the daemon is shutting down");
                        return None;
                    }
                    Some(Command::Cut { reason, reply }) => {
                        let _ = reply.send(Ok(json!({"ok": true, "cut": reason})));
                        finish(state, reason);
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
    finish(state, "nobody redeemed the invite before it expired");
    None
}

/// Say who we are, and wait for the owner's `Hello`, which *is* their consent.
async fn consent(
    cfg: &Config,
    link: &mut Link,
    state: &Arc<Mutex<State>>,
    rx: &mut mpsc::Receiver<Command>,
) -> bool {
    set_phase(state, Phase::Greeting);
    if let Err(e) = link.send(&Frame::Hello(cfg.hello())).await {
        finish(state, format!("could not greet the owner: {e}"));
        return false;
    }

    let waiting = tokio::time::sleep(CONSENT_TIMEOUT);
    tokio::pin!(waiting);

    loop {
        tokio::select! {
            incoming = link.recv() => match incoming {
                Ok(Frame::Hello(owner)) => {
                    if let Ok(mut s) = state.lock() {
                        s.owner = Some(owner);
                    }
                    set_phase(state, Phase::Open);
                    return true;
                }
                Ok(Frame::Cut { reason }) => {
                    finish(state, reason);
                    return false;
                }
                Ok(Frame::Say { .. }) | Ok(Frame::Ping { .. }) | Ok(Frame::Pong { .. }) => {}
                Ok(_) => {
                    let why = "the owner's end sent session traffic before consenting";
                    link.cut(why).await;
                    finish(state, why);
                    return false;
                }
                Err(e) => {
                    finish(state, format!("the owner's end went away while deciding: {e}"));
                    return false;
                }
            },
            command = rx.recv() => match command {
                None => {
                    link.cut("the operator's daemon is shutting down").await;
                    finish(state, "the daemon is shutting down");
                    return false;
                }
                Some(Command::Cut { reason, reply }) => {
                    link.cut(&reason).await;
                    let _ = reply.send(Ok(json!({"ok": true, "cut": reason})));
                    finish(state, reason);
                    return false;
                }
                // Worth allowing, and the reason the owner's side accepts it
                // before consent: it is how the operator says what they are
                // about to do, to somebody deciding whether to let them.
                Some(Command::Say { text, reply }) => {
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
                finish(state, why);
                return false;
            }
        }
    }
}

/// One command in flight, and what has come back for it so far.
struct Pending {
    task: TaskId,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
    deadline: tokio::time::Instant,
    killed: bool,
    reply: oneshot::Sender<Result<Value, String>>,
}

impl Pending {
    fn collect(&mut self, stream: Stream, data: Vec<u8>) {
        let sink = match stream {
            Stream::Stderr => &mut self.stderr,
            // A `Command` task never produces `Screen`, but a peer is not
            // obliged to be the peer we expect. Keeping it with stdout is
            // better than dropping bytes the owner watched go past.
            Stream::Stdout | Stream::Screen => &mut self.stdout,
        };
        let room = MAX_CAPTURE.saturating_sub(sink.len());
        if data.len() > room {
            self.truncated = true;
        }
        sink.extend_from_slice(&data[..room.min(data.len())]);
    }

    fn answer(self, ended: Ended) {
        let (code, signal) = match ended {
            Ended::Code(c) => (Some(c), None),
            Ended::Signal(s) => (None, Some(s)),
        };
        let _ = self.reply.send(Ok(json!({
            "ok": code == Some(0),
            "task": self.task,
            "exit_code": code,
            "signal": signal,
            "killed": self.killed,
            "truncated": self.truncated,
            "stdout": String::from_utf8_lossy(&self.stdout),
            "stderr": String::from_utf8_lossy(&self.stderr),
        })));
    }

    fn abandon(self, why: &str) {
        let _ = self.reply.send(Err(format!(
            "{why}; what the command had produced by then: {}{}",
            String::from_utf8_lossy(&self.stdout),
            String::from_utf8_lossy(&self.stderr),
        )));
    }
}

/// Carry the open session until either side ends it.
async fn carry(link: &mut Link, state: &Arc<Mutex<State>>, rx: &mut mpsc::Receiver<Command>) {
    let mut pending: Option<Pending> = None;
    let mut next_task: TaskId = 1;
    let mut nonce: u64 = 0;
    let mut beat = tokio::time::interval(KEEPALIVE);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    beat.tick().await; // the first tick is immediate; the socket is fresh

    loop {
        // Rebuilt each turn so it tracks whatever is in flight now. A sleep is
        // cheap and the loop only turns when something happened.
        let overrun = match &pending {
            Some(p) => tokio::time::sleep_until(p.deadline),
            None => tokio::time::sleep(Duration::from_secs(3600)),
        };
        tokio::pin!(overrun);

        tokio::select! {
            incoming = link.recv() => match incoming {
                Ok(Frame::Output { task, stream, data }) => {
                    if let Some(p) = pending.as_mut().filter(|p| p.task == task) {
                        p.collect(stream, data);
                    }
                }
                Ok(Frame::Exit { task, ended }) => {
                    if pending.as_ref().is_some_and(|p| p.task == task) {
                        pending.take().unwrap().answer(ended);
                    }
                }
                Ok(Frame::Cut { reason }) => {
                    if let Some(p) = pending.take() {
                        p.abandon(&format!("the owner ended the session: {reason}"));
                    }
                    finish(state, reason);
                    return;
                }
                Ok(Frame::Ping { nonce }) => {
                    if link.send(&Frame::Pong { nonce }).await.is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    if let Some(p) = pending.take() {
                        p.abandon(&format!("the session ended: {e}"));
                    }
                    finish(state, format!("the owner's end went away: {e}"));
                    return;
                }
            },

            command = rx.recv() => match command {
                None => {
                    link.cut("the operator's daemon is shutting down").await;
                    if let Some(p) = pending.take() {
                        p.abandon("the daemon shut down while the command was running");
                    }
                    finish(state, "the daemon is shutting down");
                    return;
                }
                Some(Command::Cut { reason, reply }) => {
                    link.cut(&reason).await;
                    if let Some(p) = pending.take() {
                        p.abandon(&format!("the session was cut: {reason}"));
                    }
                    let _ = reply.send(Ok(json!({"ok": true, "cut": reason})));
                    finish(state, reason);
                    return;
                }
                Some(Command::Say { text, reply }) => {
                    let sent = link.send(&Frame::Say { text }).await;
                    let _ = reply.send(match sent {
                        Ok(()) => Ok(json!({"ok": true, "said": true})),
                        Err(e) => Err(e.to_string()),
                    });
                }
                Some(Command::Run { command, reply }) => {
                    if pending.is_some() {
                        let _ = reply.send(Err(
                            "this session is already running a command; wait for it \
                             or cut the session"
                                .into(),
                        ));
                        continue;
                    }
                    let task = next_task;
                    next_task += 1;
                    let opened = link.send(&Frame::Open {
                        task,
                        kind: Kind::Command,
                        command: Some(command),
                        cols: 120,
                        rows: 40,
                    }).await;
                    // End-of-input immediately, because nothing here will ever
                    // type at it. Without this a command that reads until its
                    // input runs out — `cat`, `sort`, anything in a pipeline —
                    // waits for a keyboard that does not exist.
                    let opened = opened.and(
                        link.send(&Frame::Input { task, data: Vec::new() }).await
                    );
                    match opened {
                        Ok(()) => {
                            if let Ok(mut s) = state.lock() {
                                s.tasks += 1;
                            }
                            pending = Some(Pending {
                                task,
                                stdout: Vec::new(),
                                stderr: Vec::new(),
                                truncated: false,
                                deadline: tokio::time::Instant::now() + RUN_TIMEOUT,
                                killed: false,
                                reply,
                            });
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e.to_string()));
                            finish(state, format!("the session ended: {e}"));
                            return;
                        }
                    }
                }
            },

            _ = &mut overrun => {
                // Two deadlines, and they mean different things. The first
                // stops the command, which the protocol answers with an `Exit`
                // like any other ending. The second is for a peer that does
                // not answer at all, and it ends the session rather than
                // leaving the caller holding a socket forever.
                match pending.as_mut() {
                    Some(p) if !p.killed => {
                        p.killed = true;
                        p.deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                        let task = p.task;
                        if link.send(&Frame::Close { task }).await.is_err() {
                            break;
                        }
                    }
                    Some(_) => {
                        let p = pending.take().unwrap();
                        p.abandon("the command was stopped but the owner's end never \
                                   reported how it ended");
                        let why = "the owner's end stopped answering";
                        link.cut(why).await;
                        finish(state, why);
                        return;
                    }
                    None => {}
                }
            }

            _ = beat.tick() => {
                nonce += 1;
                if link.send(&Frame::Ping { nonce }).await.is_err() {
                    break;
                }
            }
        }
    }

    if let Some(p) = pending.take() {
        p.abandon("the session ended while the command was running");
    }
    finish(state, "the session ended");
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

    #[test]
    fn output_beyond_the_cap_is_kept_up_to_it_and_reported_as_cut_short() {
        let (tx, _rx) = oneshot::channel();
        let mut pending = Pending {
            task: 1,
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: false,
            deadline: tokio::time::Instant::now(),
            killed: false,
            reply: tx,
        };
        pending.collect(Stream::Stdout, vec![b'x'; MAX_CAPTURE - 1]);
        assert!(!pending.truncated);
        pending.collect(Stream::Stdout, vec![b'y'; 10]);
        assert!(pending.truncated);
        assert_eq!(pending.stdout.len(), MAX_CAPTURE);
    }
}
