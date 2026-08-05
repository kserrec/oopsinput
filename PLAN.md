# oopsinput — Plan

Milestones are sized to complete in one focused session each. A milestone is
done when its acceptance checks pass. SPEC.md is canonical; check items off
here as they land.

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

## M2 — Lexer + typo layer (first visible value)

- [ ] lexer.rs: conservative lexer per SPEC §13, fuzz smoke test, never panics
- [ ] Deferred bughunt finding: the 1 MB input cap errors (fail-open, event
      lost) when it lands mid-UTF-8-character — read bytes and truncate at a
      char boundary so oversized input is capped-and-analyzed instead
- [ ] layers/typo.rs: exact resolution check, bounded edit distance vs PATH +
      plugin-supplied names
- [ ] ui.rs: /dev/tty single-key prompts; escaping pass (control/ANSI/OSC/bidi
      neutralized) + fuzz target
- [ ] `y` runs correction / `n` runs original / Ctrl-C cancels; replacement
      returns on fd 3, never argv/stdout — **security-critical channel** (audit
      2026-08-05): the one path where binary output becomes an executed
      command; same rigor as the event log (exact bytes, no interpretation,
      pinned tests)
- [ ] `suggest` mode enabled by default post-install
- [ ] Acceptance: golden typo cases pass; command words that resolve NEVER
      prompt; p95 within budget

## M3 — Danger + context layers, policy, warning UI

- [ ] layers/danger.rs: rule tables per SPEC §5-L2 (fs, git, system, privilege)
- [ ] layers/context.rs: git facts, target facts, recency relation, near-miss
      targets — all hard-capped syscall collectors
- [ ] policy.rs: evidence → decision matrix; direct-catastrophic subset;
      intervention budget + per-rule cooldown; shadow conversion
- [ ] Warning UI: anatomy per SPEC §7; e/edit c/cancel r/run-once; exact
      buffer restore on edit (PTY-tested)
- [ ] eval/golden: paired counterfactual cases for every danger rule (≥30%
      pairs); CI runs the corpus
- [ ] Acceptance: flagship pair behaves (dirty `git reset --hard` warns; clean
      scratch-branch reset silent); corpus green; zero side effects on cancel

## M4 — Inference layer

- [ ] model.rs: minimal loopback HTTP/1.1 client (std only), hard timeout,
      response size cap
- [ ] Prompt assembly: trusted evidence / untrusted command separation; schema-
      constrained output via Ollama structured outputs; validation → invalid =
      unavailable evidence
- [ ] Candidate gate: L4 only when L2 candidate ∧ L3 ambiguous; <1% invocation
      target measured on replayed history
- [ ] Deterministic fallback path tested: Ollama down, slow, malformed, lying
- [ ] Paired-corpus comparison: deterministic-only vs +model; model joins
      default config only if SPEC §11 bar is met
- [ ] Acceptance: injection strings in command text cannot flip policy; model
      recommendation never overrides direct-catastrophic rules

## M5 — Shadow pilot + report

- [ ] `oopsinput report`: rates, latencies, evidence-code ranking, hypothetical
      interventions from shadow data
- [ ] `oopsinput purge`; retention pruning
- [ ] Author pilot: ≥1,000 natural commands in shadow+suggest; review top
      candidates + random allow sample; findings → regression fixtures
- [ ] Tune budgets/thresholds from pilot data; graduate first warn category if
      evidence supports it
- [ ] Acceptance: pilot writeup in eval/; decision recorded per category

## M6 — Share-ready polish

- [ ] README: honest pitch, install, what it does/doesn't do, uninstall
- [ ] CI (audit 2026-08-05): fmt --check, clippy -D warnings, cargo test, and
      cargo-deny (supply-chain + license check; also vets unfamiliar
      transitive deps like serde_json's `zmij`) on every push
- [ ] SECURITY.md (audit 2026-08-05): security posture, the accepted
      same-user trust boundary, what the tool does/doesn't defend against,
      vulnerability-report contact
- [ ] `oopsinput doctor` covers: plugin installed, widgets wrapped, config
      valid, model reachable (optional), state perms
- [ ] Clean-machine test: fresh user → install → shadow → report → uninstall
      leaves no trace
- [ ] Cut v0.1.0 tag; send to first outside testers

## Later (v2+ candidates — see SPEC §17)

- [ ] Agent adapter over `check --json` with goal context + provenance
- [ ] Bash adapter
- [ ] Daemon (only if spawn cost measurably matters)
- [ ] Context packs: kubectl / cloud / SQL
- [ ] Packaging: AUR, deb
