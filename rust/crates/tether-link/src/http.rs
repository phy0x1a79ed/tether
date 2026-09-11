//! One HTTP request, written by hand.
//!
//! The owner's client makes exactly one plain request in its life: it claims a
//! ticket before it opens the socket. That claim is not an implementation
//! detail to be optimised away — it is the only point on the path where the
//! relay can still see who is asking, because the edge forwards no client
//! address across a WebSocket upgrade. The per-address limit lives on this
//! request or it lives nowhere.
//!
//! Hand-rolled rather than reached for, because the surface a general client
//! brings is surface this binary then carries onto a stranger's machine:
//! redirects to follow, proxies to honour, cookies to keep, compression to
//! decode. None of that is wanted here, and `Connection: close` plus
//! read-to-end removes the last of it — there is no response framing to get
//! wrong. What this does not support, it refuses rather than half-implements.

use std::fmt;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

use crate::relay::Relay;

/// A response body this size is a misdirected request, not an answer: every
/// reply on this path is a line of JSON.
const MAX_BODY: usize = 64 * 1024;

#[derive(Debug)]
pub enum HttpError {
    Connect(std::io::Error),
    Tls(String),
    /// The status the relay answered with, when it was not 200. `404` is the
    /// ordinary one and means the slot is not there — expired, spent, or never
    /// issued. The relay answers all three the same way on purpose.
    Status(u16),
    Malformed(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "could not reach the relay: {e}"),
            Self::Tls(e) => write!(f, "could not set up TLS to the relay: {e}"),
            Self::Status(404) => write!(
                f,
                "the relay has no such session — the code may be mistyped, or it may have expired"
            ),
            Self::Status(429) => {
                write!(f, "the relay is asking us to slow down; try again shortly")
            }
            Self::Status(s) => write!(f, "the relay answered {s}"),
            Self::Malformed(e) => write!(f, "the relay's answer made no sense: {e}"),
        }
    }
}

impl std::error::Error for HttpError {}

/// POST to a path on the relay and read the JSON back.
///
/// The bearer is for the operator's two routes. The owner's claim carries
/// none — it is the public half of the service, and a credential on it would
/// be a credential the owner had to be given.
pub async fn post_json(
    relay: &Relay,
    path: &str,
    bearer: Option<&str>,
) -> Result<serde_json::Value, HttpError> {
    let auth = match bearer {
        Some(token) => format!("Authorization: Bearer {token}\r\n"),
        None => String::new(),
    };
    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: tether\r\n\
         {auth}\
         Content-Length: 0\r\n\
         Connection: close\r\n\r\n",
        relay.authority()
    );

    let tcp = TcpStream::connect((relay.host(), relay.port()))
        .await
        .map_err(HttpError::Connect)?;

    let raw = if relay.tls() {
        let name = rustls_pki_types::ServerName::try_from(relay.host().to_string())
            .map_err(|e| HttpError::Tls(e.to_string()))?;
        let stream = TlsConnector::from(tls_config())
            .connect(name, tcp)
            .await
            .map_err(|e| HttpError::Tls(e.to_string()))?;
        exchange(stream, request).await
    } else {
        exchange(tcp, request).await
    }
    .map_err(HttpError::Connect)?;

    let text = String::from_utf8_lossy(&raw);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| HttpError::Malformed("no status line".into()))?;
    if status != 200 {
        return Err(HttpError::Status(status));
    }
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .ok_or_else(|| HttpError::Malformed("no body".into()))?;
    serde_json::from_str(body).map_err(|e| HttpError::Malformed(e.to_string()))
}

async fn exchange<S>(mut stream: S, request: String) -> std::io::Result<Vec<u8>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    let mut raw = Vec::new();
    // `Connection: close` means the body ends where the socket does, so there
    // is no length header or chunked encoding to parse. The cap is here so a
    // host answering with something enormous cannot be the way this ends.
    let read = (&mut stream)
        .take(MAX_BODY as u64)
        .read_to_end(&mut raw)
        .await?;
    if read >= MAX_BODY {
        return Err(std::io::Error::other("the relay's answer was too long"));
    }
    Ok(raw)
}

/// The trust store, from the roots compiled into this binary.
///
/// Mozilla's roots rather than the host's, deliberately. The owner's machine is
/// one we know nothing about — it may be a fresh install, a Mac whose keychain
/// the launcher cannot read, or a box whose store somebody has added to. What
/// this binary will accept should be a property of this binary.
fn tls_config() -> Arc<ClientConfig> {
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_404_is_explained_as_a_code_that_does_not_work() {
        let text = HttpError::Status(404).to_string();
        assert!(text.contains("mistyped"), "{text}");
        assert!(text.contains("expired"), "{text}");
    }

    #[test]
    fn the_trust_store_is_not_empty() {
        assert!(!webpki_roots::TLS_SERVER_ROOTS.is_empty());
        // Building it must not panic, which is the failure mode when rustls has
        // no crypto provider to pick.
        let _ = tls_config();
    }
}
