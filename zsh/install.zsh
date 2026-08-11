#!/usr/bin/env zsh
# oopsinput installer: guided fresh setup plus rollback-safe updates. Copies
# the release binary, plugin, and uninstaller to stable user-level paths, then
# adds one marked source block to ~/.zshrc. Idempotent. No root or network.
set -eu
umask 077

# Do not let a repository-local executable shadow an installer helper.
PATH=/usr/bin:/bin

SCRIPT_DIR=${0:A:h}
REPO_ROOT=${SCRIPT_DIR:h}
if [[ -x $SCRIPT_DIR/oopsinput ]]; then
    DEFAULT_BIN_SRC=$SCRIPT_DIR/oopsinput
else
    DEFAULT_BIN_SRC=$REPO_ROOT/target/release/oopsinput
fi
# These are private test seams. A release bundle and a source checkout both
# resolve their own artifacts without setting them.
BIN_SRC=${OOPSINPUT_BIN_SRC:-$DEFAULT_BIN_SRC}
PLUGIN_SRC=${OOPSINPUT_PLUGIN_SRC:-$SCRIPT_DIR/oopsinput.zsh}
UNINSTALL_SRC=${OOPSINPUT_UNINSTALL_SRC:-$SCRIPT_DIR/uninstall.zsh}

BIN_DIR=$HOME/.local/bin
BIN_DST=$BIN_DIR/oopsinput
PLUGIN_DIR=$HOME/.local/share/oopsinput
PLUGIN_DST=$PLUGIN_DIR/oopsinput.zsh
UNINSTALL_DST=$PLUGIN_DIR/uninstall.zsh
CONFIG_DIR=${XDG_CONFIG_HOME:-$HOME/.config}/oopsinput
CONFIG=$CONFIG_DIR/config
ZSHRC=$HOME/.zshrc
ZSHRC_BACKUP=$HOME/.zshrc.oopsinput-backup
MARK_BEGIN="# >>> oopsinput >>>"
MARK_END="# <<< oopsinput <<<"
MARK_RESTORE_NO_FINAL="# oopsinput: restore preceding no-final-newline"

# Render terminal controls visibly. Zsh's (V) handles C0/C1/DEL, while the
# explicit pass covers the Unicode bidi and invisible-format characters that
# (V) otherwise preserves raw. Paths and environment overrides are untrusted.
_oopsinput_escape_for_display() {
    local shown=$1 i
    local -a chars codes
    # Exact UTF-8 byte spellings work under both C and UTF-8 locales;
    # $'\u....' itself errors under LC_ALL=C.
    chars=($'\xD8\x9C' $'\xE2\x80\x8B' $'\xE2\x80\x8C' $'\xE2\x80\x8D'
        $'\xE2\x80\x8E' $'\xE2\x80\x8F' $'\xE2\x80\xA8' $'\xE2\x80\xA9'
        $'\xE2\x80\xAA' $'\xE2\x80\xAB' $'\xE2\x80\xAC' $'\xE2\x80\xAD'
        $'\xE2\x80\xAE' $'\xE2\x81\xA0' $'\xE2\x81\xA6' $'\xE2\x81\xA7'
        $'\xE2\x81\xA8' $'\xE2\x81\xA9' $'\xEF\xBB\xBF')
    codes=(061C 200B 200C 200D 200E 200F 2028 2029 202A 202B 202C 202D 202E
        2060 2066 2067 2068 2069 FEFF)
    for (( i = 1; i <= ${#chars}; i++ )); do
        shown=${shown//$chars[i]/\\u{${codes[i]}}}
    done
    print -rn -- ${(V)shown}
}

fail() {
    print -u2 -r -- "install: $(_oopsinput_escape_for_display "$1")"
    exit 1
}

usage() {
    print "usage: zsh install.zsh [--mode shadow|suggest|warn|confirm]"
    print ""
    print "A fresh interactive install asks for a starting mode. --mode is the"
    print "required equivalent when no controlling terminal is available."
}

is_mode() {
    case $1 in
        shadow|suggest|warn|confirm) return 0 ;;
        *) return 1 ;;
    esac
}

[[ -n ${HOME:-} && $HOME == /* ]] || fail "HOME must be an absolute path"

REQUESTED_MODE=""
while (( $# > 0 )); do
    case $1 in
        --mode)
            (( $# >= 2 )) || fail "--mode requires shadow, suggest, warn, or confirm"
            [[ -z $REQUESTED_MODE ]] || fail "--mode may be supplied only once"
            REQUESTED_MODE=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown argument: $1"
            ;;
    esac
done
[[ -z $REQUESTED_MODE ]] || is_mode $REQUESTED_MODE || \
    fail "invalid mode: $REQUESTED_MODE (choose shadow, suggest, warn, or confirm)"

if [[ ! -x $BIN_SRC || ! -f $BIN_SRC || -L $BIN_SRC ]]; then
    print -u2 -r -- "install: release binary not found at $(_oopsinput_escape_for_display "$BIN_SRC")"
    print -u2 "install: build it first:  cargo build --release"
    exit 1
fi
[[ -f $PLUGIN_SRC && ! -L $PLUGIN_SRC ]] || fail "plugin not found as a regular file at $PLUGIN_SRC"
[[ -f $UNINSTALL_SRC && ! -L $UNINSTALL_SRC ]] || fail "uninstaller not found as a regular file at $UNINSTALL_SRC"

# Never copy through a destination symlink or replace a non-regular path.
# `cp source symlink` follows the link and overwrites its target.
[[ ! -L $BIN_DST ]] || fail "refusing to replace symlink at $BIN_DST"
[[ ! -e $BIN_DST || -f $BIN_DST ]] || fail "refusing to replace non-file at $BIN_DST"
[[ ! -L $PLUGIN_DIR ]] || fail "refusing to enter symlink at $PLUGIN_DIR"
[[ ! -e $PLUGIN_DIR || -d $PLUGIN_DIR ]] || fail "refusing to replace non-directory at $PLUGIN_DIR"
[[ ! -L $PLUGIN_DST ]] || fail "refusing to replace symlink at $PLUGIN_DST"
[[ ! -e $PLUGIN_DST || -f $PLUGIN_DST ]] || fail "refusing to replace non-file at $PLUGIN_DST"
[[ ! -L $UNINSTALL_DST ]] || fail "refusing to replace symlink at $UNINSTALL_DST"
[[ ! -e $UNINSTALL_DST || -f $UNINSTALL_DST ]] || fail "refusing to replace non-file at $UNINSTALL_DST"
[[ ! -L $CONFIG_DIR ]] || fail "refusing to enter symlink at $CONFIG_DIR"
[[ ! -e $CONFIG_DIR || -d $CONFIG_DIR ]] || fail "refusing to replace non-directory at $CONFIG_DIR"

# Replacing a symlinked shell file would sever the link; following one would
# edit a file the installer never resolved. Leave either case to the user.
[[ ! -L $ZSHRC ]] || fail "refusing to edit symlinked $ZSHRC"
[[ ! -e $ZSHRC || -f $ZSHRC ]] || fail "refusing to edit non-file $ZSHRC"
[[ ! -L $ZSHRC_BACKUP ]] || fail "refusing to overwrite symlink at $ZSHRC_BACKUP"
[[ ! -e $ZSHRC_BACKUP || -f $ZSHRC_BACKUP ]] || fail "refusing to overwrite non-file at $ZSHRC_BACKUP"

# Validate the marker block before asking a question or installing anything.
# Multiple, missing, or reversed markers make the edit boundary ambiguous.
integer B_COUNT=0 E_COUNT=0 B=0 E=0 ADDED_SEPARATOR=0
if [[ -f $ZSHRC ]]; then
    B_COUNT=$(grep -cF -- $MARK_BEGIN $ZSHRC || true)
    E_COUNT=$(grep -cF -- $MARK_END $ZSHRC || true)
    integer B_EXACT E_EXACT
    B_EXACT=$(grep -cxF -- $MARK_BEGIN $ZSHRC || true)
    E_EXACT=$(grep -cxF -- $MARK_END $ZSHRC || true)
    if (( B_COUNT != B_EXACT || E_COUNT != E_EXACT || B_COUNT != E_COUNT || B_COUNT > 1 )); then
        fail "oopsinput block markers in $ZSHRC are damaged; refusing to edit the file"
    fi
    if (( B_COUNT == 1 )); then
        B=$(grep -nxF -m1 -- $MARK_BEGIN $ZSHRC | cut -d: -f1)
        E=$(grep -nxF -m1 -- $MARK_END $ZSHRC | cut -d: -f1)
        (( E >= B )) || fail "oopsinput block markers in $ZSHRC are out of order; refusing to edit the file"
        integer RESTORE_COUNT
        RESTORE_COUNT=$(sed -n "${B},${E}p" -- $ZSHRC | grep -cxF -- $MARK_RESTORE_NO_FINAL || true)
        (( RESTORE_COUNT <= 1 )) || fail "oopsinput newline receipt in $ZSHRC is damaged; refusing to edit the file"
        ADDED_SEPARATOR=$RESTORE_COUNT
    elif [[ -s $ZSHRC ]]; then
        # Command substitution strips a final newline. Any nonempty result
        # therefore means the existing final byte is not newline.
        [[ -z $(tail -c 1 -- $ZSHRC) ]] || ADDED_SEPARATOR=1
    fi
fi

# A healthy marker block is the install receipt that authorizes an update.
if (( B_COUNT == 0 )); then
    [[ ! -e $BIN_DST ]] || fail "file already exists at $BIN_DST; refusing to overwrite it without an existing oopsinput block"
    [[ ! -e $PLUGIN_DST ]] || fail "file already exists at $PLUGIN_DST; refusing to overwrite it without an existing oopsinput block"
    [[ ! -e $UNINSTALL_DST ]] || fail "file already exists at $UNINSTALL_DST; refusing to overwrite it without an existing oopsinput block"
    if [[ -e $ZSHRC_BACKUP && ! -w $ZSHRC_BACKUP ]]; then
        fail "backup is not writable at $ZSHRC_BACKUP"
    fi
fi

integer CONFIG_EXISTS=0
[[ ! -e $CONFIG && ! -L $CONFIG ]] || CONFIG_EXISTS=1
if (( CONFIG_EXISTS == 1 )) && [[ -n $REQUESTED_MODE ]]; then
    fail "config already exists at $CONFIG; remove --mode and edit that user-owned file directly to change it"
fi

typeset TTY_FD=""
typeset SELECTED_MODE=""
typeset -a MODE_VALUES MODE_LABELS
MODE_VALUES=(shadow suggest warn confirm)
MODE_LABELS=(Shadow Suggest Warn Confirm)

cancel_from_tty() {
    [[ -z ${TTY_FD:-} ]] || print -u $TTY_FD
    print -u2 "install: cancelled"
    exit 130
}
trap cancel_from_tty INT
trap 'exit 129' HUP
trap 'exit 143' TERM

choose_mode() {
    if ! exec {TTY_FD}<>/dev/tty 2>/dev/null; then
        fail "no controlling terminal; rerun with --mode shadow, suggest, warn, or confirm"
    fi

    print -u $TTY_FD "Choose how oopsinput may interrupt you (required):"
    print -u $TTY_FD ""
    print -u $TTY_FD "1  Shadow   Never interrupts; analyzes and records locally."
    print -u $TTY_FD "2  Suggest  Also asks about likely misspelled command names."
    print -u $TTY_FD "3  Warn     Also shows danger prompts; no answer eventually runs the original."
    print -u $TTY_FD "4  Confirm  Highest-risk prompts require a choice; no answer cancels."
    print -u $TTY_FD ""
    print -u $TTY_FD "Press 1–4, or Tab to focus an option and Enter to choose."
    print -n -u $TTY_FD -- "Focus: none"

    integer focus=0
    typeset key=""
    while true; do
        if ! IFS= read -r -k 1 key <&$TTY_FD; then
            print -u $TTY_FD
            fail "mode selection ended before a choice; no files were changed"
        fi
        case $key in
            [1-4])
                focus=$key
                SELECTED_MODE=$MODE_VALUES[$focus]
                break
                ;;
            $'\t')
                (( focus = focus % 4 + 1 ))
                print -n -u $TTY_FD -- $'\r\033[2K'
                print -n -u $TTY_FD -- "Focus: "
                print -n -u $TTY_FD -- $'\033[7m'"$MODE_LABELS[$focus]"$'\033[0m'
                print -n -u $TTY_FD -- " (Enter to choose)"
                ;;
            $'\n'|$'\r')
                if (( focus > 0 )); then
                    SELECTED_MODE=$MODE_VALUES[$focus]
                    break
                fi
                ;;
            *)
                # Unknown keys cannot become consent and leave no bytes for a
                # later shell because this process owns the terminal read.
                ;;
        esac
    done
    print -u $TTY_FD
    exec {TTY_FD}>&-
    TTY_FD=""
}

if (( CONFIG_EXISTS == 0 )); then
    if [[ -n $REQUESTED_MODE ]]; then
        SELECTED_MODE=$REQUESTED_MODE
    else
        choose_mode
    fi
fi

print "oopsinput will make these user-level changes:"
print -r -- "  install binary:      $(_oopsinput_escape_for_display "$BIN_DST")"
print -r -- "  install plugin:      $(_oopsinput_escape_for_display "$PLUGIN_DST")"
print -r -- "  install uninstaller: $(_oopsinput_escape_for_display "$UNINSTALL_DST")"
if (( CONFIG_EXISTS == 0 )); then
    print -r -- "  create config:       $(_oopsinput_escape_for_display "$CONFIG") (mode = $SELECTED_MODE)"
else
    print -r -- "  preserve config:     $(_oopsinput_escape_for_display "$CONFIG")"
fi
print -r -- "  add/update block:    $(_oopsinput_escape_for_display "$ZSHRC")"
[[ ! -f $ZSHRC ]] || print -r -- "  preserve backup:     $(_oopsinput_escape_for_display "$ZSHRC_BACKUP")"

# Every complete new file and every rollback copy is staged before commit.
typeset BIN_TMP="" PLUGIN_TMP="" UNINSTALL_TMP="" CONFIG_TMP=""
typeset ZSHRC_TMP="" BACKUP_TMP=""
typeset BIN_OLD="" PLUGIN_OLD="" UNINSTALL_OLD=""
typeset ZSHRC_OLD="" BACKUP_OLD=""
integer HAD_BIN=0 HAD_PLUGIN=0 HAD_UNINSTALL=0 HAD_ZSHRC=0 HAD_BACKUP=0
integer BACKUP_WILL_WRITE=0 CREATED_BIN_DIR=0 CREATED_PLUGIN_DIR=0 CREATED_CONFIG_DIR=0
integer COMMIT_STARTED=0 COMMIT_DONE=0

[[ ! -e $BIN_DST ]] || HAD_BIN=1
[[ ! -e $PLUGIN_DST ]] || HAD_PLUGIN=1
[[ ! -e $UNINSTALL_DST ]] || HAD_UNINSTALL=1
[[ ! -e $ZSHRC ]] || HAD_ZSHRC=1
[[ ! -e $ZSHRC_BACKUP ]] || HAD_BACKUP=1

cleanup() {
    set +e
    if (( COMMIT_STARTED == 1 && COMMIT_DONE == 0 )); then
        if (( HAD_BIN == 1 )); then
            mv -f -- $BIN_OLD $BIN_DST
            BIN_OLD=""
        else
            rm -f -- $BIN_DST
        fi
        if (( HAD_PLUGIN == 1 )); then
            mv -f -- $PLUGIN_OLD $PLUGIN_DST
            PLUGIN_OLD=""
        else
            rm -f -- $PLUGIN_DST
        fi
        if (( HAD_UNINSTALL == 1 )); then
            mv -f -- $UNINSTALL_OLD $UNINSTALL_DST
            UNINSTALL_OLD=""
        else
            rm -f -- $UNINSTALL_DST
        fi
        if (( HAD_ZSHRC == 1 )); then
            mv -f -- $ZSHRC_OLD $ZSHRC
            ZSHRC_OLD=""
        else
            rm -f -- $ZSHRC
        fi
        if (( CONFIG_EXISTS == 0 )); then
            rm -f -- $CONFIG
        fi
        if (( BACKUP_WILL_WRITE == 1 )); then
            if (( HAD_BACKUP == 1 )); then
                mv -f -- $BACKUP_OLD $ZSHRC_BACKUP
                BACKUP_OLD=""
            else
                rm -f -- $ZSHRC_BACKUP
            fi
        fi
    fi

    local tmp
    for tmp in $BIN_TMP $PLUGIN_TMP $UNINSTALL_TMP $CONFIG_TMP $ZSHRC_TMP \
            $BACKUP_TMP $BIN_OLD $PLUGIN_OLD $UNINSTALL_OLD $ZSHRC_OLD $BACKUP_OLD; do
        [[ -z $tmp ]] || rm -f -- $tmp
    done

    if (( COMMIT_DONE == 0 )); then
        (( CREATED_CONFIG_DIR == 0 )) || rmdir -- $CONFIG_DIR 2>/dev/null
        (( CREATED_PLUGIN_DIR == 0 )) || rmdir -- $PLUGIN_DIR 2>/dev/null
        (( CREATED_BIN_DIR == 0 )) || rmdir -- $BIN_DIR 2>/dev/null
    fi
}
trap cleanup EXIT

if [[ ! -d $BIN_DIR ]]; then
    mkdir -p -- $BIN_DIR
    CREATED_BIN_DIR=1
fi
if [[ ! -d $PLUGIN_DIR ]]; then
    mkdir -p -- $PLUGIN_DIR
    chmod 700 -- $PLUGIN_DIR
    CREATED_PLUGIN_DIR=1
fi
if (( CONFIG_EXISTS == 0 )) && [[ ! -d $CONFIG_DIR ]]; then
    mkdir -p -- $CONFIG_DIR
    chmod 700 -- $CONFIG_DIR
    CREATED_CONFIG_DIR=1
fi

write_plugin_block() {
    print -r -- $MARK_BEGIN
    (( ADDED_SEPARATOR == 0 )) || print -r -- $MARK_RESTORE_NO_FINAL
    print -r -- "source ${(q)PLUGIN_DST}"
    print -r -- $MARK_END
}

ZSHRC_TMP=$(mktemp $HOME/.zshrc.oopsinput.XXXXXX)
if [[ -f $ZSHRC ]]; then
    if (( B_COUNT == 1 )); then
        : > $ZSHRC_TMP
        (( B <= 1 )) || sed -n "1,$(( B - 1 ))p" -- $ZSHRC >> $ZSHRC_TMP
        write_plugin_block >> $ZSHRC_TMP
        sed -n "$(( E + 1 )),\$p" -- $ZSHRC >> $ZSHRC_TMP
    else
        sed -n '1,$p' -- $ZSHRC > $ZSHRC_TMP
        (( ADDED_SEPARATOR == 0 )) || print -r -- "" >> $ZSHRC_TMP
        write_plugin_block >> $ZSHRC_TMP
    fi
    chmod --reference=$ZSHRC $ZSHRC_TMP
else
    write_plugin_block > $ZSHRC_TMP
    chmod 600 -- $ZSHRC_TMP
fi

if [[ -f $ZSHRC ]] && { (( B_COUNT == 0 )) || [[ ! -e $ZSHRC_BACKUP ]]; }; then
    BACKUP_WILL_WRITE=1
    BACKUP_TMP=$(mktemp $HOME/.zshrc.oopsinput-backup.XXXXXX)
    cp -p -- $ZSHRC $BACKUP_TMP
fi

BIN_TMP=$(mktemp $BIN_DIR/.oopsinput.install.XXXXXX)
PLUGIN_TMP=$(mktemp $PLUGIN_DIR/.oopsinput.zsh.install.XXXXXX)
UNINSTALL_TMP=$(mktemp $PLUGIN_DIR/.uninstall.zsh.install.XXXXXX)
cp -- $BIN_SRC $BIN_TMP
chmod 755 -- $BIN_TMP
cp -- $PLUGIN_SRC $PLUGIN_TMP
chmod 600 -- $PLUGIN_TMP
cp -- $UNINSTALL_SRC $UNINSTALL_TMP
chmod 700 -- $UNINSTALL_TMP

if (( CONFIG_EXISTS == 0 )); then
    CONFIG_TMP=$(mktemp $CONFIG_DIR/.config.install.XXXXXX)
    {
        print "# oopsinput config (see SPEC.md §15 for the full surface)"
        print "mode = $SELECTED_MODE   # shadow | suggest | warn | confirm"
    } > $CONFIG_TMP
    chmod 600 -- $CONFIG_TMP
fi

# Rollback copies live beside their destinations so restoration is an atomic
# rename on the same filesystem.
if (( HAD_BIN == 1 )); then
    BIN_OLD=$(mktemp $BIN_DIR/.oopsinput.rollback.XXXXXX)
    cp -p -- $BIN_DST $BIN_OLD
fi
if (( HAD_PLUGIN == 1 )); then
    PLUGIN_OLD=$(mktemp $PLUGIN_DIR/.oopsinput.zsh.rollback.XXXXXX)
    cp -p -- $PLUGIN_DST $PLUGIN_OLD
fi
if (( HAD_UNINSTALL == 1 )); then
    UNINSTALL_OLD=$(mktemp $PLUGIN_DIR/.uninstall.zsh.rollback.XXXXXX)
    cp -p -- $UNINSTALL_DST $UNINSTALL_OLD
fi
if (( HAD_ZSHRC == 1 )); then
    ZSHRC_OLD=$(mktemp $HOME/.zshrc.oopsinput-rollback.XXXXXX)
    cp -p -- $ZSHRC $ZSHRC_OLD
fi
if (( BACKUP_WILL_WRITE == 1 && HAD_BACKUP == 1 )); then
    BACKUP_OLD=$(mktemp $HOME/.zshrc.oopsinput-backup-rollback.XXXXXX)
    cp -p -- $ZSHRC_BACKUP $BACKUP_OLD
fi

maybe_test_fail() {
    [[ ${OOPSINPUT_TEST_FAIL_AFTER:-} != $1 ]] || fail "injected test failure after $1"
}

# Private acceptance seam: deliver a real signal at a named commit boundary so
# the shipped script's signal trap and rollback path can be exercised without
# relying on an unrepeatable race against fast renames.
maybe_test_signal() {
    [[ ${OOPSINPUT_TEST_SIGNAL_AFTER:-} != $1 ]] || kill -TERM $$
}

COMMIT_STARTED=1
if (( BACKUP_WILL_WRITE == 1 )); then
    mv -f -- $BACKUP_TMP $ZSHRC_BACKUP
    BACKUP_TMP=""
fi
mv -f -- $BIN_TMP $BIN_DST
BIN_TMP=""
maybe_test_signal binary
maybe_test_fail binary
mv -f -- $PLUGIN_TMP $PLUGIN_DST
PLUGIN_TMP=""
maybe_test_signal plugin
maybe_test_fail plugin
mv -f -- $UNINSTALL_TMP $UNINSTALL_DST
UNINSTALL_TMP=""
maybe_test_signal uninstaller
maybe_test_fail uninstaller
if (( CONFIG_EXISTS == 0 )); then
    mv -f -- $CONFIG_TMP $CONFIG
    CONFIG_TMP=""
fi
maybe_test_signal config
maybe_test_fail config
mv -f -- $ZSHRC_TMP $ZSHRC
ZSHRC_TMP=""
maybe_test_signal zshrc
maybe_test_fail zshrc
COMMIT_DONE=1
cleanup
trap - EXIT HUP INT TERM

print -r -- "installed binary: $(_oopsinput_escape_for_display "$BIN_DST")"
print -r -- "installed plugin: $(_oopsinput_escape_for_display "$PLUGIN_DST")"
print -r -- "installed uninstaller: $(_oopsinput_escape_for_display "$UNINSTALL_DST")"
if (( B_COUNT == 1 )); then
    print -r -- "updated plugin block in $(_oopsinput_escape_for_display "$ZSHRC")"
else
    print -r -- "added plugin block to $(_oopsinput_escape_for_display "$ZSHRC")"
fi
if (( CONFIG_EXISTS == 0 )); then
    print "mode: $SELECTED_MODE"
else
    print -r -- "mode: preserved from $(_oopsinput_escape_for_display "$CONFIG")"
fi
print "installed — open a new terminal, then verify the live shell with:"
DOCTOR_COMMAND="${(q)BIN_DST} doctor"
print -r -- "  $(_oopsinput_escape_for_display "$DOCTOR_COMMAND")"
print "remove later without this checkout or archive with:"
UNINSTALL_COMMAND="zsh ${(q)UNINSTALL_DST}"
print -r -- "  $(_oopsinput_escape_for_display "$UNINSTALL_COMMAND")"
