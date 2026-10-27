//! The resume layer against a real relay, through a proxy each test can cut.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tether_link::resume::{mint_keys, Incoming, OperatorRejoin, OwnerRejoin, Resumable};
use tether_link::{hold, Carry, Link, Relay};
use tether_proto::frame::{Frame, Stream};
use tether_proto::invite::{InviteCode, Phrase, Slot};
use tether_relay::config::{Config, Limits};
use tether_relay::start;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

const BEARER: &str = "a-test-bearer-nothing-guards";

fn config(port: u16) -> Config {
    Config {
        port,
        issue_token: BEARER.into(),
        assets: None,
        max_sessions: 64,
        client_ip_header: "cf-connecting-ip".into(),
        build: "test".into(),
        limits: Limits::default(),
    }
}

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

async fn issue(relay: &Relay) -> (Slot, String) {
    let answer = tether_link::http::post_json(relay, &relay.issue_path(), Some(BEARER))
        .await
        .unwrap();
    let slot = Slot::new(answer["slot"].as_u64().unwrap() as u32).unwrap();
    (slot, answer["token"].as_str().unwrap().to_string())
}

struct Pair {
    operator: Resumable,
    owner: Resumable,
}

/// Two ends through their own proxies, consented and resuming.
async fn pair(operator_path: &Proxy, owner_path: &Proxy) -> Pair {
    let (op_relay, ow_relay) = (operator_path.relay(), owner_path.relay());
    let (slot, seat) = issue(&op_relay).await;
    let code = InviteCode {
        slot,
        phrase: Phrase::mint(2).unwrap(),
    };
    let (operator, owner) = tokio::join!(
        Link::dial_operator(&op_relay, slot, &seat, &code.phrase),
        Link::dial_owner_via(&ow_relay, &code, Carry::Socket),
    );
    let (operator, owner) = (operator.unwrap(), owner.unwrap());
    let keys = mint_keys();
    hold(&op_relay, slot, &seat, &keys.token).await.unwrap();
    let operator = Resumable::operator(
        operator,
        keys,
        OperatorRejoin {
            relay: op_relay,
            bearer: Some(BEARER.into()),
            slot,
            seat,
        },
    );
    let mut owner = Resumable::owner(
        owner,
        OwnerRejoin {
            relay: ow_relay,
            slot,
            carry: Carry::Socket,
        },
    );
    // The owner can resume only once `Hold` has arrived, and `Hold` is not
    // handed up, so wait for a frame sent after it.
    operator
        .send(&Frame::Say {
            text: "ready".into(),
        })
        .unwrap();
    expect_frame(
        &mut owner,
        Frame::Say {
            text: "ready".into(),
        },
    )
    .await;
    Pair { operator, owner }
}

async fn next(end: &mut Resumable, within: Duration) -> Incoming {
    tokio::time::timeout(within, end.recv())
        .await
        .expect("nothing happened in time")
}

async fn expect_frame(end: &mut Resumable, want: Frame) {
    loop {
        match next(end, Duration::from_secs(30)).await {
            Incoming::Frame(Frame::Pong { .. }) | Incoming::Frame(Frame::Ping { .. }) => {}
            Incoming::Frame(got) => {
                assert_eq!(got, want);
                return;
            }
            other => panic!("wanted {want:?}, got {other:?}"),
        }
    }
}

fn output(n: u32) -> Frame {
    Frame::Output {
        task: 1,
        stream: Stream::Stdout,
        data: format!("line {n}\n").into_bytes(),
    }
}

/// Collect `count` outputs, ignoring the gap's own events.
async fn outputs(end: &mut Resumable, count: u32) -> Vec<u32> {
    let mut seen = Vec::new();
    while seen.len() < count as usize {
        match next(end, Duration::from_secs(60)).await {
            Incoming::Frame(Frame::Output { data, .. }) => {
                let text = String::from_utf8(data).unwrap();
                seen.push(text.trim().trim_start_matches("line ").parse().unwrap());
            }
            Incoming::Ended(e) => panic!("ended after {} outputs: {e}", seen.len()),
            _ => {}
        }
    }
    seen
}

#[tokio::test]
async fn output_sent_through_an_owner_side_gap_all_arrives_once_and_in_order() {
    let relay = start(config(0)).await.unwrap();
    let (op_path, ow_path) = (Proxy::to(relay.addr).await, Proxy::to(relay.addr).await);
    let Pair {
        mut operator,
        owner,
    } = pair(&op_path, &ow_path).await;

    for n in 0..50 {
        owner.send(&output(n)).unwrap();
    }
    ow_path.cut();
    // Keep printing through the gap, as a running command would.
    for n in 50..100 {
        owner.send(&output(n)).unwrap();
    }
    match next(&mut operator, Duration::from_secs(10)).await {
        Incoming::Frame(_) | Incoming::Down { .. } => {}
        other => panic!("{other:?}"),
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    ow_path.restore();

    let seen = outputs(&mut operator, 100).await;
    assert_eq!(seen, (0..100).collect::<Vec<_>>());
    relay.stop();
}

#[tokio::test]
async fn input_sent_through_an_operator_side_gap_arrives_after_the_resume() {
    let relay = start(config(0)).await.unwrap();
    let (op_path, ow_path) = (Proxy::to(relay.addr).await, Proxy::to(relay.addr).await);
    let Pair {
        operator,
        mut owner,
    } = pair(&op_path, &ow_path).await;

    op_path.cut();
    let mut down = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !down && tokio::time::Instant::now() < deadline {
        down = !operator.is_up();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(down, "the operator noticed its link went");
    assert!(operator.gives_up_at().is_some());
    operator
        .send(&Frame::Input {
            task: 1,
            data: b"y\n".to_vec(),
        })
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    op_path.restore();

    loop {
        match next(&mut owner, Duration::from_secs(60)).await {
            Incoming::Frame(Frame::Input { data, .. }) => {
                assert_eq!(data, b"y\n");
                break;
            }
            Incoming::Ended(e) => panic!("{e}"),
            _ => {}
        }
    }
    relay.stop();
}

/// A restarted relay remembers nothing. The operator puts its slot back, and
/// the owner finds it there.
#[tokio::test]
async fn a_session_survives_its_relay_restarting() {
    let relay = start(config(0)).await.unwrap();
    let port = relay.addr.port();
    let (op_path, ow_path) = (Proxy::to(relay.addr).await, Proxy::to(relay.addr).await);
    let Pair {
        mut operator,
        owner,
    } = pair(&op_path, &ow_path).await;

    op_path.cut();
    ow_path.cut();
    relay.stop();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let relay = start(config(port)).await.unwrap();
    owner.send(&output(1)).unwrap();
    op_path.restore();
    ow_path.restore();

    assert_eq!(outputs(&mut operator, 1).await, vec![1]);
    relay.stop();
}

#[tokio::test]
async fn an_owner_that_leaves_during_a_gap_ends_the_session_for_the_operator() {
    let relay = start(config(0)).await.unwrap();
    let (op_path, ow_path) = (Proxy::to(relay.addr).await, Proxy::to(relay.addr).await);
    let Pair {
        mut operator,
        owner,
    } = pair(&op_path, &ow_path).await;

    // The operator's path goes, so the owner cannot say goodbye in band.
    op_path.cut();
    tokio::time::sleep(Duration::from_millis(500)).await;
    owner.cut("the owner ended it").await;
    op_path.restore();

    loop {
        match next(&mut operator, Duration::from_secs(60)).await {
            Incoming::Ended(e) => {
                assert!(matches!(e, tether_link::LinkError::Ended), "{e}");
                break;
            }
            // The owner's cut may also arrive in band, if its link was still
            // up when it left.
            Incoming::Frame(Frame::Cut { .. }) => break,
            _ => {}
        }
    }
    relay.stop();
}

/// An older peer never says `resume`, so it gets a plain link, which ends at
/// the first drop as it always did.
#[tokio::test]
async fn a_plain_link_ends_at_the_first_drop() {
    let relay = start(config(0)).await.unwrap();
    let path = Proxy::to(relay.addr).await;
    let r = path.relay();
    let (slot, seat) = issue(&r).await;
    let code = InviteCode {
        slot,
        phrase: Phrase::mint(2).unwrap(),
    };
    let (operator, owner) = tokio::join!(
        Link::dial_operator(&r, slot, &seat, &code.phrase),
        Link::dial_owner_via(&r, &code, Carry::Socket),
    );
    let mut operator = Resumable::plain(operator.unwrap());
    let _owner = Resumable::plain(owner.unwrap());
    path.cut();
    match next(&mut operator, Duration::from_secs(10)).await {
        Incoming::Ended(_) => {}
        other => panic!("{other:?}"),
    }
    relay.stop();
}
