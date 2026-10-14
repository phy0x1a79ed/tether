//! Reaching the relay: directly, or through the proxy the machine says to use.
//!
//! Every byte this crate sends leaves through [`dial`], so the claim, the
//! socket and anything after them take one route and fail in one vocabulary.
//!
//! # Why a proxy is honoured
//!
//! Some owners sit on machines whose only way out is an egress proxy: a
//! sandboxed VM, a corporate desktop. There, the proxy is not a detour around
//! anything, it is the network. The launcher's `curl` already goes through it,
//! so a client that ignored it would be the one program on the machine that
//! could not leave.
//!
//! Only `CONNECT` is spoken. The proxy carries a tunnel and sees a host name
//! and a port, and the relay's TLS runs inside it, end to end, as it does on a
//! direct route.
//!
//! # Why extra roots are trusted
//!
//! A proxy that inspects TLS re-signs every certificate with its own CA, and
//! the machine's owner has installed that CA on purpose. `SSL_CERT_FILE` and
//! `SSL_CERT_DIR` are where such a machine says so. Trusting them costs the
//! session nothing: what is said inside it is sealed by the handshake in
//! [`tether_proto::handshake`], keyed by the phrase, which never crosses the
//! wire. A proxy that can read the TLS sees a slot number, a one-use ticket and
//! sealed frames.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

use crate::relay::Relay;

/// A byte stream to the relay, whatever it is underneath.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub type Stream = Box<dyn Io>;

/// The proxy variables, in the order they are read. Lowercase first, as curl
/// reads them, so the launcher's download and this client take one route.
const TLS_VARS: [&str; 4] = ["https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"];
const PLAIN_VARS: [&str; 4] = ["http_proxy", "HTTP_PROXY", "all_proxy", "ALL_PROXY"];
const NO_PROXY_VARS: [&str; 2] = ["no_proxy", "NO_PROXY"];

/// A `CONNECT` reply's head larger than this is not a proxy talking.
const MAX_HEAD: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Direct,
    Proxy(Proxy),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    tls: bool,
    host: String,
    port: u16,
    /// `user:password`, decoded, for Basic authentication.
    credentials: Option<String>,
    /// The variable that named it, so the owner's screen can say where the
    /// route came from.
    source: &'static str,
}

impl fmt::Display for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{} (from {})", self.host, self.port, self.source)
    }
}

#[derive(Debug)]
pub enum DialError {
    /// A proxy variable is set to something this client cannot use.
    ProxySetting {
        var: &'static str,
        why: String,
    },
    Connect(std::io::Error),
    ProxyConnect {
        proxy: String,
        error: std::io::Error,
    },
    /// The proxy answered the `CONNECT` with something other than 200.
    ProxyRefused {
        proxy: String,
        status: u16,
    },
    Tls {
        error: String,
        proxy: Option<String>,
    },
}

impl fmt::Display for DialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProxySetting { var, why } => write!(f, "{var} is set, but {why}"),
            Self::Connect(e) => write!(f, "could not reach the relay: {e}"),
            Self::ProxyConnect { proxy, error } => {
                write!(f, "could not reach the proxy at {proxy}: {error}")
            }
            Self::ProxyRefused { proxy, status: 407 } => write!(
                f,
                "the proxy at {proxy} wants credentials; put them in the variable \
                 as http://user:password@host:port"
            ),
            Self::ProxyRefused { proxy, status } => write!(
                f,
                "the proxy at {proxy} would not connect to the relay (it answered {status}); \
                 it may need the relay's host on its allow-list"
            ),
            Self::Tls {
                error,
                proxy: Some(proxy),
            } if error.contains("UnknownIssuer") || error.contains("certificate") => write!(
                f,
                "the relay's certificate was not one this client trusts: {error}. \
                 The proxy at {proxy} may be re-signing TLS; if so, set SSL_CERT_FILE \
                 to its CA certificate"
            ),
            Self::Tls { error, .. } => write!(f, "could not set up TLS to the relay: {error}"),
        }
    }
}

impl std::error::Error for DialError {}

impl Route {
    /// The route this machine's environment says to take to the relay.
    pub fn for_relay(relay: &Relay) -> Result<Self, DialError> {
        Self::from_vars(relay, |name| std::env::var(name).ok())
    }

    fn from_vars(relay: &Relay, var: impl Fn(&str) -> Option<String>) -> Result<Self, DialError> {
        let set = |name: &&str| var(name).filter(|v| !v.trim().is_empty());
        let names = if relay.tls() { TLS_VARS } else { PLAIN_VARS };
        let Some((source, value)) = names.iter().find_map(|n| set(n).map(|v| (*n, v))) else {
            return Ok(Self::Direct);
        };
        if let Some(list) = NO_PROXY_VARS.iter().find_map(|n| set(n)) {
            if bypassed(&list, relay.host()) {
                return Ok(Self::Direct);
            }
        }
        Proxy::parse(&value, source).map(Self::Proxy)
    }

    /// One line for the owner's screen, or nothing when the route is direct.
    pub fn describe(&self) -> Option<String> {
        match self {
            Self::Direct => None,
            Self::Proxy(proxy) => Some(format!("through the proxy at {proxy}")),
        }
    }

    fn proxy_name(&self) -> Option<String> {
        match self {
            Self::Direct => None,
            Self::Proxy(proxy) => Some(proxy.to_string()),
        }
    }
}

impl Proxy {
    fn parse(value: &str, source: &'static str) -> Result<Self, DialError> {
        let bad = |why: &str| DialError::ProxySetting {
            var: source,
            why: why.to_string(),
        };
        let value = value.trim();
        let (tls, rest) = match value.split_once("://") {
            Some((scheme, rest)) => match scheme.to_ascii_lowercase().as_str() {
                "http" => (false, rest),
                "https" => (true, rest),
                s if s.starts_with("socks") => {
                    return Err(bad(&format!(
                        "it names a {s} proxy and this client speaks only HTTP proxies"
                    )))
                }
                s => {
                    return Err(bad(&format!(
                        "`{s}` is not a proxy scheme this client knows"
                    )))
                }
            },
            None => (false, value),
        };
        let authority = rest.split('/').next().unwrap_or_default();
        let (credentials, hostport) = match authority.rsplit_once('@') {
            Some((userinfo, hostport)) => (Some(percent_decode(userinfo)), hostport),
            None => (None, authority),
        };
        let (host, port) = split_port(hostport);
        if host.is_empty() {
            return Err(bad("it names no host"));
        }
        let port = match port {
            // curl's default, which is what the launcher's download used.
            None => 1080,
            Some(p) => p
                .parse()
                .map_err(|_| bad(&format!("`{p}` is not a port")))?,
        };
        Ok(Self {
            tls,
            host: host.to_string(),
            port,
            credentials,
            source,
        })
    }
}

/// Open a stream to the relay, TLS included when the relay speaks it.
pub async fn dial(relay: &Relay, route: &Route) -> Result<Stream, DialError> {
    let raw: Stream = match route {
        Route::Direct => Box::new(
            TcpStream::connect((relay.host(), relay.port()))
                .await
                .map_err(DialError::Connect)?,
        ),
        Route::Proxy(proxy) => tunnel(proxy, relay.host(), relay.port()).await?,
    };
    if !relay.tls() {
        return Ok(raw);
    }
    tls(relay.host(), raw)
        .await
        .map_err(|error| DialError::Tls {
            error,
            proxy: route.proxy_name(),
        })
}

async fn tls(host: &str, stream: Stream) -> Result<Stream, String> {
    let name = ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?;
    let stream = TlsConnector::from(tls_config())
        .connect(name, stream)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Box::new(stream))
}

/// Ask the proxy for a tunnel to `host:port` and hand back the stream inside.
///
/// The proxy resolves the name. On a machine whose only way out is the proxy,
/// a lookup here would be the thing that fails.
async fn tunnel(proxy: &Proxy, host: &str, port: u16) -> Result<Stream, DialError> {
    let named = proxy.to_string();
    let lost = |error: std::io::Error| DialError::ProxyConnect {
        proxy: named.clone(),
        error,
    };
    let tcp = TcpStream::connect((proxy.host.as_str(), proxy.port))
        .await
        .map_err(lost)?;
    let mut stream: Stream = if proxy.tls {
        tls(&proxy.host, Box::new(tcp))
            .await
            .map_err(|error| DialError::Tls {
                error: format!("to the proxy: {error}"),
                proxy: Some(named.clone()),
            })?
    } else {
        Box::new(tcp)
    };

    let target = format!("{host}:{port}");
    let auth = match &proxy.credentials {
        Some(c) => format!("Proxy-Authorization: Basic {}\r\n", base64(c.as_bytes())),
        None => String::new(),
    };
    let request =
        format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nUser-Agent: tether\r\n{auth}\r\n");
    stream.write_all(request.as_bytes()).await.map_err(lost)?;
    stream.flush().await.map_err(lost)?;

    let head = read_head(&mut stream).await.map_err(lost)?;
    let status = status_of(&head).ok_or_else(|| {
        lost(std::io::Error::other(
            "the proxy's answer had no status line",
        ))
    })?;
    if status != 200 {
        return Err(DialError::ProxyRefused {
            proxy: named,
            status,
        });
    }
    Ok(stream)
}

/// Read up to the blank line that ends a response head, and not one byte
/// further: everything after it belongs to the tunnel.
pub(crate) async fn read_head<S: AsyncRead + Unpin + ?Sized>(
    stream: &mut S,
) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            return Err(std::io::Error::other("the answer's headers were too long"));
        }
        if stream.read(&mut byte).await? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        head.push(byte[0]);
    }
    Ok(head)
}

pub(crate) fn status_of(head: &[u8]) -> Option<u16> {
    let line = head.split(|b| *b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// The trust store: the roots compiled into this binary, and any the machine
/// names through `SSL_CERT_FILE` or `SSL_CERT_DIR`.
pub fn tls_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let (extra, _) = extra_roots();
            roots.add_parsable_certificates(extra);
            Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        })
        .clone()
}

/// The certificates the environment adds, and a line saying where they came
/// from when there are any.
pub fn extra_roots() -> (Vec<CertificateDer<'static>>, Option<String>) {
    let mut certs = Vec::new();
    let mut from = Vec::new();
    if let Ok(file) = std::env::var("SSL_CERT_FILE") {
        let found = read_pem(Path::new(&file));
        if !found.is_empty() {
            from.push(format!("SSL_CERT_FILE={file}"));
            certs.extend(found);
        }
    }
    if let Ok(dirs) = std::env::var("SSL_CERT_DIR") {
        for dir in std::env::split_paths(&dirs) {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let before = certs.len();
            for entry in entries.flatten() {
                certs.extend(read_pem(&entry.path()));
            }
            if certs.len() > before {
                from.push(format!("SSL_CERT_DIR={}", dir.display()));
            }
        }
    }
    let note = (!certs.is_empty()).then(|| {
        let n = certs.len();
        let s = if n == 1 { "" } else { "s" };
        format!("trusting {n} extra certificate{s} from {}", from.join(", "))
    });
    (certs, note)
}

fn read_pem(path: &Path) -> Vec<CertificateDer<'static>> {
    match CertificateDer::pem_file_iter(path) {
        Ok(iter) => iter.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// Whether a `NO_PROXY` list names this host. Entries match the host itself or
/// any name under it; `*` matches everything. Ports in entries are ignored.
fn bypassed(list: &str, host: &str) -> bool {
    let host = host
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    list.split([',', ' '])
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .any(|entry| {
            if entry == "*" {
                return true;
            }
            let (name, _) = split_port(entry);
            let name = name
                .trim_start_matches("*.")
                .trim_start_matches('.')
                .trim_matches(|c| c == '[' || c == ']')
                .to_ascii_lowercase();
            host == name || host.ends_with(&format!(".{name}"))
        })
}

fn split_port(hostport: &str) -> (&str, Option<&str>) {
    if let Some(rest) = hostport.strip_prefix('[') {
        // [v6]:port
        return match rest.split_once(']') {
            Some((host, tail)) => (host, tail.strip_prefix(':')),
            None => (hostport, None),
        };
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host, Some(port)),
        _ => (hostport, None),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn route(base: &str, vars: &[(&str, &str)]) -> Result<Route, DialError> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Route::from_vars(&Relay::parse(base).unwrap(), |n| vars.get(n).cloned())
    }

    const RELAY: &str = "https://nexus.tony-xy-liu.com/tether";

    #[test]
    fn no_variable_means_the_direct_route() {
        assert_eq!(route(RELAY, &[]).unwrap(), Route::Direct);
        assert_eq!(
            route(RELAY, &[("HTTPS_PROXY", " ")]).unwrap(),
            Route::Direct
        );
    }

    #[test]
    fn the_variables_are_read_as_curl_reads_them() {
        let Route::Proxy(p) = route(
            RELAY,
            &[
                ("HTTPS_PROXY", "http://upper:1"),
                ("https_proxy", "lower:3128"),
            ],
        )
        .unwrap() else {
            panic!("no proxy")
        };
        assert_eq!(
            (p.host.as_str(), p.port, p.source),
            ("lower", 3128, "https_proxy")
        );

        let Route::Proxy(p) = route(RELAY, &[("ALL_PROXY", "http://egress")]).unwrap() else {
            panic!("no proxy")
        };
        assert_eq!((p.host.as_str(), p.port), ("egress", 1080));

        // A plain relay is a test relay, and it reads the plain variables.
        let Route::Proxy(p) = route(
            "http://127.0.0.1:9",
            &[("HTTPS_PROXY", "a:1"), ("http_proxy", "b:2")],
        )
        .unwrap() else {
            panic!("no proxy")
        };
        assert_eq!(p.host, "b");
    }

    #[test]
    fn credentials_are_decoded_and_sent_as_basic() {
        let Route::Proxy(p) = route(
            RELAY,
            &[("HTTPS_PROXY", "http://me%40corp:p%3Ass@10.0.0.1:8080/")],
        )
        .unwrap() else {
            panic!("no proxy")
        };
        assert_eq!(p.credentials.as_deref(), Some("me@corp:p:ss"));
        assert_eq!((p.host.as_str(), p.port), ("10.0.0.1", 8080));
        assert_eq!(
            base64(b"Aladdin:open sesame"),
            "QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"a"), "YQ==");
    }

    #[test]
    fn no_proxy_matches_the_host_and_the_names_under_it() {
        for list in [
            "nexus.tony-xy-liu.com",
            ".tony-xy-liu.com",
            "*.tony-xy-liu.com",
            "localhost, tony-xy-liu.com:443",
            "*",
        ] {
            assert_eq!(
                route(RELAY, &[("HTTPS_PROXY", "p:1"), ("NO_PROXY", list)]).unwrap(),
                Route::Direct,
                "{list}"
            );
        }
        for list in ["xy-liu.com", "other.example", "nexus.tony-xy-liu.co"] {
            assert!(
                matches!(
                    route(RELAY, &[("HTTPS_PROXY", "p:1"), ("no_proxy", list)]).unwrap(),
                    Route::Proxy(_)
                ),
                "{list}"
            );
        }
    }

    #[test]
    fn a_proxy_this_client_cannot_use_is_named_rather_than_ignored() {
        let e = route(RELAY, &[("ALL_PROXY", "socks5h://127.0.0.1:1080")]).unwrap_err();
        let text = e.to_string();
        assert!(text.starts_with("ALL_PROXY is set"), "{text}");
        assert!(text.contains("socks5h"), "{text}");
        assert!(route(RELAY, &[("HTTPS_PROXY", "http://:8080")]).is_err());
        assert!(route(RELAY, &[("HTTPS_PROXY", "http://p:port")]).is_err());
    }

    #[test]
    fn a_refusal_says_which_proxy_and_what_to_do() {
        let e = DialError::ProxyRefused {
            proxy: "p:1 (from HTTPS_PROXY)".into(),
            status: 403,
        };
        assert!(e.to_string().contains("allow-list"));
        let e = DialError::Tls {
            error: "invalid peer certificate: UnknownIssuer".into(),
            proxy: Some("p:1".into()),
        };
        assert!(e.to_string().contains("SSL_CERT_FILE"));
    }

    #[test]
    fn the_status_is_read_off_the_first_line() {
        assert_eq!(
            status_of(b"HTTP/1.1 200 Connection established\r\n\r\n"),
            Some(200)
        );
        assert_eq!(
            status_of(b"HTTP/1.0 407 Proxy Authentication Required\r\n"),
            Some(407)
        );
        assert_eq!(status_of(b"garbage"), None);
    }

    #[tokio::test]
    async fn the_head_stops_at_the_blank_line() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(b"HTTP/1.1 200 OK\r\nX: y\r\n\r\ntunnel")
            .await
            .unwrap();
        let head = read_head(&mut b).await.unwrap();
        assert!(head.ends_with(b"\r\n\r\n"));
        let mut rest = [0u8; 6];
        b.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"tunnel");
    }

    #[test]
    fn the_trust_store_builds() {
        assert!(!webpki_roots::TLS_SERVER_ROOTS.is_empty());
        // Building it must not panic, which is the failure mode when rustls has
        // no crypto provider to pick.
        let _ = tls_config();
    }
}
