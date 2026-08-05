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
START=$EPOCHSECONDS
ZDOTDIR=$WORK TERM=xterm script -qec "zsh -i" /dev/null < $WORK/stream > $WORK/out 2>/dev/null
ELAPSED=$(( EPOCHSECONDS - START ))

FOUND=$(grep -c -- '^gate-.*-ok' $WORK/out || true)
EVENTS=$(wc -l < $WORK/state/events.jsonl 2>/dev/null || print 0)

print "submissions: $N"
print "outputs:     $FOUND"
print "events:      $EVENTS"
print "elapsed:     ${ELAPSED}s"

if (( FOUND == N )); then
    print "GATE PASS: zero lost/altered commands"
else
    print -u2 "GATE FAIL: $(( N - FOUND )) missing outputs"
    exit 1
fi
