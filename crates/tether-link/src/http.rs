//! HTTP requests, written by hand.
//!
//! Two kinds. [`post_json`] is one request on its own connection, closed when
//! it is answered: the owner's claim, and the operator's issue and release.
//! The claim is not an implementation detail to be optimised away — it is the
//! only point on the path where the relay can still see who is asking, because
//! the edge forwards no client address across a WebSocket upgrade. The
//! per-address limit lives on that request or it lives nowhere.
//!
//! [`Conn`] keeps its connection open between requests, for the polled
//! session in [`crate::poll`]. That session is a request every keystroke and
//! one held open every half-minute, and a TLS handshake through a proxy for
//! each of them would be the slowest part of typing.
//!
//! Hand-rolled rather than reached for, because the surface a general client
//! brings is surface this binary then carries onto a stranger's machine:
//! redirects to follow, cookies to keep, compression to decode. None of that
//! is wanted here. What this does not support, it refuses rather than
//! half-implements. The route, proxy included, is [`crate::net`]'s.

use std::fmt;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::net::{self, read_head, status_of, DialError, Route};
use crate::relay::Relay;

/// A response body this size is a misdirected request, not an answer: every
/// reply on this path is a line of JSON.
const MAX_BODY: usize = 64 * 1024;

#[derive(Debug)]
pub enum HttpError {
    Dial(DialError),
    Connect(std::io::Error),
    /// The status the relay answered with, when it was not 200. `404` is the
    /// ordinary one and means the slot is not there — expired, spent, or never
    /// issued. The relay answers all three the same way on purpose.
    Status(u16),
    Malformed(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dial(e) => write!(f, "{e}"),
            Self::Connect(e) => write!(f, "lost the relay mid-request: {e}"),
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
    post_json_body(relay, path, bearer, "").await
}

/// [`post_json`] with a JSON body. The body is for a credential that must not
/// ride in the path, where every access log on the way would keep it.
pub async fn post_json_body(
    relay: &Relay,
    path: &str,
    bearer: Option<&str>,
    body: &str,
) -> Result<serde_json::Value, HttpError> {
    let auth = match bearer {
        Some(token) => format!("Authorization: Bearer {token}\r\n"),
        None => String::new(),
    };
    let kind = if body.is_empty() {
        ""
    } else {
        "Content-Type: application/json\r\n"
    };
    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: tether\r\n\
         {auth}\
         {kind}\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n\
         {body}",
        relay.authority(),
        body.len()
    );

    let route = Route::for_relay(relay).map_err(HttpError::Dial)?;
    let stream = net::dial(relay, &route).await.map_err(HttpError::Dial)?;
    let raw = exchange(stream, request)
        .await
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

/// The largest body a [`Conn`] will read: a polled answer is at most a batch
/// and one frame past it.
const MAX_POLLED: usize = 4 * 1024 * 1024;

/// One connection to the relay, kept open across requests.
pub struct Conn {
    relay: Relay,
    route: Route,
    stream: Option<net::Stream>,
}

impl Conn {
    pub fn new(relay: Relay, route: Route) -> Self {
        Self {
            relay,
            route,
            stream: None,
        }
    }

    /// Send one request and read its answer, opening a connection if there is
    /// none.
    ///
    /// A connection the far side closed while it sat idle is only found out by
    /// using it, so a failure on a reused connection is retried once on a fresh
    /// one. That makes every request here one that may arrive twice, which the
    /// polled session is built to allow and a claim is not — so a claim never
    /// goes through here.
    pub async fn request(
        &mut self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), HttpError> {
        let reused = self.stream.is_some();
        match self.attempt(method, path, body).await {
            Err(HttpError::Connect(_)) if reused => self.attempt(method, path, body).await,
            other => other,
        }
    }

    async fn attempt(
        &mut self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), HttpError> {
        let mut stream = match self.stream.take() {
            Some(stream) => stream,
            None => net::dial(&self.relay, &self.route)
                .await
                .map_err(HttpError::Dial)?,
        };
        let head = format!(
            "{method} {path} HTTP/1.1\r\n\
             Host: {}\r\n\
             User-Agent: tether\r\n\
             Content-Length: {}\r\n\
             Connection: keep-alive\r\n\r\n",
            self.relay.authority(),
            body.len()
        );
        let (status, answer, keep) = round_trip(&mut stream, head.as_bytes(), body)
            .await
            .map_err(HttpError::Connect)?;
        if keep {
            self.stream = Some(stream);
        }
        Ok((status, answer))
    }

    /// Drop the connection, so the next request opens a fresh one.
    pub fn reset(&mut self) {
        self.stream = None;
    }
}

async fn round_trip<S>(
    stream: &mut S,
    head: &[u8],
    body: &[u8],
) -> std::io::Result<(u16, Vec<u8>, bool)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(head).await?;
    stream.write_all(body).await?;
    stream.flush().await?;

    let head = read_head(stream).await?;
    let status =
        status_of(&head).ok_or_else(|| std::io::Error::other("the answer had no status line"))?;
    let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
    let header = |name: &str| {
        text.lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .find(|(n, _)| n.trim() == name)
            .map(|(_, v)| v.trim().to_string())
    };
    let close = header("connection").is_some_and(|v| v.contains("close"));

    if header("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        return Ok((status, read_chunked(stream).await?, !close));
    }
    match header("content-length") {
        Some(len) => {
            let len: usize = len
                .parse()
                .map_err(|_| std::io::Error::other("a length that is not a number"))?;
            if len > MAX_POLLED {
                return Err(std::io::Error::other("the relay's answer was too long"));
            }
            let mut answer = vec![0; len];
            stream.read_exact(&mut answer).await?;
            Ok((status, answer, !close))
        }
        // No length and no chunks: the body ends where the connection does.
        None => {
            let mut answer = Vec::new();
            stream
                .take(MAX_POLLED as u64 + 1)
                .read_to_end(&mut answer)
                .await?;
            if answer.len() > MAX_POLLED {
                return Err(std::io::Error::other("the relay's answer was too long"));
            }
            Ok((status, answer, false))
        }
    }
}

/// A chunked body, which a proxy may produce even from an answer that had a
/// length when it left the relay.
async fn read_chunked<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = read_line(stream).await?;
        let size = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size, 16)
            .map_err(|_| std::io::Error::other("a chunk size that is not a number"))?;
        if size == 0 {
            // Trailers, if any, then the blank line that ends them.
            while !read_line(stream).await?.is_empty() {}
            return Ok(body);
        }
        if body.len() + size > MAX_POLLED {
            return Err(std::io::Error::other("the relay's answer was too long"));
        }
        let start = body.len();
        body.resize(start + size, 0);
        stream.read_exact(&mut body[start..]).await?;
        read_line(stream).await?;
    }
}

async fn read_line<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        if line.len() > 1024 {
            return Err(std::io::Error::other("a chunk header that never ends"));
        }
        if stream.read(&mut byte).await? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        line.push(byte[0]);
    }
    line.truncate(line.len() - 2);
    Ok(String::from_utf8_lossy(&line).into_owned())
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn answer(raw: &[u8]) -> (u16, Vec<u8>, bool) {
        let (mut ours, mut theirs) = tokio::io::duplex(64 * 1024);
        theirs.write_all(raw).await.unwrap();
        round_trip(&mut ours, b"GET / HTTP/1.1\r\n\r\n", b"")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_kept_connection_reads_exactly_its_body() {
        let (status, body, keep) =
            answer(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloEXTRA").await;
        assert_eq!((status, body.as_slice(), keep), (200, &b"hello"[..], true));
    }

    #[tokio::test]
    async fn a_chunked_body_is_put_back_together() {
        let (status, body, keep) = answer(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
              3\r\nhel\r\n2;x=y\r\nlo\r\n0\r\n\r\n",
        )
        .await;
        assert_eq!((status, body.as_slice(), keep), (200, &b"hello"[..], false));
    }

    #[test]
    fn a_404_is_explained_as_a_code_that_does_not_work() {
        let text = HttpError::Status(404).to_string();
        assert!(text.contains("mistyped"), "{text}");
        assert!(text.contains("expired"), "{text}");
    }
}
