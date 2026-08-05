# oopsinput — Specification

**Version:** 1.0-draft · **Status:** canonical · **License:** Apache-2.0 · **Updated:** 2026-08-05

This document is the source of truth for the project. It supersedes the earlier
"Binput Guard" canonical spec (kept privately as background reading; its long-horizon
vision, threat analysis, and evaluation philosophy inform this document, but where the
two disagree, **this document wins**). Changes to scope, invariants, interfaces, or UX
are made here first.

---

## 1. What oopsinput is

**oopsinput** (noun): a command you typed that is probably not what you meant —
a misspelling, a dangerous slip, or a valid command aimed at the wrong target,
scope, branch, or environment.

**oopsinput** (the tool): a local, open-source guard that sits between pressing
Enter and execution in your interactive shell. It checks the command in
milliseconds and, on the rare occasion something looks off, asks — specifically,
with evidence — before the command runs. The user always runs the show: the tool
never executes anything the user didn't explicitly consent to, and it never
silently rewrites a command.

```
you press Enter ──► zsh widget ──► oopsinput binary ──► allow (silent, ~99%+)
                                          │
                                          ├──► "did you mean git pull? [y/n]"   (typo)
                                          ├──► specific warning + edit/cancel   (danger/intent)
                                          └──► (rare) local model consulted first
```

It is **not** a sandbox, an antivirus, a policy engine, or a guarantee of
safety. An allowed command is "no intervention under current evidence," never
"safe."

## 2. Principles

1. **The user runs the show.** No auto-execution of corrections. No hard denial
   from probabilistic inference. Every intervention offers an obvious path to
   run the original command unchanged.
2. **Rare intervention is a feature.** The decisive metric is false or
   unhelpful visible interventions per 1,000 commands — not recall, not F1.
   Missing a real oopsinput is acceptable; crying wolf is fatal.
3. **The common path is boring and fast.** Deterministic, no model, no visible
   output, imperceptible latency.
4. **Specific beats generic.** A warning names the target, the scope, and why
   *this context* makes the command unusual. Never "Are you sure? [y/N]".
5. **Local only.** Nothing leaves the machine. No telemetry, ever, in any build.
6. **Never execute what you're analyzing.** Analysis must not evaluate
   substitutions, expand globs through a shell, source files, or run the
   candidate command. No exceptions.
7. **The model is evidence, not authority.** Model output is untrusted input to
   a deterministic policy. Model unavailable ⇒ deterministic-only, silently.
8. **Lean and mostly functional.** Plain data + free functions. Abstractions
   must earn their place with ≥2 real implementations. Sync only — no async
   runtime. Performance and accuracy are paramount; cleverness is not.
9. **Minimal dependencies.** Self-write anything small and safe to own
   (see §12). Every dependency needs a one-line defense.
10. **Honest claims.** No "safe" badge, no novelty overclaims, no security
    marketing for a fail-open tool.

## 3. Version 1 scope

### Included

- Linux, interactive **Zsh** only, commands intercepted at ZLE `accept-line`
  (and sibling accept widgets — see §6) before execution.
- A single Rust binary, `oopsinput`, spawned per command. **No daemon.**
  (Ollama is already a resident daemon; it does the model keep-alive for us.)
- Four detection layers (§5): typo, danger, context, inference.
- Shadow mode (observe, never interrupt) as the default for new installs.
- JSONL event log with structural features only — no raw commands by default.
- `oopsinput report` summarizing shadow data; `oopsinput doctor` for setup checks.
- Golden evaluation corpus with counterfactual paired cases (§11).
- Install/uninstall scripts that touch shell config only with explicit markers.

### Excluded from v1 (deferred, not rejected)

Bash/Fish/other shells · daemon + socket protocol · SQLite · agent/tool-call
adapters (the check seam is origin-tagged so these arrive later without a
rewrite) · non-interactive scripts · cloud models · fine-tuning · persistent
cross-session personalization · packaging beyond a repo install script ·
Windows/macOS.

## 4. Vocabulary

- **Proposal** — one submitted command buffer plus context (cwd, git state,
  recent history summaries, origin).
- **Origin** — who proposed it: `human` in v1; the field exists so `agent` and
  `script` adapters can be added later without schema surgery.
- **Evidence** — a typed fact produced by analysis, with a stable code
  (e.g. `fs.target_is_cwd`, `git.dirty=14`, `typo.nearest=git`), a severity,
  and a reliability class. Facts, never speculation.
- **Decision** — `allow` | `observe` (shadow record) | `suggest` (typo y/n) |
  `warn` (nonblocking notice) | `confirm` (pause for choice).
- **Outcome** — what the user did: ran unchanged, accepted suggestion, edited,
  cancelled, timed out. Central to evaluation; distinct from the decision.

## 5. The four layers

Layers run in order. Each is independently testable and independently
disableable. Earlier layers are cheaper; the pipeline exits at the first
definitive decision.

### L1 — Typo layer (daily value)

Fires only when the command word does **not** resolve to anything (not a
binary in PATH, not an alias, function, builtin, or reserved word — resolution
metadata is supplied by the zsh plugin, which can see aliases/functions; the
binary never asks a shell). Since the typed command would have failed anyway,
this intervention is free: the alternative was an error message.

- Find nearest candidates by edit distance (self-written, bounded) against
  PATH executables + plugin-supplied aliases/functions/builtins.
- Prompt: `oopsinput: 'gti' not found — did you mean 'git pull ...'? [y/n]`
- **`y` runs the corrected command** — strong consent is acceptable here
  precisely because the original was unexecutable. `n` runs the original
  unchanged (it fails naturally). Ctrl-C cancels. Default on timeout: `n`.
- Never fires when the command word resolves. Zero tolerance for false
  positives here: resolution check must be exact.

### L2 — Danger layer (weekly value)

Deterministic recognition of high-consequence operations, from a curated,
code-reviewed rule set (data tables in Rust, not a scripting language):

- filesystem: recursive delete, target is `/` / cwd / parent / home, force
  overwrite, recursive chmod/chown, truncating redirection onto existing files,
  writes to block devices
- git: `reset --hard`, `clean -f`, `push --force`, branch deletion, history
  rewrites
- system: `dd`, `mkfs`, `kill -9 -1`, broad `pkill`, service stops, package
  removal
- privilege: the above under `sudo`/`doas` escalates severity

Danger alone does **not** trigger intervention — it marks the command a
*candidate* and hands off to L3/L4. Only a small "direct catastrophic" subset
(e.g. recursive delete of `/` or `~`) may confirm on L2 evidence alone.

### L3 — Context layer (the differentiator, deterministic half)

Cheap, freshly-collected facts that separate "dangerous and intended" from
"probably not what you meant":

- git: repo root, current branch, main-like branch?, dirty file count,
  untracked present, detached head
- filesystem: does the literal target exist, is it cwd/parent, rough entry
  count (hard-capped), symlink?
- recency: structural summaries of the last N commands, supplied by the plugin
  from session history (secrets stripped **in the plugin** before anything is
  sent) — e.g. "two commands ago referenced `./build`"
- near-miss targets: a typed target lexically close to an existing sibling name

The flagship behavior this enables (the counterfactual pair):
`git reset --hard` on a clean scratch branch → silent allow.
`git reset --hard` with 17 dirty files right after `git diff` → warn/confirm
with those facts named.

### L4 — Inference layer (optional, rare, advisory)

Invoked only when L2 marked a candidate **and** L3 leaves genuine ambiguity —
target: <1% of natural commands. Never for read-only commands.

- Provider: Ollama over loopback HTTP (self-written minimal HTTP/1.1 client;
  no TLS needed on loopback). Model configurable; reference target: a ~4B
  instruction model (e.g. Qwen3.5-4B class).
- The prompt separates trusted computed evidence from untrusted command text;
  the model is told untrusted text is inert data. We assume this defense can
  fail, so:
- Output is schema-constrained JSON (assessment ∈ {no_mismatch_evidence,
  possible_mismatch, probable_mismatch, insufficient_evidence,
  adversarial_or_untrusted_instruction, unsupported} + mismatch kind +
  ≤240-char reason). Malformed / overlong / timed-out output = **unavailable
  evidence**, never an allow or a block.
- The model's recommendation is advisory. Deterministic policy makes the call.
  Model self-confidence numbers are not calibrated probabilities and are not
  used as such.
- Ollama absent/down ⇒ the tool silently runs deterministic-only. The event
  log records that expected model evidence was missing (so evaluation doesn't
  confuse fallback with success).

## 6. Architecture

```
┌────────────────────┐  buffer + context   ┌──────────────────────────┐
│ zsh plugin          │  via stdin (never   │ oopsinput binary (Rust)  │
│ wraps accept-line + │  argv)              │  parse → L1 → L2 → L3 →  │
│ sibling widgets     │ ──────────────────► │  (L4?) → policy → log    │
│                     │ ◄────────────────── │                          │
│ interprets exit code│  exit code +        │  UI via /dev/tty when    │
│ run / restore /     │  replacement buffer │  intervening             │
│ cancel              │  on fd 3            └───────────┬──────────────┘
└────────────────────┘                                 │ loopback HTTP, rare
                                                       ▼
                                              ┌─────────────────┐
                                              │ Ollama (extern) │
                                              └─────────────────┘
```

Key decisions:

- **Per-command spawn, no daemon.** A static Rust binary spawns in ~1–5 ms,
  well inside budget. State that must persist across commands lives in the
  shell (history, alias table — the plugin's job) or in files. A daemon is a
  v2 option if measurements ever demand it; the user-visible contract wouldn't
  change.
- **The binary owns intervention UI** by reading/writing `/dev/tty` directly
  (single keypress reads, no line editing). The plugin only interprets the
  result: accept original buffer, replace buffer (typo `y`), restore buffer
  for editing, or cancel. Exact original bytes are preserved in every path.
- **The buffer travels over stdin, never argv** (argv is world-visible in
  `/proc`). Replacement text returns on a pipe (fd 3), never stdout (reserved
  for structured decisions in `check --json` mode).
- **Wrap every accept widget**: `accept-line`, `accept-line-and-down-history`,
  `accept-and-hold`, `accept-and-infer-next-history`, in both Emacs and Vi
  keymaps, preserving any previously-installed wrapper (capture-and-delegate,
  never assume defaults).
- **Fail open, bounded.** Plugin-side timeout (default 150 ms for the
  deterministic path; the binary signals "model pending" before exceeding it,
  extending the wait to the model deadline, default 2 s). On any failure —
  binary missing, crash, timeout, malformed output — the original command runs
  unchanged, with at most one concise diagnostic per session.
- **`check --json` seam.** The same analysis is reachable as
  `oopsinput check --json < proposal.json` → decision JSON on stdout. This is
  the stable integration point future agent adapters use; proposals carry an
  `origin` field from day one.

## 7. Interaction design

Two consent strengths, matched to what the user typed:

| Situation | Prompt | Keys | Rationale |
|---|---|---|---|
| L1 typo (command couldn't run) | `'gti' not found — did you mean 'git'? [y/n]` | `y` = run corrected · `n` = run original · Ctrl-C = cancel | Original was unexecutable; `y` is explicit consent, not auto-fix |
| L2–L4 (command is real) | Specific warning: what it does, what it hits, why context is unusual | `e` = edit (exact buffer restored to ZLE) · `c` = cancel · `r` = run unchanged once | The command has teeth; suggestions are only ever placed in the **editable buffer**, never run |

Warning anatomy (always): 1) what the command appears to do, 2) the concrete
target/scope/environment, 3) why the current context is unusual, 4) the keys.
For irreversible predicted consequences, `e`/`c` are primary; `r` is a distinct
deliberate key, never the default.

Anti-spoofing: all untrusted text (command, paths, model reason) is escaped
before display — control chars, ANSI/OSC sequences, bidi controls neutralized;
fixed trusted prefix frames every oopsinput message. Warnings remain useful
without color.

Habituation control: an intervention budget (default: max 3 visible
interventions per session-hour, direct-catastrophic rules exempt) plus
per-rule cooldown after repeated run-unchanged outcomes. Budget exhaustion
degrades to shadow recording, never to nagging.

## 8. Modes and rollout

- **shadow** (default): everything is analyzed and recorded; nothing visible.
- **suggest**: L1 typo prompts only. (Safe to enable immediately — zero
  false-positive cost by construction.)
- **warn**: adds L2/L3/L4 nonblocking warnings.
- **confirm**: adds pausing confirmations for gated rule categories.

A rule category may graduate from shadow → warn → confirm only with evidence
from the log (see §11). New installs: `shadow` + `suggest`.

## 9. Security invariants (test-enforced)

1. Analysis never executes, expands, sources, or evaluates any part of the
   proposal. No `zsh -c`, no `eval`, no glob expansion via shell, no command
   substitution. Metadata via direct syscalls only; any external read-only
   helper (e.g. `git status --porcelain`) runs with fixed argv, no shell,
   hard timeout — and never a candidate command.
2. The original buffer bytes are preserved exactly through every path
   (allow/edit/cancel), verified by PTY tests.
3. No raw command text, path, or secret in the default event log. Structural
   features + keyed fingerprints (local random key, generated per install,
   user-only file). Redaction runs before any optional raw capture
   (opt-in, research only).
4. All state files and the config dir are user-only (0700/0600).
5. Displayed untrusted text is escaped (see §7). Fuzz target: no active escape
   sequence survives the escaper.
6. Model output is validated against the schema; anything else is discarded as
   unavailable evidence. The model cannot cause `deny`; nothing can, in v1 —
   the strongest decision is `confirm`.
7. The binary runs unprivileged, never setuid, never asks for credentials.
8. Fail-open never substitutes or truncates a command — it runs the original
   or nothing.

Threat boundary stated honestly: oopsinput does not resist malware running as
the user, a compromised kernel, or a determined bypass — it is an assistance
layer, not an enforcement boundary.

## 10. Performance budgets

Targets on an ordinary laptop (reference: this dev machine), measured, not
promised:

- Deterministic path end-to-end (Enter → verdict, including process spawn):
  p50 ≤ 10 ms, p95 ≤ 25 ms, p99 ≤ 50 ms.
- Candidate path without model: p95 ≤ 75 ms.
- Model path: report warm/cold separately; warm target p50 < 1 s on CPU;
  hard timeout default 2 s, then deterministic fallback.
- Model invocation rate after tuning: < 1% of natural commands.
- Visible interventions: < 1 per 1,000 natural commands (excluding L1, which
  is self-limiting).
- Binary size: single static-ish release binary, `strip = true`, thin LTO.

Pathological inputs (100 KB buffers, deeply nested quoting, huge directories)
must degrade to `observe` within budget, never hang the shell — enforced by
hard caps in every collector.

## 11. Accuracy and evaluation

- **Golden corpus** in `eval/golden/` as JSON fixtures: each case = command +
  context fixture + expected evidence codes + expected decision. **≥ 30% of
  cases are counterfactual pairs** — identical command, different context,
  different expected outcome. A change that breaks a pair fails CI.
- **Paired-case discipline** is what keeps oopsinput from decaying into a
  dangerous-command blocker: every danger rule must ship with at least one
  context in which the same command is silently allowed.
- **Primary metric:** false/unhelpful visible interventions per 1,000
  commands. **Secondary:** useful-intervention precision (did the user edit or
  cancel, or retrospectively judge it worthwhile), model invocation rate,
  latency percentiles, unsupported-syntax rate.
- **Shadow pilot** on the author's shell (≥ 1,000 natural commands) before any
  warn/confirm category is enabled by default; findings become regression
  fixtures. "Run unchanged" is *not* automatically a false positive —
  retrospective labels distinguish intended-and-warning-useless,
  intended-but-warning-useful, accidental-and-prevented.
- A model may join the default config only if it beats deterministic-only on
  the paired corpus in ≥ 2 categories without raising the intervention rate.

## 12. Dependency policy and ledger

Rule: self-write anything that is a small, safely-ownable sliver. Take a
dependency only for real spec depth or a security-hardened surface. Every
entry needs a one-line defense; adding one requires updating this table.

| Dependency | Status | Defense |
|---|---|---|
| `serde` + `serde_json` | **allowed** | JSON parsing of model output and proposals is a correctness/security surface with real spec depth; hand-rolled JSON parsers are a classic bug farm |
| HTTP client (reqwest etc.) | **rejected** | loopback HTTP/1.1 POST needs ~100 lines over `std::net::TcpStream`; no TLS on loopback; we own it |
| async runtime (tokio) | **rejected** | per-command sync binary; nothing is concurrent enough to justify it |
| CLI parser (clap) | **rejected** | a handful of subcommands; hand-rolled dispatch |
| tree-sitter(-bash) | **rejected for v1** | we need bounded lexical structure with honest uncertainty flags, not a full grammar; self-written conservative lexer (§13); revisit only with evidence of unsupported-syntax pain |
| SQLite crates | **deferred** | JSONL suffices for v1 volumes |
| edit distance / TOML / etc. | **rejected** | trivially self-written; config is plain `key = value` lines |

Dev-dependencies count too and follow the same rule.

## 13. Parsing stance

A self-written conservative lexer, not a shell emulator. It produces: words,
quoting state, operators (`&&`, `||`, `;`, `|`, `&`), redirections,
assignments, and **opaque nodes** for anything it can't analyze —
substitutions `$(...)`, backticks, process substitution, arithmetic, heredocs,
zsh-specific globs. Opaque nodes carry uncertainty evidence
(`syntax.opaque_substitution`); policy treats uncertainty conservatively
(observe/escalate for consequential commands, never invented semantics).
The lexer never expands anything and never panics on any input (fuzzed).

Alias/function visibility: the plugin ships the resolution kind of the command
word (alias→what, function, builtin, external path) with each proposal, since
only the live shell knows. The binary treats unresolvable meaning as
uncertainty, not as safety.

## 14. Storage and privacy

- `~/.config/oopsinput/config` — plain `key = value`.
- `~/.local/state/oopsinput/events.jsonl` — append-only structural events:
  timestamp, decision, evidence codes, layer, timings, outcome, keyed
  fingerprints. No raw commands, no paths, no goal text by default.
- `~/.local/state/oopsinput/key` — local random fingerprint key, 0600.
- `oopsinput report` — rates, latencies, top evidence codes, hypothetical
  interventions from shadow data.
- `oopsinput purge` — one command, deletes all state.
- Retention default 30 days (pruned on write).
- **No telemetry. No network beyond loopback Ollama. Ever.**

## 15. Config (initial surface)

```
mode = shadow            # shadow | suggest | warn | confirm
model = qwen3.5:4b       # empty = deterministic-only
model_timeout_ms = 2000
det_timeout_ms = 150
budget_per_hour = 3
log_raw = false          # opt-in research capture (redacted first)
```

Unknown keys: warn once, ignore. Invalid values: fall back to the default for
that key, say so once.

## 16. Repository layout

```
oopsinput/
├── SPEC.md              # this file — canonical
├── PLAN.md              # milestones and progress
├── CLAUDE.md            # working agreement for coding sessions
├── README.md
├── LICENSE              # Apache-2.0
├── Cargo.toml           # single binary crate — no workspace until earned
├── src/
│   ├── main.rs          # dispatch: check / report / doctor / purge / version
│   ├── proposal.rs      # input types + JSON seam          (M1)
│   ├── lexer.rs         # conservative shell lexer          (M2)
│   ├── layers/          # typo.rs danger.rs context.rs infer.rs  (M2–M4)
│   ├── policy.rs        # evidence → decision + budgets     (M3)
│   ├── ui.rs            # /dev/tty prompts + escaping       (M2)
│   ├── model.rs         # loopback HTTP + schema validation (M4)
│   └── events.rs        # JSONL log + report                (M1, M5)
├── zsh/
│   ├── oopsinput.zsh    # widget wrapper plugin
│   ├── install.zsh
│   └── uninstall.zsh
├── eval/golden/         # paired JSON cases
└── tests/               # integration + PTY tests
```

(Module files are created when their milestone starts — no empty placeholder
files.)

## 17. Beyond v1 (recorded so v1 doesn't paint us into corners)

Agent adapter via the `check --json` seam with explicit goal context and
provenance · bash/fish adapters · optional daemon if spawn cost ever matters ·
richer personalization (transparent thresholds only — no silent learning) ·
sandboxed effect-probing · additional context packs (kubectl/cloud/SQL) ·
packaging (AUR/deb/homebrew). None of these may weaken §9 invariants.

## 18. Success definition for v1

v1 is done when: install/uninstall are clean on this machine; 10,000 scripted
PTY submissions produce zero altered/lost buffers and zero hangs; the typo
layer works with exact-resolution zero false positives; the danger+context
layers pass the paired golden corpus; the inference layer produces valid
schema output against a local model and degrades silently without one; the
deterministic path meets its latency budget; a ≥1,000-command shadow pilot has
been reviewed; and the repo is presentable enough to send to a friend.
