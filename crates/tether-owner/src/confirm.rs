//! Consent from a machine with no terminal: shown on one run, given on the next.
//!
//! The prompt reads the keyboard, and a shell with no terminal has none. An
//! agent in a sandbox is the usual case. There the answer has always been no,
//! and that stays the first run's answer. What changes is that the first run
//! also mints a confirmation, three words, and prints them under the prompt.
//! Running the client again with `--agree` and those words is the yes.
//!
//! # Why this is not the bypass `consent` forbids
//!
//! A flag that answers the prompt is a flag the operator can put in the
//! one-liner they read out, and then nobody was asked. `--agree` cannot be put
//! there. Its words do not exist until the first run has shown the whole prompt
//! on this machine, and they never leave it: they are not sent to the relay or
//! to the operator. So whoever types them has read the screen the prompt was
//! on. A first run that already carries `--agree` finds no record and refuses,
//! and so does a second run with the wrong words, for another code, through
//! another relay, or for an operator other than the one the prompt named.
//!
//! # The record
//!
//! The words are needed again by a different process, so they go to a file in
//! between: one per user, in a directory only that user can enter, holding a
//! hash of the words and never the words or the phrase. It lives for
//! [`LIFETIME`] at most, which is the life of an invite, and is deleted the
//! moment it is used. Only the latest first run counts, so there is one file
//! rather than one per code.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use tether_link::Relay;
use tether_proto::frame::Hello;
use tether_proto::invite::InviteCode;
use tether_proto::words::WORDS;

/// How many words the confirmation is.
pub const WORDS_IN_CONFIRMATION: usize = 3;

/// How long a first run's confirmation stays good. An invite lives five
/// minutes from when it was minted, so a confirmation never outlives the slot
/// it was for.
pub const LIFETIME: Duration = Duration::from_secs(300);

/// What the first run showed, kept for the second run to check against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    slot: u32,
    relay: String,
    who: String,
    host: String,
    build: String,
    hash: String,
    expires: u64,
}

/// Why a second run was refused. Each says which check failed, because the
/// person reading it has to know whether to retype or to start over.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error(
        "--agree confirms a prompt this machine already showed, and none is waiting \
         here — run the command once without --agree first"
    )]
    NothingPending,
    #[error("the confirmation shown here has expired — run the command again without --agree")]
    Expired,
    #[error("the confirmation waiting here is for a different code — run this code once without --agree first")]
    OtherCode,
    #[error("the confirmation waiting here was for a session through {0}, not this one")]
    OtherRelay(String),
    #[error(
        "those are not the words this machine showed — check them against the prompt above it"
    )]
    WrongWords,
    #[error(
        "the operator now asking is {now}, and the prompt you confirmed named {then} — \
         run the command again without --agree to see who is asking"
    )]
    OtherOperator { now: String, then: String },
    #[error("--agree takes the {WORDS_IN_CONFIRMATION} words the prompt printed, after the code")]
    Malformed,
    #[error("the confirmation waiting here could not be read: {0}")]
    Unreadable(String),
}

impl Pending {
    /// Record what this machine is about to show, under freshly minted words.
    pub fn new(code: &InviteCode, relay: &Relay, operator: &Hello) -> (Self, Vec<&'static str>) {
        let words = mint();
        let pending = Self {
            slot: code.slot.get(),
            relay: relay.to_string(),
            who: operator.who.clone(),
            host: operator.host.clone(),
            build: operator.build.clone(),
            hash: digest(code.slot.get(), &words),
            expires: now() + LIFETIME.as_secs(),
        };
        (pending, words)
    }

    /// Write the record where the second run will look, replacing any earlier one.
    pub fn keep(&self) -> Result<PathBuf, String> {
        let path = file()?;
        let text = format!(
            "slot {}\nrelay {}\nwho {}\nhost {}\nbuild {}\nhash {}\nexpires {}\n",
            self.slot, self.relay, self.who, self.host, self.build, self.hash, self.expires
        );
        let _ = std::fs::remove_file(&path);
        write_private(&path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path)
    }

    /// The record a first run left, if any.
    pub fn load() -> Result<Self, Refusal> {
        let path = file().map_err(Refusal::Unreadable)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Refusal::NothingPending)
            }
            Err(e) => return Err(Refusal::Unreadable(format!("{}: {e}", path.display()))),
        };
        parse(&text).ok_or_else(|| Refusal::Unreadable(format!("{} is damaged", path.display())))
    }

    /// Everything that can be checked before anything leaves this machine.
    pub fn check(&self, code: &InviteCode, relay: &Relay, words: &[String]) -> Result<(), Refusal> {
        if now() >= self.expires {
            forget();
            return Err(Refusal::Expired);
        }
        if self.slot != code.slot.get() {
            return Err(Refusal::OtherCode);
        }
        if self.relay != relay.to_string() {
            return Err(Refusal::OtherRelay(self.relay.clone()));
        }
        let words: Vec<&str> = words.iter().map(|w| w.trim()).collect();
        let given = digest(code.slot.get(), &words);
        if given != self.hash {
            return Err(Refusal::WrongWords);
        }
        Ok(())
    }

    /// The last check, once the operator has said who they are. Spends the
    /// record either way: a confirmation is good for one session, and one that
    /// met the wrong operator should not wait around for the right one.
    pub fn redeem(self, operator: &Hello) -> Result<(), Refusal> {
        forget();
        self.named(operator)
    }

    fn named(&self, operator: &Hello) -> Result<(), Refusal> {
        if (
            operator.who.as_str(),
            operator.host.as_str(),
            operator.build.as_str(),
        ) != (self.who.as_str(), self.host.as_str(), self.build.as_str())
        {
            return Err(Refusal::OtherOperator {
                now: format!("{} on {}", operator.who, operator.host),
                then: format!("{} on {}", self.who, self.host),
            });
        }
        Ok(())
    }
}

/// Delete any record. Nothing to do if there is none.
pub fn forget() {
    if let Ok(path) = file() {
        let _ = std::fs::remove_file(path);
    }
}

fn mint() -> Vec<&'static str> {
    let mut seed = [0u8; 2 * WORDS_IN_CONFIRMATION];
    getrandom::getrandom(&mut seed).expect("the OS randomness source failed");
    seed.chunks(2)
        .map(|pair| WORDS[(u16::from_be_bytes([pair[0], pair[1]]) as usize) % WORDS.len()])
        .collect()
}

/// Tied to the slot, so the same words typed for another code do not match.
fn digest(slot: u32, words: &[&str]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"tether-confirm\0");
    hash.update(slot.to_be_bytes());
    for word in words {
        hash.update(word.to_ascii_lowercase().as_bytes());
        hash.update([0]);
    }
    hex::encode(hash.finalize())
}

fn parse(text: &str) -> Option<Pending> {
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
            .map(str::to_string)
    };
    Some(Pending {
        slot: field("slot")?.parse().ok()?,
        relay: field("relay")?,
        who: field("who")?,
        host: field("host")?,
        build: field("build")?,
        hash: field("hash")?,
        expires: field("expires")?.parse().ok()?,
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The record's path, making its directory if need be.
///
/// The temporary directory on Unix is shared by every user, so the directory
/// is named for this one and must be theirs and closed to everyone else. A
/// directory someone else made under that name is refused rather than used:
/// they could otherwise plant a record and answer the prompt for this user.
#[cfg(unix)]
fn file() -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let dir = std::env::temp_dir().join(format!("tether-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    }
    let meta = std::fs::symlink_metadata(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() || meta.uid() != uid || meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{} is not a private directory of this user's, so nothing is kept there",
            dir.display()
        ));
    }
    Ok(dir.join("confirm"))
}

/// Windows gives each user a temporary directory of their own.
#[cfg(not(unix))]
fn file() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("tether");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir.join("confirm"))
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tether_proto::frame::{Role, PROTOCOL_VERSION};

    fn operator(who: &str) -> Hello {
        Hello {
            version: PROTOCOL_VERSION,
            role: Role::Operator,
            who: who.into(),
            host: "altair".into(),
            os: "linux".into(),
            build: "9fda69b".into(),
            resume: false,
        }
    }

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn the_words_shown_are_the_words_that_pass() {
        let relay = Relay::parse("https://example.test/tether").unwrap();
        let code = InviteCode::parse(&["acre", "anchor", "kettle"]).unwrap();
        let (pending, words) = Pending::new(&code, &relay, &operator("awm as tony"));
        assert_eq!(words.len(), WORDS_IN_CONFIRMATION);
        assert_eq!(pending.check(&code, &relay, &strings(&words)), Ok(()));
        // Typed in capitals, which is the same words.
        let loud: Vec<String> = words.iter().map(|w| w.to_uppercase()).collect();
        assert_eq!(pending.check(&code, &relay, &loud), Ok(()));
    }

    #[test]
    fn each_wrong_detail_is_named() {
        let relay = Relay::parse("https://example.test/tether").unwrap();
        let code = InviteCode::parse(&["acre", "anchor", "kettle"]).unwrap();
        let (pending, words) = Pending::new(&code, &relay, &operator("awm as tony"));
        let words = strings(&words);

        let mut wrong = words.clone();
        wrong[2] = if wrong[2] == "abacus" {
            "abbey"
        } else {
            "abacus"
        }
        .into();
        assert_eq!(
            pending.check(&code, &relay, &wrong),
            Err(Refusal::WrongWords)
        );

        let other = InviteCode::parse(&["ability", "anchor", "kettle"]).unwrap();
        assert_eq!(
            pending.check(&other, &relay, &words),
            Err(Refusal::OtherCode)
        );

        let elsewhere = Relay::parse("https://other.test/tether").unwrap();
        assert!(matches!(
            pending.check(&code, &elsewhere, &words),
            Err(Refusal::OtherRelay(_))
        ));

        let stale = Pending {
            expires: now() - 1,
            ..pending.clone()
        };
        assert_eq!(stale.check(&code, &relay, &words), Err(Refusal::Expired));
    }

    #[test]
    fn only_the_operator_the_prompt_named_is_let_in() {
        let relay = Relay::parse("https://example.test/tether").unwrap();
        let code = InviteCode::parse(&["acre", "anchor", "kettle"]).unwrap();
        let (pending, _) = Pending::new(&code, &relay, &operator("awm as tony"));
        assert_eq!(pending.named(&operator("awm as tony")), Ok(()));
        assert!(matches!(
            pending.named(&operator("awm as someone else")),
            Err(Refusal::OtherOperator { .. })
        ));
        let rebuilt = Hello {
            build: "0000000".into(),
            ..operator("awm as tony")
        };
        assert!(pending.named(&rebuilt).is_err());
    }

    #[test]
    fn the_record_holds_neither_the_words_nor_the_phrase() {
        let relay = Relay::parse("https://example.test/tether").unwrap();
        let code = InviteCode::parse(&["acre", "anchor", "kettle"]).unwrap();
        let (pending, words) = Pending::new(&code, &relay, &operator("awm as tony"));
        let text = format!("{pending:?}");
        for word in words.iter().chain(["anchor", "kettle"].iter()) {
            assert!(!text.contains(&format!(" {word}")), "{word} in {text}");
        }
        // And it reads back as what was written.
        let written = format!(
            "slot {}\nrelay {}\nwho {}\nhost {}\nbuild {}\nhash {}\nexpires {}\n",
            pending.slot,
            pending.relay,
            pending.who,
            pending.host,
            pending.build,
            pending.hash,
            pending.expires
        );
        assert_eq!(parse(&written), Some(pending));
    }
}
