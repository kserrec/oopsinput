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

## M2 — Lexer + typo layer (first visible value) ✅ 2026-08-06

- [x] lexer.rs: conservative lexer per SPEC §13, fuzz smoke test, never
      panics; uncertainty codes (`syntax.*`) + `input.capped` wired into
      decision evidence and shadow events, so the SPEC §11 unsupported-syntax
      rate is measurable from shadow data (2026-08-05; in-binary check p50
      ~240 µs with lexing)
- [x] Deferred bughunt finding: the 1 MB input cap errors (fail-open, event
      lost) when it lands mid-UTF-8-character — read bytes and truncate at a
      char boundary so oversized input is capped-and-analyzed instead
      (2026-08-05)
- [x] layers/typo.rs: exact resolution check, bounded edit distance vs PATH +
      plugin-supplied names (2026-08-06: fires only on res=none with a literal
      first word; bounded OSA distance ≤1/≤2 by length; PATH scanned by
      readdir with x-bit verification and hard caps; plugin ships the
      alias/function/builtin/resword pool NUL-separated on stdin, only on the
      already-failing none path; shadow evidence `typo.candidate_d1/_d2`,
      names never logged; typo path ~9 ms p50 in-binary, resolving path
      unchanged ~240 µs)
- [x] ui.rs: /dev/tty single-key prompts; escaping pass (control/ANSI/OSC/bidi
      neutralized) + fuzz target (2026-08-06: escaper neutralizes all Cc
      controls, bidi embeddings/overrides/isolates, zero-width/invisible
      formatting — caret notation for C0, visible \u{...} otherwise; fuzz
      smoke asserts nothing active survives + idempotence; single-key prompt
      via stty on /dev/tty — -icanon -echo -isig, VTIME timeout 10 s → `n`,
      Ctrl-C read as 0x03 = cancel, stty -g state restored by Drop guard;
      real-tty PTY tests via debug-only `__prompt-typo-test` seam; caller
      contract: neutralize watchdog before prompting — enforced at wiring,
      next item)
- [x] `y` runs correction / `n` runs original / Ctrl-C cancels; replacement
      returns on fd 3, never argv/stdout — **security-critical channel** (audit
      2026-08-05): the one path where binary output becomes an executed
      command; same rigor as the event log (exact bytes, no interpretation,
      pinned tests) (2026-08-06: replacement_buffer() swaps only the command
      word with pinned byte-exactness tests, refuses on any boundary
      disagreement; fd 3 reopened via /dev/fd/3 (no-unsafe rule) and routed
      to the plugin through `3>&1` capture; exact bytes + one NUL sentinel —
      survives $()'s newline stripping and doubles as truncation guard,
      plugin runs replacement only with sentinel intact, else fails open;
      exit 10 only after a complete successful write; watchdog retires via
      PROMPT_ACTIVE once a prompt is on screen (PTY-tested past the
      deadline); minimal SPEC §15 mode gate landed early: $OOPSINPUT_MODE >
      config `mode` key > shadow, closed vocabulary; PTY tests cover
      y/n/Ctrl-C end-to-end incl. event outcomes typo.accepted / declined /
      cancelled)
- [x] `suggest` mode enabled by default post-install (2026-08-06: install.zsh
      writes `mode = suggest` to $XDG_CONFIG_HOME/oopsinput/config (or
      ~/.config) with 0700/0600 perms, never touching an existing config;
      uninstall message covers config removal; tests/install.rs covers fresh
      default + perms, existing-config untouched, idempotency; PTY test
      proves the exact installed config artifact enables prompts through the
      real config path)
- [x] Acceptance: golden typo cases pass; command words that resolve NEVER
      prompt; p95 within budget (2026-08-06: eval/golden/typo.json — 19 cases,
      42% counterfactual pairs (≥30% ratio asserted in the runner), hermetic
      via analyze_with_path with empty PATH, pinning candidate + exact
      evidence assembly; PTY test proves alias/builtin/command/chain words
      never prompt in suggest mode; release end-to-end incl. spawn:
      deterministic p50 3.6 ms / p95 4.4 ms (budget 25), typo path with 2k
      names + full PATH scan p50 16.3 ms / p95 19.5 ms (budget 75))

**M2 complete 2026-08-06** — typing a misspelled command in suggest mode
prompts with the nearest real command; `y` runs it, `n`/timeout runs the
original, Ctrl-C cancels; resolving words never prompt.

Post-M2 refactor + bughunt + audit passes ✅ 2026-08-06 (same session).
Refactor: deduped PTY test scaffolding and the plugin's binary invocation,
zero functional change. Bughunt fixed six findings, each regression-pinned —
doctor's config line ignored $XDG_CONFIG_HOME while its mode line honored it
(contradictory output); the lexer split words on any Unicode whitespace while
zsh splits only on space/tab/newline, so a pasted no-break space made a
"correction" that still failed (`is_shell_whitespace` now governs every
word-boundary decision); candidate names carrying whitespace/metacharacters
could reparse when spliced; exact-match suppression sat behind the stat
budget; a decision-JSON serialization failure could override the exit code
after a replacement was already delivered; a NUL separator landing exactly on
the 1 MB cap mislabeled `input.capped`. Audit fixed four, each re-verified
with the probe that proved it: `stty` was resolved via $PATH (hostile
directory leading $PATH ran on any typo — now absolute path); external
helpers had no timeout and run after the watchdog retires (now `run_bounded`
kills and reaps at a deadline, satisfying SPEC §9-1); candidate names could
carry control characters; install.zsh's `-f` guard wrote through a dangling
config symlink (now `-e || -L`).

## M3 — Danger + context layers, policy, warning UI ✅ 2026-08-06

- [x] layers/danger.rs: rule tables per SPEC §5-L2 (fs, git, system, privilege)
      — ✅ 2026-08-06: candidate marking + direct-catastrophic flag (recursive
      delete of / or ~); codes feed the shadow event log now, policy consumes
      them next. priv.sudo fires only when the wrapped command tripped a rule.
      Golden corpus eval/golden/danger.json (command-shape pairs; the
      context-flip pairs arrive with policy + L3 below)
- [x] layers/context.rs: git facts, target facts, near-miss targets — all
      hard-capped syscall collectors — ✅ 2026-08-06: runs only on L2
      candidates (common path stays syscall-free); git status as an external
      helper per the standing rule (absolute path, fixed argv, 80 ms kill);
      honest None when evidence is unavailable. Danger layer now hands its
      literal targets to L3. Measured on this repo: candidate path ~4.7 ms
      incl. git spawn (debug build; release figures in PLAN's header)
- [x] policy.rs: evidence → decision matrix; direct-catastrophic subset;
      intervention budget + per-rule cooldown; shadow conversion; full SPEC
      §15 config surface incl. warn-once on unknown keys — ✅ 2026-08-06.
      `warranted` is the mode-blind matrix the golden corpus pins;
      `cap_for_mode` downgrades preserve the policy reason (that IS the
      shadow conversion — an `observe` with reason `policy.dirty_work_at_risk`
      is a hypothetical intervention for the M5 report). Budget/cooldown
      machinery built and tested but not consumed at runtime until the
      warning UI exists (gates run with commit=false semantics; nothing
      invisible may spend budget). det_timeout_ms now drives the watchdog.
      Flagship pair proven live: dirty `git reset --hard` → observe/
      dirty_work_at_risk; probe in ~4.7 ms
- [x] Warning UI: anatomy per SPEC §7; e/edit c/cancel r/run-once; exact
      buffer restore on edit (PTY-tested) — ✅ 2026-08-06. Warn tier is
      advisory (timeout runs), confirm tier pauses (timeout cancels — `r`
      never the default). Budget/cooldown gates went live with it (spend
      only on actually-shown prompts); outcomes land in the event log. The
      deferred multi-byte key bug is fixed: the key reader consumes complete
      escape sequences (CSI/SS3/alt-chords), PTY-pinned by
      arrow_keys_at_a_prompt_leave_no_stray_bytes. Both flagship acceptance
      halves PTY-proven: dirty reset warns with facts named and cancel has
      zero side effects; clean reset passes silently
- [x] recency relation (rest of SPEC §5-L3) — ✅ 2026-08-06, stronger than
      spec'd: instead of stripping secrets from history text, the plugin
      computes the relation itself and sends only sanitized summaries (age,
      shares-a-word bit, first two words constrained to [A-Za-z0-9_-]{1,32},
      else "_") — no raw history text ever crosses; the binary re-sanitizes.
      Payload gained a third NUL section; surfaces as
      recency.target_overlap evidence and the "previous command: git diff"
      warning line (PTY-proven). Also hardened run_staged: a missing marker
      now panics with the transcript instead of hanging the suite
- [x] eval/golden: paired counterfactual cases for every danger rule (≥30%
      pairs) — ✅ 2026-08-06: danger.json 41 command-shape cases +
      policy.json 19 context-flip cases; every danger rule has a paired
      presence; both corpora enforce the ratio in cargo test (CI itself: M6)
- [x] Acceptance — ✅ 2026-08-06, all PTY-proven: dirty `git reset --hard`
      warns with facts named / clean reset silent; corpus green; cancel has
      zero side effects (dirty bytes verified untouched on disk)

Post-M3 refactor + bughunt + audit passes ✅ 2026-08-06 (same session).
Refactor: one shared bounded wait-or-kill loop (`src/proc.rs`), one home for
the deterministic deadline, one golden-corpus runner, warning-message
building moved to ui.rs; zero functional change. Bughunt fixed seven, each
regression-pinned — the typo prompt preempted policy warnings on compound
buffers (`gti status; git reset --hard` warned about nothing); short-flag
clusters were read letter-by-letter so `rm -force /` became a catastrophic
confirm though rm rejects it outright; a redirect's existing target vouched
for `rm -rf $DIR`; the recency shares-a-word bit was always 1 (zsh's reverse
subscript search `(Ie)` returns 0 when absent, making the test a tautology)
and counted shared flags as shared targets; recency cost ~7.5 ms/command at
10k history (now 0.125 ms via direct HISTCMD lookups); `>&-` was recorded as
truncating a file named `-`; the PTY harness could hang the suite in its
drain tail. Audit fixed four, each re-verified with the probe that proved it:
`git status` executed `core.fsmonitor` from the analyzed repository's own
config, so typing `rm -rf ./build` inside a directory obtained from someone
else ran a stranger's program (exec-capable keys now neutralized on the
command line); recency words reached the terminal without the escaper; state
files were written through symlinks; the policy state file had no read or
entry cap.
