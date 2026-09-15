//! How a mediated tool call reaches the pod.
//!
//! Two transports, and the choice between them is a trust decision rather than a
//! convenience one.
//!
//! * [`Transport::Unix`] — a peer-credential-verified Unix socket served by
//!   `nucleus-tool-proxy --listen-unix`. The proxy authenticates the caller from
//!   `SO_PEERCRED`, so **this bridge holds no secret at all**. That is the
//!   default and the preferred posture: a bridge with no key cannot leak one.
//! * [`Transport::Http`] — a forwarded HTTP surface, for the case where the pod
//!   is reached across a node rather than over a local socket. Here a session
//!   token is required, and it is read from the environment at each call rather
//!   than cached, so a revoked token stops working immediately.
//!
//! The HTTP/1.1 written by hand below is not an aesthetic choice. The Unix path
//! needs a client that speaks to a `UnixStream`, the request shape is one POST
//! with a JSON body, and pulling a full HTTP stack in to express that would add
//! dependencies to a binary that sits on the security boundary.

use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("cannot reach the nucleus pod: {0}")]
    Io(#[from] std::io::Error),
    #[error("the pod refused the call: HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("the pod's reply was not the shape this bridge expects: {0}")]
    Protocol(String),
    #[error("no transport configured: set NUCLEUS_POD_SOCK or NUCLEUS_PROXY_URL")]
    Unconfigured,
    /// Neither variable was set and asking the node for a pod did not work
    /// either. Distinct from `Protocol`, which is about a *pod's* reply — there
    /// is no pod here yet, and saying "the pod's reply was not the shape this
    /// bridge expects" about a node that is not running sends the reader to the
    /// wrong place.
    #[error("could not find a nucleus pod: {0}")]
    Discovery(String),
}

#[derive(Debug, Clone)]
pub enum Transport {
    Unix(PathBuf),
    Http { base: String, token: Option<String> },
}

impl Transport {
    /// Resolve the transport from the environment.
    ///
    /// The socket wins when both are set: it is the stronger of the two, and
    /// silently preferring the weaker one because it was also configured is how
    /// a deployment ends up authenticating with a bearer token it did not know
    /// it was still using.
    pub fn from_env() -> Result<Self, TransportError> {
        if let Some(sock) = non_empty("NUCLEUS_POD_SOCK") {
            return Ok(Transport::Unix(PathBuf::from(sock)));
        }
        if let Some(base) = non_empty("NUCLEUS_PROXY_URL") {
            return Ok(Transport::Http {
                base: base.trim_end_matches('/').to_string(),
                token: non_empty("NUCLEUS_SESSION_TOKEN"),
            });
        }
        Err(TransportError::Unconfigured)
    }

    /// POST `body` to `route` on the pod and return the JSON reply.
    pub async fn post(
        &self,
        route: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, TransportError> {
        let payload =
            serde_json::to_vec(body).map_err(|e| TransportError::Protocol(e.to_string()))?;
        self.request("POST", route, &payload).await
    }

    /// GET `route`. Only `/v1/health` needs this, and only to answer "is the pod
    /// serving yet" — which is a different question from "did my call work", and
    /// the one a freshly created pod makes you ask.
    pub async fn get(&self, route: &str) -> Result<serde_json::Value, TransportError> {
        self.request("GET", route, &[]).await
    }

    async fn request(
        &self,
        method: &str,
        route: &str,
        payload: &[u8],
    ) -> Result<serde_json::Value, TransportError> {
        match self {
            Transport::Unix(path) => {
                let stream = tokio::net::UnixStream::connect(path).await?;
                request_over(stream, method, "localhost", route, payload, None).await
            }
            Transport::Http { base, token } => {
                let (host, port, path_prefix) = split_base(base)?;
                let stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
                let full = format!("{path_prefix}{route}");
                request_over(stream, method, &host, &full, payload, token.as_deref()).await
            }
        }
    }

    /// The pod in as few characters as name it.
    ///
    /// [`Self::describe`] is written for `--check`, where a sentence about the
    /// trust posture is worth its width. A status line has about eighty columns
    /// for everything, so it gets the authority and nothing else.
    pub fn label(&self) -> String {
        match self {
            Transport::Unix(p) => p
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| p.display().to_string()),
            Transport::Http { base, .. } => base
                .trim_start_matches("http://")
                .trim_start_matches("https://")
                .to_string(),
        }
    }

    /// How this transport was chosen, for the check's first line and for the
    /// stderr note when a pod was discovered rather than configured.
    pub fn describe(&self) -> String {
        match self {
            Transport::Unix(p) => format!("unix socket {}", p.display()),
            Transport::Http { base, token } => match token {
                Some(_) => format!("{base} (bearer token)"),
                None => format!("{base} (no token — the node signs this hop)"),
            },
        }
    }
}

/// A variable that is set to the empty string is not configuration.
///
/// `.mcp.json` env blocks and shell exports both produce set-but-empty values
/// routinely, and treating one as an address gave `Transport::Unix("")` — which
/// fails at connect time as `Invalid argument (os error 22)`, a message that
/// says nothing about the cause. Unset and empty now mean the same thing, which
/// is also what makes `NUCLEUS_POD_SOCK=` a working way to force discovery.
fn non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// `http://host:port/prefix` split into its parts. Only plaintext is accepted:
/// the socket case is local, and the HTTP case is a node-forwarded loopback
/// hop. Accepting `https` here without doing certificate verification would be
/// worse than refusing it.
fn split_base(base: &str) -> Result<(String, u16, String), TransportError> {
    let rest = base.strip_prefix("http://").ok_or_else(|| {
        TransportError::Protocol(format!("NUCLEUS_PROXY_URL must be http://: {base}"))
    })?;
    let (authority, prefix) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| TransportError::Protocol(format!("bad port in {base}")))?,
        ),
        None => (authority.to_string(), 80),
    };
    Ok((host, port, prefix.to_string()))
}

/// One HTTP/1.1 request, read to completion, `Connection: close` so the reply
/// ends at EOF and no chunked/keep-alive framing has to be parsed.
async fn request_over<S>(
    mut stream: S,
    method: &str,
    host: &str,
    path: &str,
    payload: &[u8],
    token: Option<&str>,
) -> Result<serde_json::Value, TransportError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let auth = match token {
        Some(t) => format!("Authorization: Bearer {t}\r\n"),
        None => String::new(),
    };
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n{auth}Connection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| TransportError::Protocol("no header terminator in reply".into()))?;
    let headers = String::from_utf8_lossy(&raw[..split]).to_string();
    let body = &raw[split + 4..];

    let status: u16 = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| TransportError::Protocol("no status line in reply".into()))?;

    if !(200..300).contains(&status) {
        return Err(TransportError::Status {
            status,
            body: String::from_utf8_lossy(body).chars().take(2000).collect(),
        });
    }
    if body.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_slice(body).map_err(|e| TransportError::Protocol(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_base_url_splits_into_host_port_and_prefix() {
        let (h, p, pre) = split_base("http://127.0.0.1:8080/pods/abc").unwrap();
        assert_eq!(
            (h.as_str(), p, pre.as_str()),
            ("127.0.0.1", 8080, "/pods/abc")
        );
        let (h, p, pre) = split_base("http://node:9000").unwrap();
        assert_eq!((h.as_str(), p, pre.as_str()), ("node", 9000, ""));
    }

    /// A set-but-empty variable used to configure an empty socket path, which
    /// failed at connect with `Invalid argument` and no hint as to why. Both
    /// `.mcp.json` env blocks and shell exports produce these.
    #[test]
    fn a_variable_set_to_the_empty_string_is_not_configuration() {
        assert_eq!(non_empty("CCN_DEFINITELY_UNSET_VARIABLE"), None);
        std::env::set_var("CCN_TEST_EMPTY", "");
        assert_eq!(non_empty("CCN_TEST_EMPTY"), None);
        std::env::set_var("CCN_TEST_BLANK", "   ");
        assert_eq!(non_empty("CCN_TEST_BLANK"), None);
        std::env::set_var("CCN_TEST_SET", "/run/x.sock");
        assert_eq!(non_empty("CCN_TEST_SET"), Some("/run/x.sock".into()));
    }

    /// The status line has eighty columns for the whole of itself. A transport
    /// description that explains the trust posture belongs in `--check`.
    #[test]
    fn the_label_is_the_authority_and_nothing_else() {
        assert_eq!(
            Transport::Http {
                base: "http://127.0.0.1:52341".into(),
                token: None
            }
            .label(),
            "127.0.0.1:52341"
        );
        assert_eq!(
            Transport::Unix(PathBuf::from("/run/nucleus/pod.sock")).label(),
            "pod.sock"
        );
    }

    /// https is refused rather than silently downgraded — a bridge that accepts
    /// the scheme and then does not verify the certificate is worse than one
    /// that says no.
    #[test]
    fn https_is_refused_because_this_client_cannot_verify_it() {
        assert!(split_base("https://node:443").is_err());
    }
}
