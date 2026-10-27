//! A whole session through a dropped link, with nothing faked.
//!
//! The real relay, the real operator daemon and the real owner session, with a
//! proxy on each end's path that a test can cut. What a test checks is what the
//! person at each end would see: the command's output whole and in order, a
//! verb refused while the link is down, and every deliberate ending still
//! ending the session at once.
//!
//! These run only under `test-consent-bypass`, for the reason `session.rs`
//! gives, and only on Unix, where the daemon's control socket lives.
#![cfg(all(unix, feature = "test-consent-bypass"))]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tether_link::{Carry, Link, LinkError, Relay};
use tether_operator::config::Config;
use tether_operator::Operator;
use tether_owner::session::{self, Outcome};
use tether_proto::frame::{Hello, Role, PROTOCOL_VERSION};
use tether_proto::invite::InviteCode;
use tether_proto::RESUME_WINDOW;
use tether_relay::config::{Config as RelayConfig, Limits};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

const BEARER: &str = "a-test-bearer";

/// A TCP proxy that can drop every connection and refuse new ones for a while.
struct Proxy {
    addr: SocketAddr,
    down: Arc<AtomicBool>,
    cut: Arc<Notify>,
}

impl Proxy {
    async fn to(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let down = Arc::new(AtomicBool::new(false));
        let cut = Arc::new(Notify::new());
        let (d, c) = (Arc::clone(&down), Arc::clone(&cut));
        tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else {
                    return;
                };
                if d.load(Ordering::SeqCst) {
                    continue;
                }
                let c = Arc::clone(&c);
                tokio::spawn(async move {
                    let Ok(mut outbound) = TcpStream::connect(upstream).await else {
                        return;
                    };
                    tokio::select! {
                        _ = copy_bidirectional(&mut inbound, &mut outbound) => {}
                        _ = c.notified() => {}
                    }
                });
            }
        });
        Self { addr, down, cut }
    }

    fn relay(&self) -> Relay {
        Relay::parse(&format!("http://{}", self.addr)).unwrap()
    }

    fn cut(&self) {
        self.down.store(true, Ordering::SeqCst);
        self.cut.notify_waiters();
    }

    fn restore(&self) {
        self.down.store(false, Ordering::SeqCst);
    }
}

/// A relay, a daemon reaching it through one proxy, and a second proxy for the
/// owner's path.
struct Rig {
    relay: tether_relay::Running,
    relay_config: RelayConfig,
    operator: Arc<Operator>,
    operator_path: Proxy,
    owner_path: Proxy,
    window: Duration,
}

impl Rig {
    async fn new(window: Duration) -> Self {
        let relay_config = RelayConfig {
            port: 0,
            issue_token: BEARER.into(),
            assets: None,
            max_sessions: 8,
            client_ip_header: "cf-connecting-ip".into(),
            build: "test".into(),
            limits: Limits::default(),
        };
        let relay = tether_relay::start(relay_config.clone())
            .await
            .expect("the relay should bind");
        let operator_path = Proxy::to(relay.addr).await;
        let owner_path = Proxy::to(relay.addr).await;
        let operator = Operator::new(Config {
            relay: operator_path.relay(),
            issue_token: Some(BEARER.into()),
            socket: std::env::temp_dir().join("tether-reconnect-unused.sock"),
            who: "awm as tony".into(),
            host: "altair".into(),
            build: "test".into(),
            resume_window: window,
        });
        Self {
            relay,
            relay_config,
            operator,
            operator_path,
            owner_path,
            window,
        }
    }

    /// Mint an invite and start an owner on it, and wait for the session to
    /// open.
    async fn open(&self, can_resume: bool) -> (u64, InviteCode, JoinHandle<Outcome>) {
        let invite = self.operator.handle("invite", &json!({})).await.unwrap();
        let slot = invite["slot"].as_u64().unwrap();
        let words: Vec<&str> = invite["code"].as_str().unwrap().split(' ').collect();
        let code = InviteCode::parse(&words).unwrap();
        let owner = tokio::spawn(own(
            self.owner_path.relay(),
            code.clone(),
            can_resume,
            self.window,
        ));
        self.until(slot, "open").await;
        (slot, code, owner)
    }

    async fn status(&self, slot: u64) -> Value {
        let status = self.operator.handle("status", &json!({})).await.unwrap();
        status["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["slot"] == json!(slot))
            .cloned()
            .unwrap_or(Value::Null)
    }

    async fn until(&self, slot: u64, phase: &str) -> Value {
        for _ in 0..600 {
            let session = self.status(slot).await;
            if session["phase"] == json!(phase) {
                return session;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "session {slot} never reached {phase}: {}",
            self.status(slot).await
        );
    }

    async fn run(&self, command: &str) -> Result<u64, String> {
        self.operator
            .handle("run", &json!({ "command": command }))
            .await
            .map(|ran| ran["task"].as_u64().unwrap())
    }

    /// Wait for a task to end, and gather its standard output.
    async fn finished(&self, task: u64) -> (Value, String) {
        for _ in 0..1500 {
            let (events, _, _) = self
                .operator
                .journal()
                .since(0, tether_operator::journal::BATCH);
            if let Some(ended) = events
                .iter()
                .find(|e| e["type"] == "task.exited" && e["task"] == task)
            {
                let bytes: Vec<u8> = events
                    .iter()
                    .filter(|e| {
                        e["type"] == "task.output" && e["task"] == task && e["stream"] == "stdout"
                    })
                    .filter_map(|e| e["data"].as_str().map(unbase64))
                    .flatten()
                    .collect();
                return (ended.clone(), String::from_utf8_lossy(&bytes).into_owned());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("task {task} never reported how it ended");
    }

    /// Every phase the journal recorded, in order.
    fn phases(&self) -> Vec<Value> {
        let (events, _, _) = self
            .operator
            .journal()
            .since(0, tether_operator::journal::BATCH);
        events
            .into_iter()
            .filter(|e| e["type"] == "session.phase")
            .collect()
    }

    async fn cut(&self, reason: &str) {
        self.operator
            .handle("cut", &json!({ "reason": reason }))
            .await
            .unwrap();
    }
}

fn unbase64(s: &str) -> Vec<u8> {
    const SET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (mut bits, mut have, mut out) = (0u32, 0u32, Vec::new());
    for byte in s.bytes().filter(|b| *b != b'=') {
        let Some(value) = SET.iter().position(|c| *c == byte) else {
            continue;
        };
        bits = (bits << 6) | value as u32;
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push((bits >> have) as u8);
        }
    }
    out
}

/// The owner's side, as the binary runs it, minus the person at the keyboard.
async fn own(relay: Relay, code: InviteCode, can_resume: bool, window: Duration) -> Outcome {
    log_into_a_directory_of_our_own();
    let me = Hello {
        version: PROTOCOL_VERSION,
        role: Role::Owner,
        who: "the owner".into(),
        host: "a laptop".into(),
        os: "linux".into(),
        build: "test".into(),
        resume: can_resume,
    };
    let mut link = match Link::dial_owner_via(&relay, &code, Carry::Socket).await {
        Ok(link) => link,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    let mut log = tether_owner::log::Log::open(code.slot.get())
        .map_err(|why| format!("no record could be written to {why}"));
    let operator = match session::greet(&mut link, &relay, &code, &me, &mut log, None).await {
        Ok(operator) => operator,
        Err(outcome) => return outcome,
    };
    let link = session::carried(link, &operator, &me, &relay, code.slot, window);
    session::run(
        link,
        operator,
        relay.to_string(),
        code.slot.to_string(),
        log.ok(),
    )
    .await
}

fn log_into_a_directory_of_our_own() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("tether-logs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("TETHER_LOG_DIR", &dir);
    });
}

async fn outcome(owner: JoinHandle<Outcome>, within: Duration) -> Outcome {
    tokio::time::timeout(within, owner)
        .await
        .expect("the owner's session should end rather than hang")
        .expect("the owner's session should not panic")
}

/// Forty lines, one every tenth of a second.
const COUNTING: &str = "i=1; while [ $i -le 40 ]; do echo line $i; i=$((i+1)); sleep 0.1; done";

fn counted() -> String {
    (1..=40).map(|i| format!("line {i}\n")).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_running_through_an_owner_side_gap_comes_back_whole() {
    let rig = Rig::new(RESUME_WINDOW).await;
    let (slot, _, owner) = rig.open(true).await;
    assert_eq!(rig.status(slot).await["resumable"], json!(true));

    let task = rig.run(COUNTING).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    rig.owner_path.cut();
    let gap = rig.until(slot, "reconnecting").await;
    assert!(gap["reconnecting"]["gives_up_at_ms"].as_u64().is_some());
    // The command keeps printing on the owner's machine through the gap.
    tokio::time::sleep(Duration::from_secs(4)).await;
    rig.owner_path.restore();

    let (ended, stdout) = rig.finished(task).await;
    assert_eq!(ended["exit_code"], json!(0));
    assert_eq!(stdout, counted(), "every line once, in order");
    let back = rig.until(slot, "open").await;
    assert_eq!(back["resumes"], json!(1));
    let phases: Vec<Value> = rig.phases().iter().map(|p| p["phase"].clone()).collect();
    assert!(
        phases.ends_with(&[json!("open"), json!("reconnecting"), json!("open")]),
        "{phases:?}"
    );

    rig.cut("done here").await;
    let ended = outcome(owner, Duration::from_secs(20)).await;
    assert!(
        matches!(ended, Outcome::Cut(ref why) if why == "done here"),
        "{ended:?}"
    );
    rig.relay.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_verb_during_an_operator_side_gap_says_reconnecting_and_works_once_back() {
    let rig = Rig::new(RESUME_WINDOW).await;
    let (slot, _, owner) = rig.open(true).await;

    rig.operator_path.cut();
    rig.until(slot, "reconnecting").await;
    let refused = rig.run("echo too soon").await.unwrap_err();
    assert!(refused.starts_with("link: reconnecting"), "{refused}");
    assert!(refused.contains("ends in"), "{refused}");
    // What reads the daemon's own state still answers.
    rig.operator.handle("tasks", &json!({})).await.unwrap();

    tokio::time::sleep(Duration::from_secs(2)).await;
    rig.operator_path.restore();
    rig.until(slot, "open").await;
    let task = rig.run("echo back").await.unwrap();
    let (ended, stdout) = rig.finished(task).await;
    assert_eq!(ended["exit_code"], json!(0));
    assert_eq!(stdout, "back\n");

    rig.cut("done here").await;
    assert!(matches!(
        outcome(owner, Duration::from_secs(20)).await,
        Outcome::Cut(_)
    ));
    rig.relay.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_that_stays_down_past_its_window_ends_the_session_at_both_ends() {
    let rig = Rig::new(Duration::from_secs(3)).await;
    let (slot, _, owner) = rig.open(true).await;

    rig.owner_path.cut();
    let ended = rig.until(slot, "ended").await;
    assert_eq!(
        ended["ended"],
        json!(LinkError::GaveUp.to_string()),
        "{ended}"
    );
    match outcome(owner, Duration::from_secs(20)).await {
        Outcome::Failed(why) => assert_eq!(why, LinkError::GaveUp.to_string()),
        other => panic!("{other:?}"),
    }
    // And the relay keeps nothing of it.
    for _ in 0..100 {
        if rig.relay.relay.sessions.live() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(rig.relay.relay.sessions.live(), 0);
    rig.relay.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cut_during_a_gap_ends_the_owner_the_moment_it_is_back() {
    let rig = Rig::new(RESUME_WINDOW).await;
    let (slot, _, owner) = rig.open(true).await;

    rig.owner_path.cut();
    rig.until(slot, "reconnecting").await;
    rig.cut("done here").await;
    rig.until(slot, "ended").await;
    rig.owner_path.restore();

    // Well inside the ten minutes: the relay says the session ended.
    match outcome(owner, Duration::from_secs(30)).await {
        Outcome::Cut(why) => assert!(why.contains("ended the session"), "{why}"),
        other => panic!("{other:?}"),
    }
    rig.relay.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_code_run_again_during_a_gap_resumes_nothing_and_sees_nothing() {
    let rig = Rig::new(RESUME_WINDOW).await;
    let (slot, code, owner) = rig.open(true).await;

    rig.owner_path.cut();
    rig.until(slot, "reconnecting").await;
    // A second launcher run, or a stranger who overheard the code. Neither
    // reaches the owner's chair, and neither spends one of its tries.
    let direct = Relay::parse(&format!("http://{}", rig.relay.addr)).unwrap();
    for carry in [Carry::Socket, Carry::Poll] {
        match Link::dial_owner_via(&direct, &code, carry).await {
            Err(e @ LinkError::NoSuchSession) => {
                assert!(e.to_string().contains("new code"), "{e}")
            }
            other => panic!("the code still let somebody in: {:?}", other.err()),
        }
    }
    assert_eq!(rig.status(slot).await["misses"], json!(0));

    rig.owner_path.restore();
    rig.until(slot, "open").await;
    let task = rig.run("echo still mine").await.unwrap();
    assert_eq!(rig.finished(task).await.1, "still mine\n");

    rig.cut("done here").await;
    outcome(owner, Duration::from_secs(20)).await;
    rig.relay.stop();
}

/// A client from before resuming, opposite this daemon: the session runs as it
/// always did, and the first drop ends it.
#[tokio::test(flavor = "multi_thread")]
async fn an_owner_that_cannot_resume_gets_the_old_session() {
    let rig = Rig::new(RESUME_WINDOW).await;
    let (slot, _, owner) = rig.open(false).await;
    assert_eq!(rig.status(slot).await["resumable"], json!(false));

    let task = rig.run("echo hello").await.unwrap();
    assert_eq!(rig.finished(task).await.1, "hello\n");

    rig.owner_path.cut();
    let ended = rig.until(slot, "ended").await;
    assert!(
        ended["ended"].as_str().unwrap().contains("went away"),
        "{ended}"
    );
    assert!(!rig
        .phases()
        .iter()
        .any(|p| p["phase"] == json!("reconnecting")));
    outcome(owner, Duration::from_secs(20)).await;
    rig.relay.stop();
}

/// A restarted relay remembers nothing. The daemon puts its slot back, and the
/// owner finds it there.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_survives_its_relay_restarting() {
    let mut rig = Rig::new(RESUME_WINDOW).await;
    let (slot, _, owner) = rig.open(true).await;

    rig.operator_path.cut();
    rig.owner_path.cut();
    rig.until(slot, "reconnecting").await;
    let port = rig.relay.addr.port();
    rig.relay.stop();
    tokio::time::sleep(Duration::from_millis(200)).await;
    rig.relay = tether_relay::start(RelayConfig {
        port,
        ..rig.relay_config.clone()
    })
    .await
    .expect("the relay should bind its old port again");
    rig.operator_path.restore();
    rig.owner_path.restore();

    rig.until(slot, "open").await;
    let task = rig.run("echo after the restart").await.unwrap();
    assert_eq!(rig.finished(task).await.1, "after the restart\n");

    rig.cut("done here").await;
    assert!(matches!(
        outcome(owner, Duration::from_secs(20)).await,
        Outcome::Cut(_)
    ));
    rig.relay.stop();
}
