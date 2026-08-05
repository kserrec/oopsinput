# oopsinput zsh plugin — NOT YET FUNCTIONAL (lands in milestone M1, see PLAN.md)
#
# What this file will do:
#   - wrap accept-line and its sibling accept widgets (Emacs + Vi keymaps),
#     capturing and delegating to any previously installed widget
#   - on Enter: send $BUFFER + context (cwd, command-word resolution kind,
#     secret-stripped recent history) to `oopsinput check` over stdin
#   - interpret the exit code: 0 run unchanged · 10 replace buffer from fd 3 ·
#     11 restore buffer for editing · 12 cancel · anything else fail open
#   - bounded timeout; on any failure the original command runs unchanged
#
# Until M1 lands, sourcing this file is a no-op.
