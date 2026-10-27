//! Carrying a consented session through a dropped link.
//!
//! A [`Resumable`] owns the [`Link`] in a task of its own, so a redial never
//! blocks the session that uses it: the owner's cut key and the operator's
//! `status` keep working while the link is down. The session talks to that
//! task over two channels and reads [`Incoming`] events from it.
//!
//! # What resuming needs from both ends
//!
//! Resuming runs only when both `Hello`s say `resume`. The operator then mints
//! [`HoldKeys`], tells the relay the owner's token, and sends the keys as
//! `Frame::Hold`, the first counted frame after consent. The owner can resume
//! only once that frame has arrived.
//!
//! # Counting
//!
//! Each end numbers the frames it sends from consent onwards, leaving out the
//! ones [`Frame::counted`] excludes. It keeps each one until the peer's `Ack`
//! says it arrived. After a redial each end first sends `Resume` with how many
//! frames it received, and then resends everything the peer did not get, under
//! the new channel's keys. Nothing is lost and nothing arrives twice.
//!
//! The two directions start counting at different frames, and both ends agree:
//! the owner's frames are counted from the first frame after its `Hello`, and
//! the operator's from `Hold`. An operator `Say` that was in flight when the
//! owner consented arrives before `Hold` and is not counted by either end.
//!
//! # What ends a gap
//!
//! The link comes back, the relay says the session ended (`410`), the session
//! leaves on purpose, or [`RESUME_WINDOW`] passes. A gap starts on any link
//! error, on [`QUIET`] with nothing heard, or when this machine wakes from
//! sleep.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use futures_util::future::BoxFuture;
use tether_proto::frame::{Frame, HoldKeys, Role};
use tether_proto::RESUME_WINDOW;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep, sleep_until, timeout, Instant};

use crate::{Link, LinkError};

/// How often each end tells the other how much it has received.
///
/// Also the heartbeat: with resuming on, each end hears from the other at
/// least this often.
pub const ACK_EVERY: Duration = Duration::from_secs(15);

/// How long a resumable link may hear nothing before it is treated as down.
///
/// Four missed acks. A socket whose far end went to sleep stays open for
/// minutes before TCP says so, and the session should be redialling by then.
pub const QUIET: Duration = Duration::from_secs(60);

/// How many bytes of sent frames an end keeps for resending.
///
/// The owner stops reading its tasks' output while this is full, so a command
/// that prints during a long gap blocks on its own write rather than losing
/// anything.
pub const KEEP: usize = 8 * 1024 * 1024;

/// How long to wait between failed redials, in order. The last repeats.
const BACKOFF: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];

/// How long a redialled peer has to send its `Resume`.
const RESUME_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a departing session waits for its last frame to go.
const LEAVE_TIMEOUT: Duration = Duration::from_secs(3);

/// What the session hears from its link.
#[derive(Debug)]
pub enum Incoming {
    Frame(Frame),
    /// The link dropped. The session lives until `gives_up_at`.
    Down {
        why: String,
        gives_up_at: SystemTime,
    },
    /// The link is back. `resent` frames went again.
    Up {
        gap: Duration,
        resent: u64,
    },
    /// The session is over, and nothing follows.
    Ended(LinkError),
}

/// How an end gets back to its session, and how it ends one it cannot reach.
pub trait Rejoin: Send + 'static {
    /// Take this end's chair again and run the resume handshake.
    fn dial(&mut self, keys: &HoldKeys) -> BoxFuture<'static, Result<Link, LinkError>>;

    /// End the held slot at the relay, because the session is leaving during a
    /// gap and the peer cannot be told in band.
    fn abandon(&mut self, _keys: &HoldKeys) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

/// Mint the keys an operator hands its owner in `Frame::Hold`.
pub fn mint_keys() -> HoldKeys {
    let mut token = [0u8; 16];
    let mut secret = [0u8; 32];
    getrandom::getrandom(&mut token).expect("the OS randomness source failed");
    getrandom::getrandom(&mut secret).expect("the OS randomness source failed");
    HoldKeys {
        token: hex::encode(token),
        secret,
    }
}

enum Out {
    Frame(Frame),
    Leave(Frame, oneshot::Sender<()>),
}

struct Shared {
    up: AtomicBool,
    ended: AtomicBool,
    /// Bytes of counted frames sent and not yet acknowledged.
    kept: AtomicUsize,
    gives_up_at: Mutex<Option<SystemTime>>,
}

/// A session's link, carried through drops when both ends can.
pub struct Resumable {
    out: mpsc::UnboundedSender<Out>,
    events: mpsc::Receiver<Incoming>,
    shared: Arc<Shared>,
    /// Set once [`Self::recv`] has handed the session its ending.
    told_end: AtomicBool,
    role: Role,
    polled: Option<String>,
}

impl Resumable {
    /// A link that ends at the first drop, for a peer that cannot resume.
    pub fn plain(link: Link) -> Self {
        Self::spawn(link, Mode::Plain, None, None)
    }

    /// The operator's side: resume from now, and send `keys` as the first
    /// counted frame. The relay must already hold the slot for `keys.token`.
    pub fn operator(link: Link, keys: HoldKeys, rejoin: impl Rejoin) -> Self {
        let hold = Frame::Hold(keys.clone());
        let r = Self::spawn(
            link,
            Mode::Resume { counting_in: true },
            Some(keys),
            Some(Box::new(rejoin)),
        );
        let _ = r.send(&hold);
        r
    }

    /// The owner's side: resume once the operator's `Hold` arrives. Until
    /// then a drop ends the session, as it always did.
    pub fn owner(link: Link, rejoin: impl Rejoin) -> Self {
        Self::spawn(
            link,
            Mode::Resume { counting_in: false },
            None,
            Some(Box::new(rejoin)),
        )
    }

    fn spawn(
        link: Link,
        mode: Mode,
        keys: Option<HoldKeys>,
        rejoin: Option<Box<dyn Rejoin>>,
    ) -> Self {
        let role = link.role();
        let polled = link.polled().map(str::to_string);
        let shared = Arc::new(Shared {
            up: AtomicBool::new(true),
            ended: AtomicBool::new(false),
            kept: AtomicUsize::new(0),
            gives_up_at: Mutex::new(None),
        });
        let (out, out_rx) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::channel(256);
        let actor = Actor {
            role,
            resume: matches!(mode, Mode::Resume { .. }),
            counting_in: matches!(mode, Mode::Resume { counting_in: true }),
            keys,
            rejoin,
            sent: 0,
            received: 0,
            unacked: VecDeque::new(),
            shared: Arc::clone(&shared),
            events: events_tx,
            out: out_rx,
        };
        tokio::spawn(actor.run(link));
        Self {
            out,
            events,
            shared,
            told_end: AtomicBool::new(false),
            role,
            polled,
        }
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// Why the first link ran over plain requests, when it did.
    pub fn polled(&self) -> Option<&str> {
        self.polled.as_deref()
    }

    /// Whether the link is carrying frames right now.
    pub fn is_up(&self) -> bool {
        self.shared.up.load(Ordering::Relaxed)
    }

    /// When a gap in progress ends the session, if one is in progress.
    pub fn gives_up_at(&self) -> Option<SystemTime> {
        if self.is_up() {
            return None;
        }
        self.shared.gives_up_at.lock().ok().and_then(|g| *g)
    }

    /// Whether there is room to keep another frame for resending. A sender
    /// that can wait, such as the owner's task output, waits while this is
    /// false.
    pub fn has_room(&self) -> bool {
        self.shared.kept.load(Ordering::Relaxed) < KEEP
    }

    /// Queue a frame for the peer.
    ///
    /// During a gap a counted frame is kept and goes once the link is back,
    /// and a keepalive is dropped. An error means the session has already
    /// been told, through [`Self::recv`], that it is over.
    ///
    /// A frame sent after the link ended and before the session has read why
    /// is dropped without an error. Failing it would let the session end on
    /// "the other end went away" with the real reason, often the peer's `Cut`,
    /// still waiting to be read.
    pub fn send(&self, frame: &Frame) -> Result<(), LinkError> {
        if self.told_end.load(Ordering::Relaxed) {
            return Err(LinkError::Closed);
        }
        if self.shared.ended.load(Ordering::Relaxed) {
            return Ok(());
        }
        if frame.counted() {
            self.shared.kept.fetch_add(weight(frame), Ordering::Relaxed);
        }
        let _ = self.out.send(Out::Frame(frame.clone()));
        Ok(())
    }

    /// The next thing that happened on the link.
    pub async fn recv(&mut self) -> Incoming {
        let next = self
            .events
            .recv()
            .await
            .unwrap_or(Incoming::Ended(LinkError::Closed));
        if matches!(next, Incoming::Ended(_)) {
            self.told_end.store(true, Ordering::Relaxed);
        }
        next
    }

    /// Say why, then go. See [`Self::leave`].
    pub async fn cut(&self, reason: &str) {
        self.leave(&Frame::Cut {
            reason: reason.to_string(),
        })
        .await;
    }

    /// Send a last frame, then close, waiting a moment for it to go.
    ///
    /// During a gap the frame cannot go, so the held slot is ended at the
    /// relay instead, and the peer's next redial hears that it ended.
    pub async fn leave(&self, last: &Frame) {
        let (done, gone) = oneshot::channel();
        if self.out.send(Out::Leave(last.clone(), done)).is_ok() {
            let _ = timeout(LEAVE_TIMEOUT, gone).await;
        }
    }
}

impl Drop for Resumable {
    fn drop(&mut self) {
        self.shared.ended.store(true, Ordering::Relaxed);
    }
}

/// What a frame costs to keep. An estimate is enough: it bounds memory, and
/// the encoding adds a few dozen bytes to the payload.
fn weight(frame: &Frame) -> usize {
    64 + match frame {
        Frame::Output { data, .. } | Frame::Input { data, .. } => data.len(),
        Frame::Say { text } => text.len(),
        _ => 0,
    }
}

enum Mode {
    Plain,
    Resume { counting_in: bool },
}

/// How a stretch of carrying ended.
enum Stop {
    /// The link failed. Redial if the session can.
    Down(String),
    /// The session is over.
    Exit,
}

struct Actor {
    role: Role,
    resume: bool,
    /// Whether received frames are being numbered yet. The owner starts at
    /// `Hold`.
    counting_in: bool,
    keys: Option<HoldKeys>,
    rejoin: Option<Box<dyn Rejoin>>,
    /// The number the next counted frame sent will get.
    sent: u64,
    /// How many counted frames have arrived.
    received: u64,
    unacked: VecDeque<(u64, Frame)>,
    shared: Arc<Shared>,
    events: mpsc::Sender<Incoming>,
    out: mpsc::UnboundedReceiver<Out>,
}

impl Actor {
    /// Whether a drop now is a gap rather than the end.
    fn resumable(&self) -> bool {
        self.resume && self.keys.is_some() && self.rejoin.is_some()
    }

    async fn run(mut self, mut link: Link) {
        loop {
            let why = match self.carry(&mut link).await {
                Stop::Exit => break,
                Stop::Down(why) => why,
            };
            if !self.resumable() {
                self.end(LinkError::Closed).await;
                break;
            }
            match self.reconnect(why).await {
                Some(back) => link = back,
                None => break,
            }
        }
        self.shared.ended.store(true, Ordering::Relaxed);
        self.shared.up.store(false, Ordering::Relaxed);
    }

    /// Give a counted frame its number and keep it until it is acknowledged.
    fn keep(&mut self, frame: Frame) {
        let seq = self.sent;
        self.sent += 1;
        if self.resume {
            self.unacked.push_back((seq, frame));
        } else {
            self.shared
                .kept
                .fetch_sub(weight(&frame), Ordering::Relaxed);
        }
    }

    /// Drop what the peer says it has.
    fn acked(&mut self, received: u64) {
        while let Some((seq, frame)) = self.unacked.front() {
            if *seq >= received {
                break;
            }
            self.shared.kept.fetch_sub(weight(frame), Ordering::Relaxed);
            self.unacked.pop_front();
        }
    }

    async fn end(&mut self, why: LinkError) {
        self.shared.ended.store(true, Ordering::Relaxed);
        let _ = self.events.send(Incoming::Ended(why)).await;
    }

    /// Carry frames both ways until the link fails or the session leaves.
    async fn carry(&mut self, link: &mut Link) -> Stop {
        self.shared.up.store(true, Ordering::Relaxed);
        let mut tick = tokio::time::interval(ACK_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        let mut heard = Instant::now();
        let mut wall = SystemTime::now();
        let mut mono = Instant::now();

        loop {
            tokio::select! {
                out = self.out.recv() => match out {
                    None => {
                        link.close().await;
                        return Stop::Exit;
                    }
                    Some(Out::Frame(frame)) => {
                        if frame.counted() {
                            self.keep(frame.clone());
                        }
                        if let Err(e) = link.send(&frame).await {
                            return Stop::Down(e.to_string());
                        }
                    }
                    Some(Out::Leave(frame, done)) => {
                        let _ = link.send(&frame).await;
                        link.close().await;
                        let _ = done.send(());
                        return Stop::Exit;
                    }
                },
                incoming = link.recv() => match incoming {
                    Ok(frame) => {
                        heard = Instant::now();
                        if !self.take(frame).await {
                            link.close().await;
                            return Stop::Exit;
                        }
                    }
                    Err(e) => return Stop::Down(e.to_string()),
                },
                _ = tick.tick() => {
                    if !self.resumable() {
                        continue;
                    }
                    // A machine that slept has a wall clock that ran on while
                    // the monotonic one stood still. Its sockets are almost
                    // certainly dead, and waiting for QUIET to say so wastes a
                    // minute of the window.
                    let walked = SystemTime::now().duration_since(wall).unwrap_or_default();
                    let ticked = mono.elapsed();
                    wall = SystemTime::now();
                    mono = Instant::now();
                    if walked > ticked + ACK_EVERY {
                        return Stop::Down("this machine was asleep".into());
                    }
                    if heard.elapsed() > QUIET {
                        return Stop::Down("the other end stopped answering".into());
                    }
                    if let Err(e) = link.send(&Frame::Ack { received: self.received }).await {
                        return Stop::Down(e.to_string());
                    }
                }
            }
        }
    }

    /// Handle one frame from the peer. False when the session is over.
    async fn take(&mut self, frame: Frame) -> bool {
        match frame {
            Frame::Ack { received } => {
                self.acked(received);
                true
            }
            // Only the first frame on a resumed channel, which `reconnect`
            // reads itself.
            Frame::Resume { .. } => true,
            Frame::Hold(keys) => {
                if self.resume && self.keys.is_none() && self.role == Role::Owner {
                    self.keys = Some(keys);
                    self.counting_in = true;
                    // `Hold` is the operator's frame 0.
                    self.received = 1;
                }
                true
            }
            frame => {
                if self.counting_in && frame.counted() {
                    self.received += 1;
                }
                self.events.send(Incoming::Frame(frame)).await.is_ok()
            }
        }
    }

    /// Redial through a gap. The new link, or `None` once the session is over.
    async fn reconnect(&mut self, why: String) -> Option<Link> {
        self.shared.up.store(false, Ordering::Relaxed);
        let dropped = Instant::now();
        let deadline = dropped + RESUME_WINDOW;
        let gives_up_at = SystemTime::now() + RESUME_WINDOW;
        if let Ok(mut g) = self.shared.gives_up_at.lock() {
            *g = Some(gives_up_at);
        }
        if self
            .events
            .send(Incoming::Down { why, gives_up_at })
            .await
            .is_err()
        {
            return None;
        }

        let keys = self.keys.clone()?;
        let mut tries = 0usize;
        let mut wait = Duration::ZERO;
        loop {
            let rejoin = self.rejoin.as_mut()?;
            let dial = rejoin.dial(&keys);
            let attempt = async move {
                sleep(wait).await;
                dial.await
            };
            tokio::pin!(attempt);

            let dialled = loop {
                tokio::select! {
                    dialled = &mut attempt => break dialled,
                    _ = sleep_until(deadline) => {
                        self.end(LinkError::GaveUp).await;
                        return None;
                    }
                    out = self.out.recv() => match out {
                        None => return None,
                        Some(Out::Frame(frame)) => {
                            if frame.counted() {
                                self.keep(frame);
                            }
                        }
                        Some(Out::Leave(_, done)) => {
                            if let Some(rejoin) = self.rejoin.as_mut() {
                                let _ = timeout(LEAVE_TIMEOUT, rejoin.abandon(&keys)).await;
                            }
                            let _ = done.send(());
                            return None;
                        }
                    },
                }
            };

            match dialled {
                Ok(mut link) => match self.resend(&mut link).await {
                    Ok(resent) => {
                        if let Ok(mut g) = self.shared.gives_up_at.lock() {
                            *g = None;
                        }
                        self.shared.up.store(true, Ordering::Relaxed);
                        let up = Incoming::Up {
                            gap: dropped.elapsed(),
                            resent,
                        };
                        if self.events.send(up).await.is_err() {
                            return None;
                        }
                        return Some(link);
                    }
                    Err(Resent::Again) => {
                        wait = BACKOFF[tries.min(BACKOFF.len() - 1)];
                        tries += 1;
                    }
                    Err(Resent::Fatal(e)) => {
                        link.close().await;
                        self.end(e).await;
                        return None;
                    }
                },
                Err(LinkError::Ended) => {
                    self.end(LinkError::Ended).await;
                    return None;
                }
                // Waited in the chair and nobody came. Take it again at once:
                // waiting is the right thing to be doing.
                Err(LinkError::NobodyThere) => wait = Duration::ZERO,
                Err(_) => {
                    wait = BACKOFF[tries.min(BACKOFF.len() - 1)];
                    tries += 1;
                }
            }
        }
    }

    /// Trade `Resume` frames on a fresh link and resend what the peer missed.
    async fn resend(&mut self, link: &mut Link) -> Result<u64, Resent> {
        link.send(&Frame::Resume {
            received: self.received,
        })
        .await
        .map_err(|_| Resent::Again)?;
        let theirs = match timeout(RESUME_TIMEOUT, link.recv()).await {
            Ok(Ok(Frame::Resume { received })) => received,
            Ok(Ok(_)) => {
                return Err(Resent::Fatal(LinkError::Handshake(
                    tether_proto::HandshakeError::OutOfStep,
                )))
            }
            Ok(Err(_)) | Err(_) => return Err(Resent::Again),
        };
        let oldest = self
            .unacked
            .front()
            .map(|(seq, _)| *seq)
            .unwrap_or(self.sent);
        if theirs > self.sent || theirs < oldest {
            return Err(Resent::Fatal(LinkError::Handshake(
                tether_proto::HandshakeError::OutOfStep,
            )));
        }
        self.acked(theirs);
        let again: Vec<Frame> = self.unacked.iter().map(|(_, f)| f.clone()).collect();
        for frame in &again {
            link.send(frame).await.map_err(|_| Resent::Again)?;
        }
        Ok(again.len() as u64)
    }
}

enum Resent {
    /// The new link failed before the resend finished. Redial.
    Again,
    /// The two ends disagree about what was sent. The session cannot go on.
    Fatal(LinkError),
}

/// How the operator gets back: its own seat token, and the relay's bearer to
/// put the slot back when the relay restarted.
pub struct OperatorRejoin {
    pub relay: crate::Relay,
    pub bearer: Option<String>,
    pub slot: tether_proto::Slot,
    pub seat: String,
}

impl Rejoin for OperatorRejoin {
    fn dial(&mut self, keys: &HoldKeys) -> BoxFuture<'static, Result<Link, LinkError>> {
        let (relay, bearer, slot, seat) = (
            self.relay.clone(),
            self.bearer.clone(),
            self.slot,
            self.seat.clone(),
        );
        let (owner, secret) = (keys.token.clone(), keys.secret);
        Box::pin(async move {
            match Link::resume_operator(&relay, slot, &seat, &secret).await {
                // A relay that restarted has forgotten every slot. Put this
                // one back, then take the chair.
                Err(LinkError::NoSuchSession) => {
                    let Some(bearer) = bearer else {
                        return Err(LinkError::NoSuchSession);
                    };
                    match crate::restore(&relay, &bearer, slot, &seat, &owner).await {
                        Ok(()) => {}
                        // The number went to somebody else first. This session
                        // has no address any more.
                        Err(LinkError::Taken) => return Err(LinkError::Ended),
                        Err(e) => return Err(e),
                    }
                    Link::resume_operator(&relay, slot, &seat, &secret).await
                }
                other => other,
            }
        })
    }
}

/// How the owner gets back: the token and secret from `Hold`, over the same
/// transport choice as the first dial.
pub struct OwnerRejoin {
    pub relay: crate::Relay,
    pub slot: tether_proto::Slot,
    pub carry: crate::Carry,
}

impl Rejoin for OwnerRejoin {
    fn dial(&mut self, keys: &HoldKeys) -> BoxFuture<'static, Result<Link, LinkError>> {
        let (relay, slot, carry) = (self.relay.clone(), self.slot, self.carry);
        let (token, secret) = (keys.token.clone(), keys.secret);
        Box::pin(async move { Link::resume_owner(&relay, slot, &token, &secret, carry).await })
    }

    fn abandon(&mut self, keys: &HoldKeys) -> BoxFuture<'static, ()> {
        let (relay, slot, token) = (self.relay.clone(), self.slot, keys.token.clone());
        Box::pin(async move {
            let _ = crate::release(&relay, slot, &token).await;
        })
    }
}
