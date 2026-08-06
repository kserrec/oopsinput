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
- **Documentation stays faithful to reality at all times** (Kyle,
  2026-08-06, resolving the structure-review question about SPEC §16's
  stale tree): purely descriptive drift — file trees, module lists, "what
  exists today" statements — is fixed on sight, no queue. Substantive SPEC
  changes (behavior, scope, security posture) still go through Kyle first.

## M3 — Danger + context layers, policy, warning UI

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
      incl. git spawn
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
- [ ] recency relation (rest of SPEC §5-L3): plugin-supplied structural
      summaries of recent commands, secrets stripped in the plugin — zsh
      plugin change, so PTY tests required. (Reordered after policy + UI,
      2026-08-06: it had no consumer until policy existed, the M3 acceptance
      doesn't depend on it, and its plugin protocol should be shaped by what
      policy actually consumes)
- [ ] eval/golden: paired counterfactual cases for every danger rule (≥30%
      pairs); CI runs the corpus. Partially covered 2026-08-06: danger.json
      (34 command-shape cases) + policy.json (19 context-flip cases incl.
      the flagship pair) both enforce the ≥30% pair ratio in cargo test;
      remaining: pairs for every not-yet-paired danger rule, CI itself is M6
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
