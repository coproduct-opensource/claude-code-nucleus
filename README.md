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
| `nothing_is_mediated_to_a_route_only_some_pods_mount` | the same deadlock one repo out — a route the *pod* does not mount, which is a 404 the model cannot act on |
| `every_denial_says_what_to_do_instead_or_what_would_enable_it` | a refusal too terse to act on |

## Install

Requires a running nucleus pod. `nucleus setup --install-deps` provisions the *node* on Apple
Silicon (M3+, macOS 15+); a pod is a separate step, and the spec it should use ships here as
[`pod.yaml`](pod.yaml) — see [Which pod, and where a write lands](#which-pod-and-where-a-write-lands),
because that file decides the reach of every mediated call.

**The order below is load-bearing.** A `PreToolUse` hook takes effect in the session that writes it,
immediately. An MCP server is read once, at session start. So installing the gate before the
mediated path exists gives that session every denial and none of the replacements — including the
`Write` needed to undo it. Register the server, restart, *check*, and only then install the gate.

**1. Build the binaries.**

```sh
cargo install --git https://github.com/coproduct-opensource/claude-code-nucleus ccn-gate ccn-mcp
```

**2. Register the MCP server** (`.mcp.json`, or `claude mcp add`):

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

**3. Restart Claude Code**, and confirm the mediated path is actually there:

```sh
claude mcp list          # `nucleus` must be listed and Connected
```

If it is not, stop here. Installing the gate now is what strands a session.

**4. Turn the gate on**, per session:

```sh
claude --settings .claude/settings.nucleus.json \
       --disallowedTools Bash,Read,Write,Edit,Glob,Grep,WebFetch,WebSearch,NotebookEdit,Agent,Task,TodoWrite,BashOutput,KillShell
```

`.claude/settings.nucleus.json` ships in this repo and holds nothing but the hook. Or install the
whole thing as a plugin — the manifest in `.claude-plugin/` ships the gate and the server together,
so they arrive in the same load and the ordering problem does not arise.

### Opt-in, not `.claude/settings.json`

Putting the hook in `.claude/settings.json` applies it to every session in the directory, which is
right for a repo the agent should never touch directly and wrong for most others — in particular
**this one**. `cargo build`, `cargo test` and `git` are all host `Bash`; a contributor who installs
the gate into this checkout locks themselves out of it. The separate file is the switch.

If a session does end up stranded with no working tool, `!` at the prompt runs a command outside the
hooks:

```
! git checkout .claude/settings.json
```

### Transport

| Variable | Meaning |
|---|---|
| `NUCLEUS_POD_SOCK` | Peer-credential-verified Unix socket (`nucleus-tool-proxy --listen-unix`). **Preferred** — the bridge holds no secret at all. |
| `NUCLEUS_PROXY_URL` | Node-forwarded `http://` surface, for a non-local pod. |
| `NUCLEUS_SESSION_TOKEN` | Bearer token, HTTP transport only. Read per call, so revocation takes effect immediately. |

The socket wins when both are set. Silently preferring the weaker of two configured transports is
how a deployment ends up authenticating with a bearer token nobody knew was still in use.

## Which pod, and where a write lands

The gate refuses everything on the host and `ccn-mcp` forwards it into the pod, so **the pod's spec,
not the gate, decides the reach of a mediated call.** A bridge that achieves complete mediation
against an unspecified policy has achieved complete *routing*: the call certainly reaches the
lattice, and what the lattice then permits is out of frame. So the spec ships here:

```sh
nucleus node create pod.yaml
```

[`pod.yaml`](pod.yaml) is `work_dir: /work` under the `codegen` profile, with its choices explained
inline — including the label it deliberately omits (`enable_pod_mgmt`, which is why `Agent`/`Task`
is denied). Swap the profile to change what the session may do; nothing in this bridge needs to know
which one you pick. `nucleus profiles` lists them.

### A mediated write does not edit your working tree

This is the part the diagram above will mislead you about, so it is stated plainly.

Under the Firecracker driver — nucleus's production default — the pod's filesystem is the microVM's,
and **there is no host directory in it.** That is permanent rather than unimplemented: Firecracker
rejected virtio-fs on attack-surface grounds and a 9p implementation before it, and `PodSpec` has no
mounts, shares or volumes field to add one with. `work_dir` is a guest path.

So `Write` through this bridge creates a file *inside the pod*. The repository you have open in your
editor is not touched. Getting a source tree in front of the model means putting it in the pod:
baked into the rootfs, or handed over as `image.data_path` (a read-only block device — the supported
way to put a corpus in front of a workload), with `image.scratch_path` for what the session writes.
**Exporting the result back out is not solved here**, and that is the honest state of it: this bridge
is usable today for work that begins and ends inside the pod, and incomplete for editing a checkout
in place.

The exception is `DriverKind::Local`, which runs the proxy as a host subprocess — "process-only.
Dev/test; refused in production". There a mediated write does reach host files, bounded only by
`work_dir` and the path policy. See Known Gaps for what that implies about the gate.

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

The `--disallowedTools` argument in step 4 is the second layer:

```
Bash,Read,Write,Edit,Glob,Grep,WebFetch,WebSearch,NotebookEdit,Agent,Task,TodoWrite,BashOutput,KillShell
```

That list is *not* the boundary and cannot be — a tool added or renamed after it was written is not
on it. The gate is the boundary, because it is a default rather than a list. Use both.

It is derived from `BUILTIN_TOOLS` by `ccn_core::disallowed_tools_arg` and checked against this
README in CI, because a hand-maintained copy of a list is the thing that goes stale: the version
printed here previously was missing `Task`, one of the two names for the tool it did list.

## Known gaps

Named here rather than discovered later:

- **Inference is outside the boundary.** See above. Effects are contained; context is not.
- **The gate is a hook, and the user is not the only one who can edit it.** `disableAllHooks`, an
  uninstalled plugin, or an edited `settings.json` all remove it; there is no in-band enforcement
  that survives the harness being reconfigured. Worth saying who can do the editing: `Write`, `Edit`
  and `NotebookEdit` are mediated to `/v1/write`, so the model has a *sanctioned* write path, and
  `.claude/settings.json` is the file that installs the gate mediating it. That is the constrained
  party removing its own constraint by the route the bridge provides, not operator error — and
  `.mcp.json` is the worse version, since rewriting it adds an **unmediated** MCP server rather than
  merely removing a hook.

  Whether it is reachable is decided by the driver, not by this repo. Under Firecracker it is not:
  no host directory is in the pod, so a mediated write cannot touch the harness config (see above).
  Under `DriverKind::Local` it is live, and there the pod's `work_dir` and path policy are the only
  thing standing between the model and the gate's own configuration — keep the harness config
  outside `work_dir`. Nucleus's canonical profiles block credential material (`**/.ssh/**`,
  `**/.env*`, `**/credentials*`) but none of them blocks `.claude/` or `.mcp.json`, filed as
  [nucleus#2782](https://github.com/coproduct-opensource/nucleus/issues/2782).
- **`Edit` is mediated as a whole-file `write`.** The pod owns the file, so a partial edit would have
  to be applied on the far side; today the model reads then writes. Semantically weaker than the
  built-in.
- **Background shells are refused, not mediated.** `BashOutput`/`KillShell` outlive a single call, so
  their output cannot be bound to one receipt. Refusing is the honest answer until the proxy models
  a stream.
- **Subagents are refused, not mediated.** A subagent ought to be a sub-pod with its own flow state,
  and `POST /v1/pod/create` exists — but it is mounted only on an orchestrator pod (a spec labelled
  `enable_pod_mgmt`), its body is a whole `PodSpec` rather than a prompt, and it needs `manage_pods`
  above `never`, which `codegen` does not grant. Mediating to it redirected the model into a 404. The
  denial names all three conditions; `Agent`/`Task` work again if nucleus makes the route
  unconditional or this bridge learns to require an orchestrator pod.
- **The pod's filesystem is not your working tree.** Stated above and repeated here because it is the
  gap most likely to surprise: under the production driver a mediated write lands inside the microVM,
  and there is no export step. Work that begins and ends in the pod is fine; editing a checkout in
  place is not supported yet.
- **Receipts are passed through, not verified here.** Verification is
  [`nucleus-verifier`](https://github.com/coproduct-opensource/nucleus)'s job; this bridge would only
  be marking its own homework.

## Licence

MIT OR Apache-2.0.
