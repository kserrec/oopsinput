#!/usr/bin/env zsh
# oopsinput uninstaller: removes the marked block from ~/.zshrc and the binary
# from ~/.local/bin. Leaves state/config for `oopsinput purge` (not yet built;
# rm -r ~/.local/state/oopsinput does the same by hand). Idempotent.
set -eu

ZSHRC=$HOME/.zshrc
BIN=$HOME/.local/bin/oopsinput
MARK_BEGIN="# >>> oopsinput >>>"
MARK_END="# <<< oopsinput <<<"

if [[ -f $ZSHRC ]] && grep -qF $MARK_BEGIN $ZSHRC; then
    cp $ZSHRC $ZSHRC.oopsinput-backup
    # Delete the marked block (inclusive). sed -i with a range on fixed markers.
    sed -i "\|^${MARK_BEGIN}\$|,\|^${MARK_END}\$|d" $ZSHRC
    print "removed plugin block from $ZSHRC (backup at $ZSHRC.oopsinput-backup)"
else
    print "no plugin block found in $ZSHRC"
fi

if [[ -x $BIN ]]; then
    rm $BIN
    print "removed $BIN"
fi

print "done — open a new terminal for a clean shell."
print "shadow data (if any) remains in ~/.local/state/oopsinput — delete it with:"
print "  rm -r ~/.local/state/oopsinput"
