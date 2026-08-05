#!/usr/bin/env zsh
# oopsinput installer: copies the release binary to ~/.local/bin and adds a
# marked source block to ~/.zshrc (backed up first). Idempotent. No root.
set -eu

SCRIPT_DIR=${0:A:h}
REPO_ROOT=${SCRIPT_DIR:h}
BIN_SRC=$REPO_ROOT/target/release/oopsinput
BIN_DST=$HOME/.local/bin/oopsinput
ZSHRC=$HOME/.zshrc
MARK_BEGIN="# >>> oopsinput >>>"
MARK_END="# <<< oopsinput <<<"

if [[ ! -x $BIN_SRC ]]; then
    print -u2 "install: release binary not found at $BIN_SRC"
    print -u2 "install: build it first:  cargo build --release"
    exit 1
fi

mkdir -p $HOME/.local/bin
cp $BIN_SRC $BIN_DST
chmod 755 $BIN_DST
print "installed binary: $BIN_DST"

if [[ -f $ZSHRC ]] && grep -qF $MARK_BEGIN $ZSHRC; then
    print "plugin block already present in $ZSHRC — leaving it as is"
else
    if [[ -f $ZSHRC ]]; then
        cp $ZSHRC $ZSHRC.oopsinput-backup
        print "backed up $ZSHRC -> $ZSHRC.oopsinput-backup"
    fi
    {
        print ""
        print $MARK_BEGIN
        print "source ${(q)SCRIPT_DIR}/oopsinput.zsh"
        print $MARK_END
    } >> $ZSHRC
    print "added plugin block to $ZSHRC"
fi

print "done — open a new terminal (or: source $ZSHRC). Mode: shadow (observe only)."
