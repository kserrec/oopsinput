#!/usr/bin/env zsh
# oopsinput installer: copies the release binary and plugin to stable paths,
# then adds one marked source block to ~/.zshrc. Idempotent. No root.
set -eu
umask 077

# Do not let a repository-local executable shadow an installer helper.
PATH=/usr/bin:/bin

SCRIPT_DIR=${0:A:h}
REPO_ROOT=${SCRIPT_DIR:h}
# These are test seams. Users build the release binary and run this script
# without setting either variable.
BIN_SRC=${OOPSINPUT_BIN_SRC:-$REPO_ROOT/target/release/oopsinput}
PLUGIN_SRC=${OOPSINPUT_PLUGIN_SRC:-$SCRIPT_DIR/oopsinput.zsh}
BIN_DIR=$HOME/.local/bin
BIN_DST=$BIN_DIR/oopsinput
PLUGIN_DIR=$HOME/.local/share/oopsinput
PLUGIN_DST=$PLUGIN_DIR/oopsinput.zsh
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

if [[ ! -x $BIN_SRC ]]; then
    print -u2 -r -- "install: release binary not found at $(_oopsinput_escape_for_display "$BIN_SRC")"
    print -u2 "install: build it first:  cargo build --release"
    exit 1
fi
[[ -f $PLUGIN_SRC ]] || fail "plugin not found at $PLUGIN_SRC"

# Never copy through a destination symlink or replace a non-regular path.
# `cp source symlink` follows the link and overwrites its target.
[[ ! -L $BIN_DST ]] || fail "refusing to replace symlink at $BIN_DST"
[[ ! -e $BIN_DST || -f $BIN_DST ]] || fail "refusing to replace non-file at $BIN_DST"
[[ ! -L $PLUGIN_DIR ]] || fail "refusing to enter symlink at $PLUGIN_DIR"
[[ ! -e $PLUGIN_DIR || -d $PLUGIN_DIR ]] || fail "refusing to replace non-directory at $PLUGIN_DIR"
[[ ! -L $PLUGIN_DST ]] || fail "refusing to replace symlink at $PLUGIN_DST"
[[ ! -e $PLUGIN_DST || -f $PLUGIN_DST ]] || fail "refusing to replace non-file at $PLUGIN_DST"

# Replacing a symlinked shell file would sever the link; following one would
# edit a file the installer never resolved. Leave either case to the user.
[[ ! -L $ZSHRC ]] || fail "refusing to edit symlinked $ZSHRC"
[[ ! -e $ZSHRC || -f $ZSHRC ]] || fail "refusing to edit non-file $ZSHRC"
[[ ! -L $ZSHRC_BACKUP ]] || fail "refusing to overwrite symlink at $ZSHRC_BACKUP"
[[ ! -e $ZSHRC_BACKUP || -f $ZSHRC_BACKUP ]] || fail "refusing to overwrite non-file at $ZSHRC_BACKUP"

# Validate the marker block before installing anything. Multiple, missing, or
# reversed markers make the edit boundary ambiguous, so leave ~/.zshrc alone.
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
        # therefore means the existing final byte is not newline (zsh startup
        # files cannot meaningfully contain NUL).
        [[ -z $(tail -c 1 -- $ZSHRC) ]] || ADDED_SEPARATOR=1
    fi
fi

# A healthy marker block is the install receipt that authorizes an update.
# Without it, an existing regular file at either destination may belong to
# something else that happens to use the same name; a fresh install refuses.
if (( B_COUNT == 0 )); then
    [[ ! -e $BIN_DST ]] || fail "file already exists at $BIN_DST; refusing to overwrite it without an existing oopsinput block"
    [[ ! -e $PLUGIN_DST ]] || fail "file already exists at $PLUGIN_DST; refusing to overwrite it without an existing oopsinput block"
fi

# Default config (SPEC §8: new installs run shadow + suggest). Never touches
# an existing config; user-only permissions (SPEC §9-4). -e misses a
# dangling symlink, so -L is deliberately checked too.
CONFIG_DIR=${XDG_CONFIG_HOME:-$HOME/.config}/oopsinput
CONFIG=$CONFIG_DIR/config
if [[ -e $CONFIG || -L $CONFIG ]]; then
    print -r -- "config already present: $(_oopsinput_escape_for_display "$CONFIG") — leaving it as is"
else
    mkdir -p -- $CONFIG_DIR
    chmod 700 -- $CONFIG_DIR
    {
        print "# oopsinput config (see SPEC.md §15 for the full surface)"
        print "mode = suggest   # shadow | suggest | warn | confirm"
    } > $CONFIG
    chmod 600 -- $CONFIG
    print -r -- "wrote default config: $(_oopsinput_escape_for_display "$CONFIG") (mode = suggest)"
fi

mkdir -p -- $BIN_DIR $PLUGIN_DIR
chmod 700 -- $PLUGIN_DIR

# Stage every complete file before installing any runtime asset. In particular,
# a backup or `.zshrc` staging failure must happen before fresh binary/plugin
# destinations exist; otherwise the missing marker makes both retry and
# uninstall correctly refuse those now-stranded paths.
BIN_TMP=""
PLUGIN_TMP=""
ZSHRC_TMP=""
integer FRESH_BIN_INSTALLED=0 FRESH_PLUGIN_INSTALLED=0
cleanup() {
    [[ -z ${BIN_TMP:-} ]] || rm -f -- $BIN_TMP
    [[ -z ${PLUGIN_TMP:-} ]] || rm -f -- $PLUGIN_TMP
    [[ -z ${ZSHRC_TMP:-} ]] || rm -f -- $ZSHRC_TMP
    if (( B_COUNT == 0 )); then
        (( FRESH_BIN_INSTALLED == 0 )) || rm -f -- $BIN_DST
        (( FRESH_PLUGIN_INSTALLED == 0 )) || rm -f -- $PLUGIN_DST
    fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

write_plugin_block() {
    print -r -- $MARK_BEGIN
    (( ADDED_SEPARATOR == 0 )) || print -r -- $MARK_RESTORE_NO_FINAL
    print -r -- "source ${(q)PLUGIN_DST}"
    print -r -- $MARK_END
}

# A repeat install also migrates the old repository-pointing source block to
# the installed plugin. Preserve every line outside the validated block and
# preserve the shell file's mode.
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
    if (( B_COUNT == 0 )) || [[ ! -e $ZSHRC_BACKUP ]]; then
        cp -p -- $ZSHRC $ZSHRC_BACKUP
        print -r -- "backed up $(_oopsinput_escape_for_display "$ZSHRC") -> $(_oopsinput_escape_for_display "$ZSHRC_BACKUP")"
    else
        print -r -- "kept original backup: $(_oopsinput_escape_for_display "$ZSHRC_BACKUP")"
    fi
else
    write_plugin_block > $ZSHRC_TMP
    chmod 600 -- $ZSHRC_TMP
fi

BIN_TMP=$(mktemp $BIN_DIR/.oopsinput.install.XXXXXX)
PLUGIN_TMP=$(mktemp $PLUGIN_DIR/.oopsinput.zsh.install.XXXXXX)
cp -- $BIN_SRC $BIN_TMP
chmod 755 -- $BIN_TMP
cp -- $PLUGIN_SRC $PLUGIN_TMP
chmod 600 -- $PLUGIN_TMP
(( B_COUNT != 0 )) || FRESH_BIN_INSTALLED=1
mv -f -- $BIN_TMP $BIN_DST
BIN_TMP=""
(( B_COUNT != 0 )) || FRESH_PLUGIN_INSTALLED=1
mv -f -- $PLUGIN_TMP $PLUGIN_DST
PLUGIN_TMP=""

mv -f -- $ZSHRC_TMP $ZSHRC
ZSHRC_TMP=""
trap - EXIT HUP INT TERM

print -r -- "installed binary: $(_oopsinput_escape_for_display "$BIN_DST")"
print -r -- "installed plugin: $(_oopsinput_escape_for_display "$PLUGIN_DST")"

if (( B_COUNT == 1 )); then
    print -r -- "updated plugin block in $(_oopsinput_escape_for_display "$ZSHRC")"
else
    print -r -- "added plugin block to $(_oopsinput_escape_for_display "$ZSHRC")"
fi

print -r -- "done — open a new terminal (or: source $(_oopsinput_escape_for_display "$ZSHRC"))."
print "mode: suggest — typo prompts for commands that don't resolve; everything else is shadow-recorded only."
