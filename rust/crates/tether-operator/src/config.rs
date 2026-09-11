//! What the daemon needs to know, and where it refuses to guess.
//!
//! Everything arrives in the environment, because the adapter that spawns this
//! process is the thing that knows the answers: which relay this fleet uses,
//! which bearer reaches its authenticated half, and where the control socket
//! belongs on this host.
//!
//! # A missing bearer is reportable, not fatal
//!
//! The relay refuses to start without its issue token, and that is right: a
//! relay with no token would accept sessions from anyone. This daemon does the
//! opposite and starts anyway. Without the token it can mint nothing, but it
//! can still answer `status`, which is the one verb whose whole job is to say
//! why the others will not work. A daemon that exits on a missing variable
//! makes that answer unreachable exactly when it is needed.

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use tether_link::{Relay, RelayError};
use tether_proto::frame::{Hello, Role, PROTOCOL_VERSION};

/// Where the control socket goes when the adapter does not say.
pub const SOCKET_ENV: &str = "TETHER_CONTROL_SOCKET";
pub const TOKEN_ENV: &str = "AWM_TETHER_ISSUE_TOKEN";
pub const WHO_ENV: &str = "TETHER_WHO";

/// How long a command may run before the daemon stops waiting for it.
///
/// Under the manifest's 600-second ceiling by a wide margin, so a command that
/// overruns comes back as a reply that says so rather than as a control-socket
/// timeout, which tells the caller nothing about whether the command is still
/// running on the owner's machine.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(480);

/// How much of one task's output the daemon keeps, per stream.
///
/// The owner sees all of it either way — this is a cap on what is quoted back
/// into a reply, not on what runs. A command that prints a gigabyte should
/// truncate the answer, not end the daemon.
pub const MAX_CAPTURE: usize = 512 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    pub relay: Relay,
    /// The bearer for the relay's authenticated half. `None` means this host
    /// was never given one and can mint nothing.
    pub issue_token: Option<String>,
    pub socket: PathBuf,
    pub who: String,
    pub host: String,
    pub build: String,
}

impl Config {
    pub fn from_env() -> Result<Self, RelayError> {
        let user = env::var("USER")
            .or_else(|_| env::var("LOGNAME"))
            .unwrap_or_else(|_| "someone".into());
        Ok(Self {
            relay: Relay::from_env()?,
            issue_token: env::var(TOKEN_ENV)
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
            socket: env::var(SOCKET_ENV)
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/tmp/tether-operator.sock")),
            who: env::var(WHO_ENV)
                .ok()
                .map(|w| w.trim().to_string())
                .filter(|w| !w.is_empty())
                .unwrap_or_else(|| format!("awm as {user}")),
            host: hostname(),
            build: option_env!("TETHER_BUILD")
                .unwrap_or("unstamped")
                .to_string(),
        })
    }

    /// Who this end says it is, shown on the owner's screen before they answer.
    ///
    /// The whole reason this carries a name and a host is that "allow an
    /// operator?" gives the person at the keyboard nothing to decide on.
    pub fn hello(&self) -> Hello {
        Hello {
            version: PROTOCOL_VERSION,
            role: Role::Operator,
            who: self.who.clone(),
            host: self.host.clone(),
            os: std::env::consts::OS.to_string(),
            build: self.build.clone(),
        }
    }
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this machine".into())
}
