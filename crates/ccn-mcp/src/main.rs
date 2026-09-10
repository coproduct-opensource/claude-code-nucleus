//! `ccn-mcp` — the mediated tool surface.
//!
//! A stdio MCP server. Claude Code connects to it, sees one tool per mediated
//! effect, and every call it makes is forwarded into a running nucleus pod where
//! `nucleus-tool-proxy` applies the permission lattice before performing the
//! effect. Nothing in this process performs an effect itself: it is a translator
//! between two wire formats, and that is the whole of its job.
//!
//! ## What this deliberately does not do
//!
//! It does not decide policy. It does not cache. It does not retry. Each of
//! those would put a second, unproven decision procedure outside the microVM,
//! and the value of the design is that there is exactly one — the lattice, on
//! the inside, which is the thing with the machine-checked non-interference
//! result behind it.
//!
//! The one judgement it does make is fail-closed: a transport error becomes a
//! tool error the model can see, never a synthesised success.

mod transport;

use ccn_core::mediated_tools;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use transport::Transport;

/// The MCP protocol revision this server implements.
const PROTOCOL_VERSION: &str = "2025-06-18";

#[tokio::main]
async fn main() {
    let transport = Transport::from_env();
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = tokio::io::stdout();

    // MCP over stdio is newline-delimited JSON-RPC. A line we cannot parse is
    // skipped rather than fatal: killing the server would take the mediated
    // path down and leave the model with nothing but denials.
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        // A notification has no `id` and takes no reply. `notifications/initialized`
        // is the common one; replying to it is a protocol error.
        let Some(id) = req.get("id").cloned() else {
            continue;
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        let response = match method {
            "initialize" => ok(id, initialize()),
            "tools/list" => ok(id, tools_list()),
            "tools/call" => match &transport {
                Ok(t) => match call_tool(t, &params).await {
                    Ok(v) => ok(id, v),
                    Err(text) => ok(id, tool_error(&text)),
                },
                // Misconfiguration surfaces as a tool error rather than a
                // protocol error, so the reason reaches the user's screen
                // instead of a log nobody reads.
                Err(e) => ok(id, tool_error(&e.to_string())),
            },
            "ping" => ok(id, json!({})),
            _ => err(id, -32601, &format!("method not found: {method}")),
        };

        let mut buf = serde_json::to_vec(&response).unwrap_or_default();
        buf.push(b'\n');
        if stdout.write_all(&buf).await.is_err() || stdout.flush().await.is_err() {
            break;
        }
    }
}

fn initialize() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "nucleus", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Every tool here executes inside a Firecracker microVM under the nucleus \
                         permission lattice. Host built-ins are denied by the PreToolUse gate; use \
                         these instead. A refusal from one of these tools is a policy verdict, not \
                         a bug — do not attempt to work around it."
    })
}

/// The advertised tool list, derived from `ccn-core`'s map so the server cannot
/// advertise something the gate does not redirect to, or omit something it does.
fn tools_list() -> Value {
    let tools: Vec<Value> = mediated_tools()
        .into_iter()
        .map(|(name, route)| {
            json!({
                "name": name,
                "description": describe(name, route.0),
                "inputSchema": schema_for(name),
            })
        })
        .collect();
    json!({ "tools": tools })
}

fn describe(name: &str, route: &str) -> String {
    let what = match name {
        "run" => "Run a shell command inside the pod",
        "read" => "Read a file inside the pod",
        "write" => "Write a file inside the pod",
        "glob" => "Match files by glob inside the pod",
        "grep" => "Search file contents inside the pod",
        "web_fetch" => "Fetch a URL through the pod's mediated egress (taints the session)",
        "web_search" => "Search the web through the pod's mediated egress (taints the session)",
        "subagent" => "Start a child pod for a delegated task, with its own flow state",
        _ => "Mediated effect",
    };
    format!("{what}. Enforced by nucleus at {route}; returns a signed mediation receipt.")
}

/// Input schemas mirror the host built-ins' argument names, so the model does
/// not have to learn a second vocabulary when the gate redirects it. The proxy
/// validates properly on the far side; these exist to keep the call shapes
/// familiar, and are permissive on purpose rather than a second validator.
fn schema_for(name: &str) -> Value {
    let (props, required): (Value, Vec<&str>) = match name {
        "run" => (
            json!({
                "command": { "type": "string", "description": "Shell command to run" },
                "timeout_ms": { "type": "integer", "description": "Optional timeout in milliseconds" }
            }),
            vec!["command"],
        ),
        "read" => (
            json!({
                "file_path": { "type": "string", "description": "Absolute path inside the pod" },
                "offset": { "type": "integer" },
                "limit": { "type": "integer" }
            }),
            vec!["file_path"],
        ),
        "write" => (
            json!({
                "file_path": { "type": "string", "description": "Absolute path inside the pod" },
                "content": { "type": "string", "description": "Full file contents" }
            }),
            vec!["file_path", "content"],
        ),
        "glob" => (
            json!({
                "pattern": { "type": "string" },
                "path": { "type": "string", "description": "Directory to search from" }
            }),
            vec!["pattern"],
        ),
        "grep" => (
            json!({
                "pattern": { "type": "string" },
                "path": { "type": "string" },
                "glob": { "type": "string" }
            }),
            vec!["pattern"],
        ),
        "web_fetch" => (
            json!({
                "url": { "type": "string" },
                "prompt": { "type": "string" }
            }),
            vec!["url"],
        ),
        "web_search" => (json!({ "query": { "type": "string" } }), vec!["query"]),
        "subagent" => (
            json!({
                "prompt": { "type": "string", "description": "Task for the child pod" },
                "description": { "type": "string" }
            }),
            vec!["prompt"],
        ),
        _ => (json!({}), vec![]),
    };
    json!({ "type": "object", "properties": props, "required": required })
}

/// Forward one `tools/call` into the pod.
async fn call_tool(transport: &Transport, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("tools/call with no tool name")?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let route = mediated_tools()
        .into_iter()
        .find(|(t, _)| *t == name)
        .map(|(_, r)| r)
        // An unknown MCP tool is refused for the same reason an unknown built-in
        // is: this bridge serves a closed set.
        .ok_or_else(|| format!("`{name}` is not a tool this bridge serves"))?;

    let reply = transport
        .post(route.0, &args)
        .await
        .map_err(|e| e.to_string())?;

    // The proxy's reply carries the result and, when the effect was performed,
    // the mediation receipt. Both are handed back verbatim: summarising a
    // receipt would make it unverifiable, which is the only thing it is for.
    Ok(json!({
        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&reply).unwrap_or_default() }],
        "isError": false
    }))
}

/// A tool-level error: visible to the model, and marked so it cannot be mistaken
/// for a result.
fn tool_error(text: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": format!("nucleus refused or could not complete this call: {text}") }],
        "isError": true
    })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccn_core::{disposition, Disposition};

    /// The server advertises exactly what the gate redirects to. Drift here is
    /// the deadlock case: the model is told to call a tool that is not served.
    #[test]
    fn the_advertised_tools_are_exactly_the_gates_redirect_targets() {
        let listed = tools_list();
        let names: Vec<String> = listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        for builtin in ccn_core::BUILTIN_TOOLS {
            if let Disposition::Mediated { mcp_tool, .. } = disposition(builtin) {
                assert!(
                    names.iter().any(|n| n == mcp_tool),
                    "gate redirects {builtin} -> {mcp_tool}, which is not advertised"
                );
            }
        }
    }

    #[test]
    fn every_advertised_tool_has_a_usable_schema() {
        for t in tools_list()["tools"].as_array().unwrap() {
            let schema = &t["inputSchema"];
            assert_eq!(
                schema["type"], "object",
                "{} has no object schema",
                t["name"]
            );
            assert!(
                schema["properties"].is_object(),
                "{} has no properties",
                t["name"]
            );
        }
    }

    /// Fail-closed: a transport that is not configured must produce an error the
    /// model can see, never an empty success it would read as "the file is
    /// empty" or "the command printed nothing".
    #[test]
    fn a_failure_is_marked_as_an_error_not_an_empty_result() {
        let e = tool_error("socket refused");
        assert_eq!(e["isError"], true);
        assert!(e["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("socket refused"));
    }
}
