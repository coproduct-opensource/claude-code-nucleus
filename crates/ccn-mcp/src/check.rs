//! `ccn-mcp --check` — prove the mediated path, not just the gate.
//!
//! The README's verification piped a hook event into `ccn-gate` and expected a
//! deny. That checks the **gate**, which was never the broken half. Meanwhile
//! every `read`, `write` and `glob` through the mediated path returned 422 and
//! `run` could not succeed at all, and nothing in the repo would have said so.
//!
//! A verification step should exercise the property the repo exists to hold, so
//! this one walks the whole path: reach the pod, run something, write a file and
//! read it back, and — the part that distinguishes enforcement from routing —
//! make a call that **must** be refused and confirm it was.
//!
//! ## What counts as a failure
//!
//! Not "a call was refused". A refusal is a verdict, and a bridge that reported
//! one as a fault would be telling the user their policy is a bug. The failures
//! are the ones that mean *this bridge is broken or is not enforcing*:
//!
//! | Outcome | Verdict |
//! |---|---|
//! | cannot reach the pod | fail — there is no mediated path |
//! | `404` | fail — this pod does not serve a route the bridge advertises |
//! | `422` | fail — the bridge is sending a body the route cannot read |
//! | `403`, or any policy refusal | **pass**, reported as the verdict it is |
//! | the escape probe *succeeding* | fail, loudly — containment is not holding |
//!
//! The last row is the one worth having. Everything above it proves calls
//! arrive; only that row proves something is deciding when they do.

use crate::translate::to_proxy_body;
use crate::transport::{Transport, TransportError};
use serde_json::{json, Value};

/// A path no pod should serve from, used as the refusal probe.
///
/// Deliberately not a profile setting. Probing something like `web_fetch` would
/// test whether *this* profile denies egress, and a profile that allows it would
/// fail a check about the bridge. An absolute path outside the pod's root is
/// refused by containment itself — `Sandbox::root_relative` resolves it against
/// the root and refuses what does not fall under it — so the probe means the
/// same thing under every profile.
const ESCAPE_PROBE: &str = "/etc/shadow";

/// Run the check. Returns the process exit code.
pub async fn run() -> i32 {
    println!("nucleus bridge check\n");

    let transport = match crate::resolve_transport() {
        Ok(t) => t,
        Err(e) => {
            println!("  transport   FAIL  {e}");
            println!("\nthere is no mediated path. Nothing below could be checked.");
            return 1;
        }
    };
    println!("  transport   {}", transport.describe());

    match transport.get("/v1/health").await {
        Ok(_) => println!("  health      ok"),
        Err(e) => {
            println!("  health      FAIL  {e}");
            println!("\nthe pod is not answering. Nothing below could be checked.");
            return 1;
        }
    }

    let tools = ccn_core::mediated_tools();
    let names: Vec<&str> = tools.iter().map(|(t, _)| *t).collect();
    println!("  tools       {} — {}\n", names.len(), names.join(", "));

    // A per-run name, so two checks against the same long-lived pod do not
    // collide and a leftover file from a previous run cannot pass step 3.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    // Short enough to leave the result column where every other line puts it.
    let file = format!("ccn-check-{:08x}.txt", stamp as u32);
    let content = format!("mediated at {stamp}\n");

    let mut failures = 0;
    let mut step = 0;

    // 1. Something runs.
    step += 1;
    failures += act(
        step,
        "run true",
        call(&transport, "run", &json!({ "command": "true" })).await,
        |_| Ok("the pod executed a command".into()),
    );

    // 2. A write is performed.
    step += 1;
    failures += act(
        step,
        &format!("write {file}"),
        call(
            &transport,
            "write",
            &json!({ "file_path": file, "content": content }),
        )
        .await,
        |_| Ok(format!("{} bytes", content.len())),
    );

    // 3. The write was the write. Reading back is what makes step 2 a claim
    //    about the filesystem rather than about a status code.
    step += 1;
    failures += act(
        step,
        "read it back",
        call(&transport, "read", &json!({ "file_path": file })).await,
        |reply| {
            let got = reply.get("contents").and_then(Value::as_str).unwrap_or("");
            if got == content {
                Ok("identical".into())
            } else {
                Err(format!("the pod returned {got:?}, not what was written"))
            }
        },
    );

    // 4. The one that separates enforcement from routing.
    step += 1;
    print!(
        "  [{step}] {:<COLUMN$}",
        format!("read {ESCAPE_PROBE} (must be refused)")
    );
    match call(&transport, "read", &json!({ "file_path": ESCAPE_PROBE })).await {
        Err(CallError::Refused(why)) => println!("ok    refused: {why}"),
        Err(CallError::Broken(why)) => {
            // A 404 or 422 here is still a bridge fault, not a refusal.
            println!("FAIL  {why}");
            failures += 1;
        }
        Ok(_) => {
            println!("FAIL  the pod served it");
            println!(
                "\n  This is the serious one. A path outside the pod's root was read successfully,\n  \
                 so the containment boundary is not holding and every other line above is\n  \
                 routing rather than enforcement."
            );
            failures += 1;
        }
    }

    println!();
    // Said here because the tool descriptions used to claim otherwise, and a
    // user who goes looking for a receipt in a reply will not find one.
    println!(
        "  Receipts are not in these replies. The pod ships each signed MediationReceipt to\n  \
         the node over vsock as it is produced; they are collected at\n  \
         <node-state>/pods/<pod-id>/collected-receipts.jsonl and verified with\n  \
         `nucleus-audit verify-mediation-receipts`."
    );

    println!();
    if failures == 0 {
        println!("the mediated path works, and the boundary refused what it should.");
        0
    } else {
        println!("{failures} check(s) failed — the mediated path is not sound.");
        1
    }
}

/// What a mediated call did, split by whose fault it is.
enum CallError {
    /// A verdict. The lattice, the path policy or the command policy said no.
    Refused(String),
    /// A fault in this bridge or in the pod's surface: unreachable, a route the
    /// pod does not serve, or a body it cannot deserialise.
    Broken(String),
}

async fn call(transport: &Transport, tool: &str, args: &Value) -> Result<Value, CallError> {
    let body = to_proxy_body(tool, args).map_err(CallError::Broken)?;
    let route = ccn_core::mediated_tools()
        .into_iter()
        .find(|(t, _)| *t == tool)
        .map(|(_, r)| r)
        .ok_or_else(|| CallError::Broken(format!("`{tool}` is not served")))?;

    transport.post(route.0, &body).await.map_err(|e| match e {
        // 403 and 409 are the lattice and the flow state deciding. Anything else
        // in the 4xx/5xx range is the surface being wrong.
        TransportError::Status { status, ref body } if status == 403 || status == 409 => {
            CallError::Refused(summarise(body))
        }
        TransportError::Status { status: 404, .. } => CallError::Broken(format!(
            "404 — this pod does not serve {route}. The bridge advertises a tool the pod has not \
             got."
        )),
        TransportError::Status {
            status: 422,
            ref body,
        } => CallError::Broken(format!(
            "422 — {route} could not deserialise the body this bridge sent: {}",
            summarise(body)
        )),
        other => CallError::Broken(other.to_string()),
    })
}

/// Width of the description column, so every result starts in the same place.
const COLUMN: usize = 38;

/// Print one act's line and return 1 if it counted as a failure.
fn act(
    n: usize,
    what: &str,
    outcome: Result<Value, CallError>,
    verify: impl FnOnce(&Value) -> Result<String, String>,
) -> usize {
    print!("  [{n}] {what:<COLUMN$}");
    match outcome {
        Ok(reply) => match verify(&reply) {
            Ok(detail) => {
                println!("ok    {detail}");
                0
            }
            Err(why) => {
                println!("FAIL  {why}");
                1
            }
        },
        // A refusal is a verdict, and the check says so rather than failing:
        // a stricter profile than `codegen` is a choice, not a defect.
        Err(CallError::Refused(why)) => {
            println!("--    refused by policy: {why}");
            0
        }
        Err(CallError::Broken(why)) => {
            println!("FAIL  {why}");
            1
        }
    }
}

/// The proxy's error bodies are JSON with a `reason`/`error` field; fall back to
/// the raw text, trimmed, so a line stays a line.
fn summarise(body: &str) -> String {
    let text = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            ["reason", "error", "message", "detail"]
                .iter()
                .find_map(|k| v.get(*k).and_then(Value::as_str).map(str::to_string))
        })
        .unwrap_or_else(|| body.trim().to_string());
    text.chars().take(160).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe has to be refused by *containment* rather than by a profile
    /// setting, or the check fails on a permissive pod for no good reason.
    #[test]
    fn the_escape_probe_is_outside_any_pod_root() {
        assert!(
            ESCAPE_PROBE.starts_with('/') && !ESCAPE_PROBE.starts_with("/work"),
            "the probe must be an absolute path outside the pod's work_dir"
        );
    }

    #[test]
    fn an_error_body_is_summarised_to_its_reason() {
        assert_eq!(
            summarise(r#"{"error":"sandbox_escape","reason":"resolves outside the root"}"#),
            "resolves outside the root"
        );
        assert_eq!(summarise("  plain text  "), "plain text");
    }

    /// A refusal is a verdict, not a fault. If this inverts, a stricter profile
    /// than `codegen` starts reporting itself as a broken bridge.
    #[test]
    fn a_policy_refusal_is_not_counted_as_a_failure() {
        let refused = act(
            1,
            "probe",
            Err(CallError::Refused("denied by lattice".into())),
            |_| Ok(String::new()),
        );
        assert_eq!(refused, 0);

        let broken = act(1, "probe", Err(CallError::Broken("422".into())), |_| {
            Ok(String::new())
        });
        assert_eq!(broken, 1);
    }

    /// Step 3 exists to catch a write that reported success without writing.
    #[test]
    fn a_read_back_that_does_not_match_is_a_failure() {
        let mismatched = act(
            3,
            "read it back",
            Ok(json!({ "contents": "something else" })),
            |reply| {
                if reply["contents"] == "expected" {
                    Ok("identical".into())
                } else {
                    Err("mismatch".into())
                }
            },
        );
        assert_eq!(mismatched, 1);
    }
}
