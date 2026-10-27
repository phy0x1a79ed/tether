//! `tether acre anchor kettle` — the whole interface the owner ever sees.
//!
//! Plain tokens, nothing to quote, no punctuation inside the code. The launcher
//! passes whatever followed `sh -s` straight through, so a longer code costs
//! nothing here and a shorter one was never the point: the code is read aloud,
//! and reading aloud is what it is shaped for.
//!
//! # What is left behind
//!
//! This file, wherever the launcher put it, and a record of the session beside
//! it. Both are named in the prompt before the owner answers, because what is
//! kept is part of what is being agreed to. Nothing else: no key is written
//! anywhere, every key in the session being derived in memory from the words
//! and gone when the process is, and nothing is added to `PATH`, no login item,
//! no launch agent, no service, no cron.
//!
//! Nothing here waits for a reboot to disappear, but do not rely on the reboot.
//! A temporary directory is where systems put things they clear on their own
//! schedule, and Windows does not clear its one at all. The launcher prints the
//! directory; deleting it is one command and needs no waiting.

use std::process::ExitCode;

use tether_link::resume::OwnerRejoin;
use tether_link::{Carry, Link, Relay, Resumable};
use tether_owner::confirm::{Pending, Refusal};
use tether_owner::session::{self, Outcome};
use tether_proto::frame::{Hello, Role, PROTOCOL_VERSION};
use tether_proto::invite::InviteCode;

const USAGE: &str = "\
tether — let someone you trust run commands on this machine, while you watch.

    tether <word> <word> <word>
    tether <word> <word> <word> --agree <word> <word> <word>

The words are the invite code you were given, in the order you were given
them. Nothing is installed. A record of the session is written where the
prompt will name, for you to keep or delete. You will be asked here, at this
keyboard, before anyone gets in.

With no terminal here to answer at, the prompt is printed with three more
words, and running the same command again with `--agree` and those words is
the yes. That works once, for that code and that operator, for five minutes.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--version") {
        // The launcher reads the platform off this line, so a binary that runs
        // under a translator or an emulator still says what it was built for.
        println!(
            "tether {} (protocol v{PROTOCOL_VERSION}) for {}-{}",
            build(),
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        return ExitCode::SUCCESS;
    }

    let (code, agree) = match args.iter().position(|a| a == "--agree") {
        Some(at) => (&args[..at], Some(&args[at + 1..])),
        None => (&args[..], None),
    };
    // Anything that goes wrong this badly is said on the screen the owner was
    // left with, rather than on the full-screen display it takes down.
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tether_owner::ui::release();
        report(info);
    }));

    let code = match InviteCode::parse(code) {
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

    // Everything a confirmation can be checked against without the network is
    // checked here, so a wrong one never spends a pairing from the slot.
    let agreed = match agree {
        None => None,
        Some(words) => match agreement(words, &code, &relay) {
            Ok(pending) => Some(pending),
            Err(why) => {
                eprintln!("not connected: {why}");
                return ExitCode::from(2);
            }
        },
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

    let outcome = runtime.block_on(connect(relay, code, agreed));
    match &outcome {
        Outcome::Cut(_) => {}
        other => eprintln!("{other}"),
    }
    ExitCode::from(outcome.code())
}

/// A second run's `--agree`, checked against what the first run left.
fn agreement(words: &[String], code: &InviteCode, relay: &Relay) -> Result<Pending, String> {
    if tether_owner::consent::keyboard_here() {
        return Err(
            "--agree is for a machine with no terminal to answer at, and this one \
                    has one — run the command without --agree and answer the prompt"
                .into(),
        );
    }
    if words.len() != tether_owner::confirm::WORDS_IN_CONFIRMATION {
        return Err(Refusal::Malformed.to_string());
    }
    let pending = Pending::load().map_err(|e| e.to_string())?;
    pending
        .check(code, relay, words)
        .map_err(|e| e.to_string())?;
    Ok(pending)
}

async fn connect(relay: Relay, code: InviteCode, agreed: Option<Pending>) -> Outcome {
    eprintln!("  reaching {relay} …");
    // Said before anything leaves, so a route the owner did not expect is on
    // the screen rather than inferred from a failure.
    match tether_link::Route::for_relay(&relay) {
        Ok(route) => {
            if let Some(line) = route.describe() {
                eprintln!("  {line}");
            }
        }
        Err(e) => return Outcome::Failed(e.to_string()),
    }
    if let (_, Some(note)) = tether_link::net::extra_roots() {
        eprintln!("  {note}");
    }
    let mut link = match Link::dial_owner(&relay, &code).await {
        Ok(link) => link,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    if let Some(why) = link.polled() {
        eprintln!("  carrying the session over plain requests, because {why}");
    }

    // Opened before the prompt, so the path it names is a file that already
    // exists rather than one this intends to make. It takes the slot and never
    // the code: the phrase is the credential and must not reach a file.
    let mut log = tether_owner::log::Log::open(code.slot.get())
        .map_err(|why| format!("no record could be written to {why}"));

    let me = me();
    let operator = match session::greet(&mut link, &relay, &code, &me, &mut log, agreed).await {
        Ok(operator) => operator,
        Err(outcome) => {
            // Nobody consented, so there was no session to have a record of.
            // Otherwise saying no would fill the directory the owner was just
            // told they could simply delete.
            if let Ok(log) = log {
                log.discard();
            }
            return outcome;
        }
    };

    // Resumable only when both ends can be. The same transport carries the
    // redials, so an owner behind a proxy that refused the socket goes straight
    // back to plain requests.
    let link = if operator.resume && me.resume {
        let carry = if link.polled().is_some() {
            Carry::Poll
        } else {
            Carry::from_env().unwrap_or(Carry::Either)
        };
        Resumable::owner(
            link,
            OwnerRejoin {
                relay: relay.clone(),
                slot: code.slot,
                carry,
            },
        )
    } else {
        Resumable::plain(link)
    };

    session::run(
        link,
        operator,
        relay.to_string(),
        code.to_string(),
        log.ok(),
    )
    .await
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
        resume: true,
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
