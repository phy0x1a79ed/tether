//! The owner's own record of what happened on their machine.
//!
//! The session used to leave one file behind, the client itself, and the prompt
//! said so. It now leaves two, and the prompt says that instead — because the
//! sentence a person agrees to has to be the one that is true. The log sits
//! beside the client in the directory the launcher already made and already
//! prints, so it is still one directory to delete and one path to remember.
//!
//! # Why it hangs off the transcript rather than off the frames
//!
//! Every line the owner sees passes through one funnel in [`crate::ui`]: the
//! command that was asked for, its output, how it ended, what each person said,
//! and why the session stopped. Writing the log from there rather than from the
//! wire makes "the record is what you saw" true by construction rather than by
//! review, which is the claim the consent prompt is now making on its behalf.
//!
//! # The phrase cannot get in here
//!
//! [`Log::open`] takes a slot — a small public number the relay issued — and
//! never an invite code, a phrase or the facts struct the display holds. That
//! is deliberate and it is the same doctrine the operator's daemon uses for its
//! own log. It matters here because the owner's header line, drawn on screen
//! every second, contains the whole code: a logger that took the display's
//! state would put the credential in a file on the first frame.
//!
//! A session nobody consented to leaves nothing. Otherwise saying no would fill
//! a directory the owner was told they could simply delete.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// How large the record may grow before it stops growing.
///
/// A file nobody can open is not a record. No rotation: rotation multiplies the
/// files in the directory the owner is about to delete.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// The name beside the client. Short, because it is read aloud over a phone as
/// often as it is typed.
const NAME: &str = "tether-session.log";

pub struct Log {
    file: File,
    path: PathBuf,
    started: Instant,
    written: u64,
    full: bool,
}

impl Log {
    /// Open the record for a session, before anyone has agreed to one.
    ///
    /// Created rather than merely named, so that the path the prompt shows is a
    /// file that demonstrably exists by the time the question is asked.
    pub fn open(slot: u32) -> Result<Self, String> {
        let dir = beside_the_client()?;
        let mut path = dir.join(NAME);
        let mut n = 2;
        while path.exists() && n < 100 {
            path = dir.join(format!("tether-session-{n}.log"));
            n += 1;
        }

        let mut options = OpenOptions::new();
        options.create(true).append(true);
        // It carries output from this machine, and a temporary directory on a
        // shared box is not private by default. Windows keeps a per-user
        // temporary directory already, which is why this is one system's rule
        // rather than a runtime check.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(|e| format!("{} ({e})", dir.display()))?;

        let mut log = Self {
            file,
            path,
            started: Instant::now(),
            written: 0,
            full: false,
        };
        log.raw(&format!("tether session log\nstarted   {}\nslot      {slot}\n", stamp()));
        Ok(log)
    }

    /// Say who is on the other end, once the greeting has arrived.
    pub fn peer(&mut self, who: &str, host: &str, os: &str, build: &str, relay: &str) {
        self.raw(&format!(
            "who       {who} on {host} ({os}), build {build}\nthrough   {relay}\n----\n"
        ));
    }

    /// One line of the transcript, exactly as the owner saw it.
    pub fn line(&mut self, text: &str) {
        let seconds = self.started.elapsed().as_secs();
        self.raw(&format!("+{:02}:{:02}  {text}\n", seconds / 60, seconds % 60));
    }

    /// Close the record with how long it ran.
    pub fn finish(&mut self) {
        let seconds = self.started.elapsed().as_secs();
        self.raw(&format!(
            "----\nended     {} after {}m{}s\n",
            stamp(),
            seconds / 60,
            seconds % 60
        ));
        let _ = self.file.flush();
    }

    /// Throw the record away, because there was no session to record.
    pub fn discard(self) {
        let path = self.path;
        drop(self.file);
        let _ = std::fs::remove_file(path);
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn raw(&mut self, text: &str) {
        if self.full {
            return;
        }
        if self.written >= MAX_BYTES {
            self.full = true;
            let _ = self.file.write_all(
                b"----\nthis log reached its size limit; the rest of the session \
                  is on screen only\n",
            );
            let _ = self.file.flush();
            return;
        }
        // One write per line and a flush after it. A session is human-paced
        // except for a terminal's output, and a record that survives the
        // machine being switched off is worth a syscall.
        if self.file.write_all(text.as_bytes()).is_ok() {
            self.written += text.len() as u64;
            let _ = self.file.flush();
        }
    }
}

/// The directory the launcher made for the client, or the temporary directory.
///
/// The client and its log in one place is the story the launcher already tells:
/// it prints the path before running anything and says to delete it afterwards.
fn beside_the_client() -> Result<PathBuf, String> {
    // Named outright for the same reason `TETHER_SHELL` is: the default is the
    // right answer for the person the launcher put here, and somebody driving
    // this deliberately may want it somewhere they choose.
    if let Some(dir) = std::env::var_os("TETHER_LOG_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            return Ok(dir);
        }
        return Err(format!("{} is not a directory", dir.display()));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir.is_dir() {
                return Ok(dir.to_path_buf());
            }
        }
    }
    let dir = std::env::temp_dir();
    if dir.is_dir() {
        return Ok(dir);
    }
    Err("there is nowhere to write it".into())
}

/// The current time, as a date a person can read.
fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = civil(secs / 86_400);
    let rest = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Days since the epoch to a calendar date.
///
/// Howard Hinnant's `civil_from_days`, which is the whole of what a date crate
/// would be carried here for. The owner downloads this binary over their own
/// connection, so a dependency for one line of a header is a cost they pay.
fn civil(days: u64) -> (i64, u32, u32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_calendar_agrees_with_dates_anyone_can_check() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_723), (2024, 1, 1));
        // A leap day, which is the whole reason this is not arithmetic on 365.
        assert_eq!(civil(19_782), (2024, 2, 29));
        assert_eq!(civil(20_544), (2026, 4, 1));
    }

    #[test]
    fn a_record_reads_as_a_session_and_names_no_secret() {
        let dir = std::env::temp_dir().join(format!("tether-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(NAME);
        let _ = std::fs::remove_file(&path);

        // Opened by hand rather than through `open`, which finds the directory
        // the running binary is in — under `cargo test` that is the test runner.
        let mut log = Log {
            file: File::create(&path).unwrap(),
            path: path.clone(),
            started: Instant::now(),
            written: 0,
            full: false,
        };
        log.peer("awm as tony", "altair", "linux", "9fda69b", "example/tether");
        log.line("$ df -h");
        log.line("  done");
        log.line("them: all finished");
        log.finish();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("awm as tony on altair (linux)"), "{text}");
        assert!(text.contains("+00:00  $ df -h"), "{text}");
        assert!(text.contains("them: all finished"), "{text}");
        assert!(text.contains("ended     "), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The structural claim in the module docs, as a compile-time fact rather
    /// than a promise: `open` takes a number.
    #[test]
    fn the_only_thing_a_log_is_told_about_a_session_is_its_slot() {
        fn _signature(slot: u32) -> Result<Log, String> {
            Log::open(slot)
        }
    }
}
