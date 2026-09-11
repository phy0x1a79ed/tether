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
//! Everything lives in memory. Restarting the relay ends every session, which
//! is a feature: there is no state to become stale, and no file to recover a
//! session from.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use tether_proto::invite::{Slot, MAX_SLOT};
use tokio::sync::{mpsc, oneshot};

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
    tx: Outbound,
    /// `Some` while this socket is waiting for the other chair to fill;
    /// `None` once it is paired. A peer that finds `None` here is looking at a
    /// pairing already in progress and must be refused rather than joined to a
    /// socket that is about to go away.
    handoff: Option<Handoff>,
}

struct Session {
    /// When this slot dies, if nobody is sitting in it. An occupied chair keeps
    /// the session alive past the deadline; the socket's own unpaired timeout
    /// is what bounds that.
    deadline: Instant,
    pairings_used: u32,
    operator_token: Token,
    /// Single-use owner tickets, each with its own expiry. Bounded so a claim
    /// flood cannot grow this: the oldest is evicted, which costs a genuine
    /// owner one more claim and costs an attacker the flood they wanted.
    tickets: Vec<(Token, Instant)>,
    seats: [Option<Occupant>; 2],
}

const MAX_TICKETS: usize = 8;

impl Session {
    fn occupied(&self) -> bool {
        self.seats.iter().any(|s| s.is_some())
    }
}

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
    },
    Waiting {
        seat: Seat,
        peer: oneshot::Receiver<Outbound>,
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
}

pub struct Sessions {
    inner: Mutex<HashMap<u32, Session>>,
    limits: Limits,
    max_sessions: usize,
}

impl Sessions {
    pub fn new(limits: Limits, max_sessions: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            limits,
            max_sessions,
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn live(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// Allocate a slot for an operator who has already been authenticated.
    ///
    /// The smallest free number wins, because the code is read aloud and a
    /// one-digit slot is a shorter sentence than a three-digit one. Slots are
    /// public by design, so there is nothing to gain by making them
    /// unpredictable and a real cost to making them long.
    pub fn issue(&self, now: Instant) -> Result<Issued, IssueError> {
        let mut table = self.inner.lock().unwrap();
        Self::reap_locked(&mut table, now);
        if table.len() >= self.max_sessions {
            return Err(IssueError::Full);
        }
        let n = (1..=MAX_SLOT)
            .find(|n| !table.contains_key(n))
            .ok_or(IssueError::Full)?;
        let slot = Slot::new(n).expect("the search range is the valid range");
        let token = Token::mint();
        table.insert(
            n,
            Session {
                deadline: now + self.limits.issue_ttl,
                pairings_used: 0,
                operator_token: token,
                tickets: Vec::new(),
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
    pub fn claim(&self, slot: Slot, now: Instant) -> Option<(Token, Duration)> {
        let mut table = self.inner.lock().unwrap();
        Self::reap_locked(&mut table, now);
        let session = table.get_mut(&slot.get())?;
        session.tickets.retain(|(_, expiry)| *expiry > now);
        if session.tickets.len() >= MAX_TICKETS {
            session.tickets.remove(0);
        }
        let token = Token::mint();
        session.tickets.push((token, now + self.limits.ticket_ttl));
        Some((token, self.limits.ticket_ttl))
    }

    /// Take the chair this token names, pairing with the other end if it is
    /// already sitting there.
    pub fn join(
        &self,
        slot: Slot,
        token: Token,
        tx: Outbound,
        now: Instant,
    ) -> Result<Join, JoinError> {
        let mut table = self.inner.lock().unwrap();
        Self::reap_locked(&mut table, now);
        let session = table.get_mut(&slot.get()).ok_or(JoinError::Unknown)?;

        let seat = if session.operator_token == token {
            Seat::Operator
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
                None => return Err(JoinError::Unknown),
            }
        };

        if session.seats[seat.index()].is_some() {
            return Err(JoinError::Taken);
        }

        match session.seats[seat.other().index()].as_mut() {
            Some(peer) => {
                let handoff = peer.handoff.take().ok_or(JoinError::Busy)?;
                let peer_tx = peer.tx.clone();
                // Each end is handed the *other* end's channel. Handing back
                // the channel a socket already owns wires it to itself, which
                // looks exactly like a working pairing until a byte has to
                // reach somebody.
                let mine = tx.clone();
                session.seats[seat.index()] = Some(Occupant { tx, handoff: None });
                session.pairings_used += 1;
                // The receiver is gone only if the peer's socket died between
                // taking its chair and this moment; its own cleanup is already
                // on the way, so let this join fall back to waiting.
                if handoff.send(mine).is_err() {
                    session.seats[seat.index()] = None;
                    session.pairings_used -= 1;
                    return Err(JoinError::Busy);
                }
                Ok(Join::Paired {
                    seat,
                    peer: peer_tx,
                })
            }
            None => {
                let (handoff, peer) = oneshot::channel();
                session.seats[seat.index()] = Some(Occupant {
                    tx,
                    handoff: Some(handoff),
                });
                Ok(Join::Waiting { seat, peer })
            }
        }
    }

    /// Give up a chair, and decide whether the slot survives it.
    ///
    /// Returns true if the session is gone. A slot dies here when its pairing
    /// budget is spent — that budget is the failure limit, and it is what makes
    /// a phrase short enough to say aloud safe to use.
    pub fn leave(&self, slot: Slot, seat: Seat, now: Instant) -> bool {
        let mut table = self.inner.lock().unwrap();
        let Some(session) = table.get_mut(&slot.get()) else {
            return true;
        };
        session.seats[seat.index()] = None;
        if session.pairings_used >= self.limits.max_pairings {
            table.remove(&slot.get());
            return true;
        }
        // The slot goes back to waiting for someone to redeem it, with a fresh
        // deadline, so a fumbled code costs a retry rather than a new code.
        session.deadline = now + self.limits.issue_ttl;
        false
    }

    pub fn reap(&self, now: Instant) -> usize {
        let mut table = self.inner.lock().unwrap();
        let before = table.len();
        Self::reap_locked(&mut table, now);
        before - table.len()
    }

    fn reap_locked(table: &mut HashMap<u32, Session>, now: Instant) {
        table.retain(|_, s| s.occupied() || s.deadline > now);
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
        assert!(matches!(
            s.join(issued.slot, ticket, chan(), now),
            Ok(Join::Waiting {
                seat: Seat::Owner,
                ..
            })
        ));
        // The chair is occupied now, but the point is the ticket itself: it
        // reports Unknown rather than Taken, because it no longer names a chair.
        s.leave(issued.slot, Seat::Owner, now);
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
        assert!(matches!(
            s.join(issued.slot, issued.token, chan(), now),
            Ok(Join::Waiting {
                seat: Seat::Operator,
                ..
            })
        ));
        s.leave(issued.slot, Seat::Operator, now);
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
            let _op = s.join(issued.slot, issued.token, chan(), now).unwrap();
            let (ticket, _) = s.claim(issued.slot, now).unwrap();
            let _ow = s.join(issued.slot, ticket, chan(), now).unwrap();
            s.leave(issued.slot, Seat::Owner, now);
            s.leave(issued.slot, Seat::Operator, now);
        }
        assert_eq!(s.live(), 0);
        assert_eq!(
            s.join(issued.slot, issued.token, chan(), now).err(),
            Some(JoinError::Unknown)
        );
    }

    #[test]
    fn an_occupied_slot_outlives_its_deadline() {
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

    #[test]
    fn an_unredeemed_slot_expires() {
        let s = sessions();
        let now = Instant::now();
        s.issue(now).unwrap();
        assert_eq!(s.reap(now + s.limits.issue_ttl + Duration::from_secs(1)), 1);
        assert_eq!(s.live(), 0);
    }

    #[test]
    fn leaving_a_chair_gives_the_slot_a_fresh_deadline() {
        let s = sessions();
        let now = Instant::now();
        let issued = s.issue(now).unwrap();
        let _waiting = s.join(issued.slot, issued.token, chan(), now).unwrap();
        let late = now + s.limits.issue_ttl + Duration::from_secs(60);
        assert!(!s.leave(issued.slot, Seat::Operator, late));
        assert_eq!(s.reap(late + Duration::from_secs(1)), 0);
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
}
