//! What the bridge saw, written down so a status line can read it.
//!
//! # This is an observation log, not evidence
//!
//! Say this first because the distinction is the whole reason the file needs a
//! doc comment. The authoritative record of a mediated call is the signed
//! `MediationReceipt` the tool-proxy ships to the node over vsock, collected at
//! `<node-state>/pods/<pod-id>/collected-receipts.jsonl` — host-private, and the
//! copy the pod cannot retract. **This file is not that.** It is the bridge's own
//! note of what it sent and what came back, written on the host by a process the
//! agent's own effects never touch but which is not itself attested.
//!
//! So it is fit for an at-a-glance affordance and unfit for anything else:
//!
//! * nothing reads it back to make a decision — no policy, no gate, no retry;
//! * `--check` does not consult it, and neither does any test of the boundary;
//! * a verdict here is what the bridge *observed*, and a verifier must go to the
//!   receipts.
//!
//! Treating it as evidence would be marking our own homework, which is the same
//! reason this bridge does not verify receipts either.
//!
//! # Why not read the receipts directly
//!
//! Because they are inside the microVM's node. On macOS the node runs in a Lima
//! VM, so every read would be a `limactl shell` — and a status line re-renders
//! on every assistant message and on a timer. A VM round trip at that cadence is
//! not an option, and a cache of a VM round trip is this file with more steps.
//!
//! # Why the bridge may write it at all
//!
//! `main.rs` says this process performs no effects. That is a claim about the
//! *agent's* effects: every one of those crosses the boundary into the pod. This
//! is the bridge's own bookkeeping on its own machine, in the user's state
//! directory, mode 0600, and a failure to write it is swallowed — a full disk
//! must never turn into a failed tool call.

use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;

/// Keep the file small enough that reading the tail is free. Trimmed on append
/// once it grows past this, to the most recent [`KEEP_ENTRIES`].
const MAX_BYTES: u64 = 256 * 1024;
const KEEP_ENTRIES: usize = 500;

/// How long a subject may be before it is cut. Paths and URLs can be arbitrary;
/// a status line has about eighty columns.
const SUBJECT_MAX: usize = 160;

/// What the bridge observed a call to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The pod performed the effect.
    Allowed,
    /// A verdict: the lattice, the path policy or the command policy said no.
    /// This is the one worth surfacing — a refusal the user cannot see is the
    /// defect the status line exists to fix.
    Refused(String),
    /// The call did not reach a verdict: unreachable, or a body the route could
    /// not read. A fault, not a decision.
    Failed(String),
}

impl Outcome {
    fn tag(&self) -> &'static str {
        match self {
            Outcome::Allowed => "allow",
            Outcome::Refused(_) => "refuse",
            Outcome::Failed(_) => "fail",
        }
    }

    fn detail(&self) -> Option<&str> {
        match self {
            Outcome::Allowed => None,
            Outcome::Refused(d) | Outcome::Failed(d) => Some(d),
        }
    }
}

/// One observed call.
#[derive(Debug, Clone)]
pub struct Entry {
    pub tool: String,
    /// What the call was against — a path, a URL, a command. Truncated.
    pub subject: String,
    pub outcome: Outcome,
    /// Which pod, so a status line can name it and two pods cannot be confused.
    pub pod: String,
}

/// Append one observation. Never fails the caller.
///
/// Every error path here is deliberately silent. This is bookkeeping; a status
/// line going stale is not worth turning a working tool call into a broken one,
/// and there is no user-visible surface at this point in the call anyway.
pub fn record(entry: &Entry) {
    let Some(path) = path() else { return };
    record_at(&path, entry);
}

/// [`record`] against an explicit file.
///
/// The env lookup lives in [`path`] and nowhere else, so the behaviour can be
/// tested without `set_var` — which is process-global and would make these tests
/// race each other under the default parallel runner.
pub fn record_at(path: &std::path::Path, entry: &Entry) {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }

    let line = serde_json::json!({
        "ts": now_unix(),
        "tool": entry.tool,
        "subject": truncate(&entry.subject, SUBJECT_MAX),
        "outcome": entry.outcome.tag(),
        "detail": entry.outcome.detail().map(|d| truncate(d, SUBJECT_MAX)),
        "pod": entry.pod,
    });

    let mut open = std::fs::OpenOptions::new();
    open.create(true).append(true);
    // The subjects are the user's paths and URLs. Their own machine, their own
    // state directory, and nobody else's business.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    if let Ok(mut f) = open.open(path) {
        let _ = writeln!(f, "{line}");
    }

    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
        trim(path);
    }
}

/// Rewrite the file as its most recent [`KEEP_ENTRIES`] lines.
///
/// Not atomic, and it does not need to be: losing this file loses a status
/// line's history and nothing else. A rename dance would imply it mattered.
fn trim(path: &std::path::Path) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = raw.lines().collect();
    let keep = lines.len().saturating_sub(KEEP_ENTRIES);
    let _ = std::fs::write(path, format!("{}\n", lines[keep..].join("\n")));
}

/// The most recent entries, oldest first. Empty when there is no journal, which
/// is the normal state of a session that has not made a mediated call yet.
pub fn recent(limit: usize) -> Vec<Entry> {
    match path() {
        Some(p) => recent_at(&p, limit),
        None => Vec::new(),
    }
}

/// [`recent`] against an explicit file.
pub fn recent_at(path: &std::path::Path, limit: usize) -> Vec<Entry> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = raw.lines().collect();
    let start = lines.len().saturating_sub(limit);
    lines[start..].iter().filter_map(|l| parse(l)).collect()
}

fn parse(line: &str) -> Option<Entry> {
    let v: Value = serde_json::from_str(line).ok()?;
    let detail = v
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(Entry {
        tool: v.get("tool")?.as_str()?.to_string(),
        subject: v
            .get("subject")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        outcome: match v.get("outcome").and_then(Value::as_str)? {
            "allow" => Outcome::Allowed,
            "refuse" => Outcome::Refused(detail),
            _ => Outcome::Failed(detail),
        },
        pod: v
            .get("pod")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// `$CCN_JOURNAL`, else `$XDG_STATE_HOME/ccn/journal.jsonl`, else
/// `~/.local/state/ccn/journal.jsonl`.
///
/// One file rather than one per session, because flow state is a property of the
/// **pod**, not of a conversation: two Claude Code sessions pointed at the same
/// pod share its taint, and a status line that showed only this session's half
/// would be wrong about what the next call may do. Entries carry `pod` so a
/// reader can tell them apart.
pub fn path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("CCN_JOURNAL") {
        let p = PathBuf::from(explicit);
        return (!p.as_os_str().is_empty()).then_some(p);
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("ccn").join("journal.jsonl"))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Cut on a character boundary, marking that it was cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// The subject a call is about, for the one line a status line can spare.
///
/// Each mediated tool names the thing it acts on differently, and the useful
/// half is never the whole argument object.
pub fn subject_of(tool: &str, args: &Value) -> String {
    let field = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default();
    match tool {
        "read" | "write" => field("file_path").to_string(),
        "run" => field("command").to_string(),
        "glob" | "grep" => {
            let (p, path) = (field("pattern"), field("path"));
            if path.is_empty() {
                p.to_string()
            } else {
                format!("{p} in {path}")
            }
        }
        "web_fetch" => field("url").to_string(),
        "web_search" => field("query").to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A journal file of this test's own.
    ///
    /// No `set_var`: the env lookup is [`path`]'s job and is tested separately.
    /// Tests run in parallel threads of one process, so anything process-global
    /// makes them race — which is exactly what the first version of these tests
    /// did.
    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("ccn-journal-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Temp(dir.join("journal.jsonl"))
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            if let Some(dir) = self.0.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    fn entry(tool: &str, outcome: Outcome) -> Entry {
        Entry {
            tool: tool.into(),
            subject: "s".into(),
            outcome,
            pod: "127.0.0.1:1".into(),
        }
    }

    #[test]
    fn what_was_written_is_what_comes_back() {
        let t = Temp::new("roundtrip");
        record_at(&t.0, &entry("read", Outcome::Allowed));
        record_at(
            &t.0,
            &entry("web_fetch", Outcome::Refused("denied by lattice".into())),
        );

        let got = recent_at(&t.0, 10);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].tool, "read");
        assert_eq!(got[0].outcome, Outcome::Allowed);
        assert_eq!(got[1].tool, "web_fetch");
        assert_eq!(
            got[1].outcome,
            Outcome::Refused("denied by lattice".into()),
            "the reason is the whole point of recording a refusal"
        );
    }

    /// The common case on a fresh machine, and it must not look like an error.
    #[test]
    fn no_journal_is_an_empty_history_rather_than_a_failure() {
        let t = Temp::new("absent");
        assert!(recent_at(&t.0, 10).is_empty());
    }

    /// A status line reads this on every assistant message. It cannot be allowed
    /// to grow without bound.
    #[test]
    fn the_file_is_trimmed_rather_than_grown_forever() {
        let t = Temp::new("trim");
        let long = "x".repeat(2000);
        for _ in 0..400 {
            record_at(
                &t.0,
                &Entry {
                    subject: long.clone(),
                    ..entry("read", Outcome::Allowed)
                },
            );
        }
        let size = std::fs::metadata(&t.0).unwrap().len();
        assert!(size <= MAX_BYTES * 2, "journal grew to {size} bytes");
        assert!(recent_at(&t.0, 10_000).len() <= KEEP_ENTRIES + 1);
    }

    /// A line we cannot parse is skipped, not fatal: the reader is a status
    /// line, and half a history beats a broken bar.
    #[test]
    fn a_corrupt_line_is_skipped_not_fatal() {
        let t = Temp::new("corrupt");
        record_at(&t.0, &entry("read", Outcome::Allowed));
        let mut f = std::fs::OpenOptions::new().append(true).open(&t.0).unwrap();
        writeln!(f, "{{not json").unwrap();
        drop(f);
        record_at(&t.0, &entry("write", Outcome::Allowed));

        let got = recent_at(&t.0, 10);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].tool, "write");
    }

    #[test]
    fn a_subject_is_the_argument_that_names_the_thing() {
        assert_eq!(
            subject_of("read", &json!({ "file_path": "src/a.rs" })),
            "src/a.rs"
        );
        assert_eq!(
            subject_of("run", &json!({ "command": "cargo test" })),
            "cargo test"
        );
        assert_eq!(
            subject_of("web_fetch", &json!({ "url": "https://docs.rs/x" })),
            "https://docs.rs/x"
        );
        assert_eq!(
            subject_of("glob", &json!({ "pattern": "*.rs", "path": "src" })),
            "*.rs in src"
        );
        assert_eq!(subject_of("glob", &json!({ "pattern": "*.rs" })), "*.rs");
    }

    /// Truncation happens on a character boundary; a path with multi-byte
    /// characters must not panic.
    #[test]
    fn truncation_does_not_split_a_character() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate(&"é".repeat(10), 4), "ééé…");
    }
}
