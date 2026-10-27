//! The session table: what a slot is, and every rule about when one exists.
//!
//! A session exists **only** because an authenticated operator asked for one.
//! That inversion is the whole abuse story. The public half of this service can
//! redeem a slot and nothing else, so there is no unauthenticated action that
//! allocates anything, and the flood surface is closed rather than throttled.
//!
//! What the relay holds per session is deliberately thin: a number, two chairs,
//! two deadlines and a count. No phrase, no key, no content, nothing derived
//! from any of them. The relay could not read a session if it wanted to, and
//! this module is where that claim has to stay true.
//!
//! Everything lives in memory, and nothing is written to disk. A restart
//! forgets every slot. An operator whose session was held puts its own slot
//! back with [`Sessions::restore`], so a restart costs a held session a gap
//! rather than its life, and the relay still has no file to recover from.
//!
//! # Held slots
//!
//! Once the owner has consented, the operator *holds* the slot: it names an
//! owner seat token it minted itself, inside the sealed channel. From then on
//! the slot is a resumable session rather than an open invite. Either chair may
//! go empty for [`Limits::resume_grace`], the owner's chair takes only that
//! token, `/claim` finds nothing, and the pairing budget no longer applies,
//! because the phrase it protects grants nothing any more.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use tether_proto::invite::{Slot, MAX_SLOT};
use tokio::sync::{mpsc, oneshot, Notify};

use crate::config::Limits;
use crate::token::Token;

/// The channel a socket's writer task drains. Holding one is how the other end
/// of a pairing speaks to this socket.
pub type Outbound = mpsc::Sender<Bytes>;

/// How a waiting socket is handed its peer once the peer arrives.
type Handoff = oneshot::Sender<Outbound>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Seat {
    Operator,
    Owner,
}

impl Seat {
    fn index(self) -> usize {
        match self {
            Seat::Operator => 0,
            Seat::Owner => 1,
        }
    }

    fn other(self) -> Self {
        match self {
            Seat::Operator => Seat::Owner,
            Seat::Owner => Seat::Operator,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Seat::Operator => "operator",
            Seat::Owner => "owner",
        }
    }
}

struct Occupant {
    /// Which socket this is, so a socket that was replaced cannot free the
    /// chair its replacement now sits in.
    id: u64,
    /// Ends this socket's leg when a newer socket takes its chair.
    kick: Arc<Notify>,
    /// While this socket waits, what the peer will be handed. Once paired,
    /// nothing: the peer's reader then holds the only sender for this socket's
    /// channel, so the channel closes the moment the peer goes. That is the
    /// teardown cascade in [`crate::pump`], and a copy kept here would stop it
    /// — each end would sit on its socket until its own idle deadline.
    tx: Option<Outbound>,
    /// `Some` while this socket is waiting for the other chair to fill;
    /// `None` once it is paired. A peer that finds `None` here is looking at a
    /// pairing already in progress and must be refused rather than joined to a
    /// socket that is about to go away.
    handoff: Option<Handoff>,
}

struct Session {
    /// When this slot dies, unless the operator is sitting in it. The
    /// operator's chair keeps the session alive past the deadline, and the
    /// socket's own unpaired timeout is what bounds that. The owner's chair
    /// does not: an owner alone in a slot is waiting for somebody who is not
    /// coming back.
    deadline: Instant,
    pairings_used: u32,
    operator_token: Token,
    /// Single-use owner tickets, each with its own expiry. Bounded so a claim
    /// flood cannot grow this: the oldest is evicted, which costs a genuine
    /// owner one more claim and costs an attacker the flood they wanted.
    tickets: Vec<(Token, Instant)>,
    /// The owner's seat token, once the operator holds the slot.
    owner_token: Option<Token>,
    seats: [Option<Occupant>; 2],
}

impl Session {
    fn held(&self) -> bool {
        self.owner_token.is_some()
    }
}

/// What is left of a held slot after it ends: enough to tell its owner so.
///
/// Without it an owner redialling a released slot gets the same `404` a relay
/// restart gives, and has to wait out the whole grace to learn the session is
/// over. Keyed by the owner's token, so nobody else can tell it exists.
struct Tombstone {
    slot: u32,
    owner_token: Token,
    until: Instant,
}

#[derive(Default)]
struct Table {
    live: HashMap<u32, Session>,
    ended: Vec<Tombstone>,
}

impl Table {
    fn bury(&mut self, slot: u32, owner_token: Token, until: Instant) {
        self.ended.retain(|t| t.slot != slot);
        self.ended.push(Tombstone {
            slot,
            owner_token,
            until,
        });
    }

    fn buried(&self, slot: u32, token: Token, now: Instant) -> bool {
        self.ended
            .iter()
            .any(|t| t.slot == slot && t.until > now && t.owner_token == token)
    }
}

const MAX_TICKETS: usize = 8;

static NEXT_OCCUPANT: AtomicU64 = AtomicU64::new(1);

/// What `issue` hands back to the operator.
#[derive(Debug)]
pub struct Issued {
    pub slot: Slot,
    pub token: Token,
    pub expires_in: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub enum IssueError {
    /// Every slot in the namespace is live, or the concurrent cap is reached.
    /// The same answer either way: come back later.
    Full,
}

/// The outcome of taking a chair.
pub enum Join {
    Paired {
        seat: Seat,
        peer: Outbound,
        id: u64,
        kick: Arc<Notify>,
    },
    Waiting {
        seat: Seat,
        peer: oneshot::Receiver<Outbound>,
        id: u64,
        kick: Arc<Notify>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum JoinError {
    /// No such slot, or the token names no chair at it. One error for both,
    /// answered as "there is nothing here": a caller must not be able to tell a
    /// live slot from a dead one by the shape of the refusal.
    Unknown,
    /// The chair this token names is already occupied.
    Taken,
    /// The other chair is in a pairing that is still unwinding.
    Busy,
    /// A held slot that the operator ended, asked about with its owner token.
    Ended,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RestoreError {
    /// The number now belongs to another session.
    Taken,
    /// The session was ended, not lost.
    Ended,
    /// The concurrent cap is reached.
    Full,
}

pub struct Sessions {
    inner: Mutex<Table>,
    limits: Limits,
    max_sessions: usize,
}

impl Sessions {
    pub fn new(limits: Limits, max_sessions: usize) -> Self {
        Self {
            inner: Mutex::new(Table::default()),
            limits,
            max_sessions,
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn live(&self) -> usize {
        self.inner.lock().unwrap().live.len()
    }

    /// Allocate a slot for an operator who has already been authenticated.
    ///
    /// The smallest free number wins, because the code is read aloud and a
    /// one-digit slot is a shorter sentence than a three-digit one. Slots are
    /// public by design, so there is nothing to gain by making them
    /// unpredictable and a real cost to making them long.
    pub fn issue(&self, now: Instant) -> Result<Issued, IssueError> {
        let mut table = self.inner.lock().unwrap();
        self.reap_locked(&mut table, now);
        if table.live.len() >= self.max_sessions {
            return Err(IssueError::Full);
        }
        let n = (1..=MAX_SLOT)
            .find(|n| !table.live.contains_key(n))
            .ok_or(IssueError::Full)?;
        let slot = Slot::new(n).expect("the search range is the valid range");
        let token = Token::mint();
        table.live.insert(
            n,
            Session {
                deadline: now + self.limits.issue_ttl,
                pairings_used: 0,
                operator_token: token,
                tickets: Vec::new(),
                owner_token: None,
                seats: [None, None],
            },
        );
        Ok(Issued {
            slot,
            token,
            expires_in: self.limits.issue_ttl,
        })
    }

    /// Mint a single-use ticket for the owner's chair at an existing slot.
    ///
    /// This is the public half, and it is the whole reason the join socket is
    /// not the first thing a stranger touches: this request arrives on the
    /// plain leg, where the forwarded client address still exists, so the
    /// per-address limit can be applied somewhere it means something.
    ///
    /// A held slot mints nothing. Its owner already has a seat token, and a
    /// ticket would only let a stranger with the phrase take the owner's chair
    /// while the owner is away.
    pub fn claim(&self, slot: Slot, now: Instant) -> Option<(Token, Duration)> {
        let mut table = self.inner.lock().unwrap();
        self.reap_locked(&mut table, now);
        let session = table.live.get_mut(&slot.get())?;
        if session.held() {
            return None;
        }
        session.tickets.retain(|(_, expiry)| *expiry > now);
        if session.tickets.len() >= MAX_TICKETS {
            session.tickets.remove(0);
        }
        let token = Token::mint();
        session.tickets.push((token, now + self.limits.ticket_ttl));
        Some((token, self.limits.ticket_ttl))
    }

    /// Turn a slot into a resumable session, because its owner has consented.
    ///
    /// Authorised by the operator's seat token, like [`Self::destroy`]. The
    /// owner token is minted by the operator and reaches the owner only inside
    /// the sealed channel, so the relay learns it here and nowhere else.
    pub fn hold(&self, slot: Slot, token: Token, owner_token: Token, now: Instant) -> bool {
        let mut table = self.inner.lock().unwrap();
        self.reap_locked(&mut table, now);
        let Some(session) = table.live.get_mut(&slot.get()) else {
            return false;
        };
        if session.operator_token != token {
            return false;
        }
        session.owner_token = Some(owner_token);
        session.tickets.clear();
        true
    }

    /// Put back a held slot this relay forgot, because it restarted.
    ///
    /// Reached only with the bearer `/issue` takes. Asking again for a slot
    /// that is already back is not an error, so an operator that lost the
    /// answer can repeat the request.
    pub fn restore(
        &self,
        slot: Slot,
        token: Token,
        owner_token: Token,
        now: Instant,
    ) -> Result<(), RestoreError> {
        let mut table = self.inner.lock().unwrap();
        self.reap_locked(&mut table, now);
        if let Some(session) = table.live.get(&slot.get()) {
            return if session.operator_token == token && session.owner_token == Some(owner_token) {
                Ok(())
            } else {
                Err(RestoreError::Taken)
            };
        }
        if table.buried(slot.get(), owner_token, now) {
            return Err(RestoreError::Ended);
        }
        if table.live.len() >= self.max_sessions {
            return Err(RestoreError::Full);
        }
        table.live.insert(
            slot.get(),
            Session {
                deadline: now + self.limits.resume_grace,
                pairings_used: 0,
                operator_token: token,
                tickets: Vec::new(),
                owner_token: Some(owner_token),
                seats: [None, None],
            },
        );
        Ok(())
    }

    /// Take the chair this token names, pairing with the other end if it is
    /// already sitting there.
    ///
    /// At a held slot a token may take a chair that is still occupied, and
    /// the socket sitting there is ended. That socket is the one this caller
    /// lost: its far end went to sleep or changed network, and the relay will
    /// not hear about it until a keepalive fails, minutes from now.
    pub fn join(
        &self,
        slot: Slot,
        token: Token,
        tx: Outbound,
        now: Instant,
    ) -> Result<Join, JoinError> {
        let mut table = self.inner.lock().unwrap();
        self.reap_locked(&mut table, now);
        let n = slot.get();
        let ended = table.buried(n, token, now);
        let Some(session) = table.live.get_mut(&n) else {
            return Err(if ended {
                JoinError::Ended
            } else {
                JoinError::Unknown
            });
        };

        let seat = if session.operator_token == token {
            Seat::Operator
        } else if let Some(owner_token) = session.owner_token {
            if owner_token != token {
                return Err(if ended {
                    JoinError::Ended
                } else {
                    JoinError::Unknown
                });
            }
            Seat::Owner
        } else {
            session.tickets.retain(|(_, expiry)| *expiry > now);
            let found = session.tickets.iter().position(|(t, _)| *t == token);
            match found {
                // A ticket is spent whether or not the chair turns out to be
                // free, so a leaked one is worth exactly one attempt.
                Some(i) => {
                    session.tickets.remove(i);
                    Seat::Owner
                }
                None => {
                    return Err(if ended {
                        JoinError::Ended
                    } else {
                        JoinError::Unknown
                    })
                }
            }
        };

        let held = session.held();
        if session.seats[seat.index()].is_some() {
            if !held {
                return Err(JoinError::Taken);
            }
            if let Some(stale) = session.seats[seat.index()].take() {
                stale.kick.notify_one();
            }
        }

        let id = NEXT_OCCUPANT.fetch_add(1, Ordering::Relaxed);
        let kick = Arc::new(Notify::new());
        let peer_ready = session.seats[seat.other().index()]
            .as_ref()
            .map(|peer| peer.handoff.is_some() && peer.tx.is_some());

        match peer_ready {
            Some(true) => {
                let peer = session.seats[seat.other().index()].as_mut().unwrap();
                let handoff = peer.handoff.take().unwrap();
                let peer_tx = peer.tx.take().unwrap();
                // Each end is handed the *other* end's channel. Handing back
                // the channel a socket already owns wires it to itself, which
                // looks exactly like a working pairing until a byte has to
                // reach somebody.
                session.seats[seat.index()] = Some(Occupant {
                    id,
                    kick: Arc::clone(&kick),
                    tx: None,
                    handoff: None,
                });
                session.pairings_used += 1;
                // The receiver is gone only if the peer's socket died between
                // taking its chair and this moment; its own cleanup is already
                // on the way, so let this join fall back to waiting.
                if handoff.send(tx).is_err() {
                    session.seats[seat.index()] = None;
                    session.pairings_used -= 1;
                    return Err(JoinError::Busy);
                }
                Ok(Join::Paired {
                    seat,
                    peer: peer_tx,
                    id,
                    kick,
                })
            }
            // The other chair is still paired with the socket just replaced.
            // That pairing is unwinding, and the other end will be back to
            // find this one waiting.
            Some(false) if !held => Err(JoinError::Busy),
            _ => {
                let (handoff, peer) = oneshot::channel();
                session.seats[seat.index()] = Some(Occupant {
                    id,
                    kick: Arc::clone(&kick),
                    tx: Some(tx),
                    handoff: Some(handoff),
                });
                Ok(Join::Waiting {
                    seat,
                    peer,
                    id,
                    kick,
                })
            }
        }
    }

    /// Give up a chair, and decide whether the slot survives it.
    ///
    /// Returns true if the session is gone. A slot dies here when its pairing
    /// budget is spent — that budget is the failure limit, and it is what makes
    /// a phrase short enough to say aloud safe to use.
    ///
    /// `id` is the socket leaving. A socket that was replaced at a held slot
    /// leaves a chair that is no longer its own, and changes nothing.
    pub fn leave(&self, slot: Slot, seat: Seat, id: u64, now: Instant) -> bool {
        let mut table = self.inner.lock().unwrap();
        let Some(session) = table.live.get_mut(&slot.get()) else {
            return true;
        };
        let ours = matches!(&session.seats[seat.index()], Some(occupant) if occupant.id == id);
        if !ours {
            return false;
        }
        session.seats[seat.index()] = None;
        if session.held() {
            // Either end may be the one that dropped, and the other is about
            // to drop too, because the pump's cascade ends both legs. Both are
            // coming back, so both chairs get the same grace.
            session.deadline = now + self.limits.resume_grace;
            return false;
        }
        if session.pairings_used >= self.limits.max_pairings {
            table.live.remove(&slot.get());
            return true;
        }
        session.deadline = now
            + match seat {
                // A slot is the address of an operator waiting at it, so it
                // must not outlive one by much. The operator's chair does cycle
                // — each dial gets ninety seconds and is retaken the instant it
                // ends — so this grace covers that gap and little else.
                //
                // Before this, an operator that had finished left a slot that
                // still minted tickets for five more minutes. The next person
                // to run the code sat down opposite nobody and waited out their
                // own handshake budget before being told the wrong thing.
                Seat::Operator => self.limits.operator_grace,
                // An owner leaving is a fumbled phrase or a refusal, and the
                // operator is still sitting there. Give the slot its full life
                // back, so a retry costs a retry rather than a new code.
                Seat::Owner => self.limits.issue_ttl,
            };
        false
    }

    /// End a slot because the operator says it is finished with it.
    ///
    /// The relay cannot decide this for itself. It pumps sealed bytes and holds
    /// no key, so a refusal and a mistyped phrase look identical from here —
    /// one must destroy the slot and the other must leave it open. Only the
    /// operator knows which happened, and this is how it says so.
    ///
    /// The seat token is the whole authorisation. It is what the relay handed
    /// back when it issued the slot, and it is already the secret that lets
    /// that operator take its chair. Anyone holding it could end the session
    /// by occupying it anyway.
    pub fn destroy(&self, slot: Slot, token: Token, now: Instant) -> bool {
        let mut table = self.inner.lock().unwrap();
        let Some(session) = table.live.get(&slot.get()) else {
            return false;
        };
        if session.operator_token != token {
            return false;
        }
        if let Some(owner_token) = session.owner_token {
            table.bury(slot.get(), owner_token, now + self.limits.resume_grace);
        }
        table.live.remove(&slot.get());
        true
    }

    pub fn reap(&self, now: Instant) -> usize {
        let mut table = self.inner.lock().unwrap();
        let before = table.live.len();
        self.reap_locked(&mut table, now);
        before - table.live.len()
    }

    /// Drop every slot that has run out of reasons to exist.
    ///
    /// An occupied chair used to be reason enough, either chair. It is not: an
    /// owner sitting alone in a slot whose operator has gone is exactly the
    /// hang this table is meant to prevent, and counting them as occupancy
    /// would pin that slot open for as long as they waited. Only the operator's
    /// chair keeps a slot alive past its deadline, because only the operator
    /// can answer.
    ///
    /// A held slot that expires leaves a tombstone, so its owner hears that it
    /// ended rather than that it is lost.
    fn reap_locked(&self, table: &mut Table, now: Instant) {
        table.ended.retain(|t| t.until > now);
        let expired: Vec<(u32, Option<Token>)> = table
            .live
            .iter()
            .filter(|(_, s)| s.seats[Seat::Operator.index()].is_none() && s.deadline <= now)
            .map(|(n, s)| (*n, s.owner_token))
            .collect();
        for (n, owner_token) in expired {
            table.live.remove(&n);
            if let Some(owner_token) = owner_token {
                table.bury(n, owner_token, now + self.limits.resume_grace);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions() -> Sessions {
        Sessions::new(Limits::default(), 64)
    }

    fn chan() -> Outbound {
        mpsc::channel(1).0
    }

    fn id(joined: &Join) -> u64 {
        match joined {
            Join::Paired { id, .. } | Join::Waiting { id, .. } => *id,
        }
    }

    #[test]
    fn the_first_slot_issued_is_the_shortest_one() {
        let s = sessions();
        assert_eq!(s.issue(Instant::now()).unwrap().slot.get(), 1);
        assert_eq!(s.issue(Instant::now()).unwrap().slot.get(), 2);
    }

    #[test]
    fn a_freed_slot_is_handed_out_again() {
        let s = sessions();
        let now = Instant::now();
        let first = s.issue(now).unwrap();
        let second = s.issue(now).unwrap();
        assert_eq!(second.slot.get(), 2);
        // Let the first expire unredeemed.
        s.reap(now + s.limits.issue_ttl + Duration::from_secs(1));
        assert_eq!(s.issue(now).unwrap().slot.get(), 1);
        assert_eq!(first.slot.get(), 1);
    }

    #[test]
    fn the_concurrent_cap_refuses_rather_than_queues() {
        let s = Sessions::new(Limits::default(), 2);
        let now = Instant::now();
        s.issue(now).unwrap();
        s.issue(now).unwrap();
        assert_eq!(s.issue(now).err(), Some(IssueError::Full));
    }

    #[test]
    fn claiming_a_slot_nobody_issued_finds_nothing() {
        let s = sessions();
        assert!(s.claim(Slot::new(7).unwrap(), Instant::now()).is_none());
    }

    #[test]
    fn a_ticket_is_good_once() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        let (ticket, _) = s.claim(issued.slot, now).unwrap();
        let joined = s.join(issued.slot, ticket, chan(), now);
        assert!(matches!(
            joined,
            Ok(Join::Waiting {
                seat: Seat::Owner,
                ..
            })
        ));
        // The chair is occupied now, but the point is the ticket itself: it
        // reports Unknown rather than Taken, because it no longer names a chair.
        s.leave(issued.slot, Seat::Owner, id(&joined.unwrap()), now);
        assert_eq!(
            s.join(issued.slot, ticket, chan(), now).err(),
            Some(JoinError::Unknown)
        );
    }

    #[test]
    fn an_expired_ticket_names_no_chair() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        let (ticket, ttl) = s.claim(issued.slot, now).unwrap();
        let later = now + ttl + Duration::from_secs(1);
        assert_eq!(
            s.join(issued.slot, ticket, chan(), later).err(),
            Some(JoinError::Unknown)
        );
    }

    #[test]
    fn the_operator_token_is_not_a_ticket_and_does_not_get_spent() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        let joined = s.join(issued.slot, issued.token, chan(), now);
        assert!(matches!(
            joined,
            Ok(Join::Waiting {
                seat: Seat::Operator,
                ..
            })
        ));
        s.leave(issued.slot, Seat::Operator, id(&joined.unwrap()), now);
        assert!(matches!(
            s.join(issued.slot, issued.token, chan(), now),
            Ok(Join::Waiting {
                seat: Seat::Operator,
                ..
            })
        ));
    }

    #[test]
    fn a_token_for_one_slot_is_no_good_at_another() {
        let s = sessions();
        let now = Instant::now();
        let a = s.issue(now).unwrap();
        let b = s.issue(now).unwrap();
        assert_eq!(
            s.join(b.slot, a.token, chan(), now).err(),
            Some(JoinError::Unknown)
        );
    }

    #[test]
    fn a_slot_nobody_issued_refuses_the_same_way_a_wrong_token_does() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        let stranger = Token::mint();
        assert_eq!(
            s.join(issued.slot, stranger, chan(), now).err(),
            Some(JoinError::Unknown)
        );
        assert_eq!(
            s.join(Slot::new(500).unwrap(), stranger, chan(), now).err(),
            Some(JoinError::Unknown)
        );
    }

    #[test]
    fn each_end_is_handed_the_other_ends_channel() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        let operator_tx = chan();
        let owner_tx = chan();

        let Ok(Join::Waiting {
            peer: mut handed_to_operator,
            ..
        }) = s.join(issued.slot, issued.token, operator_tx.clone(), now)
        else {
            panic!("the operator should be waiting");
        };

        let (ticket, _) = s.claim(issued.slot, now).unwrap();
        let Ok(Join::Paired {
            seat,
            peer: handed_to_owner,
            ..
        }) = s.join(issued.slot, ticket, owner_tx.clone(), now)
        else {
            panic!("the owner should pair with the operator");
        };
        assert_eq!(seat, Seat::Owner);

        // Wiring a socket to its own channel looks exactly like a working
        // pairing until a byte has to reach somebody, so check which channel
        // each end actually got rather than that it got one.
        assert!(handed_to_owner.same_channel(&operator_tx));
        let handed = handed_to_operator
            .try_recv()
            .expect("the operator should have been handed the owner's channel");
        assert!(handed.same_channel(&owner_tx));
    }

    #[test]
    fn taking_an_occupied_chair_is_refused() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        let _first = s.join(issued.slot, issued.token, chan(), now).unwrap();
        assert_eq!(
            s.join(issued.slot, issued.token, chan(), now).err(),
            Some(JoinError::Taken)
        );
    }

    #[test]
    fn a_slot_dies_once_its_pairing_budget_is_spent() {
        let limits = Limits {
            max_pairings: 2,
            ..Limits::default()
        };
        let s = Sessions::new(limits, 64);
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        for _ in 0..2 {
            let op = s.join(issued.slot, issued.token, chan(), now).unwrap();
            let (ticket, _) = s.claim(issued.slot, now).unwrap();
            let ow = s.join(issued.slot, ticket, chan(), now).unwrap();
            s.leave(issued.slot, Seat::Owner, id(&ow), now);
            s.leave(issued.slot, Seat::Operator, id(&op), now);
        }
        assert_eq!(s.live(), 0);
        assert_eq!(
            s.join(issued.slot, issued.token, chan(), now).err(),
            Some(JoinError::Unknown)
        );
    }

    #[test]
    fn a_slot_the_operator_is_sitting_in_outlives_its_deadline() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        let _waiting = s.join(issued.slot, issued.token, chan(), now).unwrap();
        assert_eq!(
            s.reap(now + s.limits.issue_ttl + Duration::from_secs(60)),
            0
        );
        assert_eq!(s.live(), 1);
    }

    /// The counterpart, and the reason the reaper asks which chair rather than
    /// whether any chair. An owner sitting alone is waiting for an operator
    /// that has gone; treating that as occupancy pins the slot open for exactly
    /// as long as they are prepared to wait.
    #[test]
    fn an_owner_alone_does_not_keep_a_slot_whose_operator_has_gone() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        let (ticket, _) = s.claim(issued.slot, now).unwrap();
        let _owner = s.join(issued.slot, ticket, chan(), now).unwrap();
        assert_eq!(s.reap(now + s.limits.issue_ttl + Duration::from_secs(1)), 1);
        assert_eq!(s.live(), 0);
    }

    #[test]
    fn an_unredeemed_slot_expires() {
        let s = sessions();
        let now = Instant::now();
        s.issue(now).unwrap();
        assert_eq!(s.reap(now + s.limits.issue_ttl + Duration::from_secs(1)), 1);
        assert_eq!(s.live(), 0);
    }

    /// A fumbled phrase pairs, fails and drops the owner. The operator is still
    /// sitting there, so the slot gets its whole life back and the code the
    /// owner was read aloud is still the code.
    #[test]
    fn an_owner_leaving_gives_the_slot_its_full_life_back() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        let _operator = s.join(issued.slot, issued.token, chan(), now).unwrap();
        let (ticket, _) = s.claim(issued.slot, now).unwrap();
        let owner = s.join(issued.slot, ticket, chan(), now).unwrap();

        let late = now + s.limits.issue_ttl + Duration::from_secs(60);
        assert!(!s.leave(issued.slot, Seat::Owner, id(&owner), late));
        assert_eq!(
            s.reap(late + s.limits.issue_ttl - Duration::from_secs(1)),
            0
        );
        assert_eq!(s.live(), 1, "the operator is still waiting at it");
    }

    /// The operator leaving is the other half, and the asymmetry is the fix. A
    /// slot is the address of an operator waiting at it, so once that operator
    /// is gone the slot has only long enough to get one back.
    #[test]
    fn an_operator_leaving_gives_the_slot_only_a_grace() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        let waiting = s.join(issued.slot, issued.token, chan(), now).unwrap();

        let late = now + s.limits.issue_ttl + Duration::from_secs(60);
        assert!(!s.leave(issued.slot, Seat::Operator, id(&waiting), late));

        // Inside the grace the slot is still there, which is what lets the
        // dial loop drop its chair and take it straight back.
        assert_eq!(
            s.reap(late + s.limits.operator_grace - Duration::from_millis(1)),
            0
        );
        // Past it, gone — where it used to get another five minutes.
        assert_eq!(
            s.reap(late + s.limits.operator_grace + Duration::from_secs(1)),
            1
        );
        assert_eq!(s.live(), 0);
    }

    /// The bug this was all for. A session ends, and the next person to run the
    /// same code must be told so rather than seated opposite nobody.
    #[test]
    fn a_released_slot_stops_handing_out_tickets_at_once() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        assert!(
            s.claim(issued.slot, now).is_some(),
            "live, before the release"
        );

        assert!(s.destroy(issued.slot, issued.token, now));

        assert!(
            s.claim(issued.slot, now).is_none(),
            "a claim on a released slot is what used to hang the owner"
        );
        assert_eq!(s.live(), 0);
        assert!(
            !s.destroy(issued.slot, issued.token, now),
            "and it is gone for good"
        );
    }

    #[test]
    fn a_release_without_the_seat_token_does_nothing() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        assert!(!s.destroy(issued.slot, Token::mint(), now));
        assert_eq!(s.live(), 1);
        assert!(s.claim(issued.slot, now).is_some());
    }

    #[test]
    fn tickets_do_not_accumulate_without_bound() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        let first = s.claim(issued.slot, now).unwrap().0;
        for _ in 0..MAX_TICKETS {
            s.claim(issued.slot, now).unwrap();
        }
        assert_eq!(
            s.join(issued.slot, first, chan(), now).err(),
            Some(JoinError::Unknown)
        );
    }

    /// An operator and an owner paired at a slot the operator then holds.
    fn held(s: &Sessions, now: Instant) -> (Issued, Token, Join, Join) {
        let issued = s.issue(now).unwrap();
        let operator = s.join(issued.slot, issued.token, chan(), now).unwrap();
        let (ticket, _) = s.claim(issued.slot, now).unwrap();
        let owner = s.join(issued.slot, ticket, chan(), now).unwrap();
        let owner_token = Token::mint();
        assert!(s.hold(issued.slot, issued.token, owner_token, now));
        (issued, owner_token, operator, owner)
    }

    #[test]
    fn only_the_seat_token_can_hold_a_slot() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        assert!(!s.hold(issued.slot, Token::mint(), Token::mint(), now));
        assert!(!s.hold(Slot::new(9).unwrap(), issued.token, Token::mint(), now));
        assert!(s.claim(issued.slot, now).is_some(), "still an open invite");
    }

    /// The phrase grants nothing once the owner has consented, so a stranger
    /// who heard it can neither take the owner's chair nor spend the slot.
    #[test]
    fn a_held_slot_mints_no_tickets() {
        let s = sessions();
        let now = Instant::now();
        let (issued, _, _operator, _owner) = held(&s, now);
        assert!(s.claim(issued.slot, now).is_none());
    }

    #[test]
    fn a_held_slot_survives_both_chairs_emptying_for_the_grace() {
        let s = sessions();
        let now = Instant::now();
        let (issued, _, operator, owner) = held(&s, now);
        assert!(!s.leave(issued.slot, Seat::Owner, id(&owner), now));
        assert!(!s.leave(issued.slot, Seat::Operator, id(&operator), now));
        let grace = s.limits.resume_grace;
        assert_eq!(s.reap(now + grace - Duration::from_secs(1)), 0);
        assert_eq!(s.reap(now + grace + Duration::from_secs(1)), 1);
    }

    #[test]
    fn the_owner_comes_back_with_its_seat_token() {
        let s = sessions();
        let now = Instant::now();
        let (issued, owner_token, operator, owner) = held(&s, now);
        s.leave(issued.slot, Seat::Owner, id(&owner), now);
        s.leave(issued.slot, Seat::Operator, id(&operator), now);
        let later = now + Duration::from_secs(300);
        let back = s.join(issued.slot, owner_token, chan(), later);
        assert!(matches!(
            back,
            Ok(Join::Waiting {
                seat: Seat::Owner,
                ..
            })
        ));
        assert!(matches!(
            s.join(issued.slot, issued.token, chan(), later),
            Ok(Join::Paired {
                seat: Seat::Operator,
                ..
            })
        ));
    }

    /// A flaky network pairs over and over. The pairing budget is for guessing
    /// a phrase, and must not end a session the owner already consented to.
    #[test]
    fn a_held_slot_has_no_pairing_budget() {
        let s = sessions();
        let now = Instant::now();
        let (issued, owner_token, mut operator, mut owner) = held(&s, now);
        for _ in 0..s.limits.max_pairings * 2 {
            assert!(!s.leave(issued.slot, Seat::Owner, id(&owner), now));
            assert!(!s.leave(issued.slot, Seat::Operator, id(&operator), now));
            operator = s.join(issued.slot, issued.token, chan(), now).unwrap();
            owner = s.join(issued.slot, owner_token, chan(), now).unwrap();
        }
        assert_eq!(s.live(), 1);
    }

    #[test]
    fn a_wrong_owner_token_at_a_held_slot_finds_nothing() {
        let s = sessions();
        let now = Instant::now();
        let (issued, _, _operator, _owner) = held(&s, now);
        assert_eq!(
            s.join(issued.slot, Token::mint(), chan(), now).err(),
            Some(JoinError::Unknown)
        );
    }

    /// The far end of a socket went to sleep, and the relay has not noticed.
    /// The owner coming back must not wait for it to.
    #[test]
    fn a_returning_owner_replaces_the_socket_it_lost() {
        let s = sessions();
        let now = Instant::now();
        let (issued, owner_token, _operator, stale) = held(&s, now);
        let (Join::Waiting {
            kick: stale_kick, ..
        }
        | Join::Paired {
            kick: stale_kick, ..
        }) = &stale;
        let fresh = s.join(issued.slot, owner_token, chan(), now).unwrap();
        assert!(matches!(
            fresh,
            Join::Waiting {
                seat: Seat::Owner,
                ..
            }
        ));
        assert!(
            futures_util::FutureExt::now_or_never(stale_kick.notified()).is_some(),
            "the stale socket is told to go"
        );
        // Its leaving does not free the chair its replacement sits in.
        assert!(!s.leave(issued.slot, Seat::Owner, id(&stale), now));
        assert_eq!(
            s.join(issued.slot, owner_token, chan(), now)
                .map(|j| id(&j) != id(&fresh)),
            Ok(true),
            "a third arrival replaces the second, which was still seated"
        );
    }

    #[test]
    fn an_owner_redialling_a_released_held_slot_hears_that_it_ended() {
        let s = sessions();
        let now = Instant::now();
        let (issued, owner_token, _operator, _owner) = held(&s, now);
        assert!(s.destroy(issued.slot, issued.token, now));
        assert_eq!(
            s.join(issued.slot, owner_token, chan(), now).err(),
            Some(JoinError::Ended)
        );
        assert_eq!(
            s.join(issued.slot, Token::mint(), chan(), now).err(),
            Some(JoinError::Unknown),
            "and nobody else can tell"
        );
        // Still true once the number belongs to somebody else.
        let next = s.issue(now).unwrap();
        assert_eq!(next.slot, issued.slot);
        assert_eq!(
            s.join(issued.slot, owner_token, chan(), now).err(),
            Some(JoinError::Ended)
        );
    }

    #[test]
    fn a_held_slot_that_expires_tells_its_owner_it_ended() {
        let s = sessions();
        let now = Instant::now();
        let (issued, owner_token, operator, owner) = held(&s, now);
        s.leave(issued.slot, Seat::Owner, id(&owner), now);
        s.leave(issued.slot, Seat::Operator, id(&operator), now);
        let late = now + s.limits.resume_grace + Duration::from_secs(1);
        assert_eq!(
            s.join(issued.slot, owner_token, chan(), late).err(),
            Some(JoinError::Ended)
        );
    }

    #[test]
    fn a_restarted_relay_takes_a_held_slot_back() {
        let s = sessions();
        let now = Instant::now();
        let slot = Slot::new(4).unwrap();
        let (token, owner_token) = (Token::mint(), Token::mint());
        assert_eq!(s.restore(slot, token, owner_token, now), Ok(()));
        assert_eq!(
            s.restore(slot, token, owner_token, now),
            Ok(()),
            "twice is fine"
        );
        assert!(s.claim(slot, now).is_none(), "it comes back held");
        let owner = s.join(slot, owner_token, chan(), now);
        assert!(matches!(
            owner,
            Ok(Join::Waiting {
                seat: Seat::Owner,
                ..
            })
        ));
        assert!(matches!(
            s.join(slot, token, chan(), now),
            Ok(Join::Paired {
                seat: Seat::Operator,
                ..
            })
        ));
    }

    #[test]
    fn a_restore_cannot_take_a_number_another_session_holds() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        assert_eq!(
            s.restore(issued.slot, Token::mint(), Token::mint(), now),
            Err(RestoreError::Taken)
        );
    }

    #[test]
    fn a_restore_cannot_revive_a_released_session() {
        let s = sessions();
        let now = Instant::now();
        let (issued, owner_token, _operator, _owner) = held(&s, now);
        s.destroy(issued.slot, issued.token, now);
        assert_eq!(
            s.restore(issued.slot, issued.token, owner_token, now),
            Err(RestoreError::Ended)
        );
    }
}
