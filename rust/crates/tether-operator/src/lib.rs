//! The operator's daemon: the half that invites, and never the half that runs.
//!
//! It mints an invite, asks the relay for a slot to carry it, hands the code
//! back for the operator to read out, and then holds the session open so that
//! the next verb costs a message rather than a fresh negotiation and a second
//! prompt at the owner's keyboard.
//!
//! # What it is not allowed to do
//!
//! Nothing here runs a command. Every task this daemon opens runs on the
//! **owner's** machine, at the owner's end, after a person there answered a
//! prompt. This side asks; it does not execute, and it cannot: the executor
//! lives in the owner's client and there is no frame that would reach it before
//! the owner's `Hello` — see [`session`] for the ordering and `tether-owner`
//! for the enforcement.
//!
//! # The shape of it
//!
//! - [`config`] reads the environment the adapter set.
//! - [`operator`] is the table of sessions and the verbs over it.
//! - [`session`] is one session's task, which owns its socket alone.
//! - [`control`] is the local socket the adapter speaks to.

pub mod config;
pub mod control;
pub mod log;
pub mod operator;
pub mod session;

pub use config::Config;
pub use operator::Operator;

use std::sync::Arc;

/// A daemon that is up: its table, and the socket it answers on.
pub struct Running {
    pub operator: Arc<Operator>,
    listening: control::Listening,
}

impl Running {
    pub fn socket(&self) -> &std::path::Path {
        &self.listening.path
    }

    pub fn stop(self) {
        self.listening.stop();
    }
}

/// Bring the daemon up on the socket its config names.
pub async fn start(config: Config) -> Result<Running, control::BindError> {
    let socket = config.socket.clone();
    let operator = Operator::new(config);
    let listening = control::serve(Arc::clone(&operator), &socket).await?;
    Ok(Running {
        operator,
        listening,
    })
}
