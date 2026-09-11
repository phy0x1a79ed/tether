//! Run the relay until something tells it to stop.
//!
//! Stopping is the whole of the cleanup story. Every session lives in memory,
//! so the process exiting ends all of them and there is nothing to reconcile on
//! the way back up. A relay that could resume a session across a restart would
//! be a relay with a session on disk, and that is a thing this tool refuses to
//! have.

use std::process::ExitCode;

use tether_relay::config::Config;
use tether_relay::{log, start};

#[tokio::main]
async fn main() -> ExitCode {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(e) => {
            log::warn(format_args!("{e}"));
            return ExitCode::FAILURE;
        }
    };

    let build = config.build.clone();
    let assets = config.assets.clone();
    let running = match start(config).await {
        Ok(running) => running,
        Err(e) => {
            log::warn(format_args!("could not bind: {e}"));
            return ExitCode::FAILURE;
        }
    };

    log::info(format_args!("build {build}, listening on {}", running.addr));
    match assets {
        Some(dir) => log::info(format_args!("serving the launcher from {}", dir.display())),
        None => log::info(format_args!(
            "no asset directory, so the launcher and the client downloads are not served here"
        )),
    }

    wait_for_a_signal().await;
    log::info(format_args!("stopping; every live session ends here"));
    running.stop();
    ExitCode::SUCCESS
}

async fn wait_for_a_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}
