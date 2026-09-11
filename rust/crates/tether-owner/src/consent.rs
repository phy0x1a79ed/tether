//! The gate. Nothing happens on this machine until a person here says yes.
//!
//! Three properties matter more than anything else in this crate, and all three
//! are structural rather than careful:
//!
//! **It reads from the controlling terminal, not from standard input.** The
//! launcher arrives through a pipe, so standard input is the tail of a shell
//! script. A prompt that read from it would be answered by whatever the pipe
//! had left, which is to say answered by the operator. Opening `/dev/tty` is
//! what makes the answer come from the keyboard.
//!
//! **No terminal means no.** If `/dev/tty` cannot be opened there is nobody
//! here to ask, and the answer to a question nobody heard is no. This is why
//! the failure direction is stated as a rule and not left to a default.
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
        writeln!(f)?;
        writeln!(
            f,
            "  They will run commands on this machine as you, and you will"
        )?;
        writeln!(
            f,
            "  see everything they do while it happens. Nothing is installed"
        )?;
        writeln!(f, "  and nothing survives this session.")?;
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
        use std::fs::File;
        use std::io::{BufRead, BufReader, Write};

        let mut err = std::io::stderr();
        let _ = write!(err, "{ask}");
        let _ = err.flush();

        // The controlling terminal, never standard input: see the module docs.
        let Ok(tty) = File::open("/dev/tty") else {
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
    fn the_prompt_names_the_four_things_worth_knowing() {
        let relay = Relay::parse("https://nexus.tony-xy-liu.com/tether").unwrap();
        let code = InviteCode::parse(&["7", "anchor", "kettle"]).unwrap();
        let operator = hello();
        let text = Ask {
            operator: &operator,
            relay: &relay,
            code: &code,
        }
        .to_string();

        // Who is being let in, named concretely rather than as a role.
        assert!(text.contains("awm as tony"));
        assert!(text.contains("altair"));
        // Which relay the session crosses.
        assert!(text.contains("nexus.tony-xy-liu.com/tether"));
        // The code being redeemed.
        assert!(text.contains("7 anchor kettle"));
        // And that the default is no.
        assert!(text.contains("[y/N]"));
    }

    #[test]
    fn the_prompt_says_what_the_operator_will_be_able_to_do() {
        let relay = Relay::parse("https://example.test").unwrap();
        let code = InviteCode::parse(&["3", "anchor", "kettle"]).unwrap();
        let operator = hello();
        let text = Ask {
            operator: &operator,
            relay: &relay,
            code: &code,
        }
        .to_string();
        assert!(text.contains("run commands on this machine"));
        assert!(text.contains("see everything they do"));
    }
}
