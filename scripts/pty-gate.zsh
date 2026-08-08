#!/usr/bin/env zsh
# M1 acceptance gate: N scripted submissions through a real PTY zsh with the
# plugin active — every command's output must appear, nothing may hang.
# Usage: scripts/pty-gate.zsh [N]   (default 10000)
set -eu
zmodload zsh/datetime

N=${1:-10000}
ROOT=${0:A:h:h}
BIN=$ROOT/target/release/oopsinput
PLUGIN=$ROOT/zsh/oopsinput.zsh

[[ -x $BIN ]] || { print -u2 "build first: cargo build --release"; exit 1 }

WORK=$(mktemp -d)
trap 'rm -rf $WORK' EXIT

cat > $WORK/.zshrc <<EOF
PS1='G%% '
export OOPSINPUT_BIN=${(q)BIN}
export OOPSINPUT_STATE_DIR=${(q)WORK}/state
source ${(q)PLUGIN}
EOF

# Command stream: unique, verifiable output per submission.
for i in {1..$N}; do
    print -r -- "print gate-$i-ok"
done > $WORK/stream
print -r -- "exit" >> $WORK/stream

print "running $N submissions through PTY zsh..."
START=$EPOCHREALTIME
# Keep this fixture hermetic: `-d` skips host global startup files but still
# loads $ZDOTDIR/.zshrc. GitHub's global compinit prompt otherwise consumes
# scripted input before this gate's prompt is ready (release CI, 2026-08-08).
ZDOTDIR=$WORK TERM=xterm script -qec "zsh -d -i" /dev/null < $WORK/stream > $WORK/out 2>/dev/null
typeset -F ELAPSED_F=$(( EPOCHREALTIME - START ))

FOUND=$(grep -c -- '^gate-.*-ok' $WORK/out || true)
EVENTS=$(wc -l < $WORK/state/events.jsonl 2>/dev/null || print 0)
typeset -F PER_CMD=$(( ELAPSED_F * 1000 / N ))

# Coarse ceiling on whole-round-trip cost per submission. This is the only
# thing measuring the *plugin* side — scripts/perf-gate.zsh measures the
# binary, and a plugin-side regression is invisible to it (test-audit
# 2026-08-06: a recency change cost ~7.5 ms per command on a large history
# and no gate noticed, because history grows as this run proceeds it shows
# up here). Deliberately loose: this includes zsh startup, PTY overhead and
# the submitted command's own execution, so it catches gross regressions,
# not drift. Tighten only with measurements, never by guess.
PER_CMD_CEILING=${OOPSINPUT_GATE_MS:-40}

print "submissions: $N"
print "outputs:     $FOUND"
print "events:      $EVENTS"
printf 'elapsed:     %.1fs (%.2f ms/submission, ceiling %s ms)\n' $ELAPSED_F $PER_CMD $PER_CMD_CEILING

typeset -i failed=0
if (( FOUND != N )); then
    print -u2 "GATE FAIL: $(( N - FOUND )) missing outputs"
    failed=1
fi
if (( PER_CMD > PER_CMD_CEILING )); then
    printf 'GATE FAIL: %.2f ms/submission exceeds the %s ms ceiling\n' \
        $PER_CMD $PER_CMD_CEILING >&2
    failed=1
fi
(( failed )) && exit 1
print "GATE PASS: zero lost/altered commands, within per-command ceiling"
