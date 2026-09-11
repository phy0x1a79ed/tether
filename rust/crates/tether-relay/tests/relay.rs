//! The relay, exercised through a real listener.
//!
//! The unit tests in `session` and `rate` check the rules in isolation. These
//! check the thing an attacker and an owner actually touch: a socket, a status
//! code, and whether bytes came out the far end unchanged. Several of the
//! acceptance criteria for this tool are only meaningful at this level — that a
//! slot the relay never issued is refused *before any pairing happens*, and
//! that a refusal looks the same as a path that does not exist.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tether_relay::config::{Config, Limits};
use tether_relay::{start, Running};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

const BEARER: &str = "a-test-bearer-nothing-guards";

fn config(limits: Limits) -> Config {
    Config {
        port: 0,
        issue_token: BEARER.into(),
        assets: None,
        max_sessions: 64,
        client_ip_header: "cf-connecting-ip".into(),
        build: "test".into(),
        limits,
    }
}

async fn relay(limits: Limits) -> Running {
    start(config(limits)).await.expect("the relay should bind")
}

/// A request on the plain leg, written by hand.
///
/// `Connection: close` so the body is everything up to EOF and there is no
/// length header to parse. A test client that can get framing wrong is a test
/// client that can fail for its own reasons.
async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: relay.invalid\r\nConnection: close\r\nContent-Length: 0\r\n"
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("not an HTTP response: {text:?}"));
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

async fn authed(addr: SocketAddr, method: &str, path: &str) -> (u16, String) {
    http(
        addr,
        method,
        path,
        &[("Authorization", &format!("Bearer {BEARER}"))],
    )
    .await
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|_| panic!("not json: {body:?}"))
}

/// Issue a slot and return its number and the operator's seat token.
async fn issue(addr: SocketAddr) -> (u64, String) {
    let (status, body) = authed(addr, "POST", "/issue").await;
    assert_eq!(status, 200, "{body}");
    let v = json(&body);
    (
        v["slot"].as_u64().unwrap(),
        v["token"].as_str().unwrap().to_string(),
    )
}

async fn claim(addr: SocketAddr, slot: u64) -> String {
    let (status, body) = http(
        addr,
        "POST",
        &format!("/claim/{slot}"),
        &[("cf-connecting-ip", "203.0.113.9")],
    )
    .await;
    assert_eq!(status, 200, "{body}");
    json(&body)["token"].as_str().unwrap().to_string()
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn join(addr: SocketAddr, slot: u64, token: &str) -> Result<Socket, WsError> {
    let url = format!("ws://{addr}/join/{slot}/{token}");
    connect_async(url).await.map(|(socket, _)| socket)
}

fn status_of(e: &WsError) -> u16 {
    match e {
        WsError::Http(response) => response.status().as_u16(),
        other => panic!("expected an HTTP refusal, got {other}"),
    }
}

/// Read until something that is not a keepalive arrives.
async fn next_data(socket: &mut Socket, within: Duration) -> Option<Message> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, socket.next()).await {
            Err(_) => return None,
            Ok(None) => return Some(Message::Close(None)),
            Ok(Some(Err(_))) => return Some(Message::Close(None)),
            Ok(Some(Ok(Message::Ping(_)))) | Ok(Some(Ok(Message::Pong(_)))) => continue,
            Ok(Some(Ok(message))) => return Some(message),
        }
    }
}

fn closed(message: Option<Message>) -> bool {
    matches!(message, Some(Message::Close(_)) | None)
}

// ---------------------------------------------------------------------------
// Only an authenticated operator can bring a session into existence.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creating_a_session_without_the_bearer_looks_like_a_route_that_is_not_there() {
    let r = relay(Limits::default()).await;

    let (status, _) = http(r.addr, "POST", "/issue", &[]).await;
    assert_eq!(status, 404);

    let (status, _) = http(
        r.addr,
        "POST",
        "/issue",
        &[("Authorization", "Bearer not-the-bearer")],
    )
    .await;
    assert_eq!(
        status, 404,
        "a wrong bearer must not be distinguishable from a missing route"
    );

    assert_eq!(r.relay.sessions.live(), 0);
    r.stop();
}

#[tokio::test]
async fn an_authenticated_operator_gets_a_slot_and_a_seat_token() {
    let r = relay(Limits::default()).await;
    let (slot, token) = issue(r.addr).await;
    assert_eq!(slot, 1, "the first slot should be the shortest to read out");
    assert_eq!(token.len(), 32);
    assert_eq!(r.relay.sessions.live(), 1);
    r.stop();
}

#[tokio::test]
async fn status_answers_the_operator_and_nobody_else() {
    let r = relay(Limits::default()).await;
    issue(r.addr).await;

    let (status, _) = http(r.addr, "GET", "/status", &[]).await;
    assert_eq!(status, 404);

    let (status, body) = authed(r.addr, "GET", "/status").await;
    assert_eq!(status, 200);
    let v = json(&body);
    assert_eq!(v["sessions"], 1);
    assert_eq!(v["build"], "test");
    assert_eq!(v["assets"], false);
    r.stop();
}

// ---------------------------------------------------------------------------
// The public half can redeem a slot and nothing else.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_slot_nobody_issued_cannot_be_claimed() {
    let r = relay(Limits::default()).await;
    let (status, _) = http(r.addr, "POST", "/claim/500", &[]).await;
    assert_eq!(status, 404);
    r.stop();
}

#[tokio::test]
async fn a_socket_naming_a_slot_the_relay_never_issued_is_refused_before_any_pairing() {
    let r = relay(Limits::default()).await;
    let err = join(r.addr, 500, &"0".repeat(32)).await.unwrap_err();
    assert_eq!(status_of(&err), 404);
    r.stop();
}

#[tokio::test]
async fn a_socket_with_a_token_for_no_chair_is_refused_the_same_way() {
    let r = relay(Limits::default()).await;
    let (slot, _) = issue(r.addr).await;

    let err = join(r.addr, slot, &"f".repeat(32)).await.unwrap_err();
    assert_eq!(
        status_of(&err),
        404,
        "a live slot with a stranger's token must answer like a dead one"
    );

    // A token that is not even the right shape gets the same answer.
    let err = join(r.addr, slot, "short").await.unwrap_err();
    assert_eq!(status_of(&err), 404);
    r.stop();
}

#[tokio::test]
async fn a_ticket_opens_one_socket_and_then_nothing() {
    let r = relay(Limits::default()).await;
    let (slot, _) = issue(r.addr).await;
    let ticket = claim(r.addr, slot).await;

    let socket = join(r.addr, slot, &ticket).await.expect("the first join");
    drop(socket);
    // Give the handler its turn to free the chair before the second attempt.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let err = join(r.addr, slot, &ticket).await.unwrap_err();
    assert_eq!(status_of(&err), 404);
    r.stop();
}

#[tokio::test]
async fn the_operators_chair_cannot_be_taken_twice() {
    let r = relay(Limits::default()).await;
    let (slot, token) = issue(r.addr).await;
    let _first = join(r.addr, slot, &token).await.expect("the first join");
    let err = join(r.addr, slot, &token).await.unwrap_err();
    assert_eq!(status_of(&err), 409);
    r.stop();
}

// ---------------------------------------------------------------------------
// What the relay is for.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_ends_that_meet_at_a_slot_exchange_bytes_unchanged() {
    let r = relay(Limits::default()).await;
    let (slot, token) = issue(r.addr).await;
    let mut operator = join(r.addr, slot, &token).await.expect("the operator");
    let ticket = claim(r.addr, slot).await;
    let mut owner = join(r.addr, slot, &ticket).await.expect("the owner");

    // Sealed frames are arbitrary bytes: not text, not valid UTF-8, and full of
    // the values a careless relay would choke on.
    let from_operator: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
    let from_owner: Vec<u8> = vec![0x00, 0xff, 0xfe, 0x80, 0x0a, 0x0d, 0x1b];

    operator
        .send(Message::Binary(from_operator.clone().into()))
        .await
        .unwrap();
    let got = next_data(&mut owner, Duration::from_secs(5)).await.unwrap();
    assert_eq!(got, Message::Binary(from_operator.clone().into()));

    owner
        .send(Message::Binary(from_owner.clone().into()))
        .await
        .unwrap();
    let got = next_data(&mut operator, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(got, Message::Binary(from_owner.into()));
    r.stop();
}

#[tokio::test]
async fn when_one_end_goes_the_other_notices() {
    let r = relay(Limits::default()).await;
    let (slot, token) = issue(r.addr).await;
    let mut operator = join(r.addr, slot, &token).await.unwrap();
    let ticket = claim(r.addr, slot).await;
    let mut owner = join(r.addr, slot, &ticket).await.unwrap();

    // Get a real frame across first, so both legs are past their first-frame
    // deadline and the close is unambiguously the cause of what follows.
    operator
        .send(Message::Binary(vec![1].into()))
        .await
        .unwrap();
    next_data(&mut owner, Duration::from_secs(5)).await.unwrap();

    operator.close(None).await.unwrap();
    assert!(
        closed(next_data(&mut owner, Duration::from_secs(5)).await),
        "the owner's socket should end when the operator cuts"
    );
    r.stop();
}

#[tokio::test]
async fn the_relay_keeps_a_quiet_session_open_with_its_own_keepalives() {
    let limits = Limits {
        keepalive: Duration::from_millis(50),
        ..Limits::default()
    };
    let r = relay(limits).await;
    let (slot, token) = issue(r.addr).await;
    let mut operator = join(r.addr, slot, &token).await.unwrap();

    // Nothing on the path between two machines will hold an idle socket open,
    // so the relay has to. This matters before pairing too: the operator's
    // socket waits there while the owner reads the code and runs the launcher.
    let ping = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match operator.next().await {
                Some(Ok(Message::Ping(_))) => return true,
                Some(Ok(_)) => continue,
                _ => return false,
            }
        }
    })
    .await
    .expect("a keepalive should have arrived");
    assert!(ping);
    r.stop();
}

// ---------------------------------------------------------------------------
// The limits.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_socket_that_pairs_and_then_says_nothing_is_dropped() {
    let limits = Limits {
        first_frame_timeout: Duration::from_millis(200),
        keepalive: Duration::from_millis(50),
        ..Limits::default()
    };
    let r = relay(limits).await;
    let (slot, token) = issue(r.addr).await;
    let mut operator = join(r.addr, slot, &token).await.unwrap();
    let ticket = claim(r.addr, slot).await;
    let mut owner = join(r.addr, slot, &ticket).await.unwrap();

    assert!(closed(next_data(&mut owner, Duration::from_secs(3)).await));
    assert!(closed(
        next_data(&mut operator, Duration::from_secs(3)).await
    ));
    r.stop();
}

#[tokio::test]
async fn a_socket_nobody_ever_joins_is_dropped_at_the_deadline() {
    let limits = Limits {
        unpaired_timeout: Duration::from_millis(200),
        keepalive: Duration::from_millis(50),
        ..Limits::default()
    };
    let r = relay(limits).await;
    let (slot, token) = issue(r.addr).await;
    let mut operator = join(r.addr, slot, &token).await.unwrap();
    assert!(closed(
        next_data(&mut operator, Duration::from_secs(3)).await
    ));
    r.stop();
}

#[tokio::test]
async fn a_slot_is_destroyed_once_its_pairings_are_spent() {
    let limits = Limits {
        max_pairings: 2,
        first_frame_timeout: Duration::from_millis(150),
        keepalive: Duration::from_millis(50),
        ..Limits::default()
    };
    let r = relay(limits).await;
    let (slot, token) = issue(r.addr).await;

    // Two attempts that go nowhere — the shape of two wrong phrases.
    for _ in 0..2 {
        let mut operator = join(r.addr, slot, &token).await.expect("the operator");
        let ticket = claim(r.addr, slot).await;
        let mut owner = join(r.addr, slot, &ticket).await.expect("the owner");
        assert!(closed(next_data(&mut owner, Duration::from_secs(3)).await));
        assert!(closed(
            next_data(&mut operator, Duration::from_secs(3)).await
        ));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(r.relay.sessions.live(), 0);
    let (status, _) = http(r.addr, "POST", &format!("/claim/{slot}"), &[]).await;
    assert_eq!(status, 404, "the slot should be gone, not merely busy");
    r.stop();
}

#[tokio::test]
async fn an_unredeemed_slot_expires_on_its_own() {
    let limits = Limits {
        issue_ttl: Duration::from_millis(150),
        ..Limits::default()
    };
    let r = relay(limits).await;
    let (slot, _) = issue(r.addr).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (status, _) = http(r.addr, "POST", &format!("/claim/{slot}"), &[]).await;
    assert_eq!(status, 404);
    r.stop();
}

#[tokio::test]
async fn the_concurrent_session_cap_refuses_rather_than_queues() {
    let mut c = config(Limits::default());
    c.max_sessions = 2;
    let r = start(c).await.unwrap();
    issue(r.addr).await;
    issue(r.addr).await;
    let (status, _) = authed(r.addr, "POST", "/issue").await;
    assert_eq!(status, 503);
    r.stop();
}

#[tokio::test]
async fn the_per_address_budget_is_spent_on_the_plain_leg() {
    let limits = Limits {
        rate_burst: 2,
        ..Limits::default()
    };
    let r = relay(limits).await;
    let (slot, _) = issue(r.addr).await;

    let path = format!("/claim/{slot}");
    for _ in 0..2 {
        let (status, _) = http(
            r.addr,
            "POST",
            &path,
            &[("cf-connecting-ip", "198.51.100.7")],
        )
        .await;
        assert_eq!(status, 200);
    }
    let (status, _) = http(
        r.addr,
        "POST",
        &path,
        &[("cf-connecting-ip", "198.51.100.7")],
    )
    .await;
    assert_eq!(status, 429);

    // Somebody else's budget is their own.
    let (status, _) = http(
        r.addr,
        "POST",
        &path,
        &[("cf-connecting-ip", "198.51.100.8")],
    )
    .await;
    assert_eq!(status, 200);
    r.stop();
}

// ---------------------------------------------------------------------------
// The downloads.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_launcher_and_the_client_come_from_the_asset_directory() {
    let dir = std::env::temp_dir().join(format!("tether-relay-assets-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("tether"), b"#!/usr/bin/env bash\n").unwrap();
    std::fs::write(dir.join("bin").join("tether-linux-x86_64"), b"\x7fELF").unwrap();

    let mut c = config(Limits::default());
    c.assets = Some(dir.clone());
    let r = start(c).await.unwrap();

    let (status, body) = http(r.addr, "GET", "/tether", &[]).await;
    assert_eq!(status, 200);
    assert!(body.starts_with("#!/usr/bin/env bash"));

    let (status, _) = http(r.addr, "GET", "/bin/tether-linux-x86_64", &[]).await;
    assert_eq!(status, 200);

    let (status, _) = http(r.addr, "GET", "/bin/nothing-here", &[]).await;
    assert_eq!(status, 404);

    r.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_host_with_no_assets_says_so_rather_than_failing() {
    let r = relay(Limits::default()).await;
    let (status, _) = http(r.addr, "GET", "/tether", &[]).await;
    assert_eq!(status, 404);
    let (status, body) = http(r.addr, "GET", "/health", &[]).await;
    assert_eq!(status, 200);
    assert_eq!(body.trim(), "ok");
    r.stop();
}
