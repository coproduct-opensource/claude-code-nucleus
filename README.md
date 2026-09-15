# claude-code-nucleus

**Every Claude Code tool call executes inside a Firecracker microVM, or it does not execute.**

This is the vendor-side half of a seam. [`nucleus`](https://github.com/coproduct-opensource/nucleus)
is a vendor-agnostic secure runtime for AI agents — it enforces what an agent may do and proves the
enforcement boundary sound — and it holds that neutrality with a CI gate (`ci/no-vendor-strings.sh`)
that fails the build on any LLM vendor's name. This repository is where the vendor-specific
knowledge lives instead: tool names, the hook protocol, the MCP wire format. Nucleus sees a
`PodSpec` and a route. It never sees which assistant is on the other end.

## The shape

```
  ┌─ host ─────────────────────────────┐      ┌─ Firecracker microVM ──────────┐
  │                                    │      │                                │
  │   Claude Code                      │      │   nucleus-tool-proxy           │
  │     │                              │      │     ├ permission lattice       │
  │     ├─ PreToolUse ──→ ccn-gate     │      │     ├ egress gate              │
  │     │                  └─ DENY ────┼──┐   │     ├ Article 12 record        │
  │     │                              │  │   │     └ mediation receipt        │
  │     └─ mcp__nucleus__* ─→ ccn-mcp ─┼──┼──→│         │                      │
  │                                    │  │   │         └─→ the actual effect  │
  └────────────────────────────────────┘  │   └────────────────────────────────┘
                                          │
                          every built-in effect is refused on the host
```

Two binaries:

- **`ccn-gate`** — a `PreToolUse` hook. It denies every built-in tool. When the tool has a mediated
  equivalent it names it in the denial, so the model retries through the pod rather than losing the
  capability. Anything it does not recognise is denied too.
- **`ccn-mcp`** — a stdio MCP server exposing one tool per mediated effect. Each call is forwarded
  into the pod, where the lattice decides before any effect happens. This process performs no
  effects itself; it is a translator between two wire formats.

## Why this is not "sandbox Claude Code"

The established approach is to put the whole agent process inside the isolation boundary — a
container, a VM, a Firecracker microVM — so that its file tools, MCP servers and hooks are all
confined ([Claude Code sandbox docs](https://code.claude.com/docs/en/sandbox-environments)). That
works, and it has a cost: the agent's whole environment moves, including credentials, editor
integration, and the terminal you were using.

This design inverts it. **The agent stays on the host; every effect moves.** What you get:

- A tool call is not merely *contained*, it is *adjudicated* — the lattice decides, and a signed
  mediation receipt records who decided, what they decided, and the hash of the Article 12 record.
- Taint is tracked across calls. A `web_fetch` marks the session, and a later privileged write is
  refused by ancestry rather than by a classifier guessing at strings.
- A subagent gets its own pod, so its taint does not flow into the parent.

And what you do not get, stated plainly: **the model's context is not inside the boundary.** The
prompt, the transcript, and everything Claude has read stay on the host. This design contains
*effects*, not *inference*. If your threat model is a hostile model rather than a confused deputy,
you want the whole process inside a VM as well — see nucleus's
[adversarial-model posture](https://github.com/coproduct-opensource/nucleus/blob/main/docs/adversarial-model-posture.md).

## Complete mediation, and how it is enforced

The property this repo exists to hold:

> Every built-in tool Claude Code can emit has a disposition — *mediated* or *denied*. There is no
> third case and no default-allow.

It is discharged by the type system, not by a test. `Disposition` is a closed enum and
`ccn_core::disposition` is total; the fall-through arm calls `unknown_tool_disposition`, which
denies. A tool that ships in a future Claude Code release reaches that arm and is refused. That is
the deliberate trade: **a new built-in is unusable until the map is updated, rather than
unmediated.**

Three tests are the falsifiers, and each fails loudly rather than subtly:

| Test | Catches |
|---|---|
| `every_builtin_has_a_disposition_and_none_is_an_allow` | a tool silently permitted |
| `an_unknown_tool_is_denied_not_allowed` | the fail-closed default regressing to fail-open |
| `every_mediated_target_is_actually_served` | the gate redirecting to a tool the server does not serve (a deadlock, not a denial) |

## Install

Requires a running nucleus pod. `nucleus setup --install-deps` provisions one on Apple Silicon
(M3+, macOS 15+); this repo does not provision it.

```sh
cargo install --git https://github.com/coproduct-opensource/claude-code-nucleus ccn-gate ccn-mcp
```

Register the MCP server (`.mcp.json`, or `claude mcp add`):

```json
{
  "mcpServers": {
    "nucleus": {
      "command": "ccn-mcp",
      "env": { "NUCLEUS_POD_SOCK": "/run/nucleus/pod.sock" }
    }
  }
}
```

Install the gate (`.claude/settings.json`):

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "*", "hooks": [{ "type": "command", "command": "ccn-gate", "timeout": 10 }] }
    ]
  }
}
```

Or install the whole thing as a plugin — the manifest in `.claude-plugin/` ships both.

### Transport

| Variable | Meaning |
|---|---|
| `NUCLEUS_POD_SOCK` | Peer-credential-verified Unix socket (`nucleus-tool-proxy --listen-unix`). **Preferred** — the bridge holds no secret at all. |
| `NUCLEUS_PROXY_URL` | Node-forwarded `http://` surface, for a non-local pod. |
| `NUCLEUS_SESSION_TOKEN` | Bearer token, HTTP transport only. Read per call, so revocation takes effect immediately. |

The socket wins when both are set. Silently preferring the weaker of two configured transports is
how a deployment ends up authenticating with a bearer token nobody knew was still in use.

## What the model will find inside the pod

The mediated tools keep the built-ins' argument names — `ccn-mcp` translates them to the proxy's
(`file_path` → `path`, `content` → `contents`, `Glob`'s `path` → `directory`), so a redirect is
actionable with the arguments already in hand. Four differences are *not* hidden, because the pod
cannot honour them and a bridge that pretended otherwise would fail on the far side:

| | |
|---|---|
| **`run` has no shell.** | `/v1/run` executes one program with arguments, and nucleus's default command policy blocks `sh -c`. Pipes, `&&`, `;`, redirection, `$VAR`, backticks and unquoted globs are **refused with the reason and the tool that does express it** — never split on whitespace and passed through, which would run `ls` against the literal arguments `|` and `wc`. Quoting and backslash escapes work. Use `run`'s `directory` instead of `cd`. |
| **`write` needs an existing parent.** | `run mkdir -p <dir>` first. |
| **`read` returns the whole file.** | There is no `offset`/`limit`. |
| **`web_fetch` does not summarise.** | It returns the response; the built-in's `prompt` has no analogue. |

Paths may be absolute under the pod's `work_dir` or relative to it; anything else is refused as a
sandbox escape.

## Verify it is on

```sh
echo '{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"id"}}' | ccn-gate
```

Expect a `deny` naming `mcp__nucleus__run`. If you get anything else, the boundary is not up.

## Defence in depth

Also pass `--disallowedTools Bash,Read,Write,Edit,Glob,Grep,WebFetch,WebSearch,NotebookEdit,Agent`.
That list is *not* the boundary and cannot be — a tool added or renamed after it was written is not
on it. The gate is the boundary, because it is a default rather than a list. Use both.

## Known gaps

Named here rather than discovered later:

- **Inference is outside the boundary.** See above. Effects are contained; context is not.
- **The gate is a hook.** `disableAllHooks`, an uninstalled plugin, or a `settings.json` the user
  edited all remove it. There is no in-band enforcement that survives the harness being
  reconfigured — that is a property of running the agent on the host.
- **`Edit` is mediated as a whole-file `write`.** The pod owns the file, so a partial edit would have
  to be applied on the far side; today the model reads then writes. Semantically weaker than the
  built-in.
- **Background shells are refused, not mediated.** `BashOutput`/`KillShell` outlive a single call, so
  their output cannot be bound to one receipt. Refusing is the honest answer until the proxy models
  a stream.
- **Receipts are passed through, not verified here.** Verification is
  [`nucleus-verifier`](https://github.com/coproduct-opensource/nucleus)'s job; this bridge would only
  be marking its own homework.

## Licence

MIT OR Apache-2.0.
