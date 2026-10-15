//! tether's shared half: one vocabulary, one wire, one handshake.
//!
//! The relay, the owner's client and the operator's daemon all depend on this
//! crate and on nothing else in common. That is deliberate. The two ends of a
//! session are separate downloads on separate machines, and the only thing
//! keeping them able to talk to each other is that the frames, the invite code
//! and the key derivation exist in exactly one place.
//!
//! # The words, and what they mean
//!
//! - **owner** — the person at the machine being helped, and the machine.
//! - **operator** — the trusted person invited in.
//! - **relay** — the public box the session passes through. It pairs two
//!   sockets and pumps bytes it cannot read.
//! - **invite code** — a slot and two words, such as `7 flying mug`. The whole
//!   credential, and the only thing a person handles.
//! - **slot** — the code's first token. The relay issues it and it is public.
//! - **phrase** — the code's words. The secret. The relay never learns it.
//! - **cut** — to end a session, from either side.
//!
//! These are the words in the code, the verbs, the interface and the docs. The
//! predecessor's vocabulary is retired: nobody is a "friend", a session is not
//! a "name", and the tool is not a "probe".
//!
//! # Reading order
//!
//! [`invite`] first: it explains why the credential is split the way it is, and
//! that split is what makes everything else safe. Then [`handshake`], then
//! [`frame`].

pub mod frame;
pub mod handshake;
pub mod invite;
pub mod words;

pub use frame::{Ended, Frame, Hello, Kind, Role, Stream, TaskId, PROTOCOL_VERSION};
pub use handshake::{Channel, HandshakeError};
pub use invite::{InviteCode, InviteError, Phrase, Slot, DEFAULT_WORDS, MAX_SLOT};
