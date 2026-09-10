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
        if let Some(sock) = std::env::var_os("NUCLEUS_POD_SOCK") {
            return Ok(Transport::Unix(PathBuf::from(sock)));
        }
        if let Ok(base) = std::env::var("NUCLEUS_PROXY_URL") {
            return Ok(Transport::Http {
                base: base.trim_end_matches('/').to_string(),
                token: std::env::var("NUCLEUS_SESSION_TOKEN").ok(),
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
        match self {
            Transport::Unix(path) => {
                let stream = tokio::net::UnixStream::connect(path).await?;
                request_over(stream, "localhost", route, &payload, None).await
            }
            Transport::Http { base, token } => {
                let (host, port, path_prefix) = split_base(base)?;
                let stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
                let full = format!("{path_prefix}{route}");
                request_over(stream, &host, &full, &payload, token.as_deref()).await
            }
        }
    }
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

/// One HTTP/1.1 POST, read to completion, `Connection: close` so the reply ends
/// at EOF and no chunked/keep-alive framing has to be parsed.
async fn request_over<S>(
    mut stream: S,
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
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
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

    /// https is refused rather than silently downgraded — a bridge that accepts
    /// the scheme and then does not verify the certificate is worse than one
    /// that says no.
    #[test]
    fn https_is_refused_because_this_client_cannot_verify_it() {
        assert!(split_base("https://node:443").is_err());
    }
}
