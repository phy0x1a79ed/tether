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
//! **No terminal means no.** The binary is run with no controlling terminal,
//! against a live relay with a real operator waiting, and the operator is
//! checked for the refusal. A prompt that silently passed when nobody could
//! answer it would be worse than no prompt.

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

#[tokio::test(flavor = "multi_thread")]
#[cfg(not(feature = "test-consent-bypass"))]
async fn with_nobody_at_the_keyboard_the_answer_is_no() {
    use std::process::Stdio;
    use std::time::Duration;

    use tether_link::{Link, Relay};
    use tether_proto::frame::{Frame, Hello, Role, PROTOCOL_VERSION};
    use tether_proto::invite::{Phrase, Slot};
    use tether_relay::config::{Config, Limits};

    let Some(setsid) = setsid() else {
        eprintln!("no setsid on this host; skipping");
        return;
    };

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

    // A real operator, waiting to be let in.
    let operator = tokio::spawn({
        let relay = relay.clone();
        let phrase = phrase.clone();
        async move {
            let mut link = Link::dial_operator(&relay, slot, &seat, &phrase)
                .await
                .expect("the operator should reach the relay");
            link.send(&Frame::Hello(Hello {
                version: PROTOCOL_VERSION,
                role: Role::Operator,
                who: "awm as tony".into(),
                host: "altair".into(),
                os: "linux".into(),
                build: "test".into(),
            }))
            .await
            .unwrap();
            loop {
                match link.recv().await {
                    Ok(Frame::Cut { reason }) => return Some(reason),
                    // Consent would show up as a Hello from the owner. It must
                    // not, because there is nobody there to give it.
                    Ok(Frame::Hello(_)) => return None,
                    Ok(_) => continue,
                    Err(_) => return Some(String::new()),
                }
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Somewhere of its own to write a record, so that finding none afterwards
    // means none was written rather than that it went elsewhere.
    let records = std::env::temp_dir().join(format!("tether-refused-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&records);
    std::fs::create_dir_all(&records).unwrap();

    let mut command = tokio::process::Command::new(setsid);
    command
        .arg(binary())
        .arg(slot.to_string())
        .env("TETHER_RELAY", format!("http://{}", running.addr))
        .env("TETHER_LOG_DIR", &records)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for word in phrase.words() {
        command.arg(word);
    }

    let done = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("the client should give up rather than wait forever")
        .expect("the client should run");
    let said = String::from_utf8_lossy(&done.stderr).into_owned();
    let exit = done.status;

    let refusal = tokio::time::timeout(Duration::from_secs(10), operator)
        .await
        .unwrap_or_else(|_| panic!("the operator heard nothing; the client said: {said}"))
        .unwrap();

    assert!(
        !exit.success(),
        "a client that was never allowed in exited 0; it said: {said}"
    );
    assert!(
        said.contains("no terminal"),
        "the owner's screen should say why, and it said: {said}"
    );
    match refusal {
        Some(reason) => assert!(
            reason.contains("no terminal"),
            "the operator should be told why, and got {reason:?}"
        ),
        None => panic!("the owner's side consented with nobody at the keyboard"),
    }

    // Nobody consented, so there was no session to have a record of. Otherwise
    // saying no would fill the directory the owner was told they could delete.
    let left: Vec<_> = std::fs::read_dir(&records)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .collect();
    assert!(
        left.is_empty(),
        "a session nobody agreed to left something behind: {left:?}"
    );
    let _ = std::fs::remove_dir_all(&records);
    running.stop();
}
