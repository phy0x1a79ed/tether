//! The daemon's table of sessions, and the verbs that act on it.
//!
//! One process holds every session this node has open, which is the reason the
//! daemon exists at all: a verb costs a message to a session that is already
//! paired, handshaked and consented, rather than a fresh negotiation with the
//! relay and a fresh prompt at the owner's keyboard.
//!
//! # Naming a session
//!
//! Sessions are keyed on the **slot**, the public first token of the invite
//! code. A caller may pass the whole code or just the slot; only the first
//! token is read. That matters for more than convenience: the phrase is the
//! credential, and a verb that *required* it would put the secret into every
//! argument list, transcript and shell history that ever drives this tool.
//!
//! With exactly one session live, naming it is optional. With several it is
//! not, and the refusal names the slots rather than guessing, because guessing
//! wrong here means running a command on the wrong person's machine.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tether_proto::invite::{InviteCode, Phrase, Slot, DEFAULT_WORDS, MAX_WORDS};
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::session::{self, Command, Session};

pub struct Operator {
    cfg: Arc<Config>,
    sessions: Mutex<BTreeMap<u32, Session>>,
    started: Instant,
}

impl Operator {
    pub fn new(cfg: Config) -> Arc<Self> {
        Arc::new(Self {
            cfg: Arc::new(cfg),
            sessions: Mutex::new(BTreeMap::new()),
            started: Instant::now(),
        })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Answer one verb from the control socket.
    pub async fn handle(&self, verb: &str, args: &Value) -> Result<Value, String> {
        match verb {
            "invite" => self.invite(count(args, "words", DEFAULT_WORDS)?).await,
            "status" => Ok(self.status()),
            "run" => {
                let command = text(args, "command")?;
                self.put(args, |reply| Command::Run { command, reply })
                    .await
            }
            "send" => {
                let text = text(args, "text")?;
                self.put(args, |reply| Command::Say { text, reply }).await
            }
            "cut" => {
                let reason = optional(args, "reason")
                    .unwrap_or_else(|| "the operator ended the session".into());
                self.put(args, |reply| Command::Cut { reason, reply }).await
            }
            other => Err(format!(
                "`{other}` is not a tether verb; this daemon answers invite, \
                 status, run, send and cut"
            )),
        }
    }

    /// Ask the relay for a slot, mint a phrase for it, and start waiting.
    ///
    /// The phrase is minted here and stays here. What goes to the relay is the
    /// request for a slot and nothing else — not the words, not a hash of them,
    /// not anything a twenty-bit space could be enumerated against.
    async fn invite(&self, words: usize) -> Result<Value, String> {
        let bearer = self.cfg.issue_token.as_deref().ok_or_else(|| {
            format!(
                "this host has no bearer for {}, so it cannot open a session there; \
                 set {} in the workspace env file and restart the service",
                self.cfg.relay,
                crate::config::TOKEN_ENV
            )
        })?;
        if !(1..=MAX_WORDS).contains(&words) {
            return Err(format!("a code is between 1 and {MAX_WORDS} words"));
        }

        let answer = tether_link::http::post_json(
            &self.cfg.relay,
            &self.cfg.relay.issue_path(),
            Some(bearer),
        )
        .await
        .map_err(|e| e.to_string())?;
        let slot = answer["slot"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .and_then(|n| Slot::new(n).ok())
            .ok_or("the relay issued no usable slot")?;
        let seat = answer["token"]
            .as_str()
            .ok_or("the relay issued no seat token")?
            .to_string();
        let lifetime = Duration::from_secs(answer["expires_in"].as_u64().unwrap_or(300));

        let phrase = Phrase::mint(words).map_err(|e| e.to_string())?;
        let code = InviteCode { slot, phrase };
        let line = session::bootstrap(&self.cfg.relay, code.slot, &code.phrase);
        let report = json!({
            "ok": true,
            "slot": slot.get(),
            "code": code.to_string(),
            "words": code.phrase.words(),
            "command": line,
            "relay": self.cfg.relay.to_string(),
            "expires_in": lifetime.as_secs(),
            "read_out": format!(
                "Run this on the machine you want help with, then answer the prompt \
                 it shows you: {line}"
            ),
        });

        let session = session::spawn(
            Arc::clone(&self.cfg),
            code,
            seat,
            // Stop waiting when the relay would have expired the slot anyway.
            // Redialling past that point can only be refused.
            lifetime,
        );
        self.sweep();
        if let Ok(mut table) = self.sessions.lock() {
            table.insert(slot.get(), session);
        }
        Ok(report)
    }

    fn status(&self) -> Value {
        self.sweep();
        let sessions: Vec<Value> = match self.sessions.lock() {
            Ok(table) => table
                .values()
                .filter_map(|s| s.state.lock().ok().map(|st| st.report()))
                .collect(),
            Err(_) => Vec::new(),
        };
        json!({
            "ok": true,
            "role": "operator",
            "who": self.cfg.who,
            "host": self.cfg.host,
            "build": self.cfg.build,
            "relay": self.cfg.relay.to_string(),
            "launcher": self.cfg.relay.launcher_url(),
            "can_invite": self.cfg.issue_token.is_some(),
            "uptime_s": self.started.elapsed().as_secs(),
            "socket": self.cfg.socket.display().to_string(),
            "sessions": sessions,
        })
    }

    /// Put one command to the session the caller named, and wait for its reply.
    async fn put<F>(&self, args: &Value, build: F) -> Result<Value, String>
    where
        F: FnOnce(oneshot::Sender<Result<Value, String>>) -> Command,
    {
        let tx = self.target(optional(args, "code").as_deref())?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(build(reply_tx))
            .await
            .map_err(|_| "that session has ended".to_string())?;
        reply_rx
            .await
            .map_err(|_| "that session ended before it answered".to_string())?
    }

    /// Which session a verb is about. See the module docs.
    fn target(&self, code: Option<&str>) -> Result<mpsc::Sender<Command>, String> {
        self.sweep();
        let table = self
            .sessions
            .lock()
            .map_err(|_| "the session table is poisoned".to_string())?;

        if let Some(code) = code {
            let first = code.split_whitespace().next().unwrap_or(code);
            let slot = Slot::parse(first).map_err(|e| e.to_string())?;
            let session = table
                .get(&slot.get())
                .ok_or_else(|| format!("there is no session {slot} on this host"))?;
            let state = session
                .state
                .lock()
                .map_err(|_| "the session's state is poisoned".to_string())?;
            if !state.live() {
                return Err(format!(
                    "session {slot} has ended: {}",
                    state.ended.as_deref().unwrap_or("no reason recorded")
                ));
            }
            return Ok(session.tx.clone());
        }

        let live: Vec<(u32, mpsc::Sender<Command>)> = table
            .iter()
            .filter(|(_, s)| s.state.lock().map(|st| st.live()).unwrap_or(false))
            .map(|(slot, s)| (*slot, s.tx.clone()))
            .collect();
        match live.len() {
            0 => Err("no session is open on this host; mint one with `tether invite`".into()),
            1 => Ok(live.into_iter().next().unwrap().1),
            _ => Err(format!(
                "{} sessions are open here — name one by its slot: {}",
                live.len(),
                live.iter()
                    .map(|(slot, _)| slot.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// Forget sessions that ended long enough ago to stop being news.
    ///
    /// An ended session lingers on purpose: the operator's next question after
    /// one stops is almost always why, and a table that forgets immediately
    /// answers that with silence.
    fn sweep(&self) {
        let now = Instant::now();
        if let Ok(mut table) = self.sessions.lock() {
            table.retain(|_, s| s.state.lock().map(|st| !st.stale(now)).unwrap_or(true));
        }
    }
}

fn text(args: &Value, key: &str) -> Result<String, String> {
    optional(args, key).ok_or_else(|| format!("`{key}` is required"))
}

fn optional(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn count(args: &Value, key: &str, fallback: usize) -> Result<usize, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(fallback),
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| format!("`{key}` must be a whole number")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operator() -> Arc<Operator> {
        Operator::new(Config {
            relay: tether_link::Relay::parse("http://127.0.0.1:1/tether").unwrap(),
            issue_token: None,
            socket: "/tmp/nowhere.sock".into(),
            who: "awm as tester".into(),
            host: "altair".into(),
            build: "test".into(),
        })
    }

    #[tokio::test]
    async fn a_host_with_no_bearer_says_so_rather_than_failing_to_reach_the_relay() {
        let err = operator()
            .handle("invite", &json!({}))
            .await
            .expect_err("a host with no bearer cannot mint");
        assert!(err.contains(crate::config::TOKEN_ENV), "{err}");
    }

    #[tokio::test]
    async fn a_verb_with_no_session_to_act_on_says_how_to_get_one() {
        let err = operator()
            .handle("run", &json!({"command": "true"}))
            .await
            .expect_err("there is no session");
        assert!(err.contains("invite"), "{err}");
    }

    #[tokio::test]
    async fn status_answers_on_a_host_that_can_do_nothing_else() {
        let report = operator().handle("status", &json!({})).await.unwrap();
        assert_eq!(report["can_invite"], json!(false));
        assert_eq!(report["sessions"], json!([]));
    }

    #[tokio::test]
    async fn an_unknown_verb_names_the_ones_that_exist() {
        let err = operator().handle("sudo", &json!({})).await.unwrap_err();
        assert!(err.contains("status"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_argument_is_named() {
        let err = operator().handle("send", &json!({})).await.unwrap_err();
        assert!(err.contains("text"), "{err}");
    }
}
