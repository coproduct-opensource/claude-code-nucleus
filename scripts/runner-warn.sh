#!/bin/sh
#
# Self-hosted CI capacity, as an event rather than an archaeology dig.
#
# THE INCIDENT. On 2026-09-22 at 17:09 EDT `lima-gatehouse-ci-3` was killed by the OOM killer
# while running `gates-can-fail`, which peaked at 12.5 GB inside a 16 GiB VM shared by three
# gatehouse runners and two olog runners. It stayed down for TWENTY-SIX HOURS and nobody knew.
#
# What makes it invisible is the exit code. The kill takes the job process, the runner's own
# supervisor notices its listener is gone and writes:
#
#   A process of this unit has been killed by the OOM killer
#   Runner listener exit with 0 return code, stop the service, no retry needed.
#   Failed with result 'oom-kill'
#
# Exit 0. So the runner treats a crash as a graceful shutdown, and with `Restart=no` systemd
# agrees with it. A third of CI capacity disappears and every surviving signal says "fine":
#
#   * the other runners keep working, so CI is not "down"
#   * checks still pass, just fewer at a time
#   * the queue drains at 2/3 speed, which reads as "CI is slow today"
#
# That is the same defect as the disk outage next door in `disk-warn.sh`: a failure whose
# symptom is indistinguishable from ordinary slowness. The cure is the same — say the thing
# out loud, early, on the tool that is waiting for it.
#
# WHY THE GITHUB API AND NOT THE VM. `limactl shell` is the wrong probe twice over: it cannot
# see runners on any other host, and when the host disk is full it HANGS, which is precisely
# the condition `disk-warn.sh` already covers. "Registered but offline" is the authoritative
# statement of lost capacity and it costs one bounded HTTP call.
#
# WHY IT NEVER BLOCKS. `scripts/gate.sh` blocks and exits 2 because a mediator that cannot run
# is a malfunction. This is advice: a hook that cannot reach GitHub has learned nothing that
# justifies stopping work. Every path here exits 0.
set -u

STAMP="${TMPDIR:-/tmp}/.ccn-runner-warn"
# One bounded network call at most this often. The disk check is a syscall and can run on every
# Bash call; this is an HTTP round trip and must not. Ten minutes is far inside the 26 hours it
# is trying to prevent, and long enough that a session never waits on it twice in a row.
QUIET_SECS=600

command -v gh >/dev/null 2>&1 || exit 0
command -v timeout >/dev/null 2>&1 || exit 0   # unbounded means a hook that hangs; prefer silence

now=$(date +%s)
if [ -f "$STAMP" ]; then
    last=$(cat "$STAMP" 2>/dev/null || echo 0)
    case "$last" in ''|*[!0-9]*) last=0 ;; esac
    [ $((now - last)) -lt "$QUIET_SECS" ] && exit 0
fi
printf '%s' "$now" > "$STAMP" 2>/dev/null || true

# The account that owns these runners. Never change the global gh account to read them.
TOKEN=$(timeout 10 gh auth token --user brandon-coproduct 2>/dev/null) || exit 0
[ -n "$TOKEN" ] || exit 0

down=""
total_down=0
for repo in coproduct-private/gatehouse coproduct-private/olog; do
    out=$(GH_TOKEN="$TOKEN" timeout 15 gh api "repos/$repo/actions/runners" \
            --jq '.runners[] | select(.status != "online") | .name' 2>/dev/null) || continue
    [ -n "$out" ] || continue
    for name in $out; do
        total_down=$((total_down + 1))
        down="$down $name"
    done
done

[ "$total_down" -eq 0 ] && exit 0

msg="CI CAPACITY LOST: ${total_down} self-hosted runner(s) registered but OFFLINE —${down}. Checks still pass, just fewer at a time, so this presents as 'CI is slow' and not as an outage. The known cause is an OOM kill during a heavy gate (gates-can-fail peaked at 12.5G in a 16GiB VM on 2026-09-22): the listener then exits 0, the runner calls that a clean shutdown, and Restart=no leaves it down indefinitely. Check with: sudo systemctl status actions.runner.<repo>.<runner>.service inside the VM, and restart it ONLY when the other runners are idle — starting a third heavy job in a 16GiB VM is how the OOM happens."

printf '{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"%s"}}\n' "$msg"
printf 'ccn-runner: %s\n' "$msg" >&2
exit 0
