//! The session: greet, ask, and then carry it until somebody ends it.
//!
//! # The consent gate is an ordering, not a check
//!
//! The owner's `Hello` **is** the consent. Nothing is sent from this side until
//! a person here has answered, and the operator's side is written to send no
//! task until it has that `Hello`. If the answer is no, what goes back is a
//! `Cut` carrying the reason, so the refusal shows up on the operator's screen
//! as a sentence rather than as a socket that died.
//!
//! Two guards for one rule, because it is the rule: [`greet`] accepts only a
//! `Hello`, a `Say` or a `Cut` before consent, and the [`Runner`] that could
//! run anything is not constructed until after. A task-shaped frame arriving
//! early is not ignored — it ends the session, because a peer that sent one is
//! not the peer this protocol describes.

use std::time::Duration;

use tether_link::{Link, LinkError};
use tether_proto::frame::{Frame, Hello, Role, PROTOCOL_VERSION, VIEWPORT};
use tether_proto::invite::InviteCode;
use tokio::sync::mpsc;

use crate::consent::{self, Answer, Ask};
use crate::log::Log;
use crate::exec::Runner;
use crate::ui::{self, Event, Facts, Typed, Ui};

/// How often the header's clock is redrawn.
const TICK: Duration = Duration::from_secs(1);

/// How long the owner waits for the operator to say who they are.
const GREETING_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub enum Outcome {
    /// Somebody ended it on purpose, and the reason is what they said.
    Cut(String),
    /// The person at the keyboard said no.
    Refused(String),
    /// The other end went away without saying anything.
    PeerLeft,
    Failed(String),
}

impl Outcome {
    pub fn code(&self) -> u8 {
        match self {
            Outcome::Cut(_) => 0,
            Outcome::Refused(_) => 1,
            Outcome::PeerLeft => 1,
            Outcome::Failed(_) => 2,
        }
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Cut(why) => write!(f, "session ended: {why}"),
            Outcome::Refused(why) => write!(f, "not connected: {why}"),
            Outcome::PeerLeft => write!(f, "the other end went away"),
            Outcome::Failed(why) => write!(f, "{why}"),
        }
    }
}

/// Learn who is calling, then ask the person here whether to let them in.
pub async fn greet(
    link: &mut Link,
    relay: &tether_link::Relay,
    code: &InviteCode,
    me: &Hello,
    log: &mut Result<Log, String>,
) -> Result<Hello, Outcome> {
    let operator = match tokio::time::timeout(GREETING_TIMEOUT, wait_for_hello(link, log)).await {
        Ok(Ok(hello)) => hello,
        Ok(Err(outcome)) => return Err(outcome),
        Err(_) => return Err(Outcome::PeerLeft),
    };

    if operator.version != PROTOCOL_VERSION {
        let why = format!(
            "the other end speaks tether v{} and this one speaks v{PROTOCOL_VERSION}; \
             one of the two is out of date",
            operator.version
        );
        link.cut(&why).await;
        return Err(Outcome::Failed(why));
    }
    if operator.role != Role::Operator {
        let why = "the other end says it is not the operator".to_string();
        link.cut(&why).await;
        return Err(Outcome::Failed(why));
    }

    if let Ok(log) = log.as_mut() {
        log.peer(
            &operator.who,
            &operator.host,
            &operator.os,
            &operator.build,
            &relay.to_string(),
        );
    }
    let ask = Ask {
        operator: &operator,
        relay,
        code,
        log: log.as_ref().map(|l| l.path()).map_err(|why| why.as_str()),
    };
    match consent::ask(&ask) {
        Answer::Allowed => {}
        Answer::Refused(why) => {
            link.cut(why).await;
            return Err(Outcome::Refused(why.to_string()));
        }
    }

    // This is the consent, on the wire. Nothing left this side before it.
    if link.send(&Frame::Hello(me.clone())).await.is_err() {
        return Err(Outcome::PeerLeft);
    }
    Ok(operator)
}

async fn wait_for_hello(link: &mut Link, log: &mut Result<Log, String>) -> Result<Hello, Outcome> {
    loop {
        match link.recv().await {
            Ok(Frame::Hello(hello)) => return Ok(hello),
            // Worth showing: it is how the operator says what they are about to
            // do, and a person deciding whether to say yes should see it. It is
            // recorded too, so the account starts at the first thing said
            // rather than at the moment consent was given.
            Ok(Frame::Say { text }) => {
                eprintln!("  them: {text}");
                if let Ok(log) = log.as_mut() {
                    log.line(&format!("them: {text}"));
                }
            }
            Ok(Frame::Cut { reason }) => return Err(Outcome::Cut(reason)),
            Ok(_) => {
                let why = "the other end tried to start work before anyone here said yes";
                link.cut(why).await;
                return Err(Outcome::Failed(why.into()));
            }
            Err(LinkError::Closed) => return Err(Outcome::PeerLeft),
            Err(e) => return Err(Outcome::Failed(e.to_string())),
        }
    }
}

/// Carry the session until either side ends it.
pub async fn run(
    mut link: Link,
    operator: Hello,
    relay: String,
    code: String,
    log: Option<Log>,
) -> Outcome {
    let mut ui = match Ui::new(Facts::new(&operator, relay, code), log) {
        Ok(ui) => ui,
        Err(e) => return Outcome::Failed(format!("could not set up the display: {e}")),
    };

    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(256);
    let mut runner = Runner::new(out_tx);

    // Only on a terminal. Under `setsid`, in a pipe or in a test there is no
    // console to attach to, and the session still runs and is still visible.
    let mut keys = ui.tty().then(ui::keys);

    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let _ = ui.draw();

    // How big a terminal this end is offering to draw. Sent before anything
    // can be opened, and again whenever it changes, so the operator sizes a
    // terminal to what the owner can actually see rather than to a guess.
    let mut advertised = ui.viewport();
    if link
        .send(&Frame::Resize {
            task: VIEWPORT,
            cols: advertised.0,
            rows: advertised.1,
        })
        .await
        .is_err()
    {
        return Outcome::PeerLeft;
    }

    let outcome = loop {
        enum Next {
            In(Result<Frame, LinkError>),
            Out(Frame),
            Term(crossterm::event::Event),
            Tick,
        }

        // The select ends before anything acts, because two of these branches
        // want the link and the borrow checker is right about that.
        let next = tokio::select! {
            incoming = link.recv() => Next::In(incoming),
            Some(frame) = out_rx.recv() => Next::Out(frame),
            Some(Ok(ev)) = next_key(&mut keys) => Next::Term(ev),
            _ = tick.tick() => Next::Tick,
        };

        match next {
            Next::Out(frame) => {
                if let Frame::Exit { task, .. } = &frame {
                    runner.forget(*task);
                }
                if let Some(event) = to_event(&frame) {
                    ui.apply(event);
                }
                if link.send(&frame).await.is_err() {
                    break Outcome::PeerLeft;
                }
            }
            Next::Term(ev) => match ui.typed(ev) {
                Typed::Nothing => {}
                Typed::Changed => {}
                // The owner answering, which is the half of `Say` that has
                // always been on the wire and never had anything to send it.
                Typed::Send(text) => {
                    ui.apply(Event::Said {
                        from_operator: false,
                        text: text.clone(),
                    });
                    if link.send(&Frame::Say { text }).await.is_err() {
                        break Outcome::PeerLeft;
                    }
                }
                Typed::Cut => {
                    let why = "the owner ended it".to_string();
                    runner.shutdown().await;
                    ui.apply(Event::Cut {
                        reason: why.clone(),
                    });
                    let _ = ui.draw();
                    link.cut(&why).await;
                    break Outcome::Cut(why);
                }
            },
            Next::Tick => {
                ui.apply(Event::Tick);
                // Compared on the tick rather than sent on the event. Dragging
                // a window corner produces dozens of resizes a second, and
                // shrinking a console pseudo-terminal that often is the
                // flakiest thing this tool can ask Windows to do.
                let now = ui.viewport();
                if now != advertised {
                    advertised = now;
                    if link
                        .send(&Frame::Resize {
                            task: VIEWPORT,
                            cols: now.0,
                            rows: now.1,
                        })
                        .await
                        .is_err()
                    {
                        break Outcome::PeerLeft;
                    }
                }
            }
            Next::In(Err(LinkError::Closed)) => {
                runner.shutdown().await;
                break Outcome::PeerLeft;
            }
            Next::In(Err(e)) => {
                runner.shutdown().await;
                break Outcome::Failed(e.to_string());
            }
            Next::In(Ok(frame)) => match frame {
                Frame::Open {
                    task,
                    kind,
                    command,
                    cols,
                    rows,
                } => {
                    ui.apply(Event::Started {
                        task,
                        kind,
                        command: command.clone(),
                        cols,
                        rows,
                    });
                    runner.open(task, kind, command, cols, rows);
                }
                Frame::Input { task, data } => runner.input(task, data).await,
                // Task zero is this end's to speak about, not the operator's.
                // Treated like any other frame only one side may send, which
                // the owner's side already does for output from a program the
                // operator is not running.
                Frame::Resize { task, .. } if task == VIEWPORT => {
                    let why = "the other end tried to resize the pane it is being shown in";
                    runner.shutdown().await;
                    link.cut(why).await;
                    break Outcome::Failed(why.into());
                }
                Frame::Resize { task, cols, rows } => {
                    ui.apply(Event::Resized { task, cols, rows });
                    runner.resize(task, cols, rows).await;
                }
                Frame::Close { task } => runner.close(task).await,
                Frame::Say { text } => ui.apply(Event::Said {
                    from_operator: true,
                    text,
                }),
                Frame::Cut { reason } => {
                    runner.shutdown().await;
                    ui.apply(Event::Cut {
                        reason: reason.clone(),
                    });
                    let _ = ui.draw();
                    break Outcome::Cut(reason);
                }
                Frame::Ping { nonce } => {
                    if link.send(&Frame::Pong { nonce }).await.is_err() {
                        break Outcome::PeerLeft;
                    }
                }
                Frame::Pong { .. } | Frame::Hello(_) => {}
                // Output and Exit are this side's to produce. A peer sending
                // one is claiming something ran here that did not.
                Frame::Output { .. } | Frame::Exit { .. } => {
                    let why = "the other end reported output from a program it is not running";
                    runner.shutdown().await;
                    ui.apply(Event::Cut { reason: why.into() });
                    let _ = ui.draw();
                    link.cut(why).await;
                    break Outcome::Failed(why.into());
                }
            },
        }

        let _ = ui.draw();
    };

    ui.restore();
    outcome
}

/// What the owner should see about a frame this side is sending.
fn to_event(frame: &Frame) -> Option<Event> {
    match frame {
        Frame::Output { task, stream, data } => Some(Event::Output {
            task: *task,
            stream: *stream,
            data: data.clone(),
        }),
        Frame::Exit { task, ended } => Some(Event::Ended {
            task: *task,
            ended: *ended,
        }),
        _ => None,
    }
}

/// The next key, or nothing at all when there is no keyboard to read.
///
/// A branch of the select above needs a future either way. Without the pending
/// arm, a client with no terminal would have one `select!` leg that resolves
/// immediately and forever, spinning the loop.
async fn next_key(
    keys: &mut Option<crossterm::event::EventStream>,
) -> Option<Result<crossterm::event::Event, std::io::Error>> {
    use futures_util::StreamExt;
    match keys {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}
