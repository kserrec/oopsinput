#!/usr/bin/env zsh
# Archive-only acceptance for the complete public installation experience.
# Every install runs the bundled entry point without source-artifact overrides.
set -eu
setopt pipefail
umask 077

PATH=/usr/bin:/bin
export LC_ALL=C

if (( $# != 1 )); then
    print -u2 "usage: scripts/install-experience-gate.zsh EXTRACTED_RELEASE_DIRECTORY"
    exit 2
fi

BUNDLE=${1:A}
INSTALL=$BUNDLE/install.zsh
RELEASE_BIN=$BUNDLE/oopsinput
PLUGIN_SRC=$BUNDLE/oopsinput.zsh
UNINSTALL_SRC=$BUNDLE/uninstall.zsh

fail() {
    print -u2 -r -- "install-experience-gate: FAIL: $1"
    exit 1
}

[[ -d $BUNDLE && ! -L $BUNDLE ]] || fail "release directory is not regular: $BUNDLE"
[[ -x $RELEASE_BIN && -f $PLUGIN_SRC && -f $INSTALL && -f $UNINSTALL_SRC ]] || \
    fail "release directory is incomplete"
for helper in cmp cp env find grep mkdir mkfifo mktemp rm script sleep sort stat timeout zsh; do
    command -v $helper >/dev/null 2>&1 || fail "required helper not found: $helper"
done

TMP_BASE=${TMPDIR:-/tmp}
[[ -d $TMP_BASE ]] || fail "temporary directory root does not exist: $TMP_BASE"
TMP_BASE=${TMP_BASE:A}
GATE_ROOT=$(mktemp -d "$TMP_BASE/oopsinput-install-experience.XXXXXX")
typeset ACTIVE_WRITER=""

cleanup() {
    if [[ -n ${ACTIVE_WRITER:-} ]]; then
        kill -TERM $ACTIVE_WRITER 2>/dev/null || true
        wait $ACTIVE_WRITER 2>/dev/null || true
        ACTIVE_WRITER=""
    fi
    if [[ -n ${GATE_ROOT:-} && -d $GATE_ROOT && ${GATE_ROOT:h} == $TMP_BASE && \
          ${GATE_ROOT:t} == oopsinput-install-experience.* ]]; then
        rm -rf -- $GATE_ROOT
    fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

typeset CASE_ROOT CASE_HOME ORIGINAL_ZSHRC
typeset INSTALLED_BIN INSTALLED_PLUGIN INSTALLED_UNINSTALLER CONFIG BACKUP STATE
typeset -a CLEAN_PREFIX

prepare_case() {
    local name=$1
    CASE_ROOT=$GATE_ROOT/$name
    CASE_HOME=$CASE_ROOT/home
    ORIGINAL_ZSHRC=$CASE_ROOT/original.zshrc
    mkdir -p -- $CASE_HOME
    {
        print -r -- "# public archive fixture: $name"
        # Every successful path must restore this valid no-final-newline shape.
        print -rn -- "export OOPSINPUT_ARCHIVE_CASE=${(q)name}"
    } > $ORIGINAL_ZSHRC
    cp -- $ORIGINAL_ZSHRC $CASE_HOME/.zshrc

    INSTALLED_BIN=$CASE_HOME/.local/bin/oopsinput
    INSTALLED_PLUGIN=$CASE_HOME/.local/share/oopsinput/oopsinput.zsh
    INSTALLED_UNINSTALLER=$CASE_HOME/.local/share/oopsinput/uninstall.zsh
    CONFIG=$CASE_HOME/.config/oopsinput/config
    BACKUP=$CASE_HOME/.zshrc.oopsinput-backup
    STATE=$CASE_HOME/.local/state/oopsinput

    CLEAN_PREFIX=(
        env
        -u XDG_CONFIG_HOME
        -u XDG_STATE_HOME
        -u OOPSINPUT_STATE_DIR
        -u OOPSINPUT_MODE
        -u OOPSINPUT_BIN
        -u OOPSINPUT_BIN_SRC
        -u OOPSINPUT_PLUGIN_SRC
        -u OOPSINPUT_UNINSTALL_SRC
        -u OOPSINPUT_TEST_FAIL_AFTER
        -u OOPSINPUT_TEST_SIGNAL_AFTER
        -u OOPSINPUT_PLUGIN_ACTIVE
        -u OOPSINPUT_WRAPPED_WIDGETS
        -u OOPSINPUT_WIDGET_STATUS_FRESH
        -u OOPSINPUT_TEST_DEADLINE_MS
        -u OOPSINPUT_TEST_HANG
        -u OOPSINPUT_TEST_OLLAMA_PORT
        HOME=$CASE_HOME
        ZDOTDIR=$CASE_HOME
        TERM=xterm
        SHELL=/bin/sh
        LC_ALL=C
    )
}

wait_for_text() {
    local file=$1 expected=$2
    integer ticks=0
    while ! grep -Fq -- $expected $file; do
        (( ticks += 1 ))
        (( ticks < 500 )) || return 1
        sleep 0.02
    done
}

typeset PTY_TRANSCRIPT PTY_OUTPUT
integer PTY_STATUS=0
run_installer_pty() {
    local keys=$1 label=$2
    local fifo=$CASE_ROOT/$label.input
    PTY_TRANSCRIPT=$CASE_ROOT/$label.typescript
    PTY_OUTPUT=$CASE_ROOT/$label.output
    : > $PTY_TRANSCRIPT
    mkfifo -- $fifo

    integer input_fd
    exec {input_fd}<>$fifo
    (
        wait_for_text $PTY_TRANSCRIPT "Focus: none" || exit 124
        print -rn -u $input_fd -- $keys
    ) &
    ACTIVE_WRITER=$!

    local command="zsh ${(q)INSTALL}"
    if ${CLEAN_PREFIX[@]} timeout 20s script -qefc $command $PTY_TRANSCRIPT \
            <&$input_fd > $PTY_OUTPUT 2>&1; then
        PTY_STATUS=0
    else
        PTY_STATUS=$?
    fi

    integer writer_status=0
    if wait $ACTIVE_WRITER; then
        writer_status=0
    else
        writer_status=$?
    fi
    ACTIVE_WRITER=""
    exec {input_fd}>&-
    rm -- $fifo
    (( writer_status == 0 )) || \
        fail "$label never reached the chooser (writer status $writer_status):\n$(< $PTY_OUTPUT)"
}

assert_no_install() {
    local label=$1
    cmp -s -- $ORIGINAL_ZSHRC $CASE_HOME/.zshrc || fail "$label changed .zshrc"
    [[ ! -e $INSTALLED_BIN && ! -L $INSTALLED_BIN ]] || fail "$label left the binary"
    [[ ! -e $INSTALLED_PLUGIN && ! -L $INSTALLED_PLUGIN ]] || fail "$label left the plugin"
    [[ ! -e $INSTALLED_UNINSTALLER && ! -L $INSTALLED_UNINSTALLER ]] || \
        fail "$label left the uninstaller"
    [[ ! -e $CONFIG && ! -L $CONFIG ]] || fail "$label left a config"
    [[ ! -e $BACKUP && ! -L $BACKUP ]] || fail "$label left a shell backup"
}

assert_installed() {
    local mode=$1
    cmp -s -- $RELEASE_BIN $INSTALLED_BIN || fail "$mode installed binary differs from archive"
    cmp -s -- $PLUGIN_SRC $INSTALLED_PLUGIN || fail "$mode installed plugin differs from archive"
    cmp -s -- $UNINSTALL_SRC $INSTALLED_UNINSTALLER || \
        fail "$mode installed uninstaller differs from archive"
    cmp -s -- $ORIGINAL_ZSHRC $BACKUP || fail "$mode shell backup differs from original"
    grep -Eq -- "^mode = ${mode}[[:space:]]" $CONFIG || fail "$mode config was not selected"
    [[ $(stat -c %a -- $INSTALLED_BIN) == 755 ]] || fail "$mode binary mode is not 755"
    [[ $(stat -c %a -- $INSTALLED_PLUGIN) == 600 ]] || fail "$mode plugin mode is not 600"
    [[ $(stat -c %a -- $INSTALLED_UNINSTALLER) == 700 ]] || \
        fail "$mode uninstaller mode is not 700"
    [[ $(stat -c %a -- $CONFIG) == 600 ]] || fail "$mode config mode is not 600"
}

run_doctor() {
    local mode=$1
    local input=$CASE_ROOT/doctor.input
    local transcript=$CASE_ROOT/doctor.transcript
    {
        print -r -- '"$HOME/.local/bin/oopsinput" doctor'
        print -r -- "exit"
    } > $input
    if ! ${CLEAN_PREFIX[@]} timeout 20s script -qec "zsh -d -i" /dev/null \
            < $input > $transcript 2>&1; then
        fail "$mode installed shell or doctor failed:\n$(< $transcript)"
    fi
    grep -Fq -- "widgets:    4/4 wrapped in this shell" $transcript || \
        fail "$mode doctor did not see four live wrappers"
    grep -Fq -- "mode:       $mode" $transcript || fail "$mode doctor reported another mode"
    grep -Fq -- "result:     ready" $transcript || fail "$mode doctor did not report ready"
}

uninstall_and_assert() {
    local mode=$1
    local config_receipt=$CASE_ROOT/config.receipt
    cp -- $CONFIG $config_receipt
    ${CLEAN_PREFIX[@]} $INSTALLED_BIN purge >/dev/null
    ${CLEAN_PREFIX[@]} zsh $INSTALLED_UNINSTALLER >/dev/null

    [[ ! -e $INSTALLED_BIN && ! -L $INSTALLED_BIN ]] || fail "$mode uninstall left binary"
    [[ ! -e $INSTALLED_PLUGIN && ! -L $INSTALLED_PLUGIN ]] || fail "$mode uninstall left plugin"
    [[ ! -e $INSTALLED_UNINSTALLER && ! -L $INSTALLED_UNINSTALLER ]] || \
        fail "$mode uninstall left uninstaller"
    [[ ! -e ${INSTALLED_PLUGIN:h} && ! -L ${INSTALLED_PLUGIN:h} ]] || \
        fail "$mode uninstall left the empty plugin directory"
    [[ ! -e $STATE && ! -L $STATE ]] || fail "$mode purge left recorded state"
    cmp -s -- $ORIGINAL_ZSHRC $CASE_HOME/.zshrc || fail "$mode uninstall changed shell bytes"
    cmp -s -- $config_receipt $CONFIG || fail "$mode uninstall changed retained config"
    cmp -s -- $ORIGINAL_ZSHRC $BACKUP || fail "$mode uninstall changed retained backup"

    local remaining
    remaining=$(find $CASE_HOME -name '*oopsinput*' -printf '%P\n' | sort)
    [[ $remaining == $'.config/oopsinput\n.zshrc.oopsinput-backup' ]] || \
        fail "$mode left unexpected oopsinput artifacts:\n${remaining:-<none>}"
}

print "install-experience-gate: four interactive modes from the shipped archive"
typeset -a MODES KEYS
MODES=(shadow suggest warn confirm)
KEYS=($'1' $'\t\t\r' $'x\r3' $'4')
integer index
typeset mode
for (( index = 1; index <= 4; index++ )); do
    mode=$MODES[$index]
    prepare_case interactive-$mode
    run_installer_pty $KEYS[$index] install-$mode
    (( PTY_STATUS == 0 )) || fail "$mode interactive install exited $PTY_STATUS:\n$(< $PTY_OUTPUT)"
    grep -Fq -- "Focus: none" $PTY_TRANSCRIPT || fail "$mode chooser had no unfocused state"
    if [[ $mode == suggest ]]; then
        grep -Fq -- $'\033[7mSuggest\033[0m' $PTY_TRANSCRIPT || \
            fail "Tab did not visibly focus Suggest before Enter"
    fi
    assert_installed $mode
    run_doctor $mode
    uninstall_and_assert $mode
done

print "install-experience-gate: cancellation and invalid promptless input"
prepare_case cancelled
run_installer_pty $'\x03' install-cancelled
(( PTY_STATUS == 130 )) || fail "Ctrl-C returned $PTY_STATUS instead of 130:\n$(< $PTY_OUTPUT)"
assert_no_install "Ctrl-C cancellation"

prepare_case invalid-mode
if ${CLEAN_PREFIX[@]} zsh $INSTALL --mode automatic </dev/null \
        > $CASE_ROOT/invalid.output 2>&1; then
    fail "invalid --mode unexpectedly succeeded"
fi
grep -Fq -- "invalid mode: automatic" $CASE_ROOT/invalid.output || \
    fail "invalid --mode did not explain the rejection"
assert_no_install "invalid --mode"

print "install-experience-gate: handled TERM after the final owned rename"
prepare_case interrupted
integer signal_status=0
if ${CLEAN_PREFIX[@]} OOPSINPUT_TEST_SIGNAL_AFTER=zshrc \
        zsh $INSTALL --mode confirm </dev/null > $CASE_ROOT/signal.output 2>&1; then
    signal_status=0
else
    signal_status=$?
fi
(( signal_status == 143 )) || fail "injected TERM returned $signal_status instead of 143"
assert_no_install "handled TERM"

print "install-experience-gate: explicit automation and byte-exact update"
prepare_case update
if ! ${CLEAN_PREFIX[@]} zsh $INSTALL --mode suggest </dev/null \
        > $CASE_ROOT/fresh.output 2>&1; then
    fail "explicit --mode suggest install failed:\n$(< $CASE_ROOT/fresh.output)"
fi
assert_installed suggest
print -r -- "# retained user byte" >> $CONFIG
cp -- $CONFIG $CASE_ROOT/user.config
cp -- $CASE_HOME/.zshrc $CASE_ROOT/installed.zshrc
print -r -- "old binary" > $INSTALLED_BIN
print -r -- "old plugin" > $INSTALLED_PLUGIN
print -r -- "old uninstaller" > $INSTALLED_UNINSTALLER

if ${CLEAN_PREFIX[@]} zsh $INSTALL --mode confirm </dev/null \
        > $CASE_ROOT/rejected-update.output 2>&1; then
    fail "update --mode unexpectedly overwrote user intent"
fi
cmp -s -- $CASE_ROOT/user.config $CONFIG || fail "rejected update changed config"
grep -Fq -- "old binary" $INSTALLED_BIN || fail "rejected update changed runtime"
grep -Fq -- "old plugin" $INSTALLED_PLUGIN || fail "rejected update changed plugin"
grep -Fq -- "old uninstaller" $INSTALLED_UNINSTALLER || \
    fail "rejected update changed uninstaller"
cmp -s -- $CASE_ROOT/installed.zshrc $CASE_HOME/.zshrc || fail "rejected update changed shell"

if ! ${CLEAN_PREFIX[@]} zsh $INSTALL </dev/null > $CASE_ROOT/update.output 2>&1; then
    fail "existing-config update failed:\n$(< $CASE_ROOT/update.output)"
fi
assert_installed suggest
cmp -s -- $CASE_ROOT/user.config $CONFIG || fail "successful update changed existing config bytes"
cmp -s -- $CASE_ROOT/installed.zshrc $CASE_HOME/.zshrc || fail "update changed healthy shell bytes"
grep -Fq -- "mode: preserved from" $CASE_ROOT/update.output || fail "update did not report preservation"
run_doctor suggest
uninstall_and_assert suggest

print "GATE PASS: shipped install experience covers every mode, rollback, update, doctor, and removal"

exit=0
