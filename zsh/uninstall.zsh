#!/usr/bin/env zsh
# oopsinput uninstaller: removes the marked block from ~/.zshrc and the binary
# from ~/.local/bin. It deliberately leaves recorded state and config; run
# `oopsinput purge` before this script if recorded state should also go.
# Idempotent.
set -eu

ZSHRC=$HOME/.zshrc
BIN=$HOME/.local/bin/oopsinput
MARK_BEGIN="# >>> oopsinput >>>"
MARK_END="# <<< oopsinput <<<"

if [[ -f $ZSHRC ]] && grep -qF -- $MARK_BEGIN $ZSHRC; then
    # A pattern-range delete with a missing/misplaced end marker would eat
    # everything to end-of-file. Resolve both markers to line numbers and
    # refuse unless they form a sane block.
    B=$(grep -nF -m1 -- $MARK_BEGIN $ZSHRC | cut -d: -f1)
    E=$(grep -nF -- $MARK_END $ZSHRC | tail -1 | cut -d: -f1 || true)
    if [[ -z ${E:-} ]] || (( E < B )); then
        print -u2 "uninstall: oopsinput block markers in $ZSHRC are damaged (begin without a matching end)"
        print -u2 "uninstall: refusing to edit the file — remove the block by hand, then re-run"
        exit 1
    fi
    cp $ZSHRC $ZSHRC.oopsinput-backup
    sed -i "${B},${E}d" $ZSHRC
    print "removed plugin block from $ZSHRC (backup at $ZSHRC.oopsinput-backup)"
else
    print "no plugin block found in $ZSHRC"
fi

if [[ -x $BIN ]]; then
    rm $BIN
    print "removed $BIN"
fi

print "done — open a new terminal for a clean shell."
print "recorded state and config (if any) remain; the uninstaller never deletes user data."
