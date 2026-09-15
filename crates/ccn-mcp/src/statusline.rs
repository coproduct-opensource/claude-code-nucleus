//! `ccn-mcp --statusline` — the lattice position, continuously.
//!
//! Claude Code has no way for a plugin to add a pane. It does have a status
//! line: a command re-run on every assistant message and on a timer, whose
//! stdout is rendered as rows above the footer, with colour and hyperlinks.
//!
//! That turns out to be the better fit anyway. A diff pane shows a *delta* you
//! review once. Information-flow state is a **lattice position** — small,
//! monotonic, and always true of the session. It is a gauge, not a document, and
//! a gauge belongs on a bar.
//!
//! ## The defect this is for
//!
//! A mediated call gets refused, the reason goes past in one tool result, and
//! from then on neither the user nor the model can see why the next write keeps
//! failing. The receipt says why, node-side, where nobody is looking. So:
//!
//! ```text
//! nucleus ● 127.0.0.1:52341 · 14 ok · 2 refused
//!   ⚠ untrusted ~ web_fetch docs.rs · ✗ web_fetch: denied by lattice
//! ```
//!
//! The second row appears only when there is something to say. A boundary that
//! is holding and has refused nothing prints one quiet line.
//!
//! ## What the `~` means, and why it is not a lie
//!
//! `~` marks an **inference**, and everything after it is inferred. The bridge
//! cannot read the session's label: `/v1/health` returns counts and deliberately
//! never labels, because it is reachable from inside the sandbox and must not
//! become a channel for reading back which invariant a probe just tripped. That
//! refusal is correct and this module does not work around it.
//!
//! What it does instead is derive from its own observations: a `web_fetch` or
//! `web_search` that the pod *performed* is untrusted content entering the
//! session, so integrity has dropped. That is sound in the direction it matters
//! — it never claims clean when tainted — and it is not the label. Anything that
//! needs the label needs nucleus to expose one node-side.

use crate::journal::{self, Entry, Outcome};
use std::io::Read;

/// How far back to read. More than a bar can say, few enough to stay free.
const WINDOW: usize = 200;

/// Rendered width to aim for. The status line gets no `columns` field, so this
/// is a budget rather than a measurement.
const WIDTH: usize = 80;

/// What a taint source gets of the second row. The reason is not fixed: it takes
/// whatever the rest of the row leaves, because real refusals are long.
///
/// Measured against a live pod, nucleus's refusals arrive with a layer prefix
/// before the part that says what happened —
/// `ifc denied: discharge denied: InScopeWithTask: operation WebFetch is not...`.
/// A fixed half-row budget spent all of it on the prefix and rendered
/// `ifc denied: discharge denied: InScopeWi...`, which names the layer and not
/// the cause. The prefix is worth keeping (it says *which* layer refused), so
/// the row gives the reason everything it has instead.
const SUBJECT_BUDGET: usize = WIDTH / 4;

pub fn run() -> i32 {
    // Claude Code writes the session JSON to stdin. Nothing here needs it, but
    // it has to be drained: leaving it unread risks the writer seeing EPIPE.
    let mut ignored = String::new();
    let _ = std::io::stdin().read_to_string(&mut ignored);

    for line in render(&journal::recent(WINDOW), colours_wanted()) {
        println!("{line}");
    }
    0
}

/// `NO_COLOR` is honoured, and a status line that is being captured rather than
/// displayed should not be full of escape sequences either.
fn colours_wanted() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

struct Palette {
    dim: &'static str,
    green: &'static str,
    yellow: &'static str,
    red: &'static str,
    reset: &'static str,
}

const COLOUR: Palette = Palette {
    dim: "\x1b[2m",
    green: "\x1b[32m",
    yellow: "\x1b[33m",
    red: "\x1b[31m",
    reset: "\x1b[0m",
};

const PLAIN: Palette = Palette {
    dim: "",
    green: "",
    yellow: "",
    red: "",
    reset: "",
};

/// One or two rows, from the history alone.
///
/// Split out from [`run`] so the whole rendering is testable without a journal,
/// a pod, or a terminal.
pub fn render(entries: &[Entry], colour: bool) -> Vec<String> {
    let p = if colour { &COLOUR } else { &PLAIN };

    if entries.is_empty() {
        return vec![format!(
            "{}nucleus{} {}● gate on · no mediated calls yet{}",
            p.green, p.reset, p.dim, p.reset
        )];
    }

    let allowed = entries
        .iter()
        .filter(|e| e.outcome == Outcome::Allowed)
        .count();
    let refused = entries
        .iter()
        .filter(|e| matches!(e.outcome, Outcome::Refused(_)))
        .count();
    let failed = entries
        .iter()
        .filter(|e| matches!(e.outcome, Outcome::Failed(_)))
        .count();

    let pod = entries
        .iter()
        .rev()
        .map(|e| e.pod.as_str())
        .find(|p| !p.is_empty())
        .unwrap_or("pod");

    // The dot is the health of the *bridge*, not a verdict: a refusal is the
    // boundary working. Only a fault — unreachable, a body the route could not
    // read — turns it red.
    let (dot, dot_colour) = if failed > 0 {
        ("●", p.red)
    } else {
        ("●", p.green)
    };

    let mut first = format!(
        "{}nucleus{} {dot_colour}{dot}{} {}{pod}{} {}·{} {allowed} ok",
        p.green, p.reset, p.reset, p.dim, p.reset, p.dim, p.reset
    );
    if refused > 0 {
        first.push_str(&format!(
            " {}·{} {}{refused} refused{}",
            p.dim, p.reset, p.yellow, p.reset
        ));
    }
    if failed > 0 {
        first.push_str(&format!(
            " {}·{} {}{failed} failed{}",
            p.dim, p.reset, p.red, p.reset
        ));
    }

    let mut rows = vec![first];
    if let Some(second) = alerts(entries, p) {
        rows.push(second);
    }
    rows
}

/// The second row, when there is something worth a row.
fn alerts(entries: &[Entry], p: &Palette) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    if let Some(source) = taint_source(entries) {
        parts.push(format!(
            "{}⚠ untrusted{} {}~ {} {}{}",
            p.yellow,
            p.reset,
            p.dim,
            source.tool,
            short(&source.subject, SUBJECT_BUDGET),
            p.reset
        ));
    }

    // The most recent refusal, which is the thing a user is trying to understand
    // when they look at the bar at all.
    if let Some(last) = entries
        .iter()
        .rev()
        .find(|e| matches!(e.outcome, Outcome::Refused(_)))
    {
        let Outcome::Refused(why) = &last.outcome else {
            unreachable!("filtered to refusals")
        };
        let why = if why.is_empty() {
            "refused"
        } else {
            why.as_str()
        };
        // Tool, subject and reason: which call, against what, and why. The
        // subject is what turns "write refused" into something actionable.
        let subject = short(&last.subject, SUBJECT_BUDGET);
        let subject = if subject.is_empty() {
            String::new()
        } else {
            format!(" {subject}")
        };
        // Whatever the row has left: two leading spaces, the separator when a
        // taint part precedes this one, and the width of tool + subject.
        let used = 2
            + parts.iter().map(|part| visible(part) + 3).sum::<usize>()
            + last.tool.chars().count()
            + subject.chars().count()
            + 3;
        parts.push(format!(
            "{}✗ {}{subject}{} {}{}{}",
            p.red,
            last.tool,
            p.reset,
            p.dim,
            short(why, WIDTH.saturating_sub(used).max(24)),
            p.reset
        ));
    }

    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "  {}",
        parts.join(&format!(" {}·{} ", p.dim, p.reset))
    ))
}

/// The call that put untrusted content into the session, if one did.
///
/// Egress the pod *performed*. A `web_fetch` that was refused brought nothing
/// in, so it does not taint — treating a denial as taint would punish the user
/// for the boundary doing its job.
fn taint_source(entries: &[Entry]) -> Option<&Entry> {
    entries.iter().rev().find(|e| {
        e.outcome == Outcome::Allowed && matches!(e.tool.as_str(), "web_fetch" | "web_search")
    })
}

/// Width as a terminal renders it: escape sequences occupy no columns.
fn visible(s: &str) -> usize {
    let mut n = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            n += 1;
        }
    }
    n
}

/// Squeeze a subject into what is left of the row.
///
/// A URL loses its scheme before it loses its host, because the host is the part
/// that tells you where the untrusted bytes came from.
fn short(s: &str, budget: usize) -> String {
    let s = s
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    if s.chars().count() <= budget {
        return s.to_string();
    }
    let kept: String = s.chars().take(budget.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(tool: &str, subject: &str, outcome: Outcome) -> Entry {
        Entry {
            tool: tool.into(),
            subject: subject.into(),
            outcome,
            pod: "127.0.0.1:52341".into(),
        }
    }

    fn plain(entries: &[Entry]) -> Vec<String> {
        render(entries, false)
    }

    /// A session that has not called anything says so, rather than looking
    /// broken or printing nothing at all.
    #[test]
    fn an_empty_history_still_says_the_gate_is_on() {
        let rows = plain(&[]);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains("gate on"), "{rows:?}");
    }

    /// A boundary that is holding gets one quiet row. A bar that always has two
    /// rows of warnings is a bar people stop reading.
    #[test]
    fn a_clean_session_is_one_quiet_row() {
        let rows = plain(&[e("read", "src/a.rs", Outcome::Allowed)]);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].contains("1 ok"));
    }

    /// The defect this exists for: the reason a call was refused survives past
    /// the turn it happened in.
    #[test]
    fn a_refusal_is_still_visible_several_calls_later() {
        let rows = plain(&[
            e(
                "web_fetch",
                "https://docs.rs/x",
                Outcome::Refused("denied by lattice".into()),
            ),
            e("read", "a.rs", Outcome::Allowed),
            e("read", "b.rs", Outcome::Allowed),
            e("read", "c.rs", Outcome::Allowed),
        ]);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].contains("1 refused"));
        assert!(rows[1].contains("web_fetch"), "{}", rows[1]);
        assert!(rows[1].contains("denied by lattice"), "{}", rows[1]);
    }

    /// Sound in the direction that matters: a performed fetch shows as taint.
    #[test]
    fn a_performed_fetch_shows_the_session_as_untrusted() {
        let rows = plain(&[e("web_fetch", "https://docs.rs/x", Outcome::Allowed)]);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[1].contains("untrusted"), "{}", rows[1]);
        assert!(
            rows[1].contains('~'),
            "inference must be marked: {}",
            rows[1]
        );
        assert!(rows[1].contains("docs.rs"), "name the source: {}", rows[1]);
    }

    /// The falsifier for the inference. A refused fetch brought nothing in, so
    /// claiming taint would punish the user for the boundary working.
    #[test]
    fn a_refused_fetch_does_not_taint_the_session() {
        let rows = plain(&[e(
            "web_fetch",
            "https://x",
            Outcome::Refused("egress denied".into()),
        )]);
        assert!(
            !rows.iter().any(|r| r.contains("untrusted")),
            "a denied fetch must not read as taint: {rows:?}"
        );
    }

    /// A fault is not a verdict, and the two must not look the same: a refusal
    /// is the boundary deciding, a failure is the bridge broken.
    #[test]
    fn a_fault_is_counted_apart_from_a_refusal() {
        let rows = plain(&[
            e("read", "a", Outcome::Failed("422".into())),
            e("write", "b", Outcome::Refused("path blocked".into())),
        ]);
        assert!(rows[0].contains("1 failed"), "{}", rows[0]);
        assert!(rows[0].contains("1 refused"), "{}", rows[0]);
    }

    /// Both at once still fit on one row.
    #[test]
    fn taint_and_a_refusal_share_the_second_row() {
        let rows = plain(&[
            e("web_fetch", "https://docs.rs/x", Outcome::Allowed),
            e(
                "run",
                "git push",
                Outcome::Refused("git_push is never".into()),
            ),
        ]);
        assert_eq!(rows.len(), 2);
        assert!(rows[1].contains("untrusted") && rows[1].contains("git_push is never"));
    }

    /// `NO_COLOR` is honoured, and the plain form carries the same words.
    #[test]
    fn no_colour_output_has_no_escapes_and_loses_nothing() {
        let entries = [e("web_fetch", "https://docs.rs/x", Outcome::Allowed)];
        let plain_rows = render(&entries, false);
        let colour_rows = render(&entries, true);
        assert!(!plain_rows.iter().any(|r| r.contains('\x1b')));
        assert!(colour_rows.iter().any(|r| r.contains('\x1b')));
        assert_eq!(plain_rows.len(), colour_rows.len());
        assert!(plain_rows[1].contains("untrusted"));
    }

    /// The bar has about eighty columns. Long paths must not push the row into
    /// a wrap that hides the part that matters.
    #[test]
    fn a_long_subject_is_cut_rather_than_wrapping_the_row() {
        let rows = plain(&[e(
            "web_fetch",
            &format!("https://example.com/{}", "a".repeat(400)),
            Outcome::Allowed,
        )]);
        assert!(rows[1].chars().count() < WIDTH, "{}", rows[1]);
    }

    /// A URL keeps its host when it is cut: the host is what says where the
    /// untrusted bytes came from.
    #[test]
    fn shortening_a_url_keeps_the_host() {
        assert_eq!(short("https://docs.rs/x", SUBJECT_BUDGET), "docs.rs/x");
        assert!(short(
            &format!("https://docs.rs/{}", "a".repeat(200)),
            SUBJECT_BUDGET
        )
        .starts_with("docs.rs/"));
    }

    /// The reason is why the user looked at the bar. Cutting it to a prefix that
    /// stops before the verb — "resolves outside the sand…" — wastes the row.
    #[test]
    fn a_refusal_keeps_enough_of_the_reason_to_act_on() {
        let rows = plain(&[e(
            "read",
            "/etc/shadow",
            Outcome::Refused("resolves outside the sandbox root".into()),
        )]);
        assert!(
            rows[1].contains("resolves outside the sandbox root"),
            "the reason was cut: {}",
            rows[1]
        );
        // The real shape from a live pod: a layer prefix before the cause. Both
        // halves have to survive, or the row names the layer and not the reason.
        let live = plain(&[e(
            "web_fetch",
            "https://example.com",
            Outcome::Refused("ifc denied: discharge denied: InScopeWithTask".into()),
        )]);
        assert!(
            live[1].contains("InScopeWithTask"),
            "the cause was cut off by the layer prefix: {}",
            live[1]
        );
        assert!(
            rows[1].contains("/etc/shadow"),
            "name the subject: {}",
            rows[1]
        );
    }
}
