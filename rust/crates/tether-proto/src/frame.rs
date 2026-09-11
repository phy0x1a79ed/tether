//! What the two ends say to each other, once the channel is sealed.
//!
//! Everything in a session is a `Frame`. The relay never sees one: by the time
//! a frame is on the wire it is inside the sealed transport, and the relay is
//! pumping opaque bytes it cannot parse.
//!
//! # The one ordering rule
//!
//! For a given task, every `Output` the peer will ever send arrives before its
//! `Exit`, and `Exit` is the last frame that task will ever produce. The owner's
//! screen depends on it: a transcript that showed a command's exit status above
//! its own output would be lying about what happened on the owner's machine.
//!
//! The rule is cheap to honour and easy to break. It holds because the executor
//! writes a task's frames from one place in one order, and because the socket
//! underneath is ordered. Anything that moves output onto a second path — a
//! thread with its own writer, a "fast path" for large chunks — breaks it
//! without failing a test unless that test watches the order.

use serde::{Deserialize, Serialize};

/// Bumped when a frame changes shape in a way an older peer cannot read.
///
/// The two ends are separate downloads, so they are not guaranteed to be the
/// same build: the owner may be running a binary minted weeks before the
/// operator's. `Hello` carries this so a mismatch is a sentence on the owner's
/// screen rather than a frame that deserializes into nonsense.
pub const PROTOCOL_VERSION: u16 = 1;

/// The largest frame either end will encode or accept.
///
/// A cap rather than trust: without one, a peer can name a length and make the
/// other end reserve it, which turns one hostile frame into an allocation the
/// machine may not survive.
pub const MAX_FRAME: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Operator,
    Owner,
}

impl Role {
    pub fn peer(self) -> Self {
        match self {
            Role::Operator => Role::Owner,
            Role::Owner => Role::Operator,
        }
    }
}

/// What a task is: a command that runs to completion, or a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// Run it, collect what it writes, report how it ended. The line-oriented
    /// case, and the one the owner's transcript renders as a block.
    Command,
    /// A pseudo-terminal. The only way a full-screen program is visible to the
    /// owner as the screen the operator is actually looking at.
    Shell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stream {
    Stdout,
    Stderr,
    /// A terminal's output. Not split, because a terminal does not split it.
    Screen,
}

/// How a task ended, in the two ways a process can end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ended {
    Code(i32),
    Signal(i32),
}

/// Names one task within a session. Minted by the operator, monotonic.
pub type TaskId = u32;

/// The opening exchange: who is on the other end, and on what.
///
/// The owner's client prints this before asking for consent, which is the whole
/// reason it carries a name and a host rather than a role alone. "Allow an
/// operator?" tells the person at the keyboard nothing they can act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub version: u16,
    pub role: Role,
    /// The awm identity on the operator's side, or the login name on the
    /// owner's. Shown to a person, so it is a name and not an id.
    pub who: String,
    pub host: String,
    pub os: String,
    /// The build this end came from, so a stale download is visible as a fact
    /// rather than as behaviour nobody can explain.
    pub build: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Frame {
    Hello(Hello),

    /// Start a task. `command` is the program for a `Command`, and the shell to
    /// launch for a `Shell` — `None` means the owner's login shell.
    Open {
        task: TaskId,
        kind: Kind,
        command: Option<String>,
        cols: u16,
        rows: u16,
    },
    /// Keystrokes, or standard input for a command.
    Input { task: TaskId, data: Vec<u8> },
    Resize { task: TaskId, cols: u16, rows: u16 },
    Output {
        task: TaskId,
        stream: Stream,
        data: Vec<u8>,
    },
    /// Last frame for this task. See the ordering rule in the module docs.
    Exit { task: TaskId, ended: Ended },
    /// Ask for a task to stop. The peer answers with `Exit`, never silence.
    Close { task: TaskId },

    /// A line from one person to the other, shown on their screen. This is how
    /// the operator says what they are about to do before doing it.
    Say { text: String },

    /// End the session. Either end may send it, at any time, and the other end
    /// exits. It carries a reason because "the session ended" on a screen with
    /// no explanation is the thing that makes people distrust a tool.
    Cut { reason: String },

    /// Keepalives. The path holds idle sockets open for an hour at nginx and
    /// less at the CDN, so a quiet session dies unless something is flowing.
    Ping { nonce: u64 },
    Pong { nonce: u64 },
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame is {0} bytes, over the {MAX_FRAME}-byte limit")]
    TooLarge(usize),
    #[error("could not encode frame: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("could not decode frame: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
}

impl Frame {
    /// Encode for the wire.
    ///
    /// Field names go on the wire rather than positions. It costs a few dozen
    /// bytes a frame and it buys the property that matters across two separately
    /// downloaded binaries: a field added in the middle of a struct shifts every
    /// position after it, and positional decoding would read the new shape as
    /// the old one without erroring.
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        let bytes = rmp_serde::to_vec_named(self)?;
        if bytes.len() > MAX_FRAME {
            return Err(FrameError::TooLarge(bytes.len()));
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        if bytes.len() > MAX_FRAME {
            return Err(FrameError::TooLarge(bytes.len()));
        }
        Ok(rmp_serde::from_slice(bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Frame {
        Frame::Hello(Hello {
            version: PROTOCOL_VERSION,
            role: Role::Operator,
            who: "awm".into(),
            host: "altair".into(),
            os: "linux".into(),
            build: "deadbeef".into(),
        })
    }

    #[test]
    fn every_frame_round_trips() {
        let frames = vec![
            hello(),
            Frame::Open {
                task: 1,
                kind: Kind::Shell,
                command: None,
                cols: 120,
                rows: 40,
            },
            Frame::Input {
                task: 1,
                data: b"ls -la\n".to_vec(),
            },
            Frame::Resize {
                task: 1,
                cols: 80,
                rows: 24,
            },
            Frame::Output {
                task: 1,
                stream: Stream::Screen,
                data: vec![0x1b, b'[', b'2', b'J'],
            },
            Frame::Exit {
                task: 1,
                ended: Ended::Code(0),
            },
            Frame::Close { task: 1 },
            Frame::Say {
                text: "about to run the backup".into(),
            },
            Frame::Cut {
                reason: "owner ended the session".into(),
            },
            Frame::Ping { nonce: 7 },
            Frame::Pong { nonce: 7 },
        ];
        for f in frames {
            assert_eq!(Frame::decode(&f.encode().unwrap()).unwrap(), f);
        }
    }

    #[test]
    fn arbitrary_bytes_survive_the_encoding() {
        let data: Vec<u8> = (0..=255u8).cycle().take(9000).collect();
        let f = Frame::Output {
            task: 3,
            stream: Stream::Stdout,
            data: data.clone(),
        };
        match Frame::decode(&f.encode().unwrap()).unwrap() {
            Frame::Output { data: got, .. } => assert_eq!(got, data),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_oversized_frame_is_refused_at_both_ends() {
        let f = Frame::Output {
            task: 1,
            stream: Stream::Stdout,
            data: vec![0u8; MAX_FRAME + 1],
        };
        assert!(matches!(f.encode(), Err(FrameError::TooLarge(_))));
        assert!(matches!(
            Frame::decode(&vec![0u8; MAX_FRAME + 1]),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn garbage_decodes_to_an_error_rather_than_a_frame() {
        assert!(Frame::decode(b"not a frame at all").is_err());
    }
}
