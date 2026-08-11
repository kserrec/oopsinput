#!/usr/bin/env zsh
# oopsinput uninstaller: removes the marked ~/.zshrc block and the installed
# binary/plugin/uninstaller. It deliberately leaves recorded state and config; run
# `oopsinput purge` first if recorded state should also go. Idempotent.
set -eu
umask 077

PATH=/usr/bin:/bin

ZSHRC=$HOME/.zshrc
ZSHRC_BACKUP=$HOME/.zshrc.oopsinput-backup
BIN=$HOME/.local/bin/oopsinput
PLUGIN_DIR=$HOME/.local/share/oopsinput
PLUGIN=$PLUGIN_DIR/oopsinput.zsh
UNINSTALLER=$PLUGIN_DIR/uninstall.zsh
MARK_BEGIN="# >>> oopsinput >>>"
MARK_END="# <<< oopsinput <<<"
MARK_RESTORE_NO_FINAL="# oopsinput: restore preceding no-final-newline"

# Keep every path printed by the uninstaller inert in a terminal. Zsh's (V)
# covers control bytes; the explicit pass covers bidi and invisible formats.
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
    print -u2 -r -- "uninstall: $(_oopsinput_escape_for_display "$1")"
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
[[ ! -e $UNINSTALLER || -f $UNINSTALLER || -L $UNINSTALLER ]] || fail "refusing to remove non-file at $UNINSTALLER"

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
        (( ADDED_SEPARATOR == 0 || B > 1 )) || fail "oopsinput newline receipt in $ZSHRC has no preceding content; refusing to edit the file"
    fi
fi

ZSHRC_TMP=""
ZSHRC_SUFFIX_TMP=""
cleanup() {
    [[ -z ${ZSHRC_TMP:-} ]] || rm -f -- $ZSHRC_TMP
    [[ -z ${ZSHRC_SUFFIX_TMP:-} ]] || rm -f -- $ZSHRC_SUFFIX_TMP
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

if (( B_COUNT == 1 )); then
    ZSHRC_TMP=$(mktemp $HOME/.zshrc.oopsinput.XXXXXX)
    if [[ ! -e $ZSHRC_BACKUP ]]; then
        cp -p -- $ZSHRC $ZSHRC_BACKUP
    fi
    : > $ZSHRC_TMP
    (( B <= 1 )) || sed -n "1,$(( B - 1 ))p" -- $ZSHRC >> $ZSHRC_TMP
    ZSHRC_SUFFIX_TMP=$(mktemp $HOME/.zshrc.oopsinput-suffix.XXXXXX)
    sed -n "$(( E + 1 )),\$p" -- $ZSHRC > $ZSHRC_SUFFIX_TMP
    if (( ADDED_SEPARATOR == 1 )) && [[ ! -s $ZSHRC_SUFFIX_TMP ]]; then
        # The installer added exactly this one byte to put its marker on a
        # separate parseable line. Restore the old final-byte shape only while
        # the block is still last; a user-added suffix needs that separator or
        # its first line would merge into the preceding command.
        truncate -s -1 -- $ZSHRC_TMP
    fi
    sed -n '1,$p' -- $ZSHRC_SUFFIX_TMP >> $ZSHRC_TMP
    rm -f -- $ZSHRC_SUFFIX_TMP
    ZSHRC_SUFFIX_TMP=""
    chmod --reference=$ZSHRC $ZSHRC_TMP
    mv -f -- $ZSHRC_TMP $ZSHRC
    ZSHRC_TMP=""
    print -r -- "removed plugin block from $(_oopsinput_escape_for_display "$ZSHRC") (backup at $(_oopsinput_escape_for_display "$ZSHRC_BACKUP"))"
else
    print -r -- "no plugin block found in $(_oopsinput_escape_for_display "$ZSHRC")"
fi

if (( B_COUNT == 1 )); then
    if [[ -e $BIN || -L $BIN ]]; then
        rm -- $BIN
        print -r -- "removed $(_oopsinput_escape_for_display "$BIN")"
    fi
    if [[ -e $PLUGIN || -L $PLUGIN ]]; then
        rm -- $PLUGIN
        print -r -- "removed $(_oopsinput_escape_for_display "$PLUGIN")"
    fi
    if [[ -e $UNINSTALLER || -L $UNINSTALLER ]]; then
        rm -- $UNINSTALLER
        print -r -- "removed $(_oopsinput_escape_for_display "$UNINSTALLER")"
    fi
    if [[ -d $PLUGIN_DIR ]]; then
        if rmdir -- $PLUGIN_DIR 2>/dev/null; then
            print -r -- "removed empty $(_oopsinput_escape_for_display "$PLUGIN_DIR")"
        else
            print -r -- "kept non-empty $(_oopsinput_escape_for_display "$PLUGIN_DIR")"
        fi
    fi
fi

trap - EXIT HUP INT TERM
print "done — open a new terminal for a clean shell."
print "recorded state and config (if any) remain; the uninstaller never deletes user data."
