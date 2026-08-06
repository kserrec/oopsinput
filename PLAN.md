# oopsinput — Plan

Milestones are sized to complete in one focused session each. A milestone is
done when its acceptance checks pass. SPEC.md is canonical; check items off
here as they land.

Completed milestones: M0 (skeleton and governance), M1 (zsh capture + shadow
passthrough; overhead p50 5 ms / p95 6 ms), and Post-M1 hardening — all
✅ 2026-08-05 → archived verbatim in [PLAN-ARCHIVE.md](PLAN-ARCHIVE.md).
That archive also holds the name decision (2026-08-05: "oopsinput" verified
clear on GitHub/crates.io/npm/PyPI/domains; `noops`, `oopsh`, `oopsys`,
`nooops` rejected — settled, don't re-litigate).

## M2 — Lexer + typo layer (first visible value)

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

## M3 — Danger + context layers, policy, warning UI

- [ ] layers/danger.rs: rule tables per SPEC §5-L2 (fs, git, system, privilege)
- [ ] layers/context.rs: git facts, target facts, recency relation, near-miss
      targets — all hard-capped syscall collectors
- [ ] policy.rs: evidence → decision matrix; direct-catastrophic subset;
      intervention budget + per-rule cooldown; shadow conversion
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
