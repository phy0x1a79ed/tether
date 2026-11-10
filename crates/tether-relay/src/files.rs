//! A room's files: bytes either side uploads, kept on disk beside the room.
//!
//! A file travels apart from the messages, so the 64 KiB message cap never
//! meets it. A side uploads the bytes, gets an id, and sends a message that
//! names the id. The relay reads no message to learn which files are named.
//!
//! A file arrives in one or more parts, because a proxy on the way caps one
//! request near 100 MB. Each later part names the offset it starts at, so a
//! part that failed is sent again at the same offset and nothing doubles. A
//! file is fetchable only once its last part is in. Until then it sits under
//! a `.part` name, so a broken upload never looks finished.
//!
//! No size cap applies to one file. The relay refuses a part only when the
//! disk would fall under [`crate::config::Limits::file_floor`], and a room
//! keeps at most [`MAX_FILES`], dropping the oldest, so a guest cannot spend
//! the host's disk or its inodes.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axum::body::{Body, Bytes};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::log;
use crate::token::Token;

/// How many files one room keeps. The oldest goes when a new one comes.
pub const MAX_FILES: usize = 256;

/// How many bytes a part writes between two checks of the free space.
const FLOOR_CHECK: u64 = 8 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum PutError {
    /// No unfinished file of that id in this room.
    NoSuchFile,
    /// The part starts elsewhere than the end of what the relay holds.
    Offset(u64),
    /// Another part of the same file is still arriving.
    Busy,
    /// The disk would fall under the free-space floor.
    Full,
    /// The body broke off, or the disk failed.
    Broken,
}

struct Entry {
    id: String,
    size: u64,
    done: bool,
    busy: bool,
}

pub struct Store {
    dir: PathBuf,
    entries: Mutex<VecDeque<Entry>>,
}

impl Store {
    /// An empty store in `dir`. The directory is made at the first upload.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            entries: Mutex::default(),
        }
    }

    /// The store a restarted relay finds in `dir`, oldest file first.
    pub fn restore(dir: PathBuf) -> Self {
        let mut found: Vec<(std::time::SystemTime, Entry)> = Vec::new();
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let (id, done) = match name.strip_suffix(".part") {
                Some(id) => (id.to_string(), false),
                None => (name, true),
            };
            let Ok(meta) = entry.metadata() else { continue };
            if parse_id(&id).is_none() || !meta.is_file() {
                continue;
            }
            let at = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            found.push((
                at,
                Entry {
                    id,
                    size: meta.len(),
                    done,
                    busy: false,
                },
            ));
        }
        found.sort_by_key(|(at, _)| *at);
        Self {
            dir,
            entries: Mutex::new(found.into_iter().map(|(_, e)| e).collect()),
        }
    }

    /// The path and size of a finished file.
    pub fn get(&self, id: &str) -> Option<(PathBuf, u64)> {
        let entries = self.entries.lock().unwrap();
        let entry = entries.iter().find(|e| e.id == id && e.done)?;
        Some((self.dir.join(&entry.id), entry.size))
    }

    /// Write one part of a file. `id` is `None` for the first part of a new
    /// file, and `more` says another part follows. Returns the id and the size
    /// the relay now holds.
    pub async fn put(
        &self,
        id: Option<&str>,
        offset: u64,
        more: bool,
        body: Body,
        floor: u64,
    ) -> Result<(String, u64), PutError> {
        let id = self.claim(id, offset)?;
        let part = self.dir.join(format!("{id}.part"));
        let written = write_part(&self.dir, &part, offset, body, floor).await;
        let mut entries = self.entries.lock().unwrap();
        let Some(at) = entries.iter().position(|e| e.id == id) else {
            // Dropped as the oldest while its part was arriving.
            remove(&part);
            return Err(PutError::NoSuchFile);
        };
        let entry = &mut entries[at];
        entry.busy = false;
        let size = match written {
            Ok(size) => size,
            Err(e) => {
                if offset == 0 {
                    entries.remove(at);
                    remove(&part);
                }
                return Err(e);
            }
        };
        entry.size = size;
        if !more {
            if fs::rename(&part, self.dir.join(&id)).is_err() {
                return Err(PutError::Broken);
            }
            entry.done = true;
        }
        Ok((id, size))
    }

    /// Mark the file a part belongs to as arriving, or make a new one.
    fn claim(&self, id: Option<&str>, offset: u64) -> Result<String, PutError> {
        let mut entries = self.entries.lock().unwrap();
        let Some(id) = id else {
            let id = Token::mint().to_hex()[..32].to_string();
            entries.push_back(Entry {
                id: id.clone(),
                size: 0,
                done: false,
                busy: true,
            });
            while entries.len() > MAX_FILES {
                let old = entries.pop_front().expect("more than MAX_FILES entries");
                remove(&self.dir.join(&old.id));
                remove(&self.dir.join(format!("{}.part", old.id)));
            }
            return Ok(id);
        };
        let entry = entries
            .iter_mut()
            .find(|e| e.id == id && !e.done)
            .ok_or(PutError::NoSuchFile)?;
        if entry.busy {
            return Err(PutError::Busy);
        }
        if entry.size != offset {
            return Err(PutError::Offset(entry.size));
        }
        entry.busy = true;
        Ok(id.to_string())
    }

    /// Delete every file, for a room that is gone.
    pub fn remove_all(&self) {
        self.entries.lock().unwrap().clear();
        if let Err(e) = fs::remove_dir_all(&self.dir) {
            if e.kind() != io::ErrorKind::NotFound {
                log::warn(format_args!("could not remove a room's files: {e}"));
            }
        }
    }
}

/// Append `body` to `part` at `offset`, and return the size it then has. On
/// any failure the part is cut back to `offset`, so the same part can be sent
/// again.
async fn write_part(
    dir: &Path,
    part: &Path,
    offset: u64,
    body: Body,
    floor: u64,
) -> Result<u64, PutError> {
    let mut builder = tokio::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(dir).await.map_err(|_| PutError::Broken)?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(part)
        .await
        .map_err(|_| PutError::Broken)?;
    let result = async {
        file.set_len(offset).await.map_err(|_| PutError::Broken)?;
        file.seek(io::SeekFrom::Start(offset))
            .await
            .map_err(|_| PutError::Broken)?;
        if short_of_disk(dir, floor) {
            return Err(PutError::Full);
        }
        let mut size = offset;
        let mut since_check = 0;
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| PutError::Broken)?;
            file.write_all(&chunk).await.map_err(|_| PutError::Broken)?;
            size += chunk.len() as u64;
            since_check += chunk.len() as u64;
            if since_check >= FLOOR_CHECK {
                since_check = 0;
                if short_of_disk(dir, floor) {
                    return Err(PutError::Full);
                }
            }
        }
        file.flush().await.map_err(|_| PutError::Broken)?;
        Ok(size)
    }
    .await;
    if result.is_err() {
        let _ = file.set_len(offset).await;
    }
    result
}

fn remove(path: &Path) {
    if let Err(e) = fs::remove_file(path) {
        if e.kind() != io::ErrorKind::NotFound {
            log::warn(format_args!("could not remove a room file: {e}"));
        }
    }
}

#[cfg(unix)]
fn short_of_disk(dir: &Path, floor: u64) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return true;
    };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return true;
    }
    (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64) < floor
}

#[cfg(not(unix))]
fn short_of_disk(_dir: &Path, _floor: u64) -> bool {
    false
}

/// A file id exactly as the relay mints it: 32 lowercase hex digits. Exactly,
/// because the public edge forwards only that shape.
pub fn parse_id(s: &str) -> Option<&str> {
    let ok = s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    ok.then_some(s)
}

/// The bytes of a finished file, as a response body read in 64 KiB pieces.
pub async fn stream(path: &Path) -> io::Result<Body> {
    let file = tokio::fs::File::open(path).await?;
    let pieces = futures_util::stream::unfold(Some(file), |file| async move {
        let mut file = file?;
        let mut buf = vec![0u8; 64 * 1024];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(Bytes::from(buf)), Some(file)))
            }
            Err(e) => Some((Err(e), None)),
        }
    });
    Ok(Body::from_stream(pieces))
}
