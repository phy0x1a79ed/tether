//! A whole session, end to end, with nothing faked but the operator's hands.
//!
//! A real relay on a real socket, the real handshake over it, and the real
//! owner-side session loop driving the real executor. What this checks is not
//! that the pieces compile together: it is that a command the operator asked
//! for ran on this machine and that its output and its exit status came back in
//! that order, through a relay that could read neither.
//!
//! These run only under `test-consent-bypass`, because otherwise they would
//! stop at a prompt with nobody to answer it — which is the point of the
//! prompt. `tests/consent.rs` is the other half of that story.
#![cfg(feature = "test-consent-bypass")]

use std::time::Duration;

use tether_link::{Link, Relay};
use tether_owner::session::{self, Outcome};
use tether_proto::frame::{Ended, Frame, Hello, Kind, Role, Stream, PROTOCOL_VERSION};
use tether_proto::invite::{InviteCode, Phrase, Slot};
use tether_relay::config::{Config, Limits};
use tether_relay::Running;

const BEARER: &str = "a-test-bearer";

/// A relay, a slot on it, and the two halves of the credential.
#[derive(Clone)]
struct Invite {
    relay: Relay,
    code: InviteCode,
    seat: String,
}

async fn rendezvous() -> (Running, Invite) {
    let running = tether_relay::start(Config {
        port: 0,
        issue_token: BEARER.into(),
        assets: None,
        max_sessions: 8,
        client_ip_header: "cf-connecting-ip".into(),
        build: "test".into(),
        limits: Limits::default(),
    })
    .await
    .expect("the relay should bind");

    let relay = Relay::parse(&format!("http://{}", running.addr)).unwrap();
    let issued = tether_link::http::post_json(&relay, "/issue", Some(BEARER))
        .await
        .expect("the operator should be able to ask for a slot");

    let invite = Invite {
        relay,
        code: InviteCode {
            slot: Slot::new(issued["slot"].as_u64().unwrap() as u32).unwrap(),
            phrase: Phrase::mint(2).unwrap(),
        },
        seat: issued["token"].as_str().unwrap().to_string(),
    };
    (running, invite)
}

fn hello(role: Role, who: &str) -> Hello {
    Hello {
        version: PROTOCOL_VERSION,
        role,
        who: who.into(),
        host: "altair".into(),
        os: "linux".into(),
        build: "test".into(),
    }
}

/// What the operator's side saw, once the session was over.
#[derive(Default, Debug)]
struct Seen {
    consented: bool,
    output: Vec<(Stream, Vec<u8>)>,
    exit: Option<Ended>,
    /// Set if any `Output` arrived after its task's `Exit`, which the protocol
    /// forbids and the owner's transcript depends on.
    exit_came_early: bool,
    cut: Option<String>,
}

impl Seen {
    fn text(&self, stream: Stream) -> String {
        self.output
            .iter()
            .filter(|(s, _)| *s == stream)
            .map(|(_, data)| String::from_utf8_lossy(data))
            .collect()
    }

    fn screen(&self) -> String {
        self.text(Stream::Screen)
    }
}

/// What the operator does once the owner has let them in.
#[derive(Clone)]
enum Plan {
    Run {
        kind: Kind,
        command: Option<String>,
    },
    /// Say a word and end it, without asking for anything.
    LeaveAtOnce,
}

/// The operator's side: greet, follow the plan, then leave.
async fn operate(invite: Invite, plan: Plan, keystrokes: Option<&'static str>) -> Seen {
    let mut link = Link::dial_operator(
        &invite.relay,
        invite.code.slot,
        &invite.seat,
        &invite.code.phrase,
    )
    .await
    .expect("the operator should reach the relay");

    link.send(&Frame::Hello(hello(Role::Operator, "awm as tony")))
        .await
        .unwrap();

    let mut seen = Seen::default();
    loop {
        let Ok(frame) = link.recv().await else { break };
        match frame {
            // The owner's Hello *is* the consent, and nothing is asked for
            // before it arrives. That is the operator's half of the contract.
            Frame::Hello(_) => {
                seen.consented = true;
                match &plan {
                    Plan::LeaveAtOnce => {
                        link.cut("the operator changed their mind").await;
                        break;
                    }
                    Plan::Run { kind, command } => {
                        link.send(&Frame::Open {
                            task: 1,
                            kind: *kind,
                            command: command.clone(),
                            cols: 80,
                            rows: 24,
                        })
                        .await
                        .unwrap();
                        if let Some(keys) = keystrokes {
                            link.send(&Frame::Input {
                                task: 1,
                                data: keys.as_bytes().to_vec(),
                            })
                            .await
                            .unwrap();
                            if *kind == Kind::Command {
                                // An empty Input is end-of-file. Without it a
                                // command that reads its input never finishes.
                                link.send(&Frame::Input {
                                    task: 1,
                                    data: Vec::new(),
                                })
                                .await
                                .unwrap();
                            }
                        }
                    }
                }
            }
            Frame::Output { stream, data, .. } => {
                if seen.exit.is_some() {
                    seen.exit_came_early = true;
                }
                seen.output.push((stream, data));
            }
            Frame::Exit { ended, .. } => {
                seen.exit = Some(ended);
                link.cut("the operator is done").await;
                break;
            }
            Frame::Cut { reason } => {
                seen.cut = Some(reason);
                break;
            }
            _ => {}
        }
    }
    seen
}

/// The owner's side, exactly as the binary runs it.
async fn own(invite: Invite) -> Outcome {
    own_and_record(invite).await.0
}

/// The same, and where the record of it went.
async fn own_and_record(invite: Invite) -> (Outcome, Option<std::path::PathBuf>) {
    log_into_a_directory_of_our_own();
    let me = hello(Role::Owner, "the owner");
    let mut link = match Link::dial_owner(&invite.relay, &invite.code).await {
        Ok(link) => link,
        Err(e) => return (Outcome::Failed(e.to_string()), None),
    };
    let mut log = tether_owner::log::Log::open(invite.code.slot.get())
        .map_err(|why| format!("no record could be written to {why}"));
    let path = log.as_ref().ok().map(|l| l.path().to_path_buf());
    let operator = match session::greet(&mut link, &invite.relay, &invite.code, &me, &mut log).await
    {
        Ok(operator) => operator,
        Err(outcome) => {
            if let Ok(log) = log {
                log.discard();
            }
            return (outcome, path);
        }
    };
    let outcome = session::run(
        link,
        operator,
        invite.relay.to_string(),
        invite.code.slot.to_string(),
        log.ok(),
    )
    .await;
    (outcome, path)
}

/// Keep every record this test binary writes in one place of its own.
///
/// Without it they land beside the test runner, which is a build directory
/// nobody expects to fill up. Set once for the process; the names within it are
/// already distinct, so parallel tests do not collide.
fn log_into_a_directory_of_our_own() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("tether-logs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("TETHER_LOG_DIR", &dir);
    });
}

/// Run both halves against each other and hand back what each saw.
async fn session(invite: Invite, plan: Plan, keystrokes: Option<&'static str>) -> (Seen, Outcome) {
    let (seen, outcome, _) = session_and_record(invite, plan, keystrokes).await;
    (seen, outcome)
}

/// The same, and where the owner's record of it went.
async fn session_and_record(
    invite: Invite,
    plan: Plan,
    keystrokes: Option<&'static str>,
) -> (Seen, Outcome, Option<std::path::PathBuf>) {
    // The operator takes its chair first, which is the real order: the invite
    // exists because the operator asked for it.
    let operator = tokio::spawn(operate(invite.clone(), plan, keystrokes));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (outcome, record) = tokio::time::timeout(Duration::from_secs(30), own_and_record(invite))
        .await
        .expect("the owner's session should finish rather than hang");
    let seen = operator
        .await
        .expect("the operator's side should not panic");
    (seen, outcome, record)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_runs_here_and_reports_its_output_then_how_it_ended() {
    let (running, invite) = rendezvous().await;
    let (seen, outcome) = session(
        invite,
        Plan::Run {
            kind: Kind::Command,
            command: Some("printf 'hello owner\\n'; printf 'a note\\n' >&2; exit 3".into()),
        },
        None,
    )
    .await;

    assert!(seen.consented, "the operator never saw the owner's consent");
    assert_eq!(seen.text(Stream::Stdout), "hello owner\n");
    assert_eq!(seen.text(Stream::Stderr), "a note\n");
    assert_eq!(seen.exit, Some(Ended::Code(3)));
    assert!(
        !seen.exit_came_early,
        "every byte of a task's output must reach the wire before its exit"
    );
    assert!(matches!(outcome, Outcome::Cut(_)), "{outcome:?}");
    running.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_that_reads_its_input_gets_what_the_operator_typed() {
    let (running, invite) = rendezvous().await;
    let (seen, _) = session(
        invite,
        Plan::Run {
            kind: Kind::Command,
            command: Some("cat".into()),
        },
        Some("through the relay\n"),
    )
    .await;

    // `cat` ends when its input does, which is the close path doing its job:
    // dropping the child's input is how a command that reads to end-of-file
    // ever finishes.
    assert_eq!(seen.text(Stream::Stdout), "through the relay\n");
    running.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interactive_program_runs_on_a_real_terminal() {
    let (running, invite) = rendezvous().await;
    let (seen, _) = session(
        invite,
        Plan::Run {
            kind: Kind::Shell,
            // A terminal's own answer about itself: this only prints 80 if the
            // program is genuinely attached to a pty of that width.
            command: Some("/bin/sh".into()),
        },
        Some("tput cols; exit\n"),
    )
    .await;

    let screen = seen.screen();
    assert!(
        screen.contains("80"),
        "no terminal width on the screen: {screen:?}"
    );
    // Terminal output, not a pipe: a pty echoes what was typed into it.
    assert!(screen.contains("tput cols"), "{screen:?}");
    running.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn when_the_operator_cuts_the_owner_is_told_why() {
    let (running, invite) = rendezvous().await;
    let (seen, outcome) = session(invite, Plan::LeaveAtOnce, None).await;

    assert!(seen.consented);
    match outcome {
        Outcome::Cut(why) => assert_eq!(why, "the operator changed their mind"),
        other => panic!("expected a cut with a reason, got {other:?}"),
    }
    running.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_phrase_is_refused_by_the_peer_and_not_by_the_relay() {
    let (running, invite) = rendezvous().await;

    let mut wrong = invite.clone();
    wrong.code.phrase = Phrase::parse(&["anchor", "kettle"]).unwrap();
    if wrong.code.phrase == invite.code.phrase {
        // One chance in a million, and a flaky test is worse than a branch.
        running.stop();
        return;
    }

    let operator = tokio::spawn(operate(
        invite,
        Plan::Run {
            kind: Kind::Command,
            command: Some("true".into()),
        },
        None,
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let outcome = tokio::time::timeout(Duration::from_secs(30), own(wrong))
        .await
        .expect("the owner's attempt should end rather than hang");

    match outcome {
        Outcome::Failed(why) => assert!(
            why.contains("invite code"),
            "the owner should be told the code is wrong, not shown a cipher error: {why}"
        ),
        other => panic!("a wrong phrase must not open a session: {other:?}"),
    }
    let _ = operator.await;
    running.stop();
}

/// The record the consent prompt now promises, checked rather than asserted.
///
/// It has to hold what the owner watched go past, and it must not hold the one
/// thing on their screen that is a credential — the invite code sits in the
/// header line, redrawn every second, which is exactly why the logger is told a
/// slot and never a code.
#[tokio::test(flavor = "multi_thread")]
async fn the_record_is_what_the_owner_saw_and_never_the_phrase() {
    let (running, invite) = rendezvous().await;
    let words: Vec<String> = invite
        .code
        .phrase
        .words()
        .iter()
        .map(|w| w.to_string())
        .collect();

    let (_, _, record) = session_and_record(
        invite,
        Plan::Run {
            kind: Kind::Command,
            command: Some("printf 'the disk is fine\n'; exit 0".into()),
        },
        None,
    )
    .await;

    let path = record.expect("a consented session writes a record");
    let text = std::fs::read_to_string(&path).expect("and the file is there afterwards");

    assert!(text.contains("tether session log"), "{text}");
    assert!(text.contains("the disk is fine"), "the output is in it: {text}");
    assert!(text.contains("printf"), "and so is what was asked for: {text}");
    assert!(text.contains("ended     "), "and how long it ran: {text}");

    for word in words {
        assert!(
            !text.contains(&word),
            "the record leaked the phrase word {word:?}"
        );
    }
    running.stop();
}
