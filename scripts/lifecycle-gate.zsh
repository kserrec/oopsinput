#!/usr/bin/env zsh
# Release-level clean-home acceptance gate. Exercises the artifacts and exact
# commands a new user gets: install, interactive Shadow use, doctor, report,
# purge, and uninstall. Everything happens under one mktemp-owned HOME.
set -eu
setopt pipefail
umask 077

# Match the lifecycle scripts' fixed helper path and avoid repository-local
# executables. This gate is Linux-only, like oopsinput v1.
PATH=/usr/bin:/bin
export LC_ALL=C

ROOT=${0:A:h:h}
integer USE_SOURCE_OVERRIDES=1
if (( $# == 0 )); then
    RELEASE_BIN=$ROOT/target/release/oopsinput
    PLUGIN_SRC=$ROOT/zsh/oopsinput.zsh
    INSTALL=$ROOT/zsh/install.zsh
    UNINSTALL_SRC=$ROOT/zsh/uninstall.zsh
elif (( $# == 1 )); then
    BUNDLE_DIR=${1:A}
    RELEASE_BIN=$BUNDLE_DIR/oopsinput
    PLUGIN_SRC=$BUNDLE_DIR/oopsinput.zsh
    INSTALL=$BUNDLE_DIR/install.zsh
    UNINSTALL_SRC=$BUNDLE_DIR/uninstall.zsh
    USE_SOURCE_OVERRIDES=0
else
    print -u2 "usage: scripts/lifecycle-gate.zsh [EXTRACTED_RELEASE_DIRECTORY]"
    exit 2
fi

fail() {
    print -u2 -r -- "lifecycle-gate: FAIL: $1"
    exit 1
}

[[ -x $RELEASE_BIN ]] || fail "release binary missing; run cargo build --release first"
[[ -f $PLUGIN_SRC && -f $INSTALL && -f $UNINSTALL_SRC ]] || fail "lifecycle files missing"
for helper in zsh script timeout cmp grep stat wc find sort; do
    command -v $helper >/dev/null 2>&1 || fail "required helper not found: $helper"
done

TMP_BASE=${TMPDIR:-/tmp}
[[ -d $TMP_BASE ]] || fail "temporary directory root does not exist: $TMP_BASE"
TMP_BASE=${TMP_BASE:A}
GATE_ROOT=$(mktemp -d "$TMP_BASE/oopsinput-lifecycle-gate.XXXXXX")
GATE_HOME=$GATE_ROOT/home

cleanup() {
    # Recursive removal is limited to the exact mktemp directory shape under
    # the already-resolved temporary root. Never broaden this target.
    if [[ -n ${GATE_ROOT:-} && -d $GATE_ROOT && ${GATE_ROOT:h} == $TMP_BASE && \
          ${GATE_ROOT:t} == oopsinput-lifecycle-gate.* ]]; then
        rm -rf -- $GATE_ROOT
    fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

mkdir -p -- $GATE_HOME
ORIGINAL_ZSHRC=$GATE_ROOT/original.zshrc
{
    print -r -- "# clean-machine lifecycle fixture"
    # No final newline: a valid startup-file shape the release gate once
    # missed, allowing install to join its marker onto the user's last line.
    print -rn -- "PS1='LIFECYCLE%% '"
} > $ORIGINAL_ZSHRC
cp -- $ORIGINAL_ZSHRC $GATE_HOME/.zshrc

# Remove every supported configuration/test override inherited from the
# developer's shell. Only this isolated absolute HOME may participate.
run_clean() {
    local -a artifact_env
    if (( USE_SOURCE_OVERRIDES == 1 )); then
        artifact_env=(
            OOPSINPUT_BIN_SRC=$RELEASE_BIN
            OOPSINPUT_PLUGIN_SRC=$PLUGIN_SRC
            OOPSINPUT_UNINSTALL_SRC=$UNINSTALL_SRC
        )
    else
        artifact_env=()
    fi
    env \
        -u XDG_CONFIG_HOME \
        -u XDG_STATE_HOME \
        -u OOPSINPUT_STATE_DIR \
        -u OOPSINPUT_MODE \
        -u OOPSINPUT_BIN \
        -u OOPSINPUT_BIN_SRC \
        -u OOPSINPUT_PLUGIN_SRC \
        -u OOPSINPUT_UNINSTALL_SRC \
        -u OOPSINPUT_TEST_FAIL_AFTER \
        -u OOPSINPUT_PLUGIN_ACTIVE \
        -u OOPSINPUT_WRAPPED_WIDGETS \
        -u OOPSINPUT_WIDGET_STATUS_FRESH \
        -u OOPSINPUT_TEST_DEADLINE_MS \
        -u OOPSINPUT_TEST_HANG \
        -u OOPSINPUT_TEST_OLLAMA_PORT \
        HOME=$GATE_HOME \
        ZDOTDIR=$GATE_HOME \
        TERM=xterm \
        LC_ALL=C \
        $artifact_env \
        "$@"
}

print "lifecycle-gate: install -> interactive Shadow -> report -> purge -> uninstall"

if ! INSTALL_OUT=$(run_clean zsh $INSTALL --mode shadow 2>&1); then
    fail "installer failed:\n$INSTALL_OUT"
fi

INSTALLED_BIN=$GATE_HOME/.local/bin/oopsinput
INSTALLED_PLUGIN=$GATE_HOME/.local/share/oopsinput/oopsinput.zsh
INSTALLED_UNINSTALLER=$GATE_HOME/.local/share/oopsinput/uninstall.zsh
CONFIG=$GATE_HOME/.config/oopsinput/config
STATE=$GATE_HOME/.local/state/oopsinput
BACKUP=$GATE_HOME/.zshrc.oopsinput-backup

cmp -s -- $RELEASE_BIN $INSTALLED_BIN || fail "installed binary differs from the release artifact"
cmp -s -- $PLUGIN_SRC $INSTALLED_PLUGIN || fail "installed plugin differs from the repository artifact"
cmp -s -- $UNINSTALL_SRC $INSTALLED_UNINSTALLER || fail "installed uninstaller differs from the release artifact"
cmp -s -- $ORIGINAL_ZSHRC $BACKUP || fail "installer backup differs from the original .zshrc"
[[ $(stat -c %a -- $INSTALLED_BIN) == 755 ]] || fail "installed binary mode is not 755"
[[ $(stat -c %a -- $INSTALLED_PLUGIN) == 600 ]] || fail "installed plugin mode is not 600"
[[ $(stat -c %a -- $INSTALLED_UNINSTALLER) == 700 ]] || fail "installed uninstaller mode is not 700"
[[ $(stat -c %a -- $CONFIG) == 600 ]] || fail "installed config mode is not 600"
grep -Fq -- "mode = shadow" $CONFIG || fail "fresh install did not write the explicit Shadow choice"
INSTALLED_CONFIG_RECEIPT=$GATE_ROOT/installed.config
cp -- $CONFIG $INSTALLED_CONFIG_RECEIPT

# Exercise the chosen silent mode through the installed plugin in a real
# interactive ZLE shell, not through a direct debug-binary shortcut.
PTY_INPUT=$GATE_ROOT/pty-input
PTY_TRANSCRIPT=$GATE_ROOT/pty-transcript
{
    print -r -- '"$HOME/.local/bin/oopsinput" doctor'
    print -r -- "print -r -- lifecycle-shell-ok"
    print -r -- "exit"
} > $PTY_INPUT
# Reproduced in GitHub release CI on 2026-08-08: the runner's global Zsh
# setup invoked compinit, whose prompt consumed the opening quote from the
# scripted doctor command and left Zsh at `dquote>` until this timeout. `-d`
# disables host global startup files while still loading our isolated .zshrc.
if ! run_clean timeout 20s script -qec "zsh -d -i" /dev/null \
        < $PTY_INPUT > $PTY_TRANSCRIPT 2>&1; then
    fail "installed interactive shell failed or timed out:\n$(< $PTY_TRANSCRIPT)"
fi
grep -Fq -- "widgets:    4/4 wrapped in this shell" $PTY_TRANSCRIPT || \
    fail "doctor did not see all installed live wrappers"
grep -Fq -- "mode:       shadow" $PTY_TRANSCRIPT || fail "installed shell did not read Shadow mode"
grep -Fq -- "result:     ready" $PTY_TRANSCRIPT || fail "doctor did not report the clean install ready"
grep -Fq -- "lifecycle-shell-ok" $PTY_TRANSCRIPT || fail "ordinary command did not survive the installed plugin"

EVENTS=$STATE/events.jsonl
[[ -f $EVENTS ]] || fail "interactive Shadow use did not create an event log"
[[ $(wc -l < $EVENTS) -eq 3 ]] || fail "expected exactly three interactive lifecycle events"
if ! REPORT_OUT=$(run_clean $INSTALLED_BIN report 2>&1); then
    fail "report failed:\n$REPORT_OUT"
fi
print -r -- $REPORT_OUT | grep -Eq '^  events: 3$' || fail "report did not summarize all three Shadow events"

if ! PURGE_OUT=$(run_clean $INSTALLED_BIN purge 2>&1); then
    fail "purge failed:\n$PURGE_OUT"
fi
[[ ! -e $STATE && ! -L $STATE ]] || fail "purge left the oopsinput state directory behind"

if ! UNINSTALL_OUT=$(run_clean zsh $INSTALLED_UNINSTALLER 2>&1); then
    fail "uninstaller failed:\n$UNINSTALL_OUT"
fi
[[ ! -e $INSTALLED_BIN && ! -L $INSTALLED_BIN ]] || fail "uninstall left the installed binary"
[[ ! -e $INSTALLED_PLUGIN && ! -L $INSTALLED_PLUGIN ]] || fail "uninstall left the installed plugin"
[[ ! -e $INSTALLED_UNINSTALLER && ! -L $INSTALLED_UNINSTALLER ]] || fail "uninstall left the installed uninstaller"
[[ ! -e ${INSTALLED_PLUGIN:h} && ! -L ${INSTALLED_PLUGIN:h} ]] || fail "uninstall left the empty plugin directory"

# Reproduced before this gate landed (2026-08-08): a fresh install appended an
# unmarked blank separator before its block, so uninstall left one extra byte
# in an otherwise unchanged .zshrc. The only allowed retained oopsinput
# artifacts are the user-owned config and the lifecycle backup.
cmp -s -- $ORIGINAL_ZSHRC $GATE_HOME/.zshrc || fail "uninstall did not restore the original .zshrc bytes"
[[ -f $CONFIG && ! -L $CONFIG ]] || fail "uninstall did not retain the regular config"
cmp -s -- $INSTALLED_CONFIG_RECEIPT $CONFIG || fail "retained config changed"
[[ -f $BACKUP && ! -L $BACKUP ]] || fail "uninstall did not retain the regular .zshrc backup"

REMAINING=$(find $GATE_HOME -name '*oopsinput*' -printf '%P\n' | sort)
EXPECTED_REMAINING=$'.config/oopsinput\n.zshrc.oopsinput-backup'
[[ $REMAINING == $EXPECTED_REMAINING ]] || \
    fail "unexpected oopsinput artifacts remain:\n${REMAINING:-<none>}"

print "lifecycle-gate: 3 Shadow events reported; state purged; runtime removed"
print "lifecycle-gate: retained only config and .zshrc backup"
print "GATE PASS: clean-machine lifecycle matches the documented ownership contract"
