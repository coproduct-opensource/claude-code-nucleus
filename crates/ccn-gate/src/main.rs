//! `ccn-gate` — the complete-mediation boundary.
//!
//! Claude Code runs this on `PreToolUse` for every tool call. It reads the hook
//! event on stdin and writes a permission decision on stdout.
//!
//! The rule is one line: **a built-in tool never executes on the host.** Either
//! it has a mediated equivalent that runs inside the pod, in which case the gate
//! denies the built-in and tells the model which nucleus tool to call instead,
//! or it has none, in which case the gate denies it and says why.
//!
//! ## Why a hook and not just `--disallowedTools`
//!
//! Both, actually, and the asymmetry matters. `--disallowedTools` is a list, so
//! it cannot be complete — a tool added or renamed after the list was written is
//! not on it. This gate is a *default*: anything it does not recognise is denied
//! (`ccn_core::unknown_tool_disposition`). The list is defence in depth; the
//! gate is the boundary. Nucleus's own CLI draws the line the same way, and this
//! is the same reasoning moved to the vendor side of the seam.
//!
//! ## Failure posture
//!
//! A hook that crashes must not become a hook that allows. Every error path here
//! ends in a deny, and the process exits 0 with that deny rather than exiting
//! non-zero with nothing — an unparsable event is exactly when the boundary is
//! most likely to be under attack.

use ccn_core::{disposition, Disposition};
use serde::Deserialize;
use std::io::Read;

/// The subset of the `PreToolUse` event this gate reads.
///
/// Deliberately narrow. The gate decides from the tool's *name*, never from its
/// arguments: an argument-sensitive decision here would be a second policy
/// engine sitting outside the pod, which is the thing this design exists to
/// avoid. Argument-level policy belongs to the lattice, inside the microVM.
#[derive(Debug, Deserialize)]
struct PreToolUse {
    #[serde(default)]
    tool_name: String,
}

/// The MCP server name Claude Code registers this bridge under. Tools are
/// addressed as `mcp__<server>__<tool>`, which is what the model must call.
const MCP_SERVER: &str = "nucleus";

fn main() {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return emit_deny("nucleus gate could not read the hook event; refusing the call");
    }

    let event: PreToolUse = match serde_json::from_str(&raw) {
        Ok(e) => e,
        // An event we cannot parse is not an event we can clear.
        Err(_) => {
            return emit_deny("nucleus gate could not parse the hook event; refusing the call")
        }
    };

    // Calls into this bridge's own MCP server are already inside the pod — the
    // proxy on the other end is what enforces them. Re-denying them here would
    // deadlock the mediated path the gate itself just recommended.
    if event.tool_name.starts_with(&format!("mcp__{MCP_SERVER}__")) {
        return emit_allow();
    }

    match disposition(&event.tool_name) {
        Disposition::Mediated { mcp_tool, .. } => emit_deny(&format!(
            "`{}` does not run on the host. Call `mcp__{MCP_SERVER}__{mcp_tool}` instead — same \
             arguments, executed inside the Firecracker pod under the nucleus permission lattice, \
             and it returns a signed mediation receipt.",
            event.tool_name
        )),
        Disposition::Denied { reason } => emit_deny(&format!(
            "`{}` is refused by the nucleus bridge: {reason}.",
            event.tool_name
        )),
    }
}

/// Emit a deny. Exit 0 — the JSON carries the decision, and a non-zero exit
/// would be read as a hook malfunction rather than as a verdict.
fn emit_deny(reason: &str) {
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    });
    println!("{out}");
}

/// Emit an allow. Reached only for this bridge's own mediated tools, whose
/// enforcement happens inside the pod.
fn emit_allow() {
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "permissionDecisionReason": "mediated by nucleus inside the pod",
        }
    });
    println!("{out}");
}
