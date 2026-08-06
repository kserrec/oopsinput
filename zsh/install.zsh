#!/usr/bin/env zsh
# oopsinput installer: copies the release binary to ~/.local/bin and adds a
# marked source block to ~/.zshrc (backed up first). Idempotent. No root.
set -eu

SCRIPT_DIR=${0:A:h}
REPO_ROOT=${SCRIPT_DIR:h}
# OOPSINPUT_BIN_SRC is a test seam (the test suite installs a stand-in
# binary); users just build release and run this.
BIN_SRC=${OOPSINPUT_BIN_SRC:-$REPO_ROOT/target/release/oopsinput}
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

# Default config (SPEC §8: new installs run shadow + suggest — typo prompts
# are safe from day one because they only fire on commands that could not
# run). Never touches an existing config; user-only perms (SPEC §9-4).
CONFIG_DIR=${XDG_CONFIG_HOME:-$HOME/.config}/oopsinput
CONFIG=$CONFIG_DIR/config
if [[ -f $CONFIG ]]; then
    print "config already present: $CONFIG — leaving it as is"
else
    mkdir -p $CONFIG_DIR
    chmod 700 $CONFIG_DIR
    {
        print "# oopsinput config (see SPEC.md §15 for the full surface)"
        print "mode = suggest   # shadow | suggest | warn | confirm"
    } > $CONFIG
    chmod 600 $CONFIG
    print "wrote default config: $CONFIG (mode = suggest)"
fi

print "done — open a new terminal (or: source $ZSHRC)."
print "mode: suggest — typo prompts for commands that don't resolve; everything else is shadow-recorded only."
