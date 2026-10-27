//! The consent gate, checked against the binary that actually ships.
//!
//! Two claims are made about this tool and both are checked here rather than
//! promised:
//!
//! **A shipped binary contains no bypass.** Not a flag defaulting to safe, not
//! a branch behind an environment variable — no such code path at all. The
//! marker string below exists only inside the bypass, so its absence from the
//! built binary is the claim, tested by reading the file.
//!
//! **No terminal means no, until a second run says yes.** The binary is run
//! with no controlling terminal, against a live relay with a real operator
//! waiting. The operator is checked for the deferral rather than a consent, and
//! then the second run is checked: only the words the first run printed, only
//! once, and never before the first run has shown the prompt. A prompt that
//! silently passed when nobody could answer it would be worse than no prompt.

use std::path::Path;

/// Written out here rather than imported, so that the test and the code have to
/// be changed together for the check to stop meaning anything.
const MARKER: &str = "tether-consent-bypass-compiled-into-this-build";

fn binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_tether"))
}

#[test]
#[cfg(not(feature = "test-consent-bypass"))]
fn the_shipped_binary_carries_no_way_past_the_prompt() {
    let bytes = std::fs::read(binary()).expect("the binary under test should exist");
    let found = bytes
        .windows(MARKER.len())
        .any(|window| window == MARKER.as_bytes());
    assert!(
        !found,
        "the bypass marker is in {}, so a build that ships could answer its own prompt",
        binary().display()
    );
}

#[test]
#[cfg(feature = "test-consent-bypass")]
fn the_bypass_build_is_the_one_that_carries_the_marker() {
    // The other half of the check above: if the marker could never appear, its
    // absence would prove nothing.
    let bytes = std::fs::read(binary()).expect("the binary under test should exist");
    assert!(bytes
        .windows(MARKER.len())
        .any(|window| window == MARKER.as_bytes()));
}

#[cfg(not(feature = "test-consent-bypass"))]
/// The no-terminal test needs the binary to have no controlling terminal, and
/// `setsid` is how that is arranged from outside. It is absent on macOS, where
/// this test is skipped rather than weakened.
fn setsid() -> Option<&'static str> {
    ["/usr/bin/setsid", "/bin/setsid"]
        .into_iter()
        .find(|path| Path::new(path).exists())
}

/// A relay, and an operator at one slot of it that says who it is to every
/// owner who pairs and reports what each one answered.
#[cfg(not(feature = "test-consent-bypass"))]
struct Stage {
    running: tether_relay::Running,
    relay: String,
    code: Vec<String>,
    answers: tokio::sync::mpsc::UnboundedReceiver<String>,
}

#[cfg(not(feature = "test-consent-bypass"))]
async fn stage() -> Stage {
    use std::time::Duration;

    use tether_link::{Link, LinkError, Relay};
    use tether_proto::frame::{Frame, Hello, Role, PROTOCOL_VERSION};
    use tether_proto::invite::{Phrase, Slot};
    use tether_relay::config::{Config, Limits};

    const BEARER: &str = "a-test-bearer";
    let running = tether_relay::start(Config {
        port: 0,
        issue_token: BEARER.into(),
        assets: None,
        max_sessions: 4,
        client_ip_header: "cf-connecting-ip".into(),
        build: "test".into(),
        limits: Limits::default(),
    })
    .await
    .expect("the relay should bind");

    let relay = Relay::parse(&format!("http://{}", running.addr)).unwrap();
    let issued = tether_link::http::post_json(&relay, "/issue", Some(BEARER))
        .await
        .unwrap();
    let slot = Slot::new(issued["slot"].as_u64().unwrap() as u32).unwrap();
    let seat = issued["token"].as_str().unwrap().to_string();
    let phrase = Phrase::mint(2).unwrap();
    let mut code = vec![slot.word().to_string()];
    code.extend(phrase.words().iter().map(|w| w.to_string()));

    let (tell, answers) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn({
        let relay = relay.clone();
        async move {
            loop {
                let mut link = match Link::dial_operator(&relay, slot, &seat, &phrase).await {
                    Ok(link) => link,
                    Err(LinkError::NobodyThere | LinkError::Taken) => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                    Err(_) => return,
                };
                let _ = link
                    .send(&Frame::Hello(Hello {
                        version: PROTOCOL_VERSION,
                        role: Role::Operator,
                        who: "awm as tony".into(),
                        host: "altair".into(),
                        os: "linux".into(),
                        build: "test".into(),
                        resume: false,
                    }))
                    .await;
                let answer = loop {
                    match link.recv().await {
                        Ok(Frame::Defer { reason }) => break format!("defer: {reason}"),
                        Ok(Frame::Cut { reason }) => break format!("cut: {reason}"),
                        // Consent. Ended at once: being let in is all this
                        // checks.
                        Ok(Frame::Hello(_)) => {
                            link.cut("that is all").await;
                            break "hello".into();
                        }
                        Ok(_) => continue,
                        Err(_) => break "gone".into(),
                    }
                };
                if tell.send(answer).is_err() {
                    return;
                }
            }
        }
    });

    Stage {
        relay: format!("http://{}", running.addr),
        running,
        code,
        answers,
    }
}

/// What the operator heard from the next owner to pair.
#[cfg(not(feature = "test-consent-bypass"))]
async fn heard(answers: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(std::time::Duration::from_secs(10), answers.recv())
        .await
        .expect("the operator heard nothing")
        .unwrap()
}

/// Run the client with no controlling terminal, in a temporary directory of
/// the test's own so the confirmation it keeps is the test's to inspect.
#[cfg(not(feature = "test-consent-bypass"))]
async fn unattended(
    setsid: &str,
    stage: &Stage,
    tmp: &Path,
    extra: &[&str],
) -> (Option<i32>, String) {
    use std::process::Stdio;
    use std::time::Duration;

    let mut command = tokio::process::Command::new(setsid);
    command
        .arg(binary())
        .args(&stage.code)
        .args(extra)
        .env("TETHER_RELAY", &stage.relay)
        .env("TETHER_LOG_DIR", tmp.join("records"))
        .env("TMPDIR", tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let done = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("the client should give up rather than wait forever")
        .expect("the client should run");
    (
        done.status.code(),
        String::from_utf8_lossy(&done.stderr).into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(not(feature = "test-consent-bypass"))]
async fn with_nobody_at_the_keyboard_the_answer_is_no_until_a_second_run_says_yes() {
    let Some(setsid) = setsid() else {
        eprintln!("no setsid on this host; skipping");
        return;
    };
    let mut stage = stage().await;
    let tmp = std::env::temp_dir().join(format!("tether-unattended-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("records")).unwrap();

    // A confirmation before anything was shown is refused, and before the
    // network: the operator never hears of it.
    let (exit, said) = unattended(setsid, &stage, &tmp, &["--agree", "acre", "acre", "acre"]).await;
    assert_eq!(exit, Some(2), "{said}");
    assert!(said.contains("none is waiting"), "{said}");

    // The first run shows the whole prompt, and says no for now.
    let (exit, said) = unattended(setsid, &stage, &tmp, &[]).await;
    assert_eq!(exit, Some(3), "{said}");
    assert!(said.contains("awm as tony on altair"), "{said}");
    assert!(said.contains("no terminal"), "{said}");
    let answer = heard(&mut stage.answers).await;
    assert!(answer.starts_with("defer: "), "the operator got {answer:?}");
    let words: Vec<String> = said
        .lines()
        .find_map(|line| line.trim().strip_prefix("--agree "))
        .expect("the prompt should print the words that confirm it")
        .split(' ')
        .map(str::to_string)
        .collect();
    // Nobody consented yet, so there is no session to have a record of.
    let left: Vec<_> = std::fs::read_dir(tmp.join("records"))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .collect();
    assert!(
        left.is_empty(),
        "a session nobody agreed to left something behind: {left:?}"
    );
    // And nothing it kept carries the words it printed.
    let kept = std::fs::read_dir(&tmp)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().starts_with("tether-"))
        .expect("the first run should keep a confirmation");
    let record = std::fs::read_to_string(kept.path().join("confirm")).unwrap();
    for word in &words {
        assert!(
            !record.contains(&format!(" {word}\n")),
            "{word} is in {record}"
        );
    }

    // The wrong words are refused, again before the network.
    let mut wrong: Vec<&str> = words.iter().map(String::as_str).collect();
    wrong[0] = if wrong[0] == "acre" { "anchor" } else { "acre" };
    let mut args = vec!["--agree"];
    args.extend(&wrong);
    let (exit, said) = unattended(setsid, &stage, &tmp, &args).await;
    assert_eq!(exit, Some(2), "{said}");
    assert!(said.contains("not the words"), "{said}");

    // The right ones let the operator in.
    let mut args = vec!["--agree"];
    args.extend(words.iter().map(String::as_str));
    let (exit, said) = unattended(setsid, &stage, &tmp, &args).await;
    assert_eq!(
        heard(&mut stage.answers).await,
        "hello",
        "the client said: {said}"
    );
    assert!(said.contains("confirmed with --agree"), "{said}");
    assert_eq!(exit, Some(0), "{said}");

    // Once.
    let (exit, said) = unattended(setsid, &stage, &tmp, &args).await;
    assert_eq!(exit, Some(2), "{said}");
    assert!(said.contains("none is waiting"), "{said}");

    let _ = std::fs::remove_dir_all(&tmp);
    stage.running.stop();
}
