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

## M1 — Zsh capture, shadow passthrough

The riskiest surface first: prove we can intercept and restore commands with
zero corruption before building any analysis.

- [ ] `zsh/oopsinput.zsh`: wrap all accept widgets (Emacs + Vi keymaps),
      capture-and-delegate to prior widget, recursion guard
- [ ] Buffer over stdin; plugin-side fail-open timeout (det_timeout_ms);
      one-per-session diagnostic on failure
- [ ] Plugin ships context: cwd, command-word resolution kind (alias/function/
      builtin/external), last N history lines secret-stripped in-shell
- [ ] proposal.rs + events.rs: parse proposal, log structural shadow event
- [ ] install.zsh / uninstall.zsh with explicit markers + backup
- [ ] PTY test harness: scripted submissions through a real zsh; assert exact
      buffer passthrough (multiline, unicode, paste, huge buffer)
- [ ] Acceptance: 10,000 PTY submissions, zero altered/lost buffers, zero
      hangs; kill -9 the binary mid-check → command still runs

## M2 — Lexer + typo layer (first visible value)

- [ ] lexer.rs: conservative lexer per SPEC §13, fuzz smoke test, never panics
- [ ] layers/typo.rs: exact resolution check, bounded edit distance vs PATH +
      plugin-supplied names
- [ ] ui.rs: /dev/tty single-key prompts; escaping pass (control/ANSI/OSC/bidi
      neutralized) + fuzz target
- [ ] `y` runs correction / `n` runs original / Ctrl-C cancels; replacement
      returns on fd 3, never argv/stdout
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
