#!/bin/sh
#
# Is the boundary actually up ON THIS HOST, right now?
#
# WHY THIS EXISTS
#
# The README already reasons this out one level down, and stops one level short:
#
#   "A missing hook is not a blocked tool call. It is an unmediated one. So a gate that is a
#    path to a binary is a gate that is off whenever the binary is absent, SILENTLY, while the
#    user believes it is on."
#
# That was fixed by pointing `PreToolUse` at a committed script that denies when it cannot find
# `ccn-gate`, and CI checks both halves: that the path in `hooks.json` resolves to an executable
# file in a clean checkout, and that with no gate binary it denies.
#
# Both of those are properties of the REPOSITORY. Neither can observe the fact that decides
# whether any of it is in force: whether this plugin is installed and enabled on the machine
# where the tools actually run. A plugin that is not installed does not deny — it is simply
# absent. No hook runs, no JSON is emitted, nothing exits 2, and nothing says so. That is the
# same sentence the README already wrote, one scope wider, and the limitations section names it:
#
#   "`disableAllHooks`, an UNINSTALLED PLUGIN, or an edited `settings.json` all remove it;
#    there is no in-band enforcement that survives the harness being reconfigured."
#
# THE COST, MEASURED. On 2026-09-21 this repository's OTHER hook -- `disk-warn.sh`, written the
# day before after the first outage, whose comment describes the failure exactly -- did not fire,
# because the plugin was not installed. The host filled. Both Lima VMs died. The CI VM's ext4
# needed a host-side `e2fsck` (wrong free-block counts, inode-bitmap differences).
# `crates/gatehouse-ca/src/lib.rs` in a runner workspace became 11337 bytes of NUL while
# `git status` called the tree clean, and gatehouse CI stalled with 98 jobs queued and no runner
# to take them. Every one of those symptoms is in `disk-warn.sh`'s comment as the thing it exists
# to prevent. It was never given the chance.
#
# WHAT THIS IS, AND IS NOT
#
# It is NOT enforcement, and it must not be read as any. It reads host configuration, and the
# constrained party can edit host configuration -- that is the README's point about the model
# having a sanctioned write path to the very file that installs the gate. A check that the
# constrained party can also edit adds no authority.
#
# What it adds is that ABSENCE STOPS BEING SILENT. There is a difference between a control that
# can be removed and a control whose removal nothing reports, and only the second one costs you a
# day. This answers one question, out loud: is the boundary up on this host, right now.
#
# Run it after installing, and when anything looks wrong that "slow CI" would also explain.
set -u

SETTINGS="${HOME}/.claude/settings.json"
INSTALLED="${HOME}/.claude/plugins/installed_plugins.json"
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

problems=0
say()  { printf '  %s\n' "$1"; }
bad()  { printf '  NOT IN FORCE: %s\n' "$1"; problems=$((problems + 1)); }

printf 'boundary check for %s\n\n' "$root"

# ---- 1. the hooks the repo declares, and whether they can start -----------------------------
# This half CI already covers, and it is repeated here because a stale checkout on a developer's
# machine is not the clean checkout CI tested.
manifest="$root/hooks/hooks.json"
if [ ! -f "$manifest" ]; then
    bad "hooks/hooks.json is missing; this checkout declares no hooks at all"
else
    # Read the commands without a JSON parser: the manifest is committed and its shape is fixed,
    # and depending on python3 here would make the check need more than the thing it checks.
    for rel in $(sed -n 's/.*"command": *"\${CLAUDE_PLUGIN_ROOT}\/\([^"]*\)".*/\1/p' "$manifest"); do
        if [ -x "$root/$rel" ]; then
            say "declared hook is present and executable: $rel"
        else
            bad "hooks.json points at $rel, which is not an executable file here"
        fi
    done
fi

# ---- 2. is the plugin actually installed and enabled on THIS host? --------------------------
# The fact CI cannot see.
name=$(sed -n 's/.*"name": *"\([^"]*\)".*/\1/p' "$root/.claude-plugin/plugin.json" 2>/dev/null | head -1)
[ -n "$name" ] || name="claude-code-nucleus"

installed=no
[ -f "$INSTALLED" ] && grep -q "\"${name}@" "$INSTALLED" 2>/dev/null && installed=yes

enabled=no
if [ -f "$SETTINGS" ] && grep -q "\"${name}@[^\"]*\": *true" "$SETTINGS" 2>/dev/null; then
    enabled=yes
fi

# The documented alternative to installing the plugin: run with the shipped settings file, which
# carries the same hook. It cannot be detected from disk -- it is a flag on a running process --
# so it is reported as a possibility rather than asserted either way.
if [ "$installed" = yes ] && [ "$enabled" = yes ]; then
    say "plugin '$name' is installed and enabled"
else
    [ "$installed" = yes ] || bad "plugin '$name' is not installed (checked $INSTALLED)"
    [ "$enabled"  = yes ] || bad "plugin '$name' is not enabled (checked enabledPlugins in $SETTINGS)"
    say "-> unless this session was started with --settings .claude/settings.nucleus.json,"
    say "   NO hook is running: not the gate, and not the host-free-space warning."
fi

# ---- 3. disableAllHooks turns everything above off in one line ------------------------------
if [ -f "$SETTINGS" ] && grep -q '"disableAllHooks" *: *true' "$SETTINGS" 2>/dev/null; then
    bad "disableAllHooks is true in $SETTINGS; every hook is off regardless of the above"
fi

# ---- 4. can the gate reach its binary? ------------------------------------------------------
# A missing binary is SAFE -- `gate.sh` denies every tool -- but it means the bridge is unusable
# rather than protecting anything, so it is reported plainly instead of as a pass.
gate="${CCN_GATE:-}"
[ -z "$gate" ] && [ -x "$root/bin/ccn-gate" ] && gate="$root/bin/ccn-gate"
[ -z "$gate" ] && gate="$(command -v ccn-gate 2>/dev/null)" || true
if [ -n "$gate" ] && [ -x "$gate" ]; then
    say "ccn-gate found at $gate"
else
    bad "ccn-gate is not installed, so the gate would DENY every tool call (safe, but the bridge cannot be used). Install: cargo install --git https://github.com/coproduct-opensource/claude-code-nucleus ccn-gate ccn-mcp"
fi

printf '\n'
if [ "$problems" -eq 0 ]; then
    printf 'IN FORCE: the hooks this repository declares can run on this host.\n'
    printf '(Configuration, not enforcement -- whoever can edit settings.json can still remove them.)\n'
    exit 0
fi

printf 'NOT IN FORCE: %d problem(s) above.\n' "$problems"
printf 'The boundary is off and nothing else will tell you. That is the state this check exists for.\n'
exit 1
