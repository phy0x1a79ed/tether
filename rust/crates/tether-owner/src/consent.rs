//! The gate. Nothing happens on this machine until a person here says yes.
//!
//! Three properties matter more than anything else in this crate, and all three
//! are structural rather than careful:
//!
//! **It reads from the keyboard, not from standard input.** The launcher
//! arrives through a pipe, so standard input is the tail of a launcher script.
//! A prompt that read from it would be answered by whatever the pipe had left,
//! which is to say answered by the operator. Opening the keyboard directly is
//! what makes the answer come from the person. Two systems spell it two ways
//! and [`keyboard`] holds both: `/dev/tty` on Unix, `CONIN$` on Windows.
//!
//! **No keyboard means no.** If it cannot be opened there is nobody here to
//! ask, and the answer to a question nobody heard is no. This is why the
//! failure direction is stated as a rule and not left to a default.
//!
//! **The bypass is a build feature, not a flag.** A flag that exists eventually
//! gets used, and an unattended connect path is the one thing this tool refuses
//! to have. Under `test-consent-bypass` the prompt answers itself; in every
//! other build the code below does not exist to be reached. `tests/consent.rs`
//! reads the shipped binary and checks that [`BYPASS_MARKER`]'s bytes are not
//! in it, so the claim is verified rather than asserted.

use std::fmt;

use tether_proto::frame::Hello;
use tether_proto::invite::InviteCode;

use tether_link::Relay;

/// The string the bypass path prints, and the thing a shipped binary is checked
/// for the absence of.
///
/// It exists so that "the release build contains no bypass" is a test rather
/// than a promise. Do not write these bytes anywhere else in the crate.
#[cfg(feature = "test-consent-bypass")]
pub const BYPASS_MARKER: &str = "tether-consent-bypass-compiled-into-this-build";

#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Allowed,
    /// Carries what to tell the other end, which is also what the owner just
    /// saw. A session that ends with no reason on either screen is the thing
    /// that makes people distrust a tool.
    Refused(&'static str),
}

impl Answer {
    pub fn allowed(&self) -> bool {
        matches!(self, Answer::Allowed)
    }
}

/// Everything the person at the keyboard is owed before they answer.
pub struct Ask<'a> {
    /// Who is asking, as a concrete identity and host. "Allow an operator?"
    /// tells the person nothing they can act on.
    pub operator: &'a Hello,
    pub relay: &'a Relay,
    pub code: &'a InviteCode,
    /// Where the record of this session is being written, or why there will
    /// not be one. Named before the answer because it is part of what is being
    /// agreed to: this prompt used to say nothing survives the session, and a
    /// log on their disk is precisely a thing that survives it.
    pub log: Result<&'a std::path::Path, &'a str>,
}

impl fmt::Display for Ask<'_> {
    /// The prompt itself, as text, so a test can read what a person would.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let op = self.operator;
        writeln!(f)?;
        writeln!(f, "  tether — someone is asking to run commands here")?;
        writeln!(f)?;
        writeln!(f, "  who      {} on {} ({})", op.who, op.host, op.os)?;
        writeln!(f, "  through  {}", self.relay)?;
        writeln!(f, "  code     {}", self.code)?;
        writeln!(f, "  build    {}", op.build)?;
        match self.log {
            Ok(path) => writeln!(f, "  log      {}", path.display())?,
            Err(why) => writeln!(f, "  log      none — {why}")?,
        }
        writeln!(f)?;
        writeln!(
            f,
            "  They will run commands on this machine as you, and you will"
        )?;
        writeln!(
            f,
            "  see everything they do while it happens. Nothing is installed."
        )?;
        match self.log {
            Ok(_) => {
                writeln!(f, "  What is kept is the log named above — everything you see,")?;
                writeln!(f, "  on this machine only, for you to read or delete afterwards.")?;
            }
            Err(_) => {
                writeln!(f, "  Nothing will be kept: there will be no record of this")?;
                writeln!(f, "  beyond what is on your screen while it happens.")?;
            }
        }
        writeln!(f)?;
        write!(f, "  Let them in? [y/N] ")
    }
}

/// Ask, and mean it.
pub fn ask(ask: &Ask<'_>) -> Answer {
    #[cfg(feature = "test-consent-bypass")]
    {
        let _ = ask;
        eprintln!("{BYPASS_MARKER}");
        Answer::Allowed
    }

    #[cfg(not(feature = "test-consent-bypass"))]
    {
        use std::io::{BufRead, BufReader, Write};

        let mut err = std::io::stderr();
        let _ = write!(err, "{ask}");
        let _ = err.flush();

        // The keyboard, never standard input: see the module docs.
        let Ok(tty) = keyboard() else {
            let _ = writeln!(
                err,
                "\n  There is no terminal here to ask, so the answer is no."
            );
            return Answer::Refused("no terminal to ask at the owner's end");
        };
        let mut line = String::new();
        if BufReader::new(tty).read_line(&mut line).is_err() {
            return Answer::Refused("the owner's terminal could not be read");
        }
        let _ = writeln!(err);
        if is_yes(&line) {
            Answer::Allowed
        } else {
            Answer::Refused("the owner said no")
        }
    }
}

/// The keyboard of the person at this machine, opened directly.
///
/// Unix opens the controlling terminal. Windows has no such path and names the
/// same thing `CONIN$`, the console attached to this process — which is a
/// different handle from standard input and is not redirected when standard
/// input is. It has to be opened for writing as well as reading, and a
/// read-only open of it fails, which is the one detail here that is not
/// obvious from the name.
///
/// A machine that is neither refuses, which is the same answer it would give
/// with nobody at the keyboard.
#[cfg(unix)]
#[allow(dead_code)] // unused under `test-consent-bypass`, where nothing asks
fn keyboard() -> std::io::Result<std::fs::File> {
    std::fs::File::open("/dev/tty")
}

#[cfg(windows)]
#[allow(dead_code)] // unused under `test-consent-bypass`, where nothing asks
fn keyboard() -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONIN$")
}

#[cfg(not(any(unix, windows)))]
#[allow(dead_code)] // unused under `test-consent-bypass`, where nothing asks
fn keyboard() -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no keyboard on this system",
    ))
}

/// Only an explicit yes is a yes.
///
/// An empty line is the common case — somebody leaning on return — and it must
/// not mean consent. Anything unrecognised is no for the same reason.
pub fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tether_proto::frame::Role;

    fn hello() -> Hello {
        Hello {
            version: tether_proto::frame::PROTOCOL_VERSION,
            role: Role::Operator,
            who: "awm as tony".into(),
            host: "altair".into(),
            os: "linux".into(),
            build: "9fda69b".into(),
        }
    }

    #[test]
    fn nothing_but_yes_is_yes() {
        for yes in ["y", "Y", "yes", "YES", " yes \n", "Yes"] {
            assert!(is_yes(yes), "{yes:?}");
        }
        for no in [
            "", "\n", "n", "no", "ok", "sure", "yep", "1", "true", "yess",
        ] {
            assert!(!is_yes(no), "{no:?}");
        }
    }

    #[test]
    fn the_prompt_names_the_five_things_worth_knowing() {
        let relay = Relay::parse("https://nexus.tony-xy-liu.com/tether").unwrap();
        let code = InviteCode::parse(&["acre", "anchor", "kettle"]).unwrap();
        let operator = hello();
        let log = std::path::Path::new("/tmp/tether.4kPq2x/tether-session.log");
        let text = Ask {
            operator: &operator,
            relay: &relay,
            code: &code,
            log: Ok(log),
        }
        .to_string();

        // Who is being let in, named concretely rather than as a role.
        assert!(text.contains("awm as tony"));
        assert!(text.contains("altair"));
        // Which relay the session crosses.
        assert!(text.contains("nexus.tony-xy-liu.com/tether"));
        // The code being redeemed.
        assert!(text.contains("acre anchor kettle"));
        // Where the record of it will be, verbatim, so they can find the file
        // by reading the prompt rather than by hunting for it afterwards.
        assert!(
            text.contains("/tmp/tether.4kPq2x/tether-session.log"),
            "{text}"
        );
        // And that the default is no.
        assert!(text.contains("[y/N]"));
    }

    #[test]
    fn the_prompt_says_what_the_operator_will_be_able_to_do() {
        let relay = Relay::parse("https://example.test").unwrap();
        let code = InviteCode::parse(&["ability", "anchor", "kettle"]).unwrap();
        let operator = hello();
        let log = std::path::Path::new("/tmp/tether.x/tether-session.log");
        let text = Ask {
            operator: &operator,
            relay: &relay,
            code: &code,
            log: Ok(log),
        }
        .to_string();
        assert!(text.contains("run commands on this machine"));
        assert!(text.contains("see everything they do"));
        // The sentence this prompt used to end on. A log on their disk makes
        // it untrue, and it is the sentence they are agreeing to.
        assert!(!text.contains("nothing survives"), "{text}");
    }

    #[test]
    fn the_prompt_says_so_when_there_will_be_no_record() {
        let relay = Relay::parse("https://example.test").unwrap();
        let code = InviteCode::parse(&["ability", "anchor", "kettle"]).unwrap();
        let operator = hello();
        let text = Ask {
            operator: &operator,
            relay: &relay,
            code: &code,
            log: Err("/tmp is read-only"),
        }
        .to_string();
        assert!(text.contains("log      none — /tmp is read-only"), "{text}");
        assert!(text.contains("Nothing will be kept"), "{text}");
        assert!(!text.contains("What is kept is the log"), "{text}");
    }
}
