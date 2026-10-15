//! `tether-operator` — the daemon the gateway adapter keeps alive.
//!
//! It takes no arguments. Everything it needs is in the environment, because
//! the thing that starts it is the adapter, and the adapter is what knows which
//! relay this fleet uses and where this host keeps its state.
//!
//! It exits on a signal and it exits when its parent dies, and it leaves its
//! socket file behind neither way. Nothing it opens survives it: a session is a
//! socket held by a task in this process, so ending the process ends every
//! session, which is exactly the property the tool promises.

use std::process::ExitCode;

use tether_operator::{log, Config};

fn main() -> ExitCode {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(e) => {
            log::warn(format_args!("{e}"));
            return ExitCode::from(2);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            log::warn(format_args!("could not start: {e}"));
            return ExitCode::from(2);
        }
    };

    runtime.block_on(run(config))
}

async fn run(config: Config) -> ExitCode {
    let relay = config.relay.to_string();
    let can_invite = config.issue_token.is_some();
    let running = match tether_operator::start(config).await {
        Ok(running) => running,
        Err(e) => {
            log::warn(format_args!("{e}"));
            return ExitCode::from(2);
        }
    };

    log::info(format_args!(
        "listening on {} for {relay}",
        running.socket().display()
    ));
    if !can_invite {
        // Reported rather than fatal: `status` is the verb that explains this,
        // and a daemon that exited here could not answer it.
        log::warn(format_args!(
            "no {} in the environment — this host can hold no sessions open",
            tether_operator::config::TOKEN_ENV
        ));
    }

    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(term) => term,
        Err(e) => {
            log::warn(format_args!("could not listen for SIGTERM: {e}"));
            running.stop();
            return ExitCode::from(2);
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }

    log::info(format_args!("stopping; every live session ends with us"));
    running.stop();
    ExitCode::SUCCESS
}
