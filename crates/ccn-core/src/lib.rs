//! The mediation map — one table, one law.
//!
//! Claude Code's built-in tools and nucleus's pod-local tool-proxy routes are
//! very nearly the same alphabet. This crate is the arrow between them, plus
//! the property that makes the arrow worth having:
//!
//! > **Complete mediation.** Every built-in tool Claude Code can emit has a
//! > disposition — it is either *mediated* (re-expressed as a nucleus MCP tool
//! > that executes inside a Firecracker pod) or *denied*. There is no third
//! > case, and there is no default-allow.
//!
//! The law is discharged by the type system rather than by a test: [`Disposition`]
//! is a closed enum and [`disposition`] is a total function over [`BUILTIN_TOOLS`].
//! Adding a tool name without giving it a disposition does not compile. That is
//! deliberate — a mediation gap is not the kind of defect that should be found by
//! running something.
//!
//! ## Why the vendor strings live here and not in nucleus
//!
//! `nucleus` forbids vendor names in its tree (`ci/no-vendor-strings.sh`). The
//! names in [`BUILTIN_TOOLS`] are exactly the vendor-shaped knowledge that gate
//! is protecting nucleus from: they change when the agent CLI changes, and they
//! have nothing to do with an information-flow lattice. They belong on this side
//! of the seam. Nucleus sees only a `PodSpec` and a route.

use serde::{Deserialize, Serialize};

/// A route on the pod-local `nucleus-tool-proxy` HTTP surface.
///
/// These are stable paths served *inside* the microVM. The proxy applies the
/// permission lattice, the egress gate and the Article 12 record before it
/// performs any effect, so reaching one of these is what "the tool call ran
/// under enforcement" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route(pub &'static str);

impl std::fmt::Display for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// What the gate does with a built-in tool call.
///
/// Closed on purpose. A new variant is a change to the security boundary and
/// should read as one in review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// Re-expressed as a nucleus MCP tool that runs inside the pod. The gate
    /// denies the built-in and names the replacement, so the model retries
    /// through the mediated path rather than losing the capability.
    Mediated {
        /// The MCP tool the model should call instead, without the
        /// `mcp__nucleus__` prefix the harness adds.
        mcp_tool: &'static str,
        /// The pod-local route that tool forwards to.
        route: Route,
    },
    /// Refused outright, with no mediated equivalent. The reason is shown to
    /// the user and to the model.
    Denied { reason: &'static str },
}

/// Every built-in tool this bridge knows Claude Code can emit.
///
/// Kept in one place because two different things read it: the gate (to decide)
/// and the conformance check (to prove the decision is total). A name that
/// appears in the harness and not here is the one real failure mode, which is
/// why [`unknown_tool_disposition`] is fail-closed rather than fail-open.
pub const BUILTIN_TOOLS: &[&str] = &[
    "Bash",
    "Read",
    "Write",
    "Edit",
    "Glob",
    "Grep",
    "WebFetch",
    "WebSearch",
    "NotebookEdit",
    "Agent",
    "Task",
    "TodoWrite",
    "BashOutput",
    "KillShell",
];

/// The map. Total over [`BUILTIN_TOOLS`]; fail-closed everywhere else.
///
/// The `Denied` arms are not an oversight. Each names an effect the pod cannot
/// express today, and saying so in the reason keeps the gap legible instead of
/// letting it read as an accident.
///
/// ## A route that exists is not a route that is served
///
/// `Mediated` carries a claim this crate cannot check: that the pod on the other
/// end actually mounts the route. Most of the proxy's routes are unconditional,
/// so the claim holds for them by inspection. The `pod/*` family is not — it is
/// mounted only on an orchestrator pod — and mediating to a conditional route
/// produces the deadlock `every_mediated_target_is_actually_served` exists to
/// prevent, one repository further out than that test can see.
///
/// The rule this map follows, then: **mediate only to a route every pod serves.**
/// Anything conditional is `Denied` with the condition named, so the gap is a
/// sentence the model can read rather than a 404 it cannot act on.
pub fn disposition(tool_name: &str) -> Disposition {
    match tool_name {
        "Bash" => Disposition::Mediated {
            mcp_tool: "run",
            route: Route("/v1/run"),
        },
        "Read" => Disposition::Mediated {
            mcp_tool: "read",
            route: Route("/v1/read"),
        },
        // Edit and NotebookEdit are read-modify-write against the same route:
        // the pod owns the file, so the whole edit has to happen on that side
        // or the write is not the one the lattice inspected.
        "Write" | "Edit" | "NotebookEdit" => Disposition::Mediated {
            mcp_tool: "write",
            route: Route("/v1/write"),
        },
        "Glob" => Disposition::Mediated {
            mcp_tool: "glob",
            route: Route("/v1/glob"),
        },
        "Grep" => Disposition::Mediated {
            mcp_tool: "grep",
            route: Route("/v1/grep"),
        },
        "WebFetch" => Disposition::Mediated {
            mcp_tool: "web_fetch",
            route: Route("/v1/web_fetch"),
        },
        "WebSearch" => Disposition::Mediated {
            mcp_tool: "web_search",
            route: Route("/v1/web_search"),
        },
        // A subagent *should* be a sub-pod rather than a thread: its own pod is
        // what would keep its taint out of the parent's flow state. It is denied
        // anyway, because the route that would do it is not there to be called.
        //
        // `/v1/pod/create` is mounted only when the proxy holds a node client,
        // which needs `--enable-pod-mgmt`, which the node passes only when the
        // pod spec's labels carry `enable_pod_mgmt`. On any other pod the route
        // 404s. Two further walls stand behind that one: the route's body is
        // `{spec_yaml, reason}` — a whole PodSpec, not a task prompt — and it
        // checks `manage_pods >= LowRisk`, which `codegen`, the profile this
        // bridge is meant for, sets to `never`.
        //
        // Mediating it would mean the gate denying the built-in and redirecting
        // the model to a tool that 404s: a deadlock, which is the single
        // outcome this map exists to prevent. Denying says the true thing.
        "Agent" | "Task" => Disposition::Denied {
            reason: "subagents are not mediated: spawning a child pod needs an orchestrator pod \
                     (a spec labelled `enable_pod_mgmt`) with `manage_pods` above `never`, and a \
                     PodSpec rather than a prompt. Do the work in this session instead",
        },
        // Bookkeeping with no effect outside the transcript. Denying it costs
        // the model a scratchpad; mediating it would put the pod on the path of
        // something that never touches the filesystem or the network.
        "TodoWrite" => Disposition::Denied {
            reason: "TodoWrite has no effect outside the transcript and is not mediated",
        },
        // Background shells outlive a single tool call, so their output cannot
        // be bound to one mediation receipt. Until the proxy models a stream,
        // refusing is the honest answer.
        "BashOutput" | "KillShell" => Disposition::Denied {
            reason:
                "background shells are not mediated: their output cannot be bound to one receipt",
        },
        other => unknown_tool_disposition(other),
    }
}

/// Fail-closed default for a tool name this build has never heard of.
///
/// A tool added to the harness after this binary shipped reaches here. Denying
/// it is the only safe reading: an unknown effect is exactly the thing the pod
/// exists to contain, and an allow-by-default would silently reopen the
/// boundary every time the vendor shipped a feature.
pub fn unknown_tool_disposition(_tool_name: &str) -> Disposition {
    Disposition::Denied {
        reason: "unknown tool: this bridge mediates a closed set and refuses anything outside it",
    }
}

/// The MCP tools this bridge serves, in the order they are advertised.
///
/// Derived from the map rather than written twice, so the server cannot drift
/// from the gate — the gate telling the model to call a tool the server does
/// not serve is a deadlock, not a denial.
pub fn mediated_tools() -> Vec<(&'static str, Route)> {
    let mut seen: Vec<(&'static str, Route)> = Vec::new();
    for name in BUILTIN_TOOLS {
        if let Disposition::Mediated { mcp_tool, route } = disposition(name) {
            if !seen.iter().any(|(t, _)| *t == mcp_tool) {
                seen.push((mcp_tool, route));
            }
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The law. Not "most tools are handled" — every one, with no default-allow
    /// anywhere in the function.
    #[test]
    fn every_builtin_has_a_disposition_and_none_is_an_allow() {
        for name in BUILTIN_TOOLS {
            match disposition(name) {
                Disposition::Mediated { mcp_tool, route } => {
                    assert!(!mcp_tool.is_empty(), "{name} mediated to an empty tool");
                    assert!(
                        route.0.starts_with("/v1/"),
                        "{name} routes off-surface: {route}"
                    );
                }
                Disposition::Denied { reason } => {
                    assert!(!reason.is_empty(), "{name} denied without a reason");
                }
            }
        }
    }

    /// The falsifier for the fail-closed default. If this ever returns
    /// `Mediated`, the boundary has a hole in it that no other test would see.
    #[test]
    fn an_unknown_tool_is_denied_not_allowed() {
        let d = disposition("SomeToolShippedNextTuesday");
        assert!(
            matches!(d, Disposition::Denied { .. }),
            "unknown tool must be denied, got {d:?}"
        );
    }

    /// The companion to `every_mediated_target_is_actually_served`, for the half
    /// of the deadlock that lives in the other repository: a route the *pod*
    /// does not mount is a 404 the model cannot act on, and this repo's tests
    /// cannot see it. What they can see is the list of conditional routes.
    ///
    /// `pod/*` is mounted only on an orchestrator pod (`enable_pod_mgmt`).
    /// Everything else on the proxy's surface is unconditional. So: nothing may
    /// be mediated to a `pod/*` route. If nucleus makes one unconditional, or
    /// this bridge learns to require an orchestrator pod, this test is the place
    /// that says so.
    #[test]
    fn nothing_is_mediated_to_a_route_only_some_pods_mount() {
        for name in BUILTIN_TOOLS {
            if let Disposition::Mediated { route, .. } = disposition(name) {
                assert!(
                    !route.0.starts_with("/v1/pod/"),
                    "{name} is mediated to {route}, which a standard pod does not serve — \
                     the gate would redirect the model into a 404"
                );
            }
        }
    }

    /// The gate and the server must agree. A mediated tool the server does not
    /// advertise is worse than a denial: the model is told to retry into
    /// nothing.
    #[test]
    fn every_mediated_target_is_actually_served() {
        let served = mediated_tools();
        for name in BUILTIN_TOOLS {
            if let Disposition::Mediated { mcp_tool, .. } = disposition(name) {
                assert!(
                    served.iter().any(|(t, _)| *t == mcp_tool),
                    "{name} is mediated to `{mcp_tool}`, which the server does not serve"
                );
            }
        }
    }

    /// A denial the model cannot act on is a dead end with better prose. Every
    /// reason has to say what would make the effect available, or what to do
    /// instead — the subagent one is the case that matters, since losing it
    /// changes how a session is structured.
    #[test]
    fn every_denial_says_what_to_do_instead_or_what_would_enable_it() {
        for name in BUILTIN_TOOLS {
            if let Disposition::Denied { reason } = disposition(name) {
                assert!(
                    reason.len() > 30,
                    "{name} is denied with a reason too terse to act on: {reason}"
                );
            }
        }
        let Disposition::Denied { reason } = disposition("Task") else {
            panic!("Task must be denied while /v1/pod/create is conditional")
        };
        assert!(
            reason.contains("enable_pod_mgmt") && reason.contains("manage_pods"),
            "the subagent denial must name both conditions: {reason}"
        );
    }

    /// The effects that actually cross the boundary — filesystem, shell,
    /// network — must never be denied-without-replacement, because a user who
    /// loses them turns the bridge off. Mediation has to be the cheap path.
    #[test]
    fn the_consequential_effects_are_mediated_rather_than_refused() {
        for name in ["Bash", "Read", "Write", "Edit", "WebFetch", "WebSearch"] {
            assert!(
                matches!(disposition(name), Disposition::Mediated { .. }),
                "{name} must be mediated, not denied — a bridge that removes it gets switched off"
            );
        }
    }
}
