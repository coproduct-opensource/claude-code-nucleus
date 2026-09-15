#!/bin/sh
#
# The gate, as something that exists.
#
# `hooks/hooks.json` used to point `PreToolUse` straight at
# `${CLAUDE_PLUGIN_ROOT}/bin/ccn-gate`. `/bin` is gitignored and nothing builds
# into it, so in an installed plugin that path was never there — and a hook whose
# command cannot start is a *non-blocking* error in Claude Code: the shell exits
# 127, the transcript shows a notice, and the tool call proceeds. Installing this
# plugin therefore left every built-in running on the host while the user
# believed the boundary was up.
#
# `ccn-gate`'s own module doc says a hook that crashes must not become a hook
# that allows. Every error path inside that binary denies. The binary was never
# reached, which is the one failure mode those paths cannot cover — so the thing
# the hook points at has to be a file that is always present, and it has to deny
# when it cannot find the real gate. This is that file.
#
# It is committed rather than built, so it exists in a fresh clone, in a plugin
# cache, and in a release tarball alike.
set -u

# A refusal from here is a *malfunction*, not a verdict, so it blocks two ways:
# the JSON is the structured form, and exit 2 blocks on its own even if the JSON
# is never parsed. `ccn-gate` itself exits 0 with its JSON, deliberately — there
# a deny IS the verdict and a non-zero exit would read as the hook breaking.
# Here the hook really is broken, and both belts are wanted.
deny() {
    printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"%s"}}\n' "$1"
    printf 'ccn-gate: %s\n' "$1" >&2
    exit 2
}

# In order: an explicit override, a binary the plugin shipped beside this script,
# then whatever `cargo install` put on PATH. The middle case is what a release
# that does bundle binaries would populate; the last is what the README's install
# produces today.
gate="${CCN_GATE:-}"

if [ -z "$gate" ] && [ -n "${CLAUDE_PLUGIN_ROOT:-}" ] && [ -x "${CLAUDE_PLUGIN_ROOT}/bin/ccn-gate" ]; then
    gate="${CLAUDE_PLUGIN_ROOT}/bin/ccn-gate"
fi

if [ -z "$gate" ]; then
    gate="$(command -v ccn-gate 2>/dev/null)" || gate=""
fi

if [ -z "$gate" ] || [ ! -x "$gate" ]; then
    deny "the nucleus bridge is installed but its binaries are not, so nothing is mediating this call. Install them with: cargo install --git https://github.com/coproduct-opensource/claude-code-nucleus ccn-gate ccn-mcp -- then restart Claude Code. Refusing every tool until then, because the alternative is running them unmediated."
fi

# stdin, stdout and the exit code all pass straight through.
exec "$gate"
