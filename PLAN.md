# oopsinput — Plan

Milestones are sized to complete in one focused session each. A milestone is
done when its acceptance checks pass. SPEC.md is canonical; check items off
here as they land.

Completed milestones, all archived verbatim in
[PLAN-ARCHIVE.md](PLAN-ARCHIVE.md): M0 (skeleton and governance), M1 (zsh
capture + shadow passthrough), Post-M1 hardening — ✅ 2026-08-05 — M2 (lexer
+ typo layer, the first user-visible value) and M3 (danger + context layers,
policy, warning UI — the deterministic product complete), each with its
refactor/bughunt/audit passes — ✅ 2026-08-06. The archive also holds the
2026-08-05 name decision — settled, don't re-litigate.

Current measured cost, release build including process spawn (budgets 25 /
75 p95): common path p50 2.4 ms / p95 4.4 ms; typo path p50 16.3 ms / p95
19.5 ms; candidate path incl. the git helper p50 15.0 ms / p95 18.1 ms.

Standing rules carried out of archived milestones:

- **Any channel where binary output can become an executed command, or land
  in the user's buffer, gets event-log rigor**: exact bytes, no
  interpretation, an integrity check, pinned tests (audit 2026-08-05, built
  in M2 for the fd-3 correction channel; applies to every later milestone).
- **Every external helper: absolute path, fixed argv, hard timeout** — never
  resolved through $PATH (audit 2026-08-06; see M6's SECURITY.md item for
  why). **And its own configuration is untrusted input**: a helper that
  reads config from the working directory can be told by that config to
  execute something (audit 2026-08-06, proven: `git status` runs
  `core.fsmonitor` from the repo's `.git/config`, so analyzing `rm -rf
  ./build` inside an extracted archive ran a stranger's program). Neutralize
  every exec-capable key on the command line, where `-c` outranks repo
  config. Any future helper gets the same treatment before it ships.
- **Displayed text goes through the escaper unconditionally** (SPEC §9-4) —
  no exemption for text a distant charset check is believed to have made
  safe; that rule breaks silently when the distant check is edited (audit
  2026-08-06, recency words).
- **Never write through a symlink in our own state dir** — check
  `symlink_metadata` before any create/truncate/append (audit 2026-08-06;
  same rule install.zsh already followed).
- **Word boundaries follow the shell, not Unicode**: `lexer::is_shell_whitespace`
  (space/tab/newline only) governs every decision about where a word starts
  or ends (bughunt 2026-08-06).
- **Open product bug for /bughunt (found by test-audit 2026-08-06, proven,
  NOT yet fixed):** the intervention budget is lost under parallel shells.
  `warning_intervention` does load_state → apply_gates(commit) → save_state
  with no locking, so two shells that finish warnings in the same instant
  both read the same counts and the second save overwrites the first —
  simulated: two spends recorded, one survives. Effect: the user can be shown
  more than `budget_per_hour` warnings, and a cooldown can be lost. The event
  log already solved the same class with a single atomic append; this file
  needs the equivalent (write-temp-then-rename, or an advisory lock). No test
  covers concurrent policy-state access — that is why it hid.
- **Performance is gated, but only on the binary side** (test-audit
  2026-08-06): `scripts/perf-gate.zsh` enforces SPEC §10 budgets for the
  common and candidate paths, and `scripts/pty-gate.zsh` now enforces a
  coarse per-submission ceiling that covers the plugin. Neither measures
  fine-grained plugin work against history size — the exact shape of the
  ~7.5 ms recency regression. Wire both gates into M6's CI item.
- **Deferred finding (bughunt 2026-08-06):** danger's shape tables don't
  model per-tool *arity* — `mv -f a` (missing destination) still emits
  `fs.force_overwrite` evidence though mv itself errors. Deferral reason:
  arity modeling turns curated shape tables into command validators (a rule-
  layer redesign), and force_overwrite is observe-tier, so no wrong
  intervention is reachable. Revisit if that category ever graduates.
- **Documentation stays faithful to reality at all times** (Kyle,
  2026-08-06, resolving the structure-review question about SPEC §16's
  stale tree): purely descriptive drift — file trees, module lists, "what
  exists today" statements — is fixed on sight, no queue. Substantive SPEC
  changes (behavior, scope, security posture) still go through Kyle first.

M3 — Danger + context layers, policy, warning UI ✅ 2026-08-06 → archived
in [PLAN-ARCHIVE.md](PLAN-ARCHIVE.md).

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
      interventions from shadow data. The mechanism it reads (built in M3):
      a mode downgrade preserves the policy reason, so an event recorded as
      `observe` with reason `policy.*` is an intervention that *would* have
      fired — count those, don't recompute them
- [ ] `oopsinput purge`; retention pruning
- [ ] Author pilot: ≥1,000 natural commands in shadow+suggest; review top
      candidates + random allow sample; findings → regression fixtures.
      **Purge the event log before starting** (`rm ~/.local/state/oopsinput/
      events.jsonl`): the dev machine's log contains ~435 synthetic events
      written by benchmark loops and probes on 2026-08-06 that ran the real
      binary without `OOPSINPUT_STATE_DIR` set, so accumulated data is not
      all natural. For the same reason, always set `OOPSINPUT_STATE_DIR` to
      a temp dir when probing `check` by hand
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
      vulnerability-report contact. Three things it must state precisely:
      (1) "same user" does NOT imply every directory on $PATH is trusted —
      the typo layer fires on *unresolvable* commands, so any helper resolved
      by name turns any typo into execution of a predictable name from
      whatever directory leads $PATH (`.`, or a repo's ./bin added by
      direnv); this is the rationale behind the external-helper standing rule
      in the header (audit 2026-08-06, from the `stty` finding).
      (2) The residual git-helper risk (audit 2026-08-06): oopsinput runs
      `git status` in whatever directory the user is standing in, and a
      repository obtained from someone else (extracted archive, committed
      fixture repo) is untrusted input. We neutralize the exec-capable config
      keys, but that is a curated list a future git release could outgrow.
      The structural fix — reading the index ourselves, no git spawn — is a
      v2 candidate, not a v1 promise.
      (3) install.zsh leaves any existing path (dangling symlinks included)
      untouched rather than writing through it, and the binary refuses to
      write through a symlink in its own state dir.
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
