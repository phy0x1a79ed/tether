//! `tether 7 anchor kettle` — the whole interface the owner ever sees.
//!
//! Plain tokens, nothing to quote, no punctuation inside the code. The launcher
//! passes whatever followed `bash -s` straight through, so a longer code costs
//! nothing here and a shorter one was never the point: the code is read aloud,
//! and reading aloud is what it is shaped for.
//!
//! # What is left behind
//!
//! This file, wherever the launcher put it, and nothing else. No key is written
//! anywhere: every key in the session is derived in memory from the words and
//! is gone when the process is. Nothing is added to `PATH`, no login item, no
//! launch agent, no service, no cron. It does not survive a reboot because
//! nothing asked it to.

use std::process::ExitCode;

use tether_link::{Link, Relay};
use tether_owner::session::{self, Outcome};
use tether_proto::frame::{Hello, Role, PROTOCOL_VERSION};
use tether_proto::invite::InviteCode;

const USAGE: &str = "\
tether — let someone you trust run commands on this machine, while you watch.

    tether <slot> <word> <word>

The slot and the words are the invite code you were given, typed as plain
words. Nothing is installed and nothing survives the session. You will be
asked here, at this keyboard, before anyone gets in.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--version") {
        println!("tether {} (protocol v{PROTOCOL_VERSION})", build());
        return ExitCode::SUCCESS;
    }

    let code = match InviteCode::parse(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let relay = match Relay::from_env() {
        Ok(relay) => relay,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    // One thread is enough for the session itself; the pty's reader and writer
    // are blocking and get their own.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("could not start: {e}");
            return ExitCode::from(2);
        }
    };

    let outcome = runtime.block_on(connect(relay, code));
    match &outcome {
        Outcome::Cut(_) => {}
        other => eprintln!("{other}"),
    }
    ExitCode::from(outcome.code())
}

async fn connect(relay: Relay, code: InviteCode) -> Outcome {
    eprintln!("  reaching {relay} …");
    let mut link = match Link::dial_owner(&relay, &code).await {
        Ok(link) => link,
        Err(e) => return Outcome::Failed(e.to_string()),
    };

    let me = me();
    let operator = match session::greet(&mut link, &relay, &code, &me).await {
        Ok(operator) => operator,
        Err(outcome) => return outcome,
    };

    session::run(link, operator, relay.to_string(), code.to_string()).await
}

/// Who this machine says it is, for the operator's side to show.
fn me() -> Hello {
    Hello {
        version: PROTOCOL_VERSION,
        role: Role::Owner,
        // Three names for one thing. Unix sets the first two and Windows sets
        // the third, so the list is the portability rather than a fallback
        // chain within one system.
        who: std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "someone".into()),
        host: hostname(),
        os: std::env::consts::OS.to_string(),
        build: build(),
    }
}

/// Stamped in by the build that produced this binary.
///
/// A download the owner ran last month and a fresh one are the same file name
/// and the same behaviour right up until they are not, so both ends show which
/// build they came from rather than leaving a version mismatch to be deduced.
fn build() -> String {
    option_env!("TETHER_BUILD")
        .unwrap_or("unstamped")
        .to_string()
}

/// The machine's name, asked of the machine.
///
/// A subprocess rather than a crate: `hostname` is on every system this runs
/// on, it is asked once, and the alternative is a dependency carried into the
/// owner's download for one string.
///
/// Windows answers it from the environment instead, which is the same name and
/// costs no process at all. Unix does not set that variable, so the check is
/// free there.
fn hostname() -> String {
    if let Ok(name) = std::env::var("COMPUTERNAME") {
        if !name.trim().is_empty() {
            return name.trim().to_string();
        }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this machine".into())
}
