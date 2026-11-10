//! Files on their way to the owner's machine.
//!
//! A file goes as `Put` parts, one part per turn of the session loop and only
//! while the link has room, so a large file never holds up a keystroke, a
//! ping or a task's output. The caller hears back once the owner says where
//! the file landed, or why it did not.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde_json::json;
use tether_proto::frame::Frame;

use crate::session::Ack;

/// The bytes in one `Put`, well under the frame cap.
pub const PART: usize = 512 * 1024;

#[derive(Default)]
pub struct Outbox {
    next: u32,
    sending: VecDeque<Sending>,
    waiting: HashMap<u32, (Ack, u64)>,
}

struct Sending {
    id: u32,
    name: String,
    file: File,
    offset: u64,
    reply: Ack,
}

impl Outbox {
    /// Queue a file, or answer the caller at once if it cannot be read.
    pub fn add(&mut self, path: &Path, name: Option<String>, reply: Ack) {
        let name = name.or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()));
        let opened = File::open(path).and_then(|f| f.metadata().map(|m| (f, m)));
        match (name, opened) {
            (Some(name), Ok((file, meta))) if meta.is_file() => {
                self.next += 1;
                self.sending.push_back(Sending {
                    id: self.next,
                    name,
                    file,
                    offset: 0,
                    reply,
                });
            }
            (_, Err(e)) => {
                let _ = reply.send(Err(format!("cannot read {}: {e}", path.display())));
            }
            _ => {
                let _ = reply.send(Err(format!("{} is not a file", path.display())));
            }
        }
    }

    pub fn has_parts(&self) -> bool {
        !self.sending.is_empty()
    }

    /// The next part to send. The last part moves its file to waiting on the
    /// owner's answer.
    pub fn part(&mut self) -> Option<Frame> {
        let front = self.sending.front_mut()?;
        let mut data = Vec::with_capacity(PART);
        if let Err(e) = (&mut front.file).take(PART as u64).read_to_end(&mut data) {
            let gone = self.sending.pop_front()?;
            let _ = gone
                .reply
                .send(Err(format!("reading the file failed partway: {e}")));
            return None;
        }
        let last = data.len() < PART;
        let offset = front.offset;
        front.offset += data.len() as u64;
        let frame = Frame::Put {
            file: front.id,
            name: front.name.clone(),
            offset,
            data,
            last,
        };
        if last {
            let done = self.sending.pop_front()?;
            self.waiting.insert(done.id, (done.reply, done.offset));
        }
        Some(frame)
    }

    /// The owner's `Placed` or `Refused`.
    pub fn answer(&mut self, file: u32, placed: Result<String, String>) {
        let reply = match self.waiting.remove(&file) {
            Some(waiting) => Some(waiting),
            None => self
                .sending
                .iter()
                .position(|s| s.id == file)
                .and_then(|at| self.sending.remove(at))
                .map(|s| (s.reply, s.offset)),
        };
        if let Some((reply, size)) = reply {
            let _ = reply.send(placed.map(|path| json!({"ok": true, "path": path, "bytes": size})));
        }
    }
}
