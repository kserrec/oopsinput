#!/usr/bin/env zsh
# Performance gate: the SPEC §10 budgets, actually enforced.
#
# Why this exists (test-audit 2026-08-06): SPEC §10 sets latency budgets and
# CLAUDE.md calls performance paramount, but nothing failed when we blew
# them. A recency change that cost ~7.5 ms of the budget on a 10k-entry
# history shipped and was caught only because someone ran a curious probe by
# hand. No unit test can catch this class — the cost is in process spawn and
# real syscalls — so it gets its own gate rather than a cargo test.
#
# Usage: scripts/perf-gate.zsh [iterations]   (default 60)
#
# Exits nonzero if any measured percentile exceeds its budget.
set -eu
zmodload zsh/datetime

N=${1:-60}
ROOT=${0:A:h:h}
BIN=$ROOT/target/release/oopsinput

if [[ ! -x $BIN ]]; then
    print -u2 "perf-gate: build first — cargo build --release"
    exit 2
fi

# Never write into the developer's real event log: PLAN records that earlier
# benchmark loops polluted it with synthetic events and skewed the shadow
# data the M5 pilot depends on.
STATE=$(mktemp -d)
trap 'rm -rf $STATE' EXIT
export OOPSINPUT_STATE_DIR=$STATE
# Config must not leak in from the developer's machine either.
export XDG_CONFIG_HOME=$STATE/config
unset OOPSINPUT_MODE 2>/dev/null || true

typeset -i failures=0

# measure <label> <buffer> <p50 budget ms> <p95 budget ms>
measure() {
    local label=$1 buffer=$2
    local -F budget_p50=$3 budget_p95=$4
    local -a samples
    local -F t0
    local i
    for (( i = 1; i <= N; i++ )); do
        t0=$EPOCHREALTIME
        print -rn -- "$buffer" | $BIN check --res command >/dev/null 2>&1 || true
        samples+=( $(( (EPOCHREALTIME - t0) * 1000 )) )
    done
    local -a sorted=( ${(no)samples} )
    local -F p50=${sorted[$(( (N + 1) / 2 ))]}
    local -F p95=${sorted[$(( (N * 95 + 99) / 100 ))]}
    printf '  %-28s p50 %6.2f ms (budget %5.1f)   p95 %6.2f ms (budget %5.1f)' \
        $label $p50 $budget_p50 $p95 $budget_p95
    if (( p50 > budget_p50 || p95 > budget_p95 )); then
        print ' — OVER BUDGET'
        (( failures++ ))
    else
        print ' — ok'
    fi
}

print "perf-gate: $N iterations per path, release build, including process spawn"

# SPEC §10: deterministic path p50 <= 10 ms, p95 <= 25 ms.
measure "common (resolving command)" 'ls -la' 10 25

# SPEC §10: candidate path without the model, p95 <= 75 ms. Run from the repo
# root so the danger + context layers actually engage (git facts collected).
cd $ROOT
measure "candidate (danger + context)" 'git reset --hard' 40 75

if (( failures )); then
    print -u2 "perf-gate: $failures path(s) over budget — see SPEC §10"
    exit 1
fi
print "perf-gate: all paths within budget"
