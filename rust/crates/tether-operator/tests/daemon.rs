//! The daemon against a real relay, with a stand-in at the owner's end.
//!
//! The owner's end here is written out rather than being the shipped client,
//! and that is deliberate twice over. It keeps the operator's contract under
//! test on its own terms — what this side puts on the wire and in what order —
//! and it keeps `tether-owner`'s consent-bypass feature out of this crate's
//! dependency graph, because a feature enabled for a test is a feature the
//! workspace build unifies, and the test that proves a shipped binary has no
//! way past its prompt would then be proving it about a binary that does.
//!
//! The relay is real, the handshake is real, and neither end here can see the
//! other's traffic through it.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tether_link::{Link, Relay};
use tether_operator::config::Config;
use tether_operator::Operator;
use tether_proto::frame::{Ended, Frame, Hello, Role, Stream, PROTOCOL_VERSION};
use tether_proto::invite::InviteCode;
use tether_relay::config::{Config as RelayConfig, Limits};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const BEARER: &str = "a-test-bearer";

struct Rig {
    relay: tether_relay::Running,
    operator: Arc<Operator>,
    address: Relay,
}

impl Rig {
    fn stop(self) {
        self.relay.stop();
    }
}

async fn rig(limits: Limits) -> Rig {
    let relay = tether_relay::start(RelayConfig {
        port: 0,
        issue_token: BEARER.into(),
        assets: None,
        max_sessions: 8,
        client_ip_header: "cf-connecting-ip".into(),
        build: "test".into(),
        limits,
    })
    .await
    .expect("the relay should bind");

    let address = Relay::parse(&format!("http://{}", relay.addr)).unwrap();
    let operator = Operator::new(Config {
        relay: address.clone(),
        issue_token: Some(BEARER.into()),
        socket: socket_path(),
        who: "awm as tony".into(),
        host: "altair".into(),
        build: "test".into(),
    });
    Rig {
        relay,
        operator,
        address,
    }
}

/// A socket path nothing else in this run will pick.
fn socket_path() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
        "tether-test-{}-{}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}


/// Wait for a task to end, and gather what it produced.
///
/// The test's stand-in for `drain --until`, and the shape every caller now has:
/// `run` says which task, and the result is read from the stream afterwards.
async fn finished(rig: &Rig, task: u64) -> (Value, String, String) {
    for _ in 0..250 {
        let (events, _, _) = rig
            .operator
            .journal()
            .since(0, tether_operator::journal::BATCH);
        let ended = events
            .iter()
            .find(|e| e["type"] == "task.exited" && e["task"] == task)
            .cloned();
        if let Some(ended) = ended {
            let gather = |stream: &str| -> String {
                let bytes: Vec<u8> = events
                    .iter()
                    .filter(|e| {
                        e["type"] == "task.output"
                            && e["task"] == task
                            && e["stream"] == stream
                    })
                    .filter_map(|e| e["data"].as_str().map(unbase64))
                    .flatten()
                    .collect();
                String::from_utf8_lossy(&bytes).into_owned()
            };
            return (ended, gather("stdout"), gather("stderr"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {task} never reported how it ended");
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

/// What the owner's end saw, and what it did about it.
#[derive(Default, Debug)]
struct Seen {
    /// Frames that arrived before this end consented. Anything but a greeting
    /// or a spoken line in here is the operator breaking the one rule it is
    /// asked to keep.
    before_consent: Vec<String>,
    operator: Option<Hello>,
    opened: Vec<String>,
    /// Every `Input`, in order. An empty one is end-of-file.
    input: Vec<Vec<u8>>,
    /// Every `Resize`, so a terminal's negotiated size is checkable.
    resized: Vec<(u16, u16)>,
    said: Vec<String>,
    cut: Option<String>,
}

/// How the stand-in answers a task it is asked to open.
#[derive(Clone, Copy)]
enum Owner {
    /// Report some output and a status, as a real executor would.
    Report(i32),
    /// Say nothing at all, so the operator's own deadline is what ends it.
    Ignore,
}

/// The owner's end: consent, then answer whatever is asked for.
async fn own(address: Relay, code: InviteCode, how: Owner) -> Seen {
    let mut link = Link::dial_owner(&address, &code)
        .await
        .expect("the owner's end should reach the relay");
    let mut seen = Seen::default();

    // Everything before the consent below, recorded rather than acted on.
    loop {
        match link.recv().await {
            Ok(Frame::Hello(operator)) => {
                seen.operator = Some(operator);
                break;
            }
            Ok(Frame::Say { text }) => {
                seen.before_consent.push("say".into());
                seen.said.push(text);
            }
            Ok(Frame::Cut { reason }) => {
                seen.cut = Some(reason);
                return seen;
            }
            Ok(other) => seen.before_consent.push(name(&other)),
            Err(_) => return seen,
        }
    }

    // This is the consent. Nothing has run at this end before it.
    link.send(&Frame::Hello(Hello {
        version: PROTOCOL_VERSION,
        role: Role::Owner,
        who: "the owner".into(),
        host: "a mac".into(),
        os: "macos".into(),
        build: "test".into(),
    }))
    .await
    .unwrap();

    loop {
        match link.recv().await {
            Ok(Frame::Open { task, command, .. }) => {
                seen.opened.push(command.unwrap_or_default());
                if let Owner::Report(code) = how {
                    link.send(&Frame::Output {
                        task,
                        stream: Stream::Stdout,
                        data: b"from the owner's machine\n".to_vec(),
                    })
                    .await
                    .unwrap();
                    link.send(&Frame::Output {
                        task,
                        stream: Stream::Stderr,
                        data: b"a note\n".to_vec(),
                    })
                    .await
                    .unwrap();
                    link.send(&Frame::Exit {
                        task,
                        ended: Ended::Code(code),
                    })
                    .await
                    .unwrap();
                }
            }
            Ok(Frame::Input { data, .. }) => seen.input.push(data),
            Ok(Frame::Resize { cols, rows, .. }) => seen.resized.push((cols, rows)),
            Ok(Frame::Say { text }) => seen.said.push(text),
            Ok(Frame::Ping { nonce }) => link.send(&Frame::Pong { nonce }).await.unwrap(),
            Ok(Frame::Cut { reason }) => {
                seen.cut = Some(reason);
                return seen;
            }
            Ok(_) => {}
            Err(_) => return seen,
        }
    }
}

fn name(frame: &Frame) -> String {
    match frame {
        Frame::Hello(_) => "hello",
        Frame::Open { .. } => "open",
        Frame::Input { .. } => "input",
        Frame::Resize { .. } => "resize",
        Frame::Output { .. } => "output",
        Frame::Exit { .. } => "exit",
        Frame::Close { .. } => "close",
        Frame::Say { .. } => "say",
        Frame::Cut { .. } => "cut",
        Frame::Ping { .. } => "ping",
        Frame::Pong { .. } => "pong",
    }
    .into()
}

/// Mint an invite and put an owner on the other end of it.
async fn invited(rig: &Rig, how: Owner) -> (Value, tokio::task::JoinHandle<Seen>) {
    let invite = rig
        .operator
        .handle("invite", &json!({}))
        .await
        .expect("the operator should be able to mint an invite");
    let code = InviteCode::parse(
        &invite["code"]
            .as_str()
            .unwrap()
            .split(' ')
            .collect::<Vec<_>>(),
    )
    .expect("the minted code should parse as one");
    let owner = tokio::spawn(own(rig.address.clone(), code, how));
    (invite, owner)
}

/// Wait for a session to reach a phase, so a test never races the dial.
async fn until(rig: &Rig, slot: u64, phase: &str) {
    for _ in 0..300 {
        let status = rig.operator.handle("status", &json!({})).await.unwrap();
        let found = status["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["slot"] == json!(slot));
        if found.is_some_and(|s| s["phase"] == json!(phase)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("session {slot} never reached {phase}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invite_is_a_slot_from_the_relay_and_words_the_relay_never_sees() {
    let rig = rig(Limits::default()).await;
    let invite = rig.operator.handle("invite", &json!({})).await.unwrap();

    let slot = invite["slot"].as_u64().unwrap();
    assert!((1..=999).contains(&slot), "{slot} is not a spoken slot");
    assert_eq!(invite["words"].as_array().unwrap().len(), 2);

    // The line the owner is read out: plain tokens after `bash -s`, nothing to
    // quote and nothing to punctuate.
    let line = invite["command"].as_str().unwrap();
    let tail = line.split(" -s ").nth(1).unwrap();
    assert_eq!(
        tail,
        format!(
            "{slot} {}",
            invite["words"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| w.as_str().unwrap())
                .collect::<Vec<_>>()
                .join(" ")
        )
    );

    // The relay allocated the slot and knows nothing else: its own status
    // counts a session and has no field in which a phrase could sit.
    assert_eq!(rig.relay.relay.sessions.live(), 1);
    rig.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_asked_for_before_the_owner_says_yes() {
    let rig = rig(Limits::default()).await;
    let (invite, owner) = invited(&rig, Owner::Report(0)).await;
    let slot = invite["slot"].as_u64().unwrap();
    until(&rig, slot, "open").await;

    let ran = rig
        .operator
        .handle("run", &json!({"command": "echo hello"}))
        .await
        .unwrap();
    let (ended, _, _) = finished(&rig, ran["task"].as_u64().unwrap()).await;
    assert_eq!(ended["exit_code"], json!(0));

    rig.operator
        .handle("cut", &json!({"reason": "done"}))
        .await
        .unwrap();
    let seen = owner.await.unwrap();

    assert!(
        seen.before_consent.is_empty(),
        "the operator asked for something before consent: {:?}",
        seen.before_consent
    );
    assert_eq!(seen.operator.unwrap().who, "awm as tony");
    rig.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_goes_out_with_its_input_already_closed_and_comes_back_whole() {
    let rig = rig(Limits::default()).await;
    let (invite, owner) = invited(&rig, Owner::Report(3)).await;
    until(&rig, invite["slot"].as_u64().unwrap(), "open").await;

    let ran = rig
        .operator
        .handle("run", &json!({"command": "cat /etc/hostname"}))
        .await
        .unwrap();
    // The reply is the task's number and nothing else, and it arrives before
    // the command has done anything. That is the point: the caller is not
    // holding a socket open while somebody else's machine works.
    assert_eq!(ran["ok"], json!(true), "the ask succeeded; the command has not run");
    assert_eq!(ran["kind"], json!("command"));
    assert!(ran["cursor"].is_number(), "and it says where to read from");

    let (ended, out, err) = finished(&rig, ran["task"].as_u64().unwrap()).await;
    assert_eq!(ended["exit_code"], json!(3));
    assert_eq!(out, "from the owner's machine\n");
    assert_eq!(err, "a note\n");

    rig.operator.handle("cut", &json!({})).await.unwrap();
    let seen = owner.await.unwrap();

    assert_eq!(seen.opened, vec!["cat /etc/hostname".to_string()]);
    // An empty Input is end-of-file. Nothing here will ever type at a command,
    // so saying so up front is what stops `cat` waiting for a keyboard.
    assert_eq!(
        seen.input,
        vec![Vec::<u8>::new()],
        "the command was not told its input was over"
    );
    rig.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn several_commands_run_at_once_and_report_separately() {
    let rig = rig(Limits::default()).await;
    let (invite, owner) = invited(&rig, Owner::Report(0)).await;
    until(&rig, invite["slot"].as_u64().unwrap(), "open").await;

    // This used to be refused. One task at a time was never a rule about
    // safety — it was a consequence of the reply being the result, and it went
    // when the reply stopped being the result.
    let first = rig
        .operator
        .handle("run", &json!({"command": "one"}))
        .await
        .unwrap();
    let second = rig
        .operator
        .handle("run", &json!({"command": "two"}))
        .await
        .unwrap();
    assert_ne!(first["task"], second["task"], "each ask names its own task");

    let (a, _, _) = finished(&rig, first["task"].as_u64().unwrap()).await;
    let (b, _, _) = finished(&rig, second["task"].as_u64().unwrap()).await;
    assert_eq!(a["exit_code"], json!(0));
    assert_eq!(b["exit_code"], json!(0));

    rig.operator.handle("cut", &json!({})).await.unwrap();
    let _ = owner.await;
    rig.stop();
}

/// The limit that did survive, and it is about resources rather than order.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_will_not_hold_more_tasks_than_it_can_account_for() {
    let rig = rig(Limits::default()).await;
    let (invite, owner) = invited(&rig, Owner::Ignore).await;
    until(&rig, invite["slot"].as_u64().unwrap(), "open").await;

    // The stand-in never answers, so none of these ever ends.
    for _ in 0..8 {
        rig.operator
            .handle("run", &json!({"command": "sleep"}))
            .await
            .unwrap();
    }
    let err = rig
        .operator
        .handle("run", &json!({"command": "one too many"}))
        .await
        .expect_err("the ninth is refused");
    assert!(err.contains("tasks open"), "{err}");

    let listed = rig.operator.handle("tasks", &json!({})).await.unwrap();
    assert_eq!(listed["tasks"].as_array().unwrap().len(), 8);

    rig.operator.handle("cut", &json!({})).await.unwrap();
    let _ = owner.await;
    rig.stop();
}

/// A terminal, which nothing in this tool had ever asked for until now.
///
/// One per session, because the owner has one screen and a second terminal is
/// a second thing happening on their machine that they cannot watch.
#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_opens_once_and_takes_what_is_typed_at_it() {
    let rig = rig(Limits::default()).await;
    let (invite, owner) = invited(&rig, Owner::Ignore).await;
    until(&rig, invite["slot"].as_u64().unwrap(), "open").await;

    let opened = rig
        .operator
        .handle("shell", &json!({"cols": 100, "rows": 30}))
        .await
        .unwrap();
    assert_eq!(opened["kind"], json!("shell"));
    let task = opened["task"].as_u64().unwrap();

    let err = rig
        .operator
        .handle("shell", &json!({}))
        .await
        .expect_err("a second terminal is refused");
    assert!(err.contains(&task.to_string()), "and it names the open one: {err}");

    // Typing at a program is a different verb from speaking to a person, and
    // the bytes go out as given so a key with no printable form survives.
    rig.operator
        .handle("keys", &json!({"task": task, "text": "ls", "enter": true}))
        .await
        .unwrap();
    rig.operator
        .handle("keys", &json!({"task": task, "data": "Aw=="}))
        .await
        .unwrap();
    rig.operator
        .handle("resize", &json!({"task": task, "cols": 80, "rows": 24}))
        .await
        .unwrap();

    rig.operator.handle("cut", &json!({})).await.unwrap();
    let seen = owner.await.unwrap();
    assert_eq!(
        seen.input,
        vec![b"ls\n".to_vec(), vec![3]],
        "a terminal is told nothing about end of input, unlike a command"
    );
    assert_eq!(seen.resized, vec![(80, 24)]);
    rig.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn cutting_tells_the_owner_why_and_leaves_the_session_explained() {
    let rig = rig(Limits::default()).await;
    let (invite, owner) = invited(&rig, Owner::Report(0)).await;
    let slot = invite["slot"].as_u64().unwrap();
    until(&rig, slot, "open").await;

    rig.operator
        .handle("cut", &json!({"reason": "the backup is finished"}))
        .await
        .unwrap();

    let seen = owner.await.unwrap();
    assert_eq!(seen.cut.as_deref(), Some("the backup is finished"));

    // The session stays in the table long enough to say what happened to it.
    let status = rig.operator.handle("status", &json!({})).await.unwrap();
    let session = &status["sessions"].as_array().unwrap()[0];
    assert_eq!(session["phase"], json!("ended"));
    assert_eq!(session["ended"], json!("the backup is finished"));
    assert_eq!(session["tasks"], json!(0));

    // And a verb aimed at it is refused with that reason rather than silence.
    let err = rig
        .operator
        .handle("run", &json!({"command": "true", "code": slot.to_string()}))
        .await
        .unwrap_err();
    assert!(err.contains("the backup is finished"), "{err}");
    rig.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spoken_line_reaches_the_owner_while_they_are_still_deciding() {
    let rig = rig(Limits::default()).await;
    let invite = rig.operator.handle("invite", &json!({})).await.unwrap();
    let slot = invite["slot"].as_u64().unwrap();
    let code = InviteCode::parse(
        &invite["code"]
            .as_str()
            .unwrap()
            .split(' ')
            .collect::<Vec<_>>(),
    )
    .unwrap();

    // An owner that pairs and then sits on the prompt, as a person would.
    let address = rig.address.clone();
    let owner = tokio::spawn(async move {
        let mut link = Link::dial_owner(&address, &code).await.unwrap();
        let mut said = Vec::new();
        loop {
            match link.recv().await {
                Ok(Frame::Say { text }) => said.push(text),
                Ok(Frame::Cut { .. }) | Err(_) => return said,
                Ok(_) => {}
            }
        }
    });
    until(&rig, slot, "greeting").await;

    rig.operator
        .handle(
            "send",
            &json!({"text": "it's tony — about to start the backup"}),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    rig.operator.handle("cut", &json!({})).await.unwrap();

    let said = owner.await.unwrap();
    assert_eq!(
        said,
        vec!["it's tony — about to start the backup".to_string()]
    );
    rig.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invite_nobody_redeems_ends_as_expired_rather_than_lingering() {
    let rig = rig(Limits {
        issue_ttl: Duration::from_secs(2),
        ..Limits::default()
    })
    .await;
    let invite = rig.operator.handle("invite", &json!({})).await.unwrap();
    assert_eq!(invite["expires_in"], json!(2));

    until(&rig, invite["slot"].as_u64().unwrap(), "ended").await;
    let status = rig.operator.handle("status", &json!({})).await.unwrap();
    let ended = status["sessions"].as_array().unwrap()[0]["ended"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(ended.contains("expired"), "{ended}");
    rig.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_control_socket_answers_one_line_of_json_per_connection() {
    let rig = rig(Limits::default()).await;
    let running = tether_operator::start(Config {
        relay: rig.address.clone(),
        issue_token: Some(BEARER.into()),
        socket: socket_path(),
        who: "awm as tony".into(),
        host: "altair".into(),
        build: "test".into(),
    })
    .await
    .expect("the daemon should bind its socket");

    let answer = ask(running.socket(), json!({"verb": "status", "args": {}})).await;
    assert_eq!(answer["ok"], json!(true));
    assert_eq!(answer["role"], json!("operator"));
    assert_eq!(answer["can_invite"], json!(true));

    // An error is an answer too — the adapter must never have to read a
    // closed socket as a verdict.
    let answer = ask(running.socket(), json!({"verb": "reboot", "args": {}})).await;
    assert_eq!(answer["ok"], json!(false));
    assert!(answer["error"].as_str().unwrap().contains("invite"));

    // And the socket is this user's alone.
    let mode = std::fs::metadata(running.socket()).unwrap().permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
        0o600
    );

    let path = running.socket().to_path_buf();
    running.stop();
    assert!(!path.exists(), "a stopped daemon left its socket behind");
    rig.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_daemon_will_not_unlink_a_socket_that_is_being_served() {
    let rig = rig(Limits::default()).await;
    let path = socket_path();
    let config = Config {
        relay: rig.address.clone(),
        issue_token: Some(BEARER.into()),
        socket: path.clone(),
        who: "awm as tony".into(),
        host: "altair".into(),
        build: "test".into(),
    };
    let first = tether_operator::start(config.clone()).await.unwrap();

    match tether_operator::start(config.clone()).await {
        Err(tether_operator::control::BindError::Taken(taken)) => assert_eq!(taken, path),
        Ok(_) => panic!("two daemons took the same socket"),
        Err(e) => panic!("{e}"),
    }

    // A socket left by a daemon that was killed is a corpse, and taking it is
    // the whole reason the guard asks rather than looks.
    first.stop();
    std::fs::write(&path, b"").unwrap();
    let third = tether_operator::start(config).await.unwrap();
    third.stop();
    rig.stop();
}

async fn ask(socket: &std::path::Path, request: Value) -> Value {
    let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_vec(&request).unwrap();
    line.push(b'\n');
    write.write_all(&line).await.unwrap();
    write.flush().await.unwrap();

    let mut reply = String::new();
    BufReader::new(read).read_line(&mut reply).await.unwrap();
    serde_json::from_str(&reply).expect("the daemon should answer with JSON")
}

/// Everything a session did, as a reader who was not the caller sees it.
///
/// This is the property `run`'s blocking reply never had: the caller got the
/// output and nobody else could see any of it, including the same caller a
/// moment later. Here a reader that was not in the room asks afterwards and
/// gets the whole thing.
#[tokio::test]
async fn a_session_is_readable_afterwards_by_somebody_who_was_not_the_caller() {
    let rig = rig(Limits::default()).await;
    let (invite, owner) = invited(&rig, Owner::Report(0)).await;
    let slot = invite["slot"].as_u64().unwrap();
    until(&rig, slot, "open").await;

    let ran = rig
        .operator
        .handle("run", &json!({"command": "df -h"}))
        .await
        .unwrap();
    finished(&rig, ran["task"].as_u64().unwrap()).await;
    rig.operator
        .handle("send", &json!({"text": "checking the disk"}))
        .await
        .unwrap();
    rig.operator.handle("cut", &json!({})).await.unwrap();
    owner.await.unwrap();

    let (events, _, gap) = rig.operator.journal().since(0, tether_operator::journal::BATCH);
    assert!(gap.is_none(), "nothing should have been evicted");
    let kinds: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();

    for expected in [
        "daemon.started",
        "session.minted",
        "session.phase",
        "task.started",
        "task.output",
        "task.exited",
        "operator.said",
    ] {
        assert!(kinds.contains(&expected), "missing {expected}: {kinds:?}");
    }

    let started = events.iter().find(|e| e["type"] == "task.started").unwrap();
    assert_eq!(started["command"], "df -h");
    assert_eq!(started["slot"], slot);
    assert_eq!(started["kind"], "command");

    let exited = events.iter().find(|e| e["type"] == "task.exited").unwrap();
    assert_eq!(exited["exit_code"], 0);

    // Ordering within a session is what makes a transcript readable.
    let at = |kind: &str| events.iter().position(|e| e["type"] == kind).unwrap();
    assert!(at("task.started") < at("task.output"));
    assert!(at("task.output") < at("task.exited"));

    let ended = events
        .iter()
        .rev()
        .find(|e| e["type"] == "session.phase" && e["phase"] == "ended")
        .expect("the ending is a fact too");
    assert!(ended["ended"].as_str().unwrap().contains("operator"));
    rig.stop();
}

/// The credential must not travel with the story.
#[tokio::test]
async fn the_phrase_is_in_no_event_the_stream_carries() {
    let rig = rig(Limits::default()).await;
    let (invite, owner) = invited(&rig, Owner::Report(0)).await;
    until(&rig, invite["slot"].as_u64().unwrap(), "open").await;
    rig.operator
        .handle("run", &json!({"command": "true"}))
        .await
        .unwrap();
    rig.operator.handle("cut", &json!({})).await.unwrap();
    owner.await.unwrap();

    let phrase: Vec<String> = invite["words"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap().to_string())
        .collect();
    assert_eq!(phrase.len(), 2, "the fixture assumes a two-word phrase");

    let (events, _, _) = rig.operator.journal().since(0, tether_operator::journal::BATCH);
    // Values only. A field name is part of the vocabulary rather than part of
    // the session, and the word list is ordinary English — `signal` is both a
    // plausible phrase word and the name of a field on every task that ended.
    let mut said = Vec::new();
    for event in &events {
        collect_strings(event, &mut said);
    }
    for word in phrase {
        for value in &said {
            assert!(
                !value.contains(&word),
                "the stream leaked {word:?} in {value:?}"
            );
        }
    }
    rig.stop();
}

/// Every string a reader would actually receive as content.
fn collect_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| collect_strings(v, out)),
        _ => {}
    }
}

/// A watcher is told where it stands before it is told anything else, and then
/// sees what happens next without having asked for it.
#[tokio::test]
async fn a_watcher_is_placed_in_the_stream_and_then_kept_up_to_date() {
    let rig = rig(Limits::default()).await;
    let running = tether_operator::control::serve(
        Arc::clone(&rig.operator),
        &rig.operator.config().socket.clone(),
    )
    .await
    .expect("the control socket should bind");

    let stream = tokio::net::UnixStream::connect(running.path.clone())
        .await
        .unwrap();
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_vec(&json!({"verb": "watch", "args": {"since": 0}})).unwrap();
    line.push(b'\n');
    write.write_all(&line).await.unwrap();
    write.flush().await.unwrap();
    let mut reader = BufReader::new(read);

    let mut first = String::new();
    reader.read_line(&mut first).await.unwrap();
    let opening: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(opening["type"], "watch.open");
    assert_eq!(opening["cursor"], 0);
    assert!(
        opening["epoch"].as_u64().unwrap() > 0,
        "a cursor means nothing without the daemon it belongs to"
    );

    // Everything already recorded arrives without being asked for again.
    let mut next = String::new();
    reader.read_line(&mut next).await.unwrap();
    let event: Value = serde_json::from_str(&next).unwrap();
    assert_eq!(event["type"], "daemon.started");
    assert_eq!(event["seq"], 0);

    // And so does what happens after the watcher arrived.
    rig.operator.handle("status", &json!({})).await.unwrap();
    let minted = rig.operator.handle("invite", &json!({})).await.unwrap();
    let slot = minted["slot"].as_u64().unwrap();

    let seen = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut l = String::new();
            reader.read_line(&mut l).await.unwrap();
            let e: Value = serde_json::from_str(&l).unwrap();
            if e["type"] == "session.minted" {
                return e;
            }
        }
    })
    .await
    .expect("the minting should reach a watcher that was already attached");

    assert_eq!(seen["slot"], slot);
    assert!(seen.get("code").is_none(), "the code is not a fact for subscribers");
    assert!(seen.get("command").is_none(), "nor is the line that carries it");

    running.stop();
    rig.stop();
}
