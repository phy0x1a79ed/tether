//! Rooms, exercised through a real listener: an operator opens one, a guest
//! that knows only the name reads the how-to, and the two talk over polling
//! and over a websocket until one side ends it.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tether_relay::config::{Config, Limits};
use tether_relay::{start, Running};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const BEARER: &str = "a-test-bearer-nothing-guards";

async fn relay(limits: Limits) -> Running {
    relay_with_state(limits, None).await
}

async fn relay_with_state(limits: Limits, room_state: Option<std::path::PathBuf>) -> Running {
    start(Config {
        port: 0,
        issue_token: BEARER.into(),
        assets: None,
        max_sessions: 64,
        client_ip_header: "cf-connecting-ip".into(),
        room_state,
        build: "test".into(),
        limits,
    })
    .await
    .expect("the relay should bind")
}

async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: relay.invalid\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !body.is_empty() {
        request.push_str("Content-Type: application/json\r\n");
    }
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|_| panic!("not json: {body:?}"))
}

struct Opened {
    name: String,
    host: String,
}

async fn open(addr: SocketAddr) -> Opened {
    let auth = format!("Bearer {BEARER}");
    let (status, body) = http(addr, "POST", "/room", &[("Authorization", &auth)], "").await;
    assert_eq!(status, 200, "{body}");
    let v = json(&body);
    Opened {
        name: v["name"].as_str().unwrap().into(),
        host: v["host"].as_str().unwrap().into(),
    }
}

const GUEST: (&str, &str) = ("cf-connecting-ip", "198.51.100.7");

async fn guest(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    http(addr, method, path, &[GUEST], body).await
}

async fn host(
    addr: SocketAddr,
    room: &Opened,
    method: &str,
    path: &str,
    body: &str,
) -> (u16, String) {
    let auth = format!("Bearer {}", room.host);
    http(addr, method, path, &[("Authorization", &auth)], body).await
}

#[tokio::test]
async fn opening_a_room_needs_the_operator_bearer() {
    let r = relay(Limits::default()).await;
    let (status, _) = http(r.addr, "POST", "/room", &[], "").await;
    assert_eq!(status, 404);
    let (status, _) = http(
        r.addr,
        "POST",
        "/room",
        &[("Authorization", "Bearer wrong")],
        "",
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(r.relay.rooms.live(), 0);
    r.stop();
}

#[tokio::test]
async fn an_operator_opens_a_persistent_room_with_six_words_and_no_deadline() {
    let r = relay(Limits::default()).await;
    let auth = format!("Bearer {BEARER}");
    let (status, body) = http(
        r.addr,
        "POST",
        "/room",
        &[("Authorization", &auth)],
        r#"{"kind":"persistent"}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v = json(&body);
    assert_eq!(v["kind"], "persistent");
    assert!(v["idle_s"].is_null() && v["life_s"].is_null(), "{body}");
    let name = v["name"].as_str().unwrap();
    assert_eq!(name.split('-').count(), 6);
    let (status, how_to) = guest(r.addr, "GET", &format!("/room/{name}"), "").await;
    assert_eq!(status, 200);
    assert!(how_to.contains(name));

    let (status, _) = http(
        r.addr,
        "POST",
        "/room",
        &[("Authorization", &auth)],
        r#"{"kind":"forever"}"#,
    )
    .await;
    assert_eq!(status, 400);
    let (_, body) = http(r.addr, "POST", "/room", &[("Authorization", &auth)], "").await;
    assert_eq!(json(&body)["kind"], "task");
    r.stop();
}

#[tokio::test]
async fn a_persistent_room_comes_back_after_a_relay_restart() {
    let dir = std::env::temp_dir().join(format!("tether-rooms-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let r = relay_with_state(Limits::default(), Some(dir.clone())).await;
    let auth = format!("Bearer {BEARER}");
    let (_, body) = http(
        r.addr,
        "POST",
        "/room",
        &[("Authorization", &auth)],
        r#"{"kind":"persistent"}"#,
    )
    .await;
    let v = json(&body);
    let room = Opened {
        name: v["name"].as_str().unwrap().into(),
        host: v["host"].as_str().unwrap().into(),
    };
    let task = open(r.addr).await;
    let send = format!("/room/{}/send", room.name);
    let (_, body) = guest(r.addr, "POST", &send, r#"{"from":"g","text":"before"}"#).await;
    let before = json(&body)["seq"].as_u64().unwrap();
    r.stop();

    let r = relay_with_state(Limits::default(), Some(dir.clone())).await;
    let (status, _) = guest(r.addr, "GET", &format!("/room/{}", task.name), "").await;
    assert_eq!(status, 404, "a task room ends with the relay");
    let recv = format!("/room/{}/recv?after={before}", room.name);
    let (status, body) = host(r.addr, &room, "GET", &recv, "").await;
    assert_eq!(status, 200, "the host token still holds: {body}");
    assert!(json(&body)["ended"].is_null());
    let (_, body) = host(r.addr, &room, "POST", &send, r#"{"from":"opal","text":"after"}"#).await;
    assert!(json(&body)["seq"].as_u64().unwrap() > before);
    let (_, body) = guest(r.addr, "GET", &recv, "").await;
    let got = json(&body);
    assert_eq!(got["messages"][0]["text"], "after");
    assert_eq!(got["messages"][0]["by"], "host");
    r.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_guest_with_only_the_name_reads_the_how_to_and_talks_with_the_host() {
    let r = relay(Limits::default()).await;
    let room = open(r.addr).await;
    assert_eq!(room.name.split('-').count(), 3);

    let (status, how_to) = guest(r.addr, "GET", &format!("/room/{}", room.name), "").await;
    assert_eq!(status, 200);
    assert!(how_to.contains("ROOM/recv?after="), "{how_to}");

    let (status, body) = guest(
        r.addr,
        "POST",
        &format!("/room/{}/send", room.name),
        r#"{"from":"visitor","text":"hello host"}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json(&body)["seq"], 1);

    let (status, body) = host(
        r.addr,
        &room,
        "GET",
        &format!("/room/{}/recv?after=0", room.name),
        "",
    )
    .await;
    assert_eq!(status, 200);
    let got = json(&body);
    assert_eq!(got["messages"][0]["text"], "hello host");
    assert_eq!(got["messages"][0]["by"], "guest");
    assert_eq!(got["next"], 1);
    assert!(got["ended"].is_null());

    host(
        r.addr,
        &room,
        "POST",
        &format!("/room/{}/send", room.name),
        r#"{"from":"opal","text":"hello guest"}"#,
    )
    .await;
    let (_, body) = guest(r.addr, "GET", &format!("/room/{}/recv?after=1", room.name), "").await;
    let got = json(&body);
    assert_eq!(got["messages"][0]["text"], "hello guest");
    assert_eq!(got["messages"][0]["by"], "host");
    r.stop();
}

#[tokio::test]
async fn a_long_poll_answers_as_soon_as_the_other_side_speaks() {
    let r = relay(Limits::default()).await;
    let room = open(r.addr).await;
    let addr = r.addr;
    let path = format!("/room/{}/recv?after=0&wait=20", room.name);
    let waiting = tokio::spawn(async move { guest(addr, "GET", &path, "").await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    host(
        r.addr,
        &room,
        "POST",
        &format!("/room/{}/send", room.name),
        r#"{"from":"opal","text":"now"}"#,
    )
    .await;
    let (status, body) = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the poll should answer at once")
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(json(&body)["messages"][0]["text"], "now");
    r.stop();
}

#[tokio::test]
async fn a_guest_on_a_websocket_hears_and_speaks() {
    let r = relay(Limits::default()).await;
    let room = open(r.addr).await;
    host(
        r.addr,
        &room,
        "POST",
        &format!("/room/{}/send", room.name),
        r#"{"from":"opal","text":"before you came"}"#,
    )
    .await;

    let (mut ws, _) = connect_async(format!("ws://{}/room/{}/ws?after=0", r.addr, room.name))
        .await
        .expect("the guest socket should open");
    let first = next_text(&mut ws).await;
    assert_eq!(first["text"], "before you came");

    ws.send(Message::Text(
        r#"{"from":"visitor","text":"over the socket"}"#.into(),
    ))
    .await
    .unwrap();
    let echoed = next_text(&mut ws).await;
    assert_eq!(echoed["text"], "over the socket");
    assert_eq!(echoed["by"], "guest");

    let (_, body) = host(
        r.addr,
        &room,
        "GET",
        &format!("/room/{}/recv?after=1", room.name),
        "",
    )
    .await;
    assert_eq!(json(&body)["messages"][0]["text"], "over the socket");

    host(
        r.addr,
        &room,
        "POST",
        &format!("/room/{}/leave", room.name),
        "",
    )
    .await;
    let last = next_text(&mut ws).await;
    assert_eq!(last["ended"], "the host closed the room");
    r.stop();
}

#[tokio::test]
async fn the_host_on_a_websocket_is_stamped_host() {
    let r = relay(Limits::default()).await;
    let room = open(r.addr).await;
    let mut request = format!("ws://{}/room/{}/ws", r.addr, room.name)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", room.host).parse().unwrap(),
    );
    let (mut ws, _) = connect_async(request).await.unwrap();
    ws.send(Message::Text(r#"{"from":"opal","text":"hi"}"#.into()))
        .await
        .unwrap();
    assert_eq!(next_text(&mut ws).await["by"], "host");
    r.stop();
}

async fn next_text(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
) -> serde_json::Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("a frame should arrive")
            .expect("the socket should stay open")
            .unwrap();
        if let Message::Text(text) = message {
            return json(&text);
        }
    }
}

#[tokio::test]
async fn a_guest_leaving_ends_the_room_for_both_and_the_name_then_stops_answering() {
    let limits = Limits {
        room_linger: Duration::from_millis(200),
        ..Limits::default()
    };
    let r = relay(limits).await;
    let room = open(r.addr).await;

    let (status, body) = guest(r.addr, "POST", &format!("/room/{}/leave", room.name), "").await;
    assert_eq!(status, 200);
    assert_eq!(json(&body)["ended"], "the guest left");

    let (status, body) = host(
        r.addr,
        &room,
        "GET",
        &format!("/room/{}/recv?after=0&wait=10", room.name),
        "",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json(&body)["ended"], "the guest left");

    let (status, _) = host(
        r.addr,
        &room,
        "POST",
        &format!("/room/{}/send", room.name),
        r#"{"from":"opal"}"#,
    )
    .await;
    assert_eq!(status, 410);

    tokio::time::sleep(Duration::from_millis(300)).await;
    r.relay.rooms.reap(std::time::Instant::now());
    let (status, _) = guest(r.addr, "GET", &format!("/room/{}", room.name), "").await;
    assert_eq!(status, 404);
    r.stop();
}

#[tokio::test]
async fn a_room_whose_host_goes_quiet_ends() {
    let limits = Limits {
        room_idle: Duration::from_millis(200),
        ..Limits::default()
    };
    let r = relay(limits).await;
    let room = open(r.addr).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (status, body) = guest(r.addr, "GET", &format!("/room/{}/recv?after=0", room.name), "").await;
    assert_eq!(status, 200);
    assert_eq!(json(&body)["ended"], "the host went quiet");
    r.stop();
}

#[tokio::test]
async fn an_unknown_name_and_a_wrong_host_token_look_like_a_path_that_is_not_there() {
    let r = relay(Limits::default()).await;
    let room = open(r.addr).await;
    let (unknown, unknown_body) = guest(r.addr, "GET", "/room/acre-anchor-kettle", "").await;
    let (nonsense, nonsense_body) = guest(r.addr, "GET", "/room/not-a-room", "").await;
    let (missing, missing_body) = guest(r.addr, "GET", "/no/such/path", "").await;
    assert_eq!((unknown, nonsense, missing), (404, 404, 404));
    assert_eq!(unknown_body, missing_body);
    assert_eq!(nonsense_body, missing_body);

    let wrong = format!("Bearer {}", "0".repeat(32));
    let (status, body) = http(
        r.addr,
        "POST",
        &format!("/room/{}/leave", room.name),
        &[("Authorization", &wrong)],
        "",
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(body, missing_body);
    assert_eq!(
        r.relay.rooms.live(),
        1,
        "a wrong host token must not end the room"
    );
    r.stop();
}

#[tokio::test]
async fn a_guesser_out_of_budget_is_refused_for_every_name_alike() {
    let limits = Limits {
        rate_burst: 3,
        ..Limits::default()
    };
    let r = relay(limits).await;
    let room = open(r.addr).await;
    let mut guesses = 0;
    loop {
        let (status, _) = guest(r.addr, "GET", "/room/acre-anchor-kettle/recv", "").await;
        if status == 429 {
            break;
        }
        assert_eq!(status, 404);
        guesses += 1;
        assert!(guesses <= 3, "the budget should run out");
    }
    let (status, _) = guest(r.addr, "GET", &format!("/room/{}/recv", room.name), "").await;
    assert_eq!(
        status, 429,
        "a live name must answer like a miss once the budget is gone"
    );

    // Another address, with its own budget, still reaches the room.
    let (status, _) = http(
        r.addr,
        "GET",
        &format!("/room/{}/recv", room.name),
        &[("cf-connecting-ip", "192.0.2.1")],
        "",
    )
    .await;
    assert_eq!(status, 200);
    r.stop();
}

#[tokio::test]
async fn a_guest_in_a_live_room_does_not_spend_its_budget_talking() {
    let limits = Limits {
        rate_burst: 3,
        ..Limits::default()
    };
    let r = relay(limits).await;
    let room = open(r.addr).await;
    for i in 0..20 {
        let (status, _) = guest(
            r.addr,
            "POST",
            &format!("/room/{}/send", room.name),
            &format!(r#"{{"from":"visitor","text":"{i}"}}"#),
        )
        .await;
        assert_eq!(status, 200);
    }
    r.stop();
}

#[tokio::test]
async fn a_bad_message_is_refused_with_a_reason() {
    let r = relay(Limits::default()).await;
    let room = open(r.addr).await;
    let (status, body) = guest(
        r.addr,
        "POST",
        &format!("/room/{}/send", room.name),
        r#"{"text":"no from"}"#,
    )
    .await;
    assert_eq!(status, 400);
    assert!(body.contains("from"), "{body}");
    r.stop();
}

#[tokio::test]
async fn status_counts_rooms() {
    let r = relay(Limits::default()).await;
    open(r.addr).await;
    let auth = format!("Bearer {BEARER}");
    let (_, body) = http(r.addr, "GET", "/status", &[("Authorization", &auth)], "").await;
    assert_eq!(json(&body)["rooms"], 1);
    r.stop();
}
