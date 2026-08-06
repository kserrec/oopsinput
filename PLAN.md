# oopsinput — Plan

Milestones are sized to complete in one focused session each. A milestone is
done when its acceptance checks pass. SPEC.md is canonical; check items off
here as they land.

Completed milestones, all archived verbatim in
[PLAN-ARCHIVE.md](PLAN-ARCHIVE.md): M0 (skeleton and governance), M1 (zsh
capture + shadow passthrough), Post-M1 hardening — ✅ 2026-08-05 — and M2
(lexer + typo layer, the first user-visible value, with its refactor/bughunt/
audit passes) — ✅ 2026-08-06. Current measured cost, release build including
spawn: p50 3.6 ms / p95 4.4 ms on the common path, p50 16.3 ms / p95 19.5 ms
on the typo path (budgets 25 / 75). The archive also holds the 2026-08-05
name decision — settled, don't re-litigate.

Standing rules carried out of archived milestones:

- **Any channel where binary output can become an executed command, or land
  in the user's buffer, gets event-log rigor**: exact bytes, no
  interpretation, an integrity check, pinned tests (audit 2026-08-05, built
  in M2 for the fd-3 correction channel; applies to every later milestone).
- **Every external helper: absolute path, fixed argv, hard timeout** — never
  resolved through $PATH (audit 2026-08-06; see M6's SECURITY.md item for
  why).
- **Word boundaries follow the shell, not Unicode**: `lexer::is_shell_whitespace`
  (space/tab/newline only) governs every decision about where a word starts
  or ends (bughunt 2026-08-06).

## M3 — Danger + context layers, policy, warning UI

- [ ] layers/danger.rs: rule tables per SPEC §5-L2 (fs, git, system, privilege)
- [ ] layers/context.rs: git facts, target facts, recency relation, near-miss
      targets — all hard-capped syscall collectors
- [ ] policy.rs: evidence → decision matrix; direct-catastrophic subset;
      intervention budget + per-rule cooldown; shadow conversion. Note a
      minimal config reader already exists from M2 ($OOPSINPUT_MODE > config
      `mode` key > shadow, unknown values → shadow); this expands it to the
      full SPEC §15 surface incl. warn-once on unknown keys
- [ ] Warning UI: anatomy per SPEC §7; e/edit c/cancel r/run-once; exact
      buffer restore on edit (PTY-tested). Deferred bughunt finding
      (2026-08-06, deferred because this item rebuilds the prompt key
      handling): the single-byte prompt read treats a multi-byte key
      sequence (arrow keys: ESC [ A) as ESC + leftover bytes, which leak
      into the next ZLE buffer as stray characters — the new key reader
      must consume complete escape sequences
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
      vulnerability-report contact. Must state precisely (audit 2026-08-06,
      learned from the `stty` finding): "same user" does NOT imply every
      directory on $PATH is trusted — the typo layer fires on *unresolvable*
      commands, so any helper resolved by name turns any typo into execution
      of a predictable name from whatever directory leads $PATH (`.`, or a
      repo's ./bin added by direnv); document the external-helper rule that
      follows from it (see header). Also record: install.zsh leaves any
      existing path (dangling symlinks included) untouched rather than
      writing through it.
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
