//! Turning two words into a channel the relay cannot read.
//!
//! # Why this is a password-authenticated exchange and not something simpler
//!
//! The obvious thing — a key exchange with the phrase as a pre-shared key — is
//! wrong here, and wrong in a way that looks right. Those constructions assume
//! the shared secret has real entropy. This one has twenty bits. Anyone who
//! records the exchange, and the relay records everything by definition, can
//! try every phrase against the recording offline and be done in under a
//! second. The channel would look encrypted and protect nothing.
//!
//! SPAKE2 is built for exactly this case. Both ends derive a strong key from a
//! weak shared phrase, and an eavesdropper who captures the entire exchange
//! learns nothing that lets them guess offline. Every guess has to be a live
//! attempt against a live session — which is why the relay's expiry and failure
//! limit are load-bearing rather than hardening. magic-wormhole has been doing
//! this with a spoken code for a decade; this is the same shape.
//!
//! A recorded session stays unreadable even if the phrase leaks afterwards,
//! because the session key comes from ephemeral values that exist only for the
//! length of the handshake and are gone when it ends.
//!
//! # What the relay is told
//!
//! The slot, and nothing else. The phrase does not reach it hashed, salted, or
//! derived — see `invite`, which explains why any of those would be the same
//! as telling it outright.
//!
//! # The shape of it
//!
//! 1. Each end mints its SPAKE2 message and sends it. Symmetric, so neither
//!    end waits for the other and the relay may pair them in either order.
//! 2. Each end finishes with the message it received, yielding a shared key.
//! 3. Each end derives, from that key and a transcript binding, one sealing key
//!    per direction and one confirmation tag per role.
//! 4. Each end sends its tag and checks the peer's in constant time. A wrong
//!    phrase fails here, at the peer, and the relay is not the one refusing.

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use subtle::ConstantTimeEq;

use crate::frame::{Frame, Role, MAX_FRAME};
use crate::invite::{Phrase, Slot};

/// Sealed frames are at most this much larger than the frame inside them.
const OVERHEAD: usize = 16;

const INFO_OPERATOR_TO_OWNER: &[u8] = b"tether v1 operator->owner";
const INFO_OWNER_TO_OPERATOR: &[u8] = b"tether v1 owner->operator";
const INFO_CONFIRM_OPERATOR: &[u8] = b"tether v1 confirm operator";
const INFO_CONFIRM_OWNER: &[u8] = b"tether v1 confirm owner";

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("the peer replayed our own handshake message back at us")]
    Reflected,
    #[error("the peer's handshake message is malformed")]
    Malformed,
    #[error("the invite code did not match")]
    WrongCode,
    #[error("the channel is out of step and cannot be trusted")]
    OutOfStep,
    #[error("sealed frame is {0} bytes, which is over the limit")]
    TooLarge(usize),
    #[error(transparent)]
    Frame(#[from] crate::frame::FrameError),
}

/// Our half of the exchange, waiting for the peer's message.
pub struct Pending {
    role: Role,
    state: Spake2<Ed25519Group>,
    slot: Slot,
    ours: Vec<u8>,
}

/// The keys a completed handshake produced, plus the tag each end owes the
/// other. Nothing here outlives the session: the struct is consumed by
/// `confirm`, which hands back only the sealed channel.
pub struct Confirming {
    role: Role,
    send: [u8; 32],
    recv: [u8; 32],
    ours: [u8; 32],
    theirs: [u8; 32],
}

/// Begin. Returns our message, which goes to the peer verbatim.
///
/// The slot is the SPAKE2 identity, which binds the exchange to it: a message
/// captured on one slot cannot be replayed into another, even by a relay that
/// knows every slot it ever issued.
pub fn start(role: Role, slot: Slot, phrase: &Phrase) -> (Pending, Vec<u8>) {
    let (state, ours) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(phrase.as_password()),
        &Identity::new(slot.to_string().as_bytes()),
    );
    (
        Pending {
            role,
            state,
            slot,
            ours: ours.clone(),
        },
        ours,
    )
}

impl Pending {
    /// Finish the exchange with the peer's message.
    ///
    /// A correct phrase and a wrong one are indistinguishable here: SPAKE2
    /// yields a key either way, and the two ends simply derive different ones.
    /// The confirmation step below is what turns that into a refusal.
    pub fn finish(self, theirs: &[u8]) -> Result<(Confirming, [u8; 32]), HandshakeError> {
        if theirs.is_empty() {
            return Err(HandshakeError::Malformed);
        }
        // Our own message coming back means something bounced it, and finishing
        // against it would derive a key with nobody on the other end.
        if theirs == self.ours.as_slice() {
            return Err(HandshakeError::Reflected);
        }

        let slot = self.slot;
        let ours = self.ours.clone();
        let role = self.role;
        let key = self.state.finish(theirs).map_err(|_| HandshakeError::Malformed)?;

        // Both ends must salt with the same bytes, and symmetric SPAKE2 gives
        // them no agreed order, so the two messages are sorted rather than
        // concatenated in the order each end happens to see them.
        let (first, second) = if ours.as_slice() <= theirs {
            (ours.as_slice(), theirs)
        } else {
            (theirs, ours.as_slice())
        };
        let mut binding = Sha256::new();
        binding.update(b"tether v1 transcript");
        binding.update(slot.to_string().as_bytes());
        binding.update(first);
        binding.update(second);
        let salt = binding.finalize();

        let hk = Hkdf::<Sha256>::new(Some(&salt), &key);
        let derive = |info: &[u8]| {
            let mut out = [0u8; 32];
            hk.expand(info, &mut out).expect("32 bytes is a valid HKDF length");
            out
        };

        let (send, recv, ours_tag, theirs_tag) = match role {
            Role::Operator => (
                derive(INFO_OPERATOR_TO_OWNER),
                derive(INFO_OWNER_TO_OPERATOR),
                derive(INFO_CONFIRM_OPERATOR),
                derive(INFO_CONFIRM_OWNER),
            ),
            Role::Owner => (
                derive(INFO_OWNER_TO_OPERATOR),
                derive(INFO_OPERATOR_TO_OWNER),
                derive(INFO_CONFIRM_OWNER),
                derive(INFO_CONFIRM_OPERATOR),
            ),
        };

        Ok((
            Confirming {
                role,
                send,
                recv,
                ours: ours_tag,
                theirs: theirs_tag,
            },
            ours_tag,
        ))
    }
}

impl Confirming {
    /// Check the peer's tag and open the channel.
    ///
    /// This is where a wrong invite code is caught, and it is caught by the
    /// peer rather than by the relay — which is the point. The relay has no
    /// opinion on whether a phrase was right, because it never knew the phrase.
    pub fn confirm(self, peer_tag: &[u8]) -> Result<Channel, HandshakeError> {
        if peer_tag.len() != 32 || !bool::from(peer_tag.ct_eq(&self.theirs)) {
            return Err(HandshakeError::WrongCode);
        }
        Ok(Channel {
            role: self.role,
            seal: ChaCha20Poly1305::new(Key::from_slice(&self.send)),
            open: ChaCha20Poly1305::new(Key::from_slice(&self.recv)),
            sent: 0,
            received: 0,
            broken: false,
        })
    }

    pub fn our_tag(&self) -> [u8; 32] {
        self.ours
    }
}

/// The sealed channel. Frames in, opaque bytes out.
///
/// Each direction has its own key and its own counter, so the two ends never
/// reuse a nonce against each other. The counters are the session's ordering:
/// a frame that arrives out of order, twice, or not at all is an error rather
/// than something to resynchronise from, because none of those can happen to an
/// ordered transport that has not been tampered with.
pub struct Channel {
    role: Role,
    seal: ChaCha20Poly1305,
    open: ChaCha20Poly1305,
    sent: u64,
    received: u64,
    /// Set by the first frame that fails to open, and never cleared.
    ///
    /// Recovering from a bad frame would mean guessing which counter the peer
    /// meant, and an attacker who can make one frame fail could then steer that
    /// guess. There is nothing to recover: an ordered transport does not lose,
    /// duplicate or reorder frames, so a frame that will not open means the
    /// session is over.
    broken: bool,
}

/// Redacted rather than derived, for the reason `invite::Phrase` is: these hold
/// live key material, and the way key material reaches a log is that something
/// two layers away logged a struct whole.
macro_rules! opaque_debug {
    ($t:ty, $label:literal) => {
        impl std::fmt::Debug for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(concat!($label, "(redacted)"))
            }
        }
    };
}

opaque_debug!(Pending, "Pending");
opaque_debug!(Confirming, "Confirming");
opaque_debug!(Channel, "Channel");

fn nonce_for(counter: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[4..].copy_from_slice(&counter.to_be_bytes());
    *Nonce::from_slice(&bytes)
}

impl Channel {
    pub fn role(&self) -> Role {
        self.role
    }

    pub fn seal(&mut self, frame: &Frame) -> Result<Vec<u8>, HandshakeError> {
        let plain = frame.encode()?;
        let counter = self.sent;
        // The counter is authenticated but not transmitted. Both ends know
        // which frame number this is; putting it on the wire would only let a
        // tamperer choose it.
        let sealed = self
            .seal
            .encrypt(
                &nonce_for(counter),
                Payload {
                    msg: &plain,
                    aad: &counter.to_be_bytes(),
                },
            )
            .map_err(|_| HandshakeError::OutOfStep)?;
        self.sent += 1;
        Ok(sealed)
    }

    pub fn open(&mut self, sealed: &[u8]) -> Result<Frame, HandshakeError> {
        if self.broken {
            return Err(HandshakeError::OutOfStep);
        }
        if sealed.len() > MAX_FRAME + OVERHEAD {
            self.broken = true;
            return Err(HandshakeError::TooLarge(sealed.len()));
        }
        let counter = self.received;
        let plain = self
            .open
            .decrypt(
                &nonce_for(counter),
                Payload {
                    msg: sealed,
                    aad: &counter.to_be_bytes(),
                },
            )
            .map_err(|_| {
                self.broken = true;
                HandshakeError::OutOfStep
            })?;
        self.received += 1;
        Frame::decode(&plain).inspect_err(|_| self.broken = true).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Ended, Stream};

    fn phrase(words: &[&str]) -> Phrase {
        Phrase::parse(words).unwrap()
    }

    /// Run both ends against each other and return their channels.
    fn pair(slot: u32, operator: &Phrase, owner: &Phrase) -> Result<(Channel, Channel), HandshakeError> {
        let slot = Slot::new(slot).unwrap();
        let (op, op_msg) = start(Role::Operator, slot, operator);
        let (ow, ow_msg) = start(Role::Owner, slot, owner);
        let (op, op_tag) = op.finish(&ow_msg)?;
        let (ow, ow_tag) = ow.finish(&op_msg)?;
        Ok((op.confirm(&ow_tag)?, ow.confirm(&op_tag)?))
    }

    #[test]
    fn the_same_code_on_both_ends_opens_a_channel_that_carries_frames() {
        let p = phrase(&["anchor", "kettle"]);
        let (mut operator, mut owner) = pair(7, &p, &p).unwrap();

        let sent = Frame::Say {
            text: "about to run the backup".into(),
        };
        assert_eq!(owner.open(&operator.seal(&sent).unwrap()).unwrap(), sent);

        let back = Frame::Output {
            task: 1,
            stream: Stream::Screen,
            data: vec![1, 2, 3],
        };
        assert_eq!(operator.open(&owner.seal(&back).unwrap()).unwrap(), back);
    }

    #[test]
    fn a_wrong_phrase_is_refused_at_confirmation_not_at_the_relay() {
        let err = pair(7, &phrase(&["anchor", "kettle"]), &phrase(&["anchor", "orchid"]))
            .unwrap_err();
        assert!(matches!(err, HandshakeError::WrongCode), "{err:?}");
    }

    #[test]
    fn a_code_from_one_slot_does_not_open_a_channel_on_another() {
        let p = phrase(&["anchor", "kettle"]);
        let (op, op_msg) = start(Role::Operator, Slot::new(7).unwrap(), &p);
        let (ow, ow_msg) = start(Role::Owner, Slot::new(8).unwrap(), &p);
        let (op, op_tag) = op.finish(&ow_msg).unwrap();
        let (ow, ow_tag) = ow.finish(&op_msg).unwrap();
        assert!(matches!(
            op.confirm(&ow_tag).unwrap_err(),
            HandshakeError::WrongCode
        ));
        assert!(matches!(
            ow.confirm(&op_tag).unwrap_err(),
            HandshakeError::WrongCode
        ));
    }

    #[test]
    fn our_own_message_bounced_back_is_refused() {
        let p = phrase(&["anchor", "kettle"]);
        let (op, op_msg) = start(Role::Operator, Slot::new(7).unwrap(), &p);
        assert!(matches!(
            op.finish(&op_msg).unwrap_err(),
            HandshakeError::Reflected
        ));
    }

    #[test]
    fn two_sessions_on_one_code_do_not_share_a_key() {
        let p = phrase(&["anchor", "kettle"]);
        let (mut a, _) = pair(7, &p, &p).unwrap();
        let (_, mut b) = pair(7, &p, &p).unwrap();
        let sealed = a.seal(&Frame::Ping { nonce: 1 }).unwrap();
        assert!(b.open(&sealed).is_err());
    }

    #[test]
    fn a_tampered_frame_does_not_open() {
        let p = phrase(&["anchor", "kettle"]);
        let (mut operator, mut owner) = pair(7, &p, &p).unwrap();
        let mut sealed = operator.seal(&Frame::Ping { nonce: 1 }).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert!(matches!(
            owner.open(&sealed).unwrap_err(),
            HandshakeError::OutOfStep
        ));
    }

    #[test]
    fn a_replayed_frame_does_not_open_a_second_time() {
        let p = phrase(&["anchor", "kettle"]);
        let (mut operator, mut owner) = pair(7, &p, &p).unwrap();
        let sealed = operator.seal(&Frame::Ping { nonce: 1 }).unwrap();
        owner.open(&sealed).unwrap();
        assert!(owner.open(&sealed).is_err(), "a replay was accepted");
    }

    #[test]
    fn frames_out_of_order_are_an_error_rather_than_a_resync() {
        let p = phrase(&["anchor", "kettle"]);
        let (mut operator, mut owner) = pair(7, &p, &p).unwrap();
        let first = operator.seal(&Frame::Ping { nonce: 1 }).unwrap();
        let second = operator
            .seal(&Frame::Exit {
                task: 1,
                ended: Ended::Code(0),
            })
            .unwrap();
        assert!(owner.open(&second).is_err(), "a skipped frame was accepted");
        // And the channel stays refused rather than quietly catching up.
        assert!(owner.open(&first).is_err());
    }
}
