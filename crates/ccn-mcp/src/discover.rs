//! Finding the pod, when nobody told us where it is.
//!
//! `NUCLEUS_POD_SOCK` is the transport the README called preferred, and on macOS
//! it cannot exist: `--listen-unix` is the Linux container driver's transport,
//! and a Firecracker pod lives inside the Lima VM where no host path reaches it.
//! Every Mac user therefore takes the HTTP path — and its address is the
//! `proxy_addr` the node chose when the pod was created: a per-pod, ephemeral,
//! node-forwarded loopback port. Nothing can hardcode it, and nothing told the
//! user where to read it.
//!
//! So the first run of this bridge was a working afternoon of source reading
//! ending in a hand-written shell wrapper that asked `nucleus node pods` for a
//! running pod, created one if there was none, and `exec`'d `ccn-mcp` with the
//! address. That wrapper is this module.
//!
//! ## Why shelling out to `nucleus` is the right amount of coupling
//!
//! The alternative is speaking the node's HTTP API here: its URL, its
//! HMAC request signing, its secrets file. That is a second copy of an
//! authentication scheme living outside the pod — the thing this bridge exists
//! not to do. The CLI already holds it, already reads the user's
//! `~/.config/nucleus/config.toml`, and is already installed, because a pod
//! cannot exist without it. Asking it is one process and no secrets here.
//!
//! Nothing in this module performs a mediated effect or decides a policy. It
//! learns an address.

use serde_json::Value;

/// Which pod to look for. A name rather than an id, so the same session comes
/// back to the same pod across restarts.
const DEFAULT_POD_NAME: &str = "claude-code";

/// The spec to create from when no pod is running. Ships in this repo.
const DEFAULT_POD_SPEC: &str = "pod.yaml";

/// Find a running pod's proxy address, creating one if there is none.
///
/// Returns the `http://host:port` base for [`crate::transport::Transport::Http`].
/// Every step is announced on stderr: a bridge that silently boots a microVM
/// would be worse than one that cannot find a pod, and stderr is where an MCP
/// server's operational noise belongs — stdout is the JSON-RPC channel and a
/// stray byte on it breaks the protocol.
pub fn find_or_create_pod() -> Result<String, String> {
    let name = env_or("NUCLEUS_POD_NAME", DEFAULT_POD_NAME);

    if let Some(addr) = running_pod(&name)? {
        note(&format!("reusing pod `{name}` at {addr}"));
        return Ok(addr);
    }

    let spec = env_or("NUCLEUS_POD_SPEC", DEFAULT_POD_SPEC);
    note(&format!(
        "no running pod named `{name}`; creating one from {spec}"
    ));
    let addr = create_pod(&spec)?;
    note(&format!("created pod at {addr}"));
    Ok(addr)
}

/// The `proxy_addr` of a running pod with this name, if there is one.
fn running_pod(name: &str) -> Result<Option<String>, String> {
    let pods = nucleus(&["node", "pods"])?;
    let list = pods
        .as_array()
        .ok_or("`nucleus node pods` did not return a list")?;

    Ok(list
        .iter()
        .find(|p| {
            p.get("name").and_then(Value::as_str) == Some(name)
                // `PodState` is `rename_all = "snake_case"`, so the running
                // variant is the bare string. An exited pod still appears in
                // the listing and still has an address, which no longer serves.
                && p.get("state").and_then(Value::as_str) == Some("running")
        })
        .and_then(|p| p.get("proxy_addr").and_then(Value::as_str))
        .map(as_base_url))
}

fn create_pod(spec: &str) -> Result<String, String> {
    let created = nucleus(&["node", "create", spec])?;
    created
        .get("proxy_addr")
        .and_then(Value::as_str)
        .map(as_base_url)
        .ok_or_else(|| {
            format!(
                "the node created a pod but reported no proxy_addr, so there is nothing to talk \
                 to. Its reply: {created}"
            )
        })
}

/// Run a `nucleus` subcommand and parse its JSON.
///
/// The CLI prints the node's reply pretty-printed on stdout and its own logging
/// on stderr, so the JSON is taken from stdout alone — and stderr is quoted back
/// on failure, because "Connection refused" from the node is the actual answer
/// most of the time and burying it would restart the afternoon this module
/// exists to prevent.
fn nucleus(args: &[&str]) -> Result<Value, String> {
    let out = std::process::Command::new("nucleus")
        .args(args)
        .output()
        .map_err(|e| {
            format!(
                "could not run `nucleus {}`: {e}. Set NUCLEUS_PROXY_URL to a pod's proxy address \
                 to skip pod discovery entirely.",
                args.join(" ")
            )
        })?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        return Err(format!(
            "`nucleus {}` failed: {}",
            args.join(" "),
            cause(&String::from_utf8_lossy(&out.stderr))
        ));
    }

    // The CLI's own `tracing` lines can share stdout with the payload depending
    // on how it was configured, so the JSON is found rather than assumed: it
    // starts at the first `{` or `[` and runs to the end.
    let start = stdout
        .find(['{', '['])
        .ok_or_else(|| format!("`nucleus {}` printed no JSON", args.join(" ")))?;
    serde_json::from_str(&stdout[start..])
        .map_err(|e| format!("could not parse `nucleus {}` output: {e}", args.join(" ")))
}

/// The one line of the CLI's stderr that says what went wrong.
///
/// It writes coloured `tracing` lines to stderr alongside its actual error, so
/// the raw text is several lines of ANSI escapes wrapped around one useful
/// sentence — `Error: List pods failed: io: Connection refused`. Reproducing all
/// of that inside a one-line check result buries the answer. The `Error:` line
/// is taken when there is one, the last non-empty line otherwise, with the
/// escapes removed either way.
fn cause(stderr: &str) -> String {
    let clean: Vec<String> = stderr
        .lines()
        .map(strip_ansi)
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    clean
        .iter()
        .find(|l| l.starts_with("Error:"))
        .or_else(|| clean.last())
        .map(|l| l.trim_start_matches("Error:").trim().to_string())
        .unwrap_or_else(|| "no output".into())
}

/// Drop CSI sequences. Hand-rolled because pulling a terminal crate into a
/// binary on the security boundary to tidy one error message is a poor trade.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // `ESC [ ... <final byte in @..~>`; anything else is dropped whole.
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The node reports `host:port`; the transport wants a URL, and refuses
/// anything that is not `http://` because it cannot verify a certificate.
fn as_base_url(addr: &str) -> String {
    if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", addr.trim_end_matches('/'))
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn note(message: &str) {
    eprintln!("ccn-mcp: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The node reports a bare `host:port`. Handing that to the transport
    /// unchanged fails `split_base`, which is a confusing error for what is a
    /// missing scheme.
    #[test]
    fn a_bare_host_port_becomes_a_url_and_a_url_is_left_alone() {
        assert_eq!(as_base_url("127.0.0.1:52341"), "http://127.0.0.1:52341");
        assert_eq!(as_base_url("http://node:9000/"), "http://node:9000");
        assert_eq!(as_base_url("https://node:9000"), "https://node:9000");
    }

    /// The CLI wraps one useful sentence in coloured tracing output. A check
    /// result is one line, and the answer has to survive being put on it.
    #[test]
    fn the_clis_error_line_survives_its_own_logging() {
        let stderr = "\u{1b}[2m2026-09-15T01:16:07Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \
                      \u{1b}[2mnucleus\u{1b}[0m: Starting nucleus\n\
                      Error: List pods failed: io: Connection refused\n";
        assert_eq!(cause(stderr), "List pods failed: io: Connection refused");
    }

    #[test]
    fn with_no_error_line_the_last_thing_said_is_the_cause() {
        assert_eq!(
            cause("first\nsomething went wrong\n\n"),
            "something went wrong"
        );
        assert_eq!(cause("   \n"), "no output");
    }

    #[test]
    fn the_pod_name_and_spec_are_overridable_but_have_defaults() {
        assert_eq!(env_or("CCN_NOT_A_REAL_VARIABLE", "fallback"), "fallback");
    }
}
