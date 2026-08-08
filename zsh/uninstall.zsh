#!/usr/bin/env zsh
# oopsinput uninstaller: removes the marked ~/.zshrc block and the installed
# binary/plugin. It deliberately leaves recorded state and config; run
# `oopsinput purge` first if recorded state should also go. Idempotent.
set -eu
umask 077

PATH=/usr/bin:/bin

ZSHRC=$HOME/.zshrc
ZSHRC_BACKUP=$HOME/.zshrc.oopsinput-backup
BIN=$HOME/.local/bin/oopsinput
PLUGIN_DIR=$HOME/.local/share/oopsinput
PLUGIN=$PLUGIN_DIR/oopsinput.zsh
MARK_BEGIN="# >>> oopsinput >>>"
MARK_END="# <<< oopsinput <<<"

fail() {
    print -u2 -r -- "uninstall: $1"
    exit 1
}

[[ ! -L $ZSHRC ]] || fail "refusing to edit symlinked $ZSHRC"
[[ ! -e $ZSHRC || -f $ZSHRC ]] || fail "refusing to edit non-file $ZSHRC"
[[ ! -L $ZSHRC_BACKUP ]] || fail "refusing to overwrite symlink at $ZSHRC_BACKUP"
[[ ! -e $ZSHRC_BACKUP || -f $ZSHRC_BACKUP ]] || fail "refusing to overwrite non-file at $ZSHRC_BACKUP"
[[ ! -e $BIN || -f $BIN || -L $BIN ]] || fail "refusing to remove non-file at $BIN"
[[ ! -L $PLUGIN_DIR ]] || fail "refusing to enter symlink at $PLUGIN_DIR"
[[ ! -e $PLUGIN_DIR || -d $PLUGIN_DIR ]] || fail "refusing to enter non-directory at $PLUGIN_DIR"
[[ ! -e $PLUGIN || -f $PLUGIN || -L $PLUGIN ]] || fail "refusing to remove non-file at $PLUGIN"

integer B_COUNT=0 E_COUNT=0 B=0 E=0
if [[ -f $ZSHRC ]]; then
    B_COUNT=$(grep -cF -- $MARK_BEGIN $ZSHRC || true)
    E_COUNT=$(grep -cF -- $MARK_END $ZSHRC || true)
    if (( B_COUNT != E_COUNT || B_COUNT > 1 )); then
        fail "oopsinput block markers in $ZSHRC are damaged; refusing to edit the file"
    fi
    if (( B_COUNT == 1 )); then
        B=$(grep -nF -m1 -- $MARK_BEGIN $ZSHRC | cut -d: -f1)
        E=$(grep -nF -m1 -- $MARK_END $ZSHRC | cut -d: -f1)
        (( E >= B )) || fail "oopsinput block markers in $ZSHRC are out of order; refusing to edit the file"
    fi
fi

ZSHRC_TMP=""
cleanup() {
    [[ -z ${ZSHRC_TMP:-} ]] || rm -f -- $ZSHRC_TMP
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

if (( B_COUNT == 1 )); then
    ZSHRC_TMP=$(mktemp $HOME/.zshrc.oopsinput.XXXXXX)
    cp -p -- $ZSHRC $ZSHRC_BACKUP
    {
        (( B > 1 )) && sed -n "1,$(( B - 1 ))p" -- $ZSHRC
        sed -n "$(( E + 1 )),\$p" -- $ZSHRC
    } > $ZSHRC_TMP
    chmod --reference=$ZSHRC $ZSHRC_TMP
    mv -f -- $ZSHRC_TMP $ZSHRC
    ZSHRC_TMP=""
    print -r -- "removed plugin block from $ZSHRC (backup at $ZSHRC_BACKUP)"
else
    print -r -- "no plugin block found in $ZSHRC"
fi

if (( B_COUNT == 1 )); then
    if [[ -e $BIN || -L $BIN ]]; then
        rm -- $BIN
        print -r -- "removed $BIN"
    fi
    if [[ -e $PLUGIN || -L $PLUGIN ]]; then
        rm -- $PLUGIN
        print -r -- "removed $PLUGIN"
    fi
    if [[ -d $PLUGIN_DIR ]]; then
        if rmdir -- $PLUGIN_DIR 2>/dev/null; then
            print -r -- "removed empty $PLUGIN_DIR"
        else
            print -r -- "kept non-empty $PLUGIN_DIR"
        fi
    fi
fi

trap - EXIT HUP INT TERM
print "done — open a new terminal for a clean shell."
print "recorded state and config (if any) remain; the uninstaller never deletes user data."
