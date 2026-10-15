//! The control socket: one request per connection, one line of JSON each way.
//!
//! Deliberately the dullest transport that would do. The only caller is the
//! gateway adapter in the same process tree, and what it needs is a warm
//! process to talk to rather than a protocol.
//!
//! # There is no authentication, and that is a decision
//!
//! The socket lives in this user's own state directory, the directory is
//! created private, and the socket is chmodded to the owner alone. So the
//! access check is the filesystem's, which is the same check that decides
//! whether a caller could simply read this process's memory. A token here would
//! protect nothing and would need to be stored somewhere, which is worse.
//!
//! # The single-instance guard
//!
//! A socket file left behind by a killed daemon is indistinguishable, by
//! looking, from one a live daemon is serving. So this asks: it connects
//! first. An answer means another daemon owns the name and this one stands
//! down rather than unlinking a working service out from under it. A refusal
//! means the file is a corpse, and it is removed.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

use crate::operator::Operator;

/// A request larger than this is not the adapter.
const MAX_REQUEST: u64 = 256 * 1024;

/// How long a connection may stay open without saying what it wants.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
struct Request {
    verb: String,
    #[serde(default)]
    args: Value,
}

/// A bound control socket and the task accepting on it.
pub struct Listening {
    pub path: PathBuf,
    accepting: JoinHandle<()>,
}

impl Listening {
    /// Stop accepting and take the socket file with us.
    ///
    /// The file is removed here rather than left for the next start to clean
    /// up, so that the guard above only ever has a genuine crash to reason
    /// about.
    pub fn stop(self) {
        self.accepting.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
pub enum BindError {
    /// Another daemon is already serving this socket.
    Taken(PathBuf),
    Io(std::io::Error),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Taken(p) => write!(
                f,
                "another tether daemon is already serving {}",
                p.display()
            ),
            Self::Io(e) => write!(f, "could not open the control socket: {e}"),
        }
    }
}

impl std::error::Error for BindError {}

/// Bind the control socket and start answering on it.
pub async fn serve(operator: Arc<Operator>, path: &Path) -> Result<Listening, BindError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(BindError::Io)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            return Err(BindError::Taken(path.to_path_buf()));
        }
        std::fs::remove_file(path).map_err(BindError::Io)?;
    }

    let listener = UnixListener::bind(path).map_err(BindError::Io)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(BindError::Io)?;

    let accepting = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let operator = Arc::clone(&operator);
                    tokio::spawn(answer(operator, stream));
                }
                // A failed accept is not a reason to stop serving every other
                // caller; it is usually a descriptor limit and it passes.
                Err(e) => {
                    crate::log::warn(format_args!("control socket accept failed: {e}"));
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });

    Ok(Listening {
        path: path.to_path_buf(),
        accepting,
    })
}

async fn answer(operator: Arc<Operator>, stream: UnixStream) {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(read.take(MAX_REQUEST));

    let read = tokio::time::timeout(REQUEST_TIMEOUT, reader.read_line(&mut line)).await;
    if !matches!(read, Ok(Ok(n)) if n > 0) {
        return;
    }

    let request = serde_json::from_str::<Request>(&line);

    // The one verb that does not answer and stop. It takes the write half and
    // keeps it, so nothing below may run afterwards.
    if let Ok(request) = &request {
        if request.verb == "watch" {
            crate::watch::stream(operator, &request.args, write).await;
            return;
        }
    }

    let reply = match request {
        Ok(request) => match operator.handle(&request.verb, &request.args).await {
            Ok(value) => value,
            Err(error) => json!({"ok": false, "error": error}),
        },
        Err(e) => json!({"ok": false, "error": format!("that is not a request: {e}")}),
    };

    let mut bytes = serde_json::to_vec(&reply).unwrap_or_else(|_| {
        br#"{"ok":false,"error":"the daemon's own reply would not encode"}"#.to_vec()
    });
    bytes.push(b'\n');
    let _ = write.write_all(&bytes).await;
    let _ = write.flush().await;
}
