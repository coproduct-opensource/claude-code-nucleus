#!/bin/sh
#
# Host free space, as an event rather than an autopsy.
#
# THE INCIDENT, twice. A session that opens a worktree per branch accumulates a full Rust
# `target/` in each one, 7-13 GB apiece. Thirty of them is over 100 GB. On 2026-09-20 the
# scratchpad reached 113 GB and took the host to 91%, and the thing that broke was not the
# build — it was the BUILDER.
#
# The Lima VMs' disks are SPARSE images: 100-200 GB logical, far less allocated. A sparse
# image needs host blocks to grow on every guest write. With the host full the guest's writes
# fail and the filesystem dies, and the symptoms name nothing about disk:
#
#   * `limactl list` goes on reporting `Running`
#   * `limactl shell gatehouse-ci -- uptime` answers `/bin/bash: Input/output error`
#   * all three self-hosted runners register `offline`
#   * every CI check sits `pending` forever, none of them starting
#
# That last one is the expensive part: it reads as "CI is slow" or "the queue is backed up",
# and the next boot runs fsck over a 200 GB disk with a 20 GB catalog, so SSH does not answer
# for ten minutes while `limactl list` still says `Running`. The first time this happened it
# was nearly misattributed to a node deploy that had just landed.
#
# So the check is cheap, advisory, and on the tool that causes it. `df` is one syscall's worth
# of work; `du` over the scratchpad is not, and is therefore run ONLY once a threshold is
# already crossed, under a timeout, to say how much of the problem is ours to fix.
#
# WHY IT NEVER BLOCKS. `scripts/gate.sh` blocks and exits 2 because a mediator that cannot run
# is a malfunction and the boundary must fail closed. This is the opposite kind of hook: a
# disk warning is advice, and a hook that cannot measure free space has learned nothing that
# justifies stopping work. Every path here exits 0.
set -u

STAMP="${TMPDIR:-/tmp}/.ccn-disk-warn"
# Re-warn at most every 10 minutes. Without this the notice repeats on every Bash call and
# becomes the thing it is trying to prevent: a signal nobody reads.
QUIET_SECS=600

vol="/System/Volumes/Data"
[ -d "$vol" ] || vol="/"

# `df -k` is POSIX and portable; -g is not. Column 4 is available 1K blocks.
avail_k=$(df -k "$vol" 2>/dev/null | awk 'NR==2 {print $4}')
case "$avail_k" in
    ''|*[!0-9]*) exit 0 ;;   # could not look; say nothing
esac
avail_g=$((avail_k / 1024 / 1024))

# 150 GB is headroom for both VM images to grow at once plus a full workspace build. 60 GB is
# where the 2026-09-20 outage happened (86 GB free, and the VM died during a Lean build).
WARN_G=150
CRIT_G=60

[ "$avail_g" -ge "$WARN_G" ] && exit 0

now=$(date +%s)
if [ -f "$STAMP" ]; then
    last=$(cat "$STAMP" 2>/dev/null || echo 0)
    case "$last" in ''|*[!0-9]*) last=0 ;; esac
    [ $((now - last)) -lt "$QUIET_SECS" ] && exit 0
fi
printf '%s' "$now" > "$STAMP" 2>/dev/null || true

# Only now, and only bounded: a `du` over a hundred gigabytes can take a while.
# The variable name is not guessed at in one place: whichever of these the host sets, and
# nothing at all if none of them do. GNU `timeout` is not on a stock macOS, so its absence
# means SKIP the du rather than run it unbounded — a hook that hangs is worse than a vague one.
scratch="${CCN_SCRATCHPAD:-${CLAUDE_SCRATCHPAD_DIR:-}}"
ours=""
if [ -n "$scratch" ] && [ -d "$scratch" ] && command -v timeout >/dev/null 2>&1; then
    size=$( { timeout 20 du -sh "$scratch" 2>/dev/null || true; } | awk '{print $1}')
    [ -n "$size" ] && ours=" The scratchpad is $size of that; worktree target/ dirs are derived and safe to delete once the work is pushed (rm -rf <worktree>/target), EXCEPT one with a build or gate currently running in it."
fi

if [ "$avail_g" -lt "$CRIT_G" ]; then
    msg="DISK CRITICAL: ${avail_g}G free on ${vol}.${ours} At this level the Lima VMs' sparse disks cannot grow and the builder dies with I/O errors while 'limactl list' still says Running — offline runners and CI checks pending forever. Free space before starting another build."
else
    msg="DISK LOW: ${avail_g}G free on ${vol}.${ours} Below ${WARN_G}G the Lima VMs' sparse disks may fail to grow, which kills the builder in a way that looks like slow CI rather than a full disk."
fi

printf '{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"%s"}}\n' "$msg"
printf 'ccn-disk: %s\n' "$msg" >&2
exit 0
