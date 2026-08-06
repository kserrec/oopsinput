# oopsinput — Plan archive

Completed milestones moved verbatim from PLAN.md; this file is the permanent
record and is never condensed. Newest entries at the bottom.

## M0 — Skeleton and governance ✅ 2026-08-05

- [x] Repo, SPEC.md, PLAN.md, CLAUDE.md, README, Apache-2.0 LICENSE
- [x] Single binary crate; `oopsinput version | check | doctor` compile and run
- [x] `check` reads a proposal from stdin, returns an allow decision as JSON
      (placeholder analysis — the seam is real, the brain isn't yet)
- [x] Pushed to github.com/kserrec/oopsinput

## M1 — Zsh capture, shadow passthrough ✅ 2026-08-05

The riskiest surface first: prove we can intercept and restore commands with
zero corruption before building any analysis.

- [x] `zsh/oopsinput.zsh`: wrap all accept widgets (Emacs + Vi keymaps),
      capture-and-delegate to prior widget, recursion guard
- [x] Buffer over stdin; fail-open deadline via in-binary watchdog (150 ms
      det path); diagnostic on missing binary at load
- [x] Plugin ships context: command-word resolution kind (closed vocabulary,
      enforced both plugin- and binary-side); cwd read by the binary itself
      *(recent-history summaries moved to M3 — nothing consumes them earlier)*
- [x] proposal.rs + events.rs: parse proposal, log structural shadow event
      (no raw text; 0700/0600 perms; `OOPSINPUT_STATE_DIR` override for tests)
- [x] install.zsh / uninstall.zsh with explicit markers + backup
- [x] PTY test harness (`tests/pty.rs`, via util-linux `script`): passthrough,
      unicode/quoting, multiline PS2, vi keymap, missing binary, hanging
      binary vs watchdog, secret-free event log, double-source, resolution
      kinds (incl. single-word regression)
- [x] Acceptance: 10,000 PTY submissions → 10,000/10,000 outputs, zero
      altered/lost buffers, zero hangs (128 s); hanging-binary test proves
      fail-open under a wedged process
- [x] Measured: per-command overhead p50 5 ms / p95 6 ms / p99 7 ms
      (budget: p95 ≤ 25 ms); installed on the dev machine in shadow mode

Bugs found & regression-locked during M1:
- nested `${$(whence -w ...)##*: }` doesn't strip in zsh → res_kind carried
  raw text into argv; fixed with two-step extraction + closed-vocabulary
  enforcement in the plugin
- `${${(z)BUFFER}[1]}` string-indexes (first *char*) when the buffer is a
  single word → fixed with explicit array assignment

## Post-M1 hardening ✅ 2026-08-05

Same-day refactor + bughunt + audit passes over M0/M1. Landed: no-functional-
change refactor; atomic single-write event append (concurrency, pinned);
doctor executable-bit check (pinned); test hooks gated to debug builds;
uninstall refuses damaged marker blocks instead of deleting to EOF (pinned);
load diagnostic renders control chars visibly via zsh (V) — note (qqqq) is
insufficient, it leaves control bytes raw (pinned); SPEC §6 aligned with the
implemented watchdog fail-open design. Deferred items live in M2/M6 below.

Name decision (research 2026-08-05, recorded so it isn't re-litigated):
"oopsinput" verified clear on GitHub (repos + org name), crates.io, npm,
PyPI, and .com/.dev/.io/.org domains, with no product using the name.
Rejected alternatives: `noops` (established NoOps industry term, npm+PyPI
taken), `oopsh` (existing OOPSH shell project + oopsh.com taken), `oopsys`
(French IT firm Oopsys + OOP ambiguity), `nooops` (triple-o typo trap).
