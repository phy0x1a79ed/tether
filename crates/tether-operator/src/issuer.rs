//! Where this daemon's slots come from.
//!
//! The relay issues a slot only behind a bearer that never leaves the relay's
//! host. This daemon holds no bearer. It asks an [`Issuer`] for each slot, and
//! the same issuer puts a held slot back when the relay restarted and forgot it.
//!
//! # The command
//!
//! The adapter names the issuer in `TETHER_ISSUER`: a JSON array of a program
//! and its arguments, run once per request with no shell. The request is one
//! JSON object on its stdin, and the answer one JSON object on its stdout.
//!
//! - `{}` asks for a new slot. The answer is `{slot, token, expires_in}`.
//! - `{slot, token, owner}` puts a held slot back. The answer is
//!   `{restored: true}`, or `{refused: "taken"}` when the number went to
//!   another session first, or `{refused: "ended"}` when the session was ended.
//!
//! Any other answer, or a non-zero exit, is a failure, and stderr says why.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tether_link::resume::Restore;
use tether_link::LinkError;
use tether_proto::invite::Slot;
use tokio::io::AsyncWriteExt;

pub const ISSUER_ENV: &str = "TETHER_ISSUER";

/// How long one request to the issuer may take. It crosses to the relay's host.
const TIMEOUT: Duration = Duration::from_secs(120);

/// A slot the relay issued: its number, the operator's seat token, and how
/// long it waits for its owner.
#[derive(Debug, Clone)]
pub struct Issued {
    pub slot: Slot,
    pub seat: String,
    pub lifetime: Duration,
}

pub trait Issuer: Restore {
    fn issue(&self) -> BoxFuture<'static, Result<Issued, String>>;
}

/// The issuer `TETHER_ISSUER` names, or `None` when it names none.
pub fn from_env() -> Result<Option<Arc<dyn Issuer>>, String> {
    let Ok(raw) = std::env::var(ISSUER_ENV) else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let argv: Vec<String> = serde_json::from_str(&raw)
        .map_err(|e| format!("{ISSUER_ENV} is not a JSON array of strings: {e}"))?;
    if argv.is_empty() {
        return Err(format!("{ISSUER_ENV} names no program"));
    }
    Ok(Some(Arc::new(Command { argv })))
}

/// Read an issue answer: `{slot, token, expires_in}`.
pub fn issued(answer: &Value) -> Result<Issued, String> {
    let slot = answer["slot"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .and_then(|n| Slot::new(n).ok())
        .ok_or("the relay issued no usable slot")?;
    let seat = answer["token"]
        .as_str()
        .ok_or("the relay issued no seat token")?
        .to_string();
    let lifetime = Duration::from_secs(answer["expires_in"].as_u64().unwrap_or(300));
    Ok(Issued {
        slot,
        seat,
        lifetime,
    })
}

/// Read a restore answer: `{restored: true}` or `{refused: "taken" | "ended"}`.
pub fn restored(answer: &Value) -> Result<(), LinkError> {
    match (answer["restored"].as_bool(), answer["refused"].as_str()) {
        (Some(true), _) => Ok(()),
        (_, Some("taken")) => Err(LinkError::Taken),
        (_, Some("ended")) => Err(LinkError::Ended),
        _ => Err(LinkError::Issuer(format!("it answered {answer}"))),
    }
}

/// The restore request for one held slot.
pub fn restore_request(slot: Slot, seat: &str, owner: &str) -> Value {
    json!({ "slot": slot.to_string(), "token": seat, "owner": owner })
}

struct Command {
    argv: Vec<String>,
}

impl Command {
    async fn ask(argv: Vec<String>, request: Value) -> Result<Value, String> {
        let mut child = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("could not run {}: {e}", argv[0]))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(request.to_string().as_bytes())
                .await
                .map_err(|e| format!("could not write to {}: {e}", argv[0]))?;
        }
        let out = tokio::time::timeout(TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| format!("{} did not answer in {}s", argv[0], TIMEOUT.as_secs()))?
            .map_err(|e| format!("{} failed: {e}", argv[0]))?;
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(if why.is_empty() {
                format!("{} exited with {}", argv[0], out.status)
            } else {
                why
            });
        }
        serde_json::from_slice(&out.stdout)
            .map_err(|e| format!("{} answered no JSON object: {e}", argv[0]))
    }
}

impl Issuer for Command {
    fn issue(&self) -> BoxFuture<'static, Result<Issued, String>> {
        let argv = self.argv.clone();
        Box::pin(async move { issued(&Self::ask(argv, json!({})).await?) })
    }
}

impl Restore for Command {
    fn restore(
        &self,
        slot: Slot,
        seat: &str,
        owner: &str,
    ) -> BoxFuture<'static, Result<(), LinkError>> {
        let (argv, request) = (self.argv.clone(), restore_request(slot, seat, owner));
        Box::pin(async move {
            let answer = Self::ask(argv, request).await.map_err(LinkError::Issuer)?;
            restored(&answer)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(body: &str) -> Command {
        Command {
            argv: vec!["sh".into(), "-c".into(), body.into()],
        }
    }

    #[tokio::test]
    async fn a_slot_comes_from_the_command_and_the_request_goes_on_its_stdin() {
        let issued = script(
            r#"read -r req; [ "$req" = "{}" ] || exit 3
               echo '{"slot": 417, "token": "00112233445566778899aabbccddeeff", "expires_in": 60}'"#,
        )
        .issue()
        .await
        .unwrap();
        assert_eq!(
            (issued.slot.get(), issued.seat.as_str(), issued.lifetime),
            (
                417,
                "00112233445566778899aabbccddeeff",
                Duration::from_secs(60)
            )
        );
    }

    #[tokio::test]
    async fn a_failing_command_says_why_on_stderr() {
        let err = script("echo 'relay: deneb is not answering' >&2; exit 1")
            .issue()
            .await
            .unwrap_err();
        assert_eq!(err, "relay: deneb is not answering");
    }

    #[tokio::test]
    async fn a_restore_tells_taken_from_ended() {
        let slot = Slot::new(12).unwrap();
        let taken = script(r#"cat >/dev/null; echo '{"refused": "taken"}'"#);
        let ended = script(r#"cat >/dev/null; echo '{"refused": "ended"}'"#);
        let back = script(r#"grep -q '"slot":"12"' && echo '{"restored": true}'"#);
        assert!(matches!(
            taken.restore(slot, "s", "o").await,
            Err(LinkError::Taken)
        ));
        assert!(matches!(
            ended.restore(slot, "s", "o").await,
            Err(LinkError::Ended)
        ));
        back.restore(slot, "s", "o").await.unwrap();
    }

    #[test]
    fn the_command_is_a_json_array() {
        std::env::set_var(ISSUER_ENV, "python -m issuer");
        assert!(from_env().is_err());
        std::env::set_var(ISSUER_ENV, r#"["python", "-m", "issuer"]"#);
        assert!(from_env().unwrap().is_some());
        std::env::remove_var(ISSUER_ENV);
        assert!(from_env().unwrap().is_none());
    }
}
