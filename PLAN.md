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
75 p95): common path p50 2.14 ms / p95 3.13 ms; typo path p50 16.3 ms /
p95 19.5 ms; candidate path incl. the git helper p50 7.44 ms / p95 9.09 ms.
Common/candidate numbers are the 60-run M5 retention gate on 2026-08-07;
the typo number is the latest dedicated measurement from 2026-08-06.

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
- **Never send data to a loopback peer without checking who owns it**
  (audit 2026-08-06): any local account can bind a free unprivileged port —
  on a shared machine, 127.0.0.1:11434 with Ollama down is any other user's
  for the taking, and the model request carries raw command text. After
  connecting and before sending, `model::verify_peer` reads the peer's uid
  from /proc/net/tcp (the established connection's own entry, so there is
  no swap window) and requires our uid or a system account (< 1000); any
  doubt refuses and falls back to deterministic-only. Pinned by
  column-exact parser tests (uid sits right after retrnsmt, which also
  parses as a number — an off-by-one trusts everyone) plus the live
  same-uid socket suite. Any future loopback client gets the same check.
- **Never write through a symlink in our own state dir** — check
  `symlink_metadata` before any create/truncate/append (audit 2026-08-06;
  same rule install.zsh already followed).
- **Word boundaries follow the shell, not Unicode**: `lexer::is_shell_whitespace`
  (space/tab/newline only) governs every decision about where a word starts
  or ends (bughunt 2026-08-06).
- **Habituation writes append, and uncoordinated read-modify-write must never
  return** (fixed 2026-08-06 from a test-audit finding): the budget and
  cooldown once lived
  in a JSON blob that was loaded, modified and written back, so two shells
  finishing warnings in the same instant each recorded a spend and the second
  write dropped the first — the hourly cap under-counted and a cooldown could
  vanish. `policy.jsonl` now takes one locked append per shown intervention
  and the gates are a pure read over its tail. M5 retention is the sole
  rewrite: every writer coordinates on a stable cross-process lock and the
  compactor atomically replaces the log, so it cannot race away another
  shell's append. Pinned by 8-thread policy and 16-thread event tests. Any
  future per-command state gets the same treatment.
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

## M4 — Inference layer ✅ 2026-08-06 (complete — paused here by Kyle's request before starting M5)

- [x] model.rs: minimal loopback HTTP/1.1 client (std only), hard timeout,
      response size cap — ✅ 2026-08-06. Refuses non-loopback addresses;
      target fixed at 127.0.0.1:11434 (no address knob — SPEC §14). Deadline
      recomputed per socket call (a trickling server can't evade it — probed:
      naive per-read timeout rode out the whole drip run); caps on head,
      decoded body, and encoded chunk stream. First caller: `doctor`'s model
      line (POST /api/show — daemon up? model pulled? no inference), which
      pre-completes the "model reachable" part of M6's doctor item. Verified
      live against this machine's Ollama: present / not-pulled / disabled
      all correct.
- [x] Prompt assembly: trusted evidence / untrusted command separation; schema-
      constrained output via Ollama structured outputs; validation → invalid =
      unavailable evidence — ✅ 2026-08-06, layers/infer.rs. The user message
      is one JSON document: computed facts under "evidence" (closed vocab +
      numbers only, pinned by a test that walks the subtree), every human
      string under an `untrusted_` key; serde serialization keeps hostile
      buffers inert. Validator rejects unknown vocab, unknown fields, >240-
      char reasons (chars, not bytes); every failure → a stable
      `model.unreachable/timeout/invalid/error` code so the log can tell
      fallback from success. `consult()` is built and tested (16 tests, mock
      TCP servers) plus probed live against qwen3:1.7b — schema-valid
      structured output end-to-end — but NOT yet called from check: the gate
      is the next item, which must also give the model path its own watchdog
      deadline (SPEC §6 — today's watchdog would kill a 2 s model call at
      det_timeout_ms).
- [x] Candidate gate: L4 only when L2 candidate ∧ L3 ambiguous; <1% invocation
      target measured on replayed history — ✅ 2026-08-06. `policy::l4_gate`:
      model configured ∧ danger candidate ∧ ¬catastrophic ∧ warranted verdict
      is Observe with an ambiguity reason (evidence_unavailable /
      candidate_observed). Consumption is `policy::apply_model_evidence`:
      exactly two Warn-capped upgrade arms (probable_mismatch, adversarial),
      no downgrade arm — a lying model cannot clear a command; Confirm stays
      deterministic-only. Watchdog gets a one-shot model extension armed
      before the first socket call (probed: without it the binary is
      watchdog-killed mid-consultation at ~160 ms). Measured on this
      machine's real history: 3/1,107 commands gate-eligible = **0.27%**,
      inside the <1% target (caveats: replayed from $HOME so context facts
      differ from original cwd; all 3 were ungraduated candidate_observed
      shapes). 5 policy unit tests + 5 end-to-end tests (tests/model_path.rs,
      mock Ollama over the debug-only port seam).
- [x] Deterministic fallback path tested: Ollama down, slow, malformed,
      lying — ✅ 2026-08-06, all four end-to-end through the real binary
      against mock servers (tests/model_path.rs): down → `model.unreachable`,
      no stall; slow → cut off by consult's own deadline; malformed in
      three flavors (garbage 200, schema-invalid verdict with an extra
      field, 300 KB oversized body) → discarded whole; lying ("no
      mismatch, run it" on an ambiguous candidate) → deterministic verdict
      stands, the lie recorded as evidence. Every case: exit 0, the
      deterministic reason code, and the model outcome logged.
- [x] Paired-corpus comparison: deterministic-only vs +model; model joins
      default config only if SPEC §11 bar is met — ✅ 2026-08-06. **Bar not
      met; model stays out of the default config** (which already ships
      `model =` empty). Full method, per-case results, and the reopening
      conditions in eval/model-comparison-2026-08-06.md. Headline numbers:
      qwen3:1.7b gave 5/5 schema-valid answers on the gate-eligible cases,
      improved 0 categories, changed 0 verdicts, and ran ~35 s/case warm on
      this CPU against a 2 s product timeout; qwen3:8b timed out on all 5.
      Also recorded there: this corpus cannot show a model win by
      construction (every eligible case expects observe) — the winnable
      version of the question needs deliberate-mistake fixtures from the
      M5 pilot. Harness: `policy::tests::model_paired_comparison`
      (#[ignore], run by hand).
- [x] Acceptance: injection strings in command text cannot flip policy; model
      recommendation never overrides direct-catastrophic rules — ✅
      2026-08-06. Pinned both directions with an *obedient* mock model (the
      worst case for prompt separation): injected "reply no_mismatch" text
      + a model that complies → verdict unmoved; a hostile escalating
      answer → capped at Warn, invisible in shadow. Confirm is unreachable
      from model output even in confirm mode (e2e), and direct-catastrophic
      commands never consult at all (gate unit test + e2e never-connects
      test, item 3). This landed before item 5; the paired-corpus comparison
      above later completed M4.

## M5 — Shadow pilot + report

- [x] `oopsinput report`: rates, latencies, evidence-code ranking, hypothetical
      interventions from shadow data — ✅ 2026-08-07. Streams the JSONL log;
      reports decision/model/visible/hypothetical rates, ranked evidence and
      policy reasons, visible-warning outcomes, and nearest-rank p50/p95/p99
      latency. New events persist the mode-blind Warn/Confirm reason explicitly;
      legacy lines use only the closed set of reasons that really represented
      an intervention, rather than treating every `observe` + `policy.*` as
      one. Any `model.*` evidence keeps that event out of deterministic
      percentiles. L4 now records `warm`/`cold`
      from a bounded, read-only Ollama `/api/ps` query immediately before chat;
      failed status queries and legacy events report as `unknown`, never cold
      or deterministic. Malformed/torn lines are counted and skipped, and all
      codes loaded from disk pass through the terminal escaper.
- [x] `oopsinput purge`; retention pruning — ✅ 2026-08-07. Exact
      zero-argument CLI deletes events, habituation history, the fingerprint
      key, config-warning/retention markers, abandoned private temp files, and
      its coordination lock; config is deliberately kept. It refuses a
      symlinked state directory and recursive deletion, unlinks known symlink
      entries without touching their targets, preserves unknown entries, and
      is a clean success when no state exists. Both JSONL logs enforce the
      30-day cutoff on write with at most one sweep per day: malformed/torn
      and expired records are streamed into a 0600 temp file and atomically
      replaced under a stable `File::lock`, while every append takes that same
      lock. Analysis-time writes remain complete under normal concurrency and
      are bounded by the existing watchdog; post-prompt writes abandon a busy
      lock after 25 ms and append without running retention, so neither lock
      contention nor a large sweep can hold or override the user's chosen
      action. Purge, whose requested effect is deletion itself, waits.
      Waiters verify the lock inode, so purge can remove the anchor without
      splitting concurrent writers. No dependency added; Rust 1.89 is now the
      declared minimum because its standard library stabilized file locking.
      Real-CLI probes and tests pin symlink/unknown/config ownership, exact
      argv, empty state, refusal to recurse, bounded post-prompt contention,
      and non-regular-path refusal; existing concurrent append hammers pass
      through the compaction path. A rejected first attempt bounded *every*
      writer at 25 ms and lost 1/800 events in 1 of 7 full-suite runs under
      scheduler load (despite 0/20 isolated failures); the scoped design above
      passed 7/7 full-suite runs with all records present.
- [x] M5 correctness sweep — ✅ 2026-08-08. Six bughunt findings fixed and
      regression-pinned: intrinsic Observe reasons no longer inflate
      hypothetical rates; unavailable prompts neither count as visible nor
      spend budget; post-prompt appends never start retention; identical config
      warnings are coordinated across shells; purge recovers a symlinked or
      non-private regular lock anchor without following it; and an empty
      `HOME` cannot create a relative state path. A torn tail is separated
      before append so deferring retention cannot consume the new record. The
      verified release build was refreshed into the existing pilot install;
      binary/plugin checksums match their repository artifacts, while config
      and accumulated state were preserved.
- [x] M5 security-audit fixes — ✅ 2026-08-08. A reproduced relative-state-path
      ownership bug is closed: a nonempty relative `OOPSINPUT_STATE_DIR`
      disables state, relative `XDG_STATE_HOME` falls back to absolute `HOME`,
      and relative `HOME` never resolves from the working directory. Config
      warnings now reach `/dev/tty` through the real plugin exactly once, and a
      failed display does not write the “shown” marker. JSONL readers drain any
      record over 64 KiB without unbounded allocation and resume at the next
      line. Existing-file readers verify the opened inode against the inspected
      nonsymlink path; missing lock/log creation uses atomic `create_new`, so a
      raced dangling symlink cannot create its target. The safe standard
      library cannot eliminate an active same-user FIFO swap before open; that
      residual stays inside SPEC §9's accepted same-user boundary rather than
      adding `unsafe` or a platform dependency. A fresh RustSec database scan
      (commit `1237bbe0`, 2026-08-06) found no advisory entry for the locked
      dependency graph; all resolved licenses are Apache-2.0, MIT, Unlicense,
      or Unicode-3.0 combinations. Continuous enforcement also landed rather
      than being deferred to M6: cargo-deny 0.20.2 passes advisories, exact
      crate allowlist, license, duplicate/wildcard, and source checks, and its
      checksum-pinned workflow runs on every push and pull request plus weekly
      while the repository is idle. 267 tests pass by default; the one
      live-model evaluation harness remains intentionally ignored.
- [x] M5 test audit — ✅ 2026-08-08. Test claims were checked against the
      failures they are meant to catch, with three controlled mutations proving
      that the oversized-record cap, pre-open non-regular-file refusal, and
      replacement-lock-inode check each fail their regression test when
      removed. Coverage now pins caller-level retention limits and daily sweep
      coordination, newest-history preservation, warning-set re-arming,
      purge's retention-marker/temp-file scope, FIFO refusal through a real
      process, bounded analysis-time lock contention, and the purge/waiter lock
      race. A real-CLI probe also found and fixed `doctor` calling a symlinked
      config “present” even though the loader correctly ignored it; `doctor`
      now reports that non-regular config as ignored and uses defaults. 267
      tests pass by default; the one live-model evaluation harness remains
      intentionally ignored.

### Passive M5 observation — explicitly non-blocking

Natural-command collection continues as an optional local data point, not a
roadmap item, release gate, or input to `$next`. M6 work proceeds independently;
the data may never be numerous or relevant enough to use. If warning categories
are ever considered for graduation, first collect at least 1,000 natural
Shadow/Suggest commands, review the top candidates plus a random Allow sample,
turn real findings into fixtures, tune thresholds only when evidence supports
it, and write the per-category decision in `eval/`.

The clean baseline began on 2026-08-08 after `oopsinput purge`. The verified
release binary and stable plugin copy were installed, both resolved
`~/.local/bin/oopsinput`, and the immediate post-purge report showed 0 events.
The removed pre-purge log held 637 development-contaminated events (all
`allow`, including 228 typo candidates). The 0600 `mode = suggest` config was
kept. Manual `check` probes must continue to set `OOPSINPUT_STATE_DIR` to a
temporary directory so they never contaminate this passive sample.

## M6 — Share-ready polish

- [x] Shareable install: copy both the binary and plugin to stable user-owned
      locations — ✅ 2026-08-08. The release binary installs at
      `~/.local/bin/oopsinput`, the plugin at
      `~/.local/share/oopsinput/oopsinput.zsh`, and the marked `.zshrc` block
      sources the installed copy, so moving/deleting the checkout no longer
      breaks new shells. Reruns stage and atomically replace complete runtime
      files, migrate the old repository-pointing block in place, and preserve
      its surrounding bytes and mode. A healthy marker block is the ownership
      receipt for update/uninstall: without one, same-named existing files are
      never claimed or removed. Symlink/non-regular destinations and ambiguous
      marker layouts are refused before edits; uninstall keeps config, state,
      and unknown plugin-directory entries. Nineteen lifecycle/security
      integration cases pin install, update, migration, and removal.
- [x] README: honest pitch, install, what it does/does not do, uninstall, and a
      self-serve shadow/suggest testing protocol — ✅ 2026-08-08. The
      share-ready guide now leads with the fail-open boundary; distinguishes
      implemented, default, opt-in, and excluded behavior; discloses local
      storage and exactly when raw command text reaches opt-in Ollama; names
      every install/uninstall effect; and gives natural-use Shadow and Suggest
      protocols without encouraging destructive probes. All commands and
      lifecycle claims were checked against the shipped CLI and integration
      tests. The same documentation audit corrected SPEC's stale update date
      and `doctor`'s obsolete claim that the M3 warning UI was still pending;
      the latter has a regression test.
- [x] CI (audit 2026-08-05): fmt --check, clippy -D warnings, cargo test, and
      both performance gates on every push — ✅ 2026-08-08. The new
      build/test workflow also runs on pull requests and manual dispatch with
      read-only repository permissions. Independent jobs keep quality and
      acceptance results visible: one runs fmt, Clippy, and all default tests;
      the other builds release and runs the clean-machine lifecycle gate, the
      binary latency gate, and the full 10,000-submission PTY gate. Both use
      the exact minimum Rust 1.89.0,
      install only Zsh and Ubuntu's `bsdutils` PTY package, and reuse the
      commit-pinned checkout action already audited for the dependency
      workflow. `actionlint` 1.7.12 passed after its published SHA-256 was
      verified. Local minimum-toolchain acceptance: common p95 3.41 ms / 25
      ms, candidate p95 8.80 ms / 75 ms; PTY 10,000/10,000 at 5.43 ms per
      submission / 40 ms, with zero lost or altered commands. The
      dependency-policy slice remains separate: cargo-deny runs on pushes,
      pull requests, and weekly, with every resolved crate explicitly
      reviewed and allowlisted. The first hosted run exposed Ubuntu's global
      `compinit` prompt consuming scripted PTY input before the isolated
      `.zshrc`; every automated interactive shell now starts with `zsh -d -i`,
      which keeps the fixture startup file and excludes host global startup
      files. A regression checks that exact option state, and the replacement
      public run passed both jobs and every acceptance gate.
- [x] SECURITY.md (audit 2026-08-05): security posture, the accepted same-user
      trust boundary, what the tool does/doesn't defend against, and a real
      private-report contact — ✅ 2026-08-08. The policy leads with fail-open;
      treats PATH and third-party repositories as untrusted despite the
      same-user boundary; names the curated Git-config residual and index-reader
      v2 fix; explains fresh-install refusal, the `.zshrc` ownership receipt,
      symlink-safe state, and preserved config; and discloses raw command text,
      peer-UID checks, and every accepted Ollama residual. Claim verification
      proved that `doctor`, the unknown-command error, plugin load failure, and
      lifecycle scripts could display environment-derived control or bidi text
      unsafely. Their authored diagnostics now use terminal escapers, including
      under `LC_ALL=C`, with CLI, lifecycle, and real-PTY regression fixtures.
- [x] `oopsinput doctor`: plugin installed, widgets wrapped, config valid,
      optional model reachable, and state permissions — ✅ 2026-08-08. The
      read-only command now verifies the unique marked `.zshrc` block and
      regular installed plugin, all four live ZLE wrappers, parser and
      environment-mode validity, the configured model's loopback health, and
      exact `0700`/`0600` modes for recognized state. Missing state is valid;
      damage is reported without creation or repair. It exits zero only with
      `result: ready`, otherwise one with `result: problems found`. The plugin
      exposes only a closed list of its four static wrapper names and refreshes
      it immediately before an interactive doctor invocation. Six new CLI
      failure/health cases plus a real-PTY installed-shell case pin the result;
      the PTY case clears the load-time snapshot first and invokes the README's
      quoted full path, proving the status is refreshed live.
- [x] Clean-machine lifecycle test: fresh isolated user → install → shadow →
      report → purge → uninstall — ✅ 2026-08-08. A release-level gate now
      creates one `mktemp` home, installs the actual release binary and plugin,
      verifies their bytes and modes, loads the installed hook under a real PTY,
      requires live `doctor` readiness in Shadow mode, records and reports
      exactly three commands, purges all state, and uninstalls. It then proves
      the original `.zshrc` is byte-exact, runtime paths and state are gone,
      and the deliberately retained config plus `.zshrc` backup are the only
      oopsinput artifacts. The first real probe found that fresh install added
      an unmarked blank separator which uninstall left behind; the installer no
      longer creates that byte, and the gate pins the regression. Release CI
      runs this gate on the minimum Rust toolchain before the timing gates.
- [x] Cut v0.1.0 tag; publish a self-recruiting public alpha and invite
      voluntary shadow/suggest testers — ✅ 2026-08-08. Annotated tag
      `v0.1.0` points to release commit `aa4385a`; its main-branch and tag
      dependency, quality, lifecycle, latency, and 10,000-command PTY runs all
      passed before the GitHub prerelease was published. The source-only
      release leads with the fail-open boundary, Linux/interactive-Zsh scope,
      install/doctor flow, and known limits, then invites voluntary natural-use
      Shadow or Suggest trials with locally reviewed/redacted reports. No
      personally recruited tester became a gate, no destructive probe list was
      requested, and ungraduated danger categories remain silent in both trial
      modes. Release:
      https://github.com/kserrec/oopsinput/releases/tag/v0.1.0

## Later (v2+ candidates — see SPEC §17)

- [ ] Agent request schema + one named-agent adapter. The current check path
      emits decision JSON but accepts the Zsh-oriented stdin/flag format and
      carries no origin, goal, or provenance. Add a promptless non-interactive
      contract and explicit human-approval routing; enabling the Zsh plugin
      alone does not intercept agent subprocesses
- [ ] Bash adapter
- [ ] Daemon (only if spawn cost measurably matters)
- [ ] Context packs: kubectl / cloud / SQL
- [ ] Packaging: AUR, deb
