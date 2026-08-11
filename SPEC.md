# oopsinput — Specification

**Version:** 1.0-draft · **Status:** canonical · **License:** Apache-2.0 · **Updated:** 2026-08-11

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
                                          ├──► full typed/corrected comparison  (typo)
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
- A fresh install requires the user to choose Shadow, Suggest, Warn, or
  Confirm explicitly; there is no installer-selected intervention default.
- JSONL event log with structural features only — no raw commands by default.
- `oopsinput report` summarizing shadow data; `oopsinput doctor` for setup checks.
- Golden evaluation corpus with counterfactual paired cases (§11).
- A versioned, prebuilt x86_64 Linux release bundle plus source-build developer
  path; both use the same guided install/uninstall scripts, which touch shell
  config only with explicit markers.

### Excluded from v1 (deferred, not rejected)

Bash/Fish/other shells · daemon + socket protocol · SQLite · agent/tool-call
adapters (the current check path emits structured decision JSON, but an agent
request schema and origin/goal/provenance fields remain future work) ·
non-interactive command interception · cloud models · fine-tuning · persistent
cross-session personalization · prebuilt targets beyond x86_64 Linux ·
distribution packages such as AUR/deb/Homebrew · Windows/macOS.

## 4. Vocabulary

- **Proposal** — one submitted command buffer plus context (cwd, git state,
  recent history summaries, origin).
- **Origin** — who proposed it. Version 1 only has an interactive Zsh adapter,
  so origin is implicitly `human` and is not yet serialized. A future
  agent/script request schema must add this field explicitly.
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
- Prompt, beginning after a clean blank line and showing the complete original
  and corrected buffers in bounded, escaped form:

  ```text
  *** oops? ***
  You typed 'gti pull'.
  Did you mean 'git pull'?
  [y] run correction  [n] run original
  ```
- The two choices stay on that one final row. `run original` starts visibly
  focused; Tab switches focus and Enter activates the focused choice. `y` and
  `n` remain immediate shortcuts. Ctrl-C still cancels but is not advertised
  in the prompt.
- **`y` runs the corrected command** — strong consent is acceptable here
  precisely because the original was unexecutable. `n` runs the original
  unchanged (it fails naturally). Ctrl-C cancels. Default on the ten-second
  timeout: `n`; before the original runs, the prompt explicitly says it timed
  out and is running the original unchanged. A timeout can never look like
  consent to the correction.
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
- **Fail open, bounded.** The deadline (150 ms deterministic path; the model
  path gets its own longer deadline when L4 lands) is enforced by a watchdog
  inside the binary that force-exits with the fail-open code — the plugin
  itself carries no timer. Two facts make this sufficient: the binary is ours
  and arms the watchdog before doing anything else, and zsh's job control
  independently returns control to the widget if the check process is stopped
  or killed (verified by test). On any failure — binary missing, crash,
  deadline, malformed output — the original command runs unchanged, with at
  most one concise diagnostic per session. Residual boundary: a check process
  unable to exit at all (e.g. a thread wedged in uninterruptible disk I/O —
  state dir on a hung network filesystem) could still block the prompt; this
  is documented, not defended.
- **Structured `check` seam.** Today `oopsinput check --res <kind>` reads the
  Zsh adapter wire format (buffer plus NUL-separated metadata on stdin) and
  writes decision JSON on stdout. That structured output is the foundation for
  future adapters, but there is not yet a JSON request schema or serialized
  origin/goal/provenance. Merely enabling the Zsh plugin does not intercept
  commands launched by non-interactive agent subprocesses.

## 7. Interaction design

Two consent strengths, matched to what the user typed:

| Situation | Prompt | Keys | Rationale |
|---|---|---|---|
| L1 typo (command couldn't run) | `*** oops? ***` block showing the bounded, escaped full original and correction | `y` = run corrected · `n` = run original · Tab = switch focus · Enter = activate focused choice · Ctrl-C = cancel (not displayed) | Original was unexecutable; the original starts focused, so a correction still requires explicit consent |
| L2–L4 (command is real) | Specific warning: what it does, what it hits, why context is unusual | `e` = edit (exact buffer restored to ZLE) · `c` = cancel · `r` = run unchanged once | The command has teeth; suggestions are only ever placed in the **editable buffer**, never run |

Every intervention begins on a clean terminal line. This is part of the
consent surface: the question must be visually separate from the command the
user submitted.

Warning anatomy (always): 1) what the command appears to do, 2) the concrete
target/scope/environment, 3) why the current context is unusual, 4) the keys.
For irreversible predicted consequences, `e`/`c` are primary; `r` is a distinct
deliberate key, never the default.

Anti-spoofing: all untrusted text (command, paths, model reason) is escaped
before display — control chars, ANSI/OSC sequences, bidi controls neutralized;
a fixed trusted `*** oops? ***` banner frames the typo block and the fixed
`oopsinput:` prefix frames every warning line. Warnings remain useful without
color.

Habituation control: an intervention budget (default: max 3 visible
interventions per session-hour, direct-catastrophic rules exempt) plus
per-rule cooldown after repeated run-unchanged outcomes. Budget exhaustion
degrades to shadow recording, never to nagging.

## 8. Modes, installation, and rollout

- **shadow**: everything is analyzed and recorded; nothing is visible.
- **suggest**: adds L1 typo prompts. Danger decisions remain invisible and are
  recorded only as hypothetical interventions.
- **warn**: adds advisory L2/L3/L4 danger prompts. The user can edit, cancel,
  or run unchanged; if the prompt times out, the original runs unchanged.
- **confirm**: keeps typo prompts and advisory warnings, while rule categories
  assessed as Confirm pause for a deliberate choice. If a Confirm prompt times
  out, the command is cancelled; an internal failure still follows §9's
  fail-open contract.

A rule category may graduate from shadow → warn → confirm only with evidence
from the log (see §11). There is no fresh-install default. Shadow remains the
configuration parser's conservative fallback for a missing or invalid `mode`,
but a fallback is failure behavior, never consent for an installer to choose a
starting mode.

### 8.1 Required fresh-install choice

The installer performs every read-only prerequisite, source-artifact,
destination-type, ownership-marker, and backup-path check before asking about
mode or writing anything. When no config exists, an interactive installation
then displays the complete consequence of each choice:

```text
Choose how oopsinput may interrupt you (required):

1  Shadow   Never interrupts; analyzes and records locally.
2  Suggest  Also asks about likely misspelled command names.
3  Warn     Also shows danger prompts; no answer eventually runs the original.
4  Confirm  Highest-risk prompts require a choice; no answer cancels.

Press 1–4, or Tab to focus an option and Enter to choose.
```

Nothing starts focused. `1`–`4` select directly; Tab establishes and then
cycles visible focus; Enter accepts only an already-focused choice. Bare Enter
or an unrecognized key cannot select a mode. Ctrl-C or terminal EOF cancels
without being advertised as a fifth choice, exits nonzero, and changes no
file.

For deliberate automation, the only promptless fresh-install interface is
`zsh install.zsh --mode <shadow|suggest|warn|confirm>`. Inherited environment
variables do not count as a choice. A missing value, unknown flag, invalid
mode, or unavailable controlling terminal without `--mode` fails before any
write. Tests use this same public argument rather than a private mode override.

Any existing config path is user-owned. An install or update preserves it
byte-for-byte, does not show the chooser, and reports that it was retained.
Supplying `--mode` when a config already exists is rejected rather than
silently overwriting or pretending to reconfigure it; changing an existing
mode remains a deliberate config-file edit. A symlinked, non-regular,
oversized, or invalid retained config is never repaired by the installer and
will keep `doctor` from reporting `ready` as specified in §14–§15.

### 8.2 Ordinary-user release path

The primary v1 user artifact is
`oopsinput-VERSION-x86_64-unknown-linux-musl.tar.gz`, built with the declared
minimum Rust toolchain and `--locked` from the matching `vVERSION` tag. The
musl target is deliberate: the current developer build is dynamically linked
to its host GNU C library and is not a portable release contract. Each archive
has one top-level versioned directory containing:

- the static `oopsinput` release binary;
- `oopsinput.zsh`, `install.zsh`, and `uninstall.zsh`;
- the license and a short release-install readme.

The same GitHub release publishes `SHA256SUMS`, and CI generates an artifact
attestation for the archive. Public instructions require checking the archive
against `SHA256SUMS` before extraction, describe that as an integrity check
rather than proof of safety, and offer GitHub's attestation verification as an
optional provenance check for users who already have the GitHub CLI. The
GitHub CLI is not an installation prerequisite. The official path never uses
`curl | zsh` or another download-and-execute pipe: the versioned archive is
saved and verified before its local installer runs.

Ordinary-user prerequisites are x86_64 Linux, interactive Zsh, and the standard
Linux `tar`/`sha256sum` plus installer helpers under `/usr/bin` or `/bin`.
Neither Rust nor a source checkout is required. Git is optional at runtime: if
it is absent, repository dirty/untracked facts are honestly unavailable, while
the rest of oopsinput continues to work. Source building remains the developer
path and feeds its release binary into the same installer contract.

The installer itself performs no network access, launches no daemon, asks for
no credentials or root access, never sources the user's `.zshrc`, and does not
change `PATH`. Downloading the release is a separate, visible acquisition
step; runtime network behavior remains limited to optional loopback Ollama.

### 8.3 Owned changes, failure, update, verification, and removal

Before committing a fresh install, the installer names every effect. It:

- installs the binary at `~/.local/bin/oopsinput` and the plugin plus stable
  uninstaller at `~/.local/share/oopsinput/`;
- creates a user-only config containing the explicitly chosen mode only when
  the config path does not already exist;
- preserves an existing `~/.zshrc` byte-for-byte at
  `~/.zshrc.oopsinput-backup`, without replacing the original backup on an
  update; and
- adds one exact marked block to `.zshrc` that sources only the installed
  plugin. State is created later by product use, not installation.

All complete output files are staged before the first owned destination is
changed. Cancellation occurs before that commit point. On an ordinary error or
a handled HUP/INT/TERM, a fresh install removes every file and backup it alone
created and restores the prior `.zshrc`; an update restores the previously
installed runtime set rather than leaving mixed versions. Existing user-owned
config, backup, state, and unrecognized files are never cleanup targets.
Symlink refusal, regular-file checks, private staging, exact marker ownership,
and byte-exact no-final-newline restoration remain mandatory.

Rerunning a release installer over one healthy marker block is the update
path. It atomically replaces the binary, plugin, and stable uninstaller as one
rollback-capable set, retains the original shell backup, and preserves any
existing config without prompting. No marker means no authority to overwrite
same-named runtime files. Source-install test overrides remain private test
seams and are not a second user contract.

After installation, the script reports the selected or preserved mode and
instructs the user to open a new terminal, then run the absolute installed
binary's read-only `doctor` command. It does not claim success as a ready shell
until `doctor` in that newly loaded interactive Zsh reports `result: ready`.

Removal never requires the downloaded archive or source checkout. The public
command runs the installed `~/.local/share/oopsinput/uninstall.zsh`; the
healthy marker remains its ownership receipt. It removes the marker and the
three runtime files (binary, plugin, uninstaller), while retaining config,
recorded state, and the original shell backup. A user who wants recorded state
deleted runs the installed binary's exact `purge` command first. Configuration
and backup deletion remain separate manual choices because the uninstaller
does not own that data.

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
4. All state files and the config dir are user-only (0700/0600). State paths
   resolve from absolute environment paths only; a relative explicit override
   disables state rather than claiming the working directory. Product-owned
   files are created atomically without following a symlink, and an opened
   existing file must still match the nonsymlink path that was inspected.
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
Top-level newlines already present in the submitted ZLE buffer are command
operators like `;`, so every pasted/prefilled segment is analyzed. Newlines
inside quotes/substitutions remain inside their owning word. Once a heredoc is
declared, its body is opaque data rather than a source of command evidence.

Alias/function visibility: the plugin ships the resolution kind of the command
word (alias→what, function, builtin, external path) with each proposal, since
only the live shell knows. The binary treats unresolvable meaning as
uncertainty, not as safety.

## 14. Storage and privacy

- State resolution accepts an absolute `$OOPSINPUT_STATE_DIR` override, else an
  absolute `$XDG_STATE_HOME/oopsinput`, else an absolute
  `$HOME/.local/state/oopsinput`. A nonempty relative explicit override disables
  state; a relative XDG root is ignored in favor of HOME; relative or absent
  HOME leaves state unavailable. State failure always fails open.
- `~/.config/oopsinput/config` — plain `key = value`, capped at 64 KiB. An
  oversized file is rejected whole (defaults apply and `doctor` reports it
  invalid), never parsed as a valid prefix. A symlink or other non-regular
  config-file leaf is ignored, and the opened inode is checked against the
  inspected path before any bytes are consumed.
- `~/.local/state/oopsinput/events.jsonl` — structural events. New records
  append one JSON line at a time: timestamp, decision, evidence codes, layer,
  timings, outcome, keyed fingerprints, the explicit mode-blind policy reason
  when Shadow/Suggest suppressed a Warn/Confirm, and (when L4 runs) model
  state immediately before inference: `warm`, `cold`, or `unknown`. No raw
  commands, paths, or goal text by default. Retention is the only rewrite.
- `~/.local/state/oopsinput/policy.jsonl` — append-only habituation state.
  Before a visible non-catastrophic prompt, one state-locked transaction reads
  the bounded tail, applies budget/cooldown gates, and appends a short-lived
  reservation. The shown outcome completes it; a prompt that could not be
  displayed releases it; an abandoned reservation expires after ten minutes.
  This prevents concurrent shells from claiming the same last budget slot.
  Outcome records carry timestamp, rule code, and what the user did; the reader
  folds reservation/outcome pairs before computing budget and cooldown. Every
  writer uses the stable lock, so retention cannot lose concurrent appends.
- `~/.local/state/oopsinput/key` — local random fingerprint key, 0600.
- `~/.local/state/oopsinput/.oopsinput.lock` plus two retention markers —
  user-only coordination metadata; they contain no command data.
- Analysis-time state writes wait for the coordination lock under the existing
  process watchdog, preserving concurrent records without creating a new
  unbounded wait. After prompt setup begins, state writes wait at most 25 ms
  and append only: if the lock remains busy, that outcome record is omitted
  and its existing reservation expires rather than opening an extra budget
  slot; if retention is due, it is deferred to the next analysis-time write.
  The user's edit/cancel/run choice therefore takes effect without a full-log
  sweep after it. Explicit `purge` waits for the lock because completing
  deletion is the command's sole requested effect.
- `oopsinput report` — decision/model/intervention rates, deterministic and
  warm/cold/unknown model latency percentiles, ranked evidence codes, and
  hypothetical interventions from shadow data. A model consultation is
  identified by its `model.*` evidence code; legacy events without an explicit
  model state stay in the `unknown` model bucket rather than contaminating
  deterministic percentiles. New hypothetical counts use the explicit
  pre-mode reason field; legacy M5 records use only the closed set of reasons
  that actually represented Warn/Confirm, never every `policy.*` reason. One
  JSONL record is capped at 64 KiB while reading; an oversized record counts as
  malformed and is drained so valid later records remain usable.
- `oopsinput purge` — one exact, zero-argument command that deletes every
  oopsinput-owned state file and coordination marker, then removes the state
  directory if empty. It never deletes configuration, follows a symlink, or
  recursively removes an unknown entry; an unknown entry is kept and named in
  the result only as an unrecognized entry (never echoed from disk). Purge can
  unlink a corrupted lock-anchor symlink without following it and restore a
  regular anchor's private mode before coordinating deletion; other inode
  types are refused.
- Retention default 30 days, pruned on analysis-time writes. Each log checks a
  small marker on each such append and performs at most one full sweep per 24
  hours. A sweep drops records older than its cutoff plus malformed/torn
  records, including records over the 64 KiB read cap, then atomically replaces
  the log under the shared state lock. With continuing analysis-time writes,
  an expired record can remain for less than one additional day;
  post-prompt-only or no writes do not run a sweep.
- **No telemetry. No network beyond loopback Ollama. Ever.**

## 15. Config (initial surface)

```
mode = shadow            # parser fallback; installer writes explicit choice
model = qwen3.5:4b       # empty = deterministic-only
model_timeout_ms = 2000
det_timeout_ms = 150
budget_per_hour = 3
log_raw = false          # opt-in research capture (redacted first)
```

Unknown keys: warn once, ignore. Invalid values: fall back to the parser
default for that key, say so once. Shadow is the parser fallback for `mode`,
not a fresh-install selection; §8.1 requires the installer to write the user's
explicit choice. The warning fingerprint check, display, and marker
replacement are one state-locked transaction across concurrent shells. The
Zsh adapter requests direct `/dev/tty` delivery only when a warning exists; the
marker is committed only after that complete display succeeds.

## 16. Repository layout

```
oopsinput/
├── SPEC.md              # this file — canonical
├── PLAN.md              # milestones and progress
├── PLAN-ARCHIVE.md      # completed milestones, archived verbatim
├── ARCHITECTURE.md      # how the pieces fit — the developer's map
├── SECURITY.md          # threat boundary + vulnerability reporting
├── AGENTS.md            # Codex working agreement for coding sessions
├── CLAUDE.md            # Claude working agreement; keep in sync
├── README.md
├── OOPSINPUT-HANDS-ON-WALKTHROUGH.html # owner-run product/testing guide
├── LICENSE              # Apache-2.0
├── Cargo.toml           # single binary crate — no workspace until earned
├── Cargo.lock           # exact resolved dependency graph
├── deny.toml            # advisories, licenses, sources, exact crate allowlist
├── .github/workflows/
│   ├── ci.yml                # fmt, clippy, tests + release acceptance gates
│   ├── dependency-policy.yml # cargo-deny on changes + weekly
│   └── release.yml           # pinned musl build, attestation, tag publication
├── src/
│   ├── main.rs          # dispatch + check/report/purge/help/version
│   ├── doctor.rs        # read-only install/environment diagnosis
│   ├── proposal.rs      # Zsh proposal input + metadata    (M1)
│   ├── lexer.rs         # conservative shell lexer          (M2)
│   ├── distance.rs      # bounded edit distance, shared by typo + context (M3)
│   ├── proc.rs          # bounded external-helper wait/kill loop (M3)
│   ├── layers/          # typo.rs danger.rs context.rs infer.rs  (M2–M4)
│   ├── policy.rs        # evidence → decision + budgets     (M3)
│   ├── ui.rs            # /dev/tty prompts + escaping       (M2)
│   ├── model.rs         # loopback HTTP transport            (M4)
│   ├── events.rs        # JSONL log + report                (M1, M5)
│   └── state.rs         # locking, retention, purge               (M5)
├── zsh/
│   ├── oopsinput.zsh    # widget wrapper plugin
│   ├── install.zsh
│   └── uninstall.zsh
├── release/
│   └── INSTALL.md       # short readme rendered into each release archive
├── scripts/
│   ├── build-release-bundle.zsh # reproducible musl archive + SHA256SUMS
│   ├── release-bundle-gate.zsh  # static/archive/shipped-lifecycle gate
│   ├── install-experience-gate.zsh # every public installer path via archive
│   ├── lifecycle-gate.zsh       # clean-home install-to-uninstall gate
│   ├── pty-gate.zsh             # PTY volume and buffer-integrity gate
│   └── perf-gate.zsh            # release-binary latency gate
├── eval/golden/         # paired JSON cases
└── tests/               # integration + PTY tests
```

(Module files are created when their milestone starts — no empty placeholder
files.)

## 17. Beyond v1 (recorded so v1 doesn't paint us into corners)

Agent request schema plus adapters over structured `check` decisions, with
explicit origin, goal context, and provenance · bash/fish adapters · optional
daemon if spawn cost ever matters · richer personalization (transparent
thresholds only — no silent learning) · sandboxed effect-probing · additional
context packs (kubectl/cloud/SQL) · packaging (AUR/deb/homebrew). None of these
may weaken §9 invariants.

## 18. Success definition for v1

v1 is done when: Kyle has been taught the product from the ground up and has
personally exercised its complete documented user workflow in a real
interactive Zsh session; the verified prebuilt release archive has completed
the guided fresh-install, `doctor`, update, purge, and stable-uninstall journey
without a source checkout; install/uninstall are clean on this machine; 10,000
scripted PTY submissions produce zero altered/lost buffers and zero hangs; the
typo layer works with exact-resolution zero false positives; the danger+context
layers pass the paired golden corpus; the inference layer produces valid schema
output against a local model and degrades silently without one; the
deterministic path meets its latency budget; a fresh ≥1,000-command natural-use
Shadow-or-Suggest pilot, begun only after that familiarization on the stabilized
build, has been reviewed; and the tested stabilization work has been published
in a follow-up alpha that is presentable enough to send to a friend. Automated
tests, generated commands, scripted volume, replayed history, agent-produced
explanations, and pre-familiarization events cannot substitute for Kyle's own
hands-on acceptance or count toward the natural-use pilot.
