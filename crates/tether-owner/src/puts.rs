//! Files the operator puts on this machine, one `Put` part at a time.
//!
//! Each file goes into a fresh directory under the OS temporary directory, so
//! two files with one name never meet and nothing outside that directory is
//! touched. The bytes go to a `.part` name and take their own name only when
//! the last part is in, so a file cut short never looks finished.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use tether_proto::frame::Frame;

/// The files in flight in one session.
#[derive(Default)]
pub struct Puts {
    open: HashMap<u32, Open>,
    refused: HashSet<u32>,
}

struct Open {
    part: PathBuf,
    dest: PathBuf,
    file: File,
    written: u64,
}

/// What the owner should hear about a part: the frame to answer with, and a
/// line for the screen. Both empty while a file is still coming in.
pub struct Took {
    pub reply: Option<Frame>,
    pub note: Option<String>,
}

impl Puts {
    pub fn take(&mut self, file: u32, name: &str, offset: u64, data: &[u8], last: bool) -> Took {
        if self.refused.contains(&file) {
            return Took {
                reply: None,
                note: None,
            };
        }
        match self.write(file, name, offset, data, last) {
            Ok(None) => Took {
                reply: None,
                note: None,
            },
            Ok(Some(path)) => {
                let path = path.display().to_string();
                Took {
                    note: Some(format!(
                        "the operator put a file at {path} ({} bytes)",
                        offset + data.len() as u64
                    )),
                    reply: Some(Frame::Placed { file, path }),
                }
            }
            Err(why) => {
                if let Some(open) = self.open.remove(&file) {
                    let _ = std::fs::remove_file(open.part);
                }
                self.refused.insert(file);
                Took {
                    note: Some(format!("a file from the operator was not saved: {why}")),
                    reply: Some(Frame::Refused { file, why }),
                }
            }
        }
    }

    fn write(
        &mut self,
        file: u32,
        name: &str,
        offset: u64,
        data: &[u8],
        last: bool,
    ) -> Result<Option<PathBuf>, String> {
        if offset == 0 && !self.open.contains_key(&file) {
            self.open.insert(file, Open::new(name)?);
        }
        let open = self
            .open
            .get_mut(&file)
            .ok_or_else(|| format!("a part at offset {offset} came before the file's start"))?;
        if offset != open.written {
            return Err(format!(
                "a part came at offset {offset}, expected {}",
                open.written
            ));
        }
        open.file.write_all(data).map_err(|e| e.to_string())?;
        open.written += data.len() as u64;
        if !last {
            return Ok(None);
        }
        let open = self.open.remove(&file).expect("checked above");
        open.file.sync_all().map_err(|e| e.to_string())?;
        drop(open.file);
        std::fs::rename(&open.part, &open.dest).map_err(|e| e.to_string())?;
        Ok(Some(open.dest))
    }
}

impl Open {
    fn new(name: &str) -> Result<Self, String> {
        let mut tag = [0u8; 6];
        getrandom::getrandom(&mut tag).map_err(|e| e.to_string())?;
        let dir = std::env::temp_dir().join(format!("tether-{}", hex::encode(tag)));
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
        let name = safe_name(name);
        let part = dir.join(format!("{name}.part"));
        let file =
            File::create(&part).map_err(|e| format!("cannot write {}: {e}", part.display()))?;
        Ok(Self {
            part,
            dest: dir.join(name),
            file,
            written: 0,
        })
    }
}

/// A partial file is never left behind when the session ends.
impl Drop for Puts {
    fn drop(&mut self) {
        for open in self.open.values() {
            let _ = std::fs::remove_file(&open.part);
        }
    }
}

/// A file name that is safe on every owner OS: the last path component, with
/// anything outside a plain set of characters replaced.
pub fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._- ".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(128)
        .collect();
    let clean = clean.trim_start_matches('.').trim().to_string();
    let stem = clean
        .split('.')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    match clean.as_str() {
        "" => "file".into(),
        _ if reserved => format!("_{clean}"),
        _ => clean,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_keeps_only_its_last_component_and_plain_characters() {
        for (given, safe) in [
            ("report.pdf", "report.pdf"),
            ("../../etc/passwd", "passwd"),
            ("C:\\Users\\pat\\notes.txt", "notes.txt"),
            ("..hidden", "hidden"),
            ("a:b*c?.txt", "a_b_c_.txt"),
            ("", "file"),
            ("..", "file"),
            ("con.txt", "_con.txt"),
            ("COM1", "_COM1"),
            ("compact.txt", "compact.txt"),
        ] {
            assert_eq!(safe_name(given), safe, "{given}");
        }
    }

    #[test]
    fn parts_become_one_file_only_on_the_last() {
        let mut puts = Puts::default();
        assert!(puts.take(1, "x.bin", 0, b"hello ", false).reply.is_none());
        let took = puts.take(1, "x.bin", 6, b"world", true);
        let Some(Frame::Placed { path, .. }) = took.reply else {
            panic!()
        };
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
        assert!(!PathBuf::from(format!("{path}.part")).exists());
        std::fs::remove_dir_all(PathBuf::from(&path).parent().unwrap()).unwrap();
    }

    #[test]
    fn a_part_out_of_place_refuses_the_file_once_and_leaves_nothing() {
        let mut puts = Puts::default();
        puts.take(2, "y.bin", 0, b"abc", false);
        let part = puts.open[&2].part.clone();
        assert!(matches!(
            puts.take(2, "y.bin", 9, b"d", false).reply,
            Some(Frame::Refused { .. })
        ));
        assert!(!part.exists());
        assert!(puts.take(2, "y.bin", 3, b"d", true).reply.is_none());
        assert!(matches!(
            puts.take(3, "z", 5, b"d", true).reply,
            Some(Frame::Refused { .. })
        ));
        std::fs::remove_dir_all(part.parent().unwrap()).unwrap();
    }
}
