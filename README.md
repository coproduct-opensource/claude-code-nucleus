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
ccn-mcp --check          # and the path behind it must actually work
```

If either fails, stop here. Installing the gate now is what strands a session.

**4. Turn the gate on**, per session:

```sh
claude --settings .claude/settings.nucleus.json \
       --disallowedTools Bash,Read,Write,Edit,Glob,Grep,WebFetch,WebSearch,NotebookEdit,Agent,Task,TodoWrite,BashOutput,KillShell
```

`.claude/settings.nucleus.json` ships in this repo and holds nothing but the hook and the status
line. Or install the whole thing as a plugin — `.claude-plugin/plugin.json` declares the MCP server
and `hooks/hooks.json` the gate, so they arrive in the same load and the ordering problem does not
arise. **Step 1 is still required**: the plugin carries configuration, not binaries, and a plugin
that cannot find `ccn-gate` refuses every tool until you install them (see below).

### The gate is a script, and that is load-bearing

`hooks/hooks.json` points `PreToolUse` at `scripts/gate.sh`, a committed shell script, rather than
straight at the `ccn-gate` binary. The script finds the binary — `$CCN_GATE`, then a bundled
`bin/ccn-gate`, then `PATH` — and **denies when it cannot**, with JSON and with exit 2, which blocks
on its own even if the JSON is never read.

That indirection exists because of how Claude Code treats a hook it cannot start. From the hooks
documentation:

> When the script path doesn't exist or isn't executable, the shell exits with a code like 127 and
> you see the same notice… For most hook events, the action proceeds.

A missing hook is **not** a blocked tool call. It is an unmediated one. So a gate that is a path to a
binary is a gate that is off whenever the binary is absent, silently, while the user believes it is
on — which is exactly what this plugin shipped, since the hook pointed into a gitignored `bin/` that
nothing builds. `ccn-gate`'s every internal error path denies, and none of them could help, because
the process never started.

A committed script is always present, so the deny always happens. CI checks both halves: that the
path in `hooks.json` resolves to an executable file in a clean checkout, and that with no gate binary
anywhere it still denies and still exits 2.

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
| `NUCLEUS_POD_SOCK` | Peer-credential-verified Unix socket (`nucleus-tool-proxy --listen-unix`). The bridge holds no secret at all. **Linux container driver only.** |
| `NUCLEUS_PROXY_URL` | The pod's node-forwarded `http://` surface. **What you use on macOS**, and fine there. |
| `NUCLEUS_SESSION_TOKEN` | Bearer token, HTTP transport only. Read per call, so revocation takes effect immediately. Usually unnecessary — see below. |
| `NUCLEUS_POD_NAME` | Pod to look for when neither transport is set. Default `claude-code`. |
| `NUCLEUS_POD_SPEC` | Spec to create from when no such pod is running. Default `pod.yaml`. |

The socket wins when both are set. Silently preferring the weaker of two configured transports is
how a deployment ends up authenticating with a bearer token nobody knew was still in use. A variable
set to the empty string counts as unset, so `NUCLEUS_POD_SOCK=` is a way to force the HTTP path.

**On macOS the socket cannot exist, and the README used to call it preferred anyway.** `--listen-unix`
is the Linux container driver's transport; a Firecracker pod lives inside the Lima VM, where no host
path reaches it. So every Mac user takes the HTTP hop, and that is the right posture rather than a
downgrade: the address is a loopback port the node forwards, and the secret authenticating it lives
in the node, which HMACs the hop. Nothing is held here, which was the reason to prefer the socket in
the first place.

### Finding the pod

That address is the `proxy_addr` the node assigned when the pod was created — per-pod, ephemeral,
and impossible to hardcode. Nothing used to tell you where to read it.

So when neither variable is set, `ccn-mcp` asks: `nucleus node pods` for a running pod named
`$NUCLEUS_POD_NAME`, and `nucleus node create $NUCLEUS_POD_SPEC` if there is none. Each step is
announced on stderr, because a bridge that silently boots a microVM would be worse than one that
cannot find a pod:

```
ccn-mcp: no running pod named `claude-code`; creating one from pod.yaml
ccn-mcp: created pod at http://127.0.0.1:52341
```

It shells out to `nucleus` rather than speaking the node's API, deliberately: the node's URL, its
HMAC request signing and its secrets file are an authentication scheme, and a second copy of one
living outside the pod is the thing this bridge exists not to do. The CLI already holds it, already
reads your config, and is already installed — a pod cannot exist without it.

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

### What has and has not been run

Stated because the difference matters and this repo has been wrong about it before.

**Run against a live Firecracker pod** (macOS 26.6, Lima 2.2.0, nucleus 1.0.0, artifacts from the
pinned release, pod at tier 2 `spiffe-identity`). `ccn-mcp --check` completes, and every mediated
tool reaches its route and comes back with a *policy verdict* rather than a `422` — which is the
thing that was broken:

```
  [1] run true                              --    refused by policy: approval required: 'RunBash true'
  [2] write ccn-check-da6a2dc8.txt          --    refused by policy: approval required: 'WriteFiles …'
  [3] read it back                          --    refused by policy: access denied: path … blocked by policy
  [4] read /etc/shadow (must be refused)    ok    refused: sandbox escape: … resolves outside sandbox root
```

Individual calls through the MCP surface, against the same pod:

| call | result |
|---|---|
| `glob {"pattern":"*"}` | **served from inside the microVM** — `{"matches":["audit"]}` |
| `read /etc/os-release` | `403` sandbox escape — outside `work_dir` |
| `grep` | `403` kernel denied, `WithinDelegationCeiling` — that pod's grant covers glob and read |
| `web_fetch https://example.com` | `403` **ifc denied** — the flow layer refusing egress |
| `run "ls \| wc -l"` | refused *by the bridge*, before the pod: there is no shell |

So the translation, the refusal-versus-fault split, the escape probe, the shell refusal and the
status line all hold on the real path, not just against a mock.

**Not yet run**: the *discovery* path. A pod created with `nucleus node create` from the host
completes every boot stage — `vmm.preflight`, `net.create_netns`, `net.default_deny`, `prepare_jail`,
`firecracker.spawn`, `seccomp.wait`, `vsock.wait`, `attestation.hash`, `cert.issue` — and then times
out in `proxy.health_wait` after ~30 s. Two contributing causes are known: a second pod requesting a
`vsock.guest_cid` already held by a running one fails this way, and the spec `nucleus verify --tier2`
boots successfully fails the same way when created from the host. Until that is understood, set
`NUCLEUS_PROXY_URL` to a pod's `proxy_addr` (`nucleus node pods` reports it) rather than relying on
`ccn-mcp` to create one.

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

Two halves, and the second is the one that used to be missing.

```sh
ccn-mcp --check
```

```
nucleus bridge check

  transport   http://127.0.0.1:52341 (no token — the node signs this hop)
  health      ok
  tools       7 — run, read, write, glob, grep, web_fetch, web_search

  [1] run true                              ok    the pod executed a command
  [2] write ccn-check-4f0e3950.txt          ok    32 bytes
  [3] read it back                          ok    identical
  [4] read /etc/shadow (must be refused)    ok    refused: resolves outside the sandbox root

the mediated path works, and the boundary refused what it should.
```

Step 4 is the point of it. Steps 1–3 prove calls *arrive*; only a call that must be refused, and
was, proves something is *deciding* when they do. It probes an absolute path outside the pod's root
rather than a capability, so it means the same thing under every profile — a stricter policy than
`codegen` is a choice, and the check reports a policy refusal as the verdict it is rather than
failing on it. What fails: an unreachable pod, a `404` (this pod does not serve a route the bridge
advertises), a `422` (the bridge is sending a body the route cannot read), and step 4 succeeding.

The gate is the other half:

```sh
echo '{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"id"}}' | ccn-gate
```

Expect a `deny` naming `mcp__nucleus__run`. If you get anything else, the boundary is not up.

## Seeing the lattice while you work

Claude Code has no way for a plugin to add a pane — the plugin surface is skills,
agents, hooks, MCP servers, LSP servers, monitors, themes, output styles and workflows, and panes are
first-party. It does have a **status line**: a command re-run on every assistant message and on a
timer, whose stdout becomes rows above the footer.

That is the better fit anyway. A diff pane shows a *delta* you review once; information-flow state is
a **lattice position** — small, monotonic, always true of the session. A gauge, not a document.

`.claude/settings.nucleus.json` turns it on with the gate:

```
nucleus ● 127.0.0.1:52341 · 14 ok · 1 refused
  ⚠ untrusted ~ web_fetch docs.rs · ✗ run git push  git_push is never under codegen
```

The second row appears only when there is something to say; a boundary that is holding and has
refused nothing prints one quiet line. It exists for one defect: **a refusal scrolls past in a single
tool result, and from then on nobody can see why the next write keeps failing.** The reason stays on
the bar until something else refuses.

Three distinctions it is careful about:

- **A refusal is not a fault.** A `403` is the lattice deciding and keeps the dot green; only a
  `404`/`422`/unreachable — this bridge being wrong — turns it red. Counting them together would
  train you to ignore both.
- **A refused fetch does not taint.** It brought nothing in. Reading a denial as taint would punish
  you for the boundary working.
- **`~` marks an inference, and it is not the label.** See below.

### What the bar knows, and what it does not

`~` means everything after it is derived, not read. The bridge **cannot** read the session's label:
`/v1/health` returns counts and deliberately never labels, because it is reachable from inside the
sandbox and must not become a channel for reading back which invariant a probe just tripped. That
refusal is correct, and the status line does not work around it.

What it does instead is derive from its own observations — a `web_fetch` the pod *performed* is
untrusted content entering the session, so integrity has dropped. Sound in the direction that
matters, since it never claims clean when tainted, and still not the label. A true readout needs
nucleus to expose one **node-side**, which is a different endpoint from the one that correctly
refuses.

The bar reads `~/.local/state/ccn/journal.jsonl` (override with `CCN_JOURNAL`), which `ccn-mcp`
appends to as it forwards calls. **That file is an observation log, not evidence.** The
authoritative record is the signed `MediationReceipt` the pod ships to the node; nothing reads the
journal back to decide anything, and `--check` does not consult it. It exists because the receipts
live inside the microVM's node — on macOS, inside the Lima VM — and a status line that re-renders
every few seconds cannot make a VM round trip to draw itself.

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
- **A plugin carries configuration, not binaries.** `cargo install` is a prerequisite of the plugin,
  not an alternative to it. The gate denies every tool until the binaries are there rather than
  letting them through, but a `bin/` of prebuilt binaries per platform is the thing that would make
  the plugin self-contained, and this repo does not build one.
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
- **Receipts are not in the replies at all.** This README used to say each mediated call "returns a
  signed mediation receipt". It does not: a Firecracker guest cannot reach the node over HTTP, so the
  proxy ships each signed `MediationReceipt` over the workload vsock as it is produced, and the node
  collects it at `<node-state>/pods/<pod-id>/collected-receipts.jsonl` — the copy the pod cannot
  retract. Verify them with `nucleus-audit verify-mediation-receipts`. Verification is nucleus's job
  either way; this bridge would only be marking its own homework.

## Licence

MIT OR Apache-2.0.
