# oopsinput — Architecture and developer guide

This document explains the implementation as it exists today, ground-up, so a
new developer can build it, test it, and change it with confidence.

How it relates to the other documents:

- **[SPEC.md](SPEC.md)** is canonical for *design*: scope, principles, security
  invariants, interfaces, and the full four-layer vision. When this document
  and SPEC disagree, SPEC wins.
- **[PLAN.md](PLAN.md)** tracks *progress*: which milestones are done and what
  each one covered.
- **This document** covers *how the code works right now* — currently the
  M0/M1 state: command capture in zsh, shadow-mode passthrough, event logging,
  and the test harness that proves buffers survive intact. The analysis layers
  (typo, danger, context, model) are designed in SPEC but not yet built, so
  they are only mentioned here where the existing code leaves seams for them.

## 1. The pieces

oopsinput is three things working together:

1. **A zsh plugin** (`zsh/oopsinput.zsh`) — a small script sourced by your
   `~/.zshrc`. It hooks the moment you press Enter, hands the typed command to
   the binary, and interprets the binary's answer. It contains no analysis
   logic at all.
2. **A Rust binary** (`src/`, built to a single executable named `oopsinput`)
   — spawned fresh for every command. It reads the command, decides what to do
   (in M1: always "allow"), writes one line to an event log, and exits. There
   is no daemon, no background process, no state held in memory between
   commands.
3. **Install/uninstall scripts** (`zsh/install.zsh`, `zsh/uninstall.zsh`) —
   copy the binary into `~/.local/bin` and add/remove one clearly-marked block
   in `~/.zshrc`. Everything they touch is marked, backed up, and reversible.

The trust and failure model in one sentence: the plugin treats the binary as
something that can crash, hang, or be missing at any moment, and in every one
of those cases the user's original command runs unchanged ("fail open").

## 2. From a fresh clone

Prerequisites:

- **Rust** via [rustup](https://rustup.rs) (user-level install, no root). If
  `cargo` isn't found in a fresh shell:

```
. "$HOME/.cargo/env"
```

- **zsh** — the target shell, and what the PTY tests drive.
- **`script`** from util-linux — the PTY tests use it to run a real
  interactive zsh inside a pseudo-terminal. Preinstalled on essentially every
  Linux distribution.

Build and test:

```
cargo build --release
cargo test
cargo fmt --check && cargo clippy --all-targets -- -D warnings
```

- `cargo build --release` produces `target/release/oopsinput`. Performance
  claims only count in release builds.
- `cargo test` runs the unit tests inside each `src/` module plus the
  integration tests in `tests/` (including the PTY suite — see §6).
- The `fmt`/`clippy` line must be clean before any commit (project rule).

The M1 acceptance gate — thousands of scripted submissions through a real
interactive zsh, verifying that every command's output appears and nothing
hangs:

```
scripts/pty-gate.zsh
```

(Default 10,000 submissions; pass a number for a quicker run, e.g.
`scripts/pty-gate.zsh 500`.)

To install on your own machine (shadow mode — it observes and records, never
interrupts):

```
zsh/install.zsh
```

To remove:

```
zsh/uninstall.zsh
```

## 3. The zsh side, ground-up

### What ZLE and widgets are

When zsh is interactive, the line you're typing lives in the **Zsh Line
Editor (ZLE)**. Every keypress runs a **widget** — a named editing function.
Typing `a` runs the `self-insert` widget; pressing Enter runs the
`accept-line` widget, which submits the buffer for execution. Crucially, zsh
lets you *replace* a widget with your own shell function. That is the entire
interception mechanism: no patched zsh, no traps, no preexec tricks — just
widgets swapped for wrappers.

Enter is not the only way to submit a buffer. The plugin wraps all four
"accept" widgets (`accept-line`, `accept-line-and-down-history`,
`accept-and-hold`, `accept-and-infer-next-history`), and because widgets are
keymap-independent, the same wrappers cover both Emacs and Vi modes.

### What the wrapper does (`_oopsinput_handle`)

On each accepted buffer, the wrapper:

1. **Passes through untouched** — without invoking the binary at all — if any
   of these hold: we're already inside a wrapped call (recursion guard via a
   dynamically-scoped `_OOPSINPUT_ACTIVE` variable), the buffer is empty or
   whitespace-only, or this is a continuation line (`$CONTEXT != start` —
   e.g. the second line of a command with an unclosed quote, typed at the
   `PS2` prompt). Only the initial line of a command is analyzed.
2. **Resolves the command word.** Only the live shell knows the user's
   aliases and functions, so the plugin asks `whence -w` what the first word
   is (`alias`, `function`, `builtin`, `command`, `hashed`, `reserved`, or
   `none`) and passes that single token to the binary as `--res <kind>`. The
   vocabulary is enforced with a `case` whitelist *in the plugin*: anything
   unexpected collapses to `unknown`, so raw user text can never ride into
   argv. (Argv matters because `/proc/<pid>/cmdline` is world-readable —
   which is also why the buffer itself travels over stdin, never argv.)
3. **Invokes the binary**: `print -rn -- "$original" | oopsinput check --res
   <kind>`, with stdout and stderr discarded (in M1 nothing the binary prints
   is user-facing).
4. **Interprets the exit code** and nothing else:

   | Exit code | Meaning | Plugin action |
   |---|---|---|
   | `0` | allow | run the original buffer unchanged |
   | `10` | replace | *(M2 — typo correction accepted; new buffer arrives on fd 3)* |
   | `11` | edit | restore the original buffer to ZLE for editing, don't run |
   | `12` | cancel | clear the buffer, run nothing |
   | anything else | failure | **fail open: run the original buffer unchanged** |

   "Run the original" is implemented by delegating to whatever the widget was
   before wrapping — a previously-installed wrapper from another plugin if one
   existed (saved under `_oopsinput_orig_<name>` at load time), otherwise the
   ZLE builtin (`.accept-line` etc.). This capture-and-delegate discipline is
   why oopsinput composes with other ZLE plugins instead of clobbering them.

Two more load-time behaviors worth knowing: sourcing the plugin twice is
harmless (already-wrapped widgets are detected and skipped), and if the binary
is missing at load, the plugin prints one diagnostic and disables itself for
the session. That diagnostic renders the configured path with zsh's `(V)`
flag so control characters appear visibly (`^[`) — a hostile `OOPSINPUT_BIN`
value can't smuggle terminal escape sequences to your terminal. (`(qqqq)`
quoting is *not* sufficient for this; it leaves control bytes raw.)

### Two zsh traps, regression-locked

Both were real M1 bugs, now pinned by tests (see PLAN):

- `${${(z)BUFFER}[1]}` *string*-indexes (first character, not first word) when
  the split yields a single word — the plugin uses an explicit array
  assignment instead.
- Nested `${$(whence -w ...)##*: }` doesn't strip as expected — extraction is
  done in two steps, then whitelisted.

## 4. The Rust side, ground-up

Four modules today (SPEC §16 lists the files future milestones add):

### `src/main.rs` — dispatch, watchdog, the `check` path

Hand-rolled subcommand dispatch (no CLI-parsing dependency; SPEC §12):
`version`, `check`, `doctor`, `help`.

`check` is the command the plugin runs. Its first act is `arm_watchdog()`:
spawn a thread that sleeps for the deadline (150 ms, `DET_DEADLINE_MS`) and
then force-exits the whole process with code 1. If analysis ever wedges, the
process dies, the plugin sees a nonzero exit, and fails open — the user's
prompt cannot be held hostage. A blunt `exit` is safe precisely because the
process is per-command: there's nothing to clean up that matters more than
the user's prompt. (SPEC §6 records the one honest residual: a process stuck
in uninterruptible disk I/O can't even do that; documented, not defended.)

After the watchdog: read the proposal, run analysis (M1: none — always
`allow` with reason `shadow.observed`), append an event, print a `Decision`
as one JSON line on stdout, exit 0.

Two test hooks — `OOPSINPUT_TEST_DEADLINE_MS` (shorten the deadline) and
`OOPSINPUT_TEST_HANG` (sleep 30 s inside `check`) — exist **only in debug
builds** (`#[cfg(debug_assertions)]`, meaning the code is compiled out of
release binaries entirely). The PTY suite uses them to prove the watchdog
works end-to-end; a release binary has a fixed deadline and no hang hook.

`doctor` prints environment sanity checks: version, whether `zsh` is on PATH
(via direct file-metadata lookup that requires the executable bit — never by
asking a shell, per SPEC §9), whether the config file exists, and whether the
plugin block is present in `~/.zshrc`.

### `src/proposal.rs` — input parsing

A `Proposal` is the buffer (read from stdin, capped at 1 MB — oversized
input is truncated at a UTF-8 character boundary and flagged `capped`, so it
still gets analyzed rather than erroring out) plus the resolution kind.
`ResolutionKind` is a
closed enum; `parse` maps the eight known tokens and collapses *anything* else
to `Other`, so even if the plugin-side whitelist failed, raw text still
couldn't become a stored value. Unknown `check` flags are ignored — forward
compatibility for the future agent-adapter seam (SPEC §6).

### `src/lexer.rs` — the conservative lexer

Implements SPEC §13: it turns the buffer into words (with quoting resolved
where that's safe), control operators (`&&`, `|`, `;`, …), and redirections
— and marks anything it can't see through without evaluating it (command
substitution `$(…)`, backticks, process substitution, arithmetic, heredocs,
zsh glob qualifiers) as *opaque*: the raw text is kept, the word is flagged
`expands`, and a stable uncertainty code like `syntax.opaque_substitution`
is emitted. It never expands anything and never panics (a deterministic
fuzz smoke test hammers it with metacharacter soup). `command_words()`
extracts the words in command position — first word of each segment,
skipping `VAR=value` assignments and redirect targets — which is what the
upcoming typo layer will check. The uncertainty codes already flow into
`check`'s decision evidence and the shadow event log, so shadow data can
measure the unsupported-syntax rate (SPEC §11) before any layer goes live.

### `src/events.rs` — the shadow event log

Appends one JSON line per command to `events.jsonl` in the state directory
(resolution order: `$OOPSINPUT_STATE_DIR` override — used by tests — then
`$XDG_STATE_HOME/oopsinput`, then `~/.local/state/oopsinput`).

The invariants, all test-pinned:

- **Structural fields only.** Timestamp, decision, reason code, resolution
  kind, buffer byte count, word count, duration. The `Event` struct has no
  field that *could* carry command text — the type system is the redaction.
- **User-only permissions**: directory `0700`, file `0600`.
- **One write syscall per event.** The line and its trailing newline are
  joined into a single buffer before writing, because `O_APPEND` atomicity
  holds per write call — two concurrent shells appending with separate
  newline writes would interleave and corrupt the JSONL stream (a real bug
  found by review, now pinned by a 16-thread hammer test).
- **Failures are swallowed.** Logging must never cost the user their command
  or their prompt.

### Dependencies

Exactly two: `serde` and `serde_json` (JSON is a correctness/security surface
with real spec depth). Everything else — CLI dispatch, PATH lookup, eventually
the HTTP client and lexer — is self-written per the policy in SPEC §12.
Adding a dependency requires updating SPEC §12 first.

## 5. One Enter, end to end

You type `git status` and press Enter:

1. ZLE runs the wrapped `accept-line`, which calls
   `_oopsinput_handle accept-line` (`zsh/oopsinput.zsh:45`).
2. The buffer is non-empty, not a continuation line, not recursive — so the
   plugin resolves `git` via `whence -w` → `command`.
3. It pipes the exact bytes `git status` to
   `~/.local/bin/oopsinput check --res command`.
4. The binary arms the watchdog (`src/main.rs:66`), reads the buffer from
   stdin into a `Proposal` (`src/proposal.rs:71`), lexes it for structure
   and uncertainty evidence (`src/lexer.rs`), and — the decision layers not
   being built yet — decides `allow` / `shadow.observed`.
5. It appends `{"ts_ms":…,"decision":"allow","reason_code":"shadow.observed",
   "evidence":[],"res_kind":"command","cmd_expands":false,"buffer_bytes":10,
   "word_count":2,"duration_us":…}` to
   `~/.local/state/oopsinput/events.jsonl` (`src/events.rs:53`), prints the
   decision JSON on stdout (discarded by the plugin), exits 0.
6. The plugin sees exit 0, restores `BUFFER=$original`, and delegates to the
   real `accept-line`. zsh executes `git status` exactly as typed.

Measured cost of that round trip on the dev machine: p50 5 ms, p95 6 ms —
against a 25 ms p95 budget (SPEC §10). If step 3–5 fails *in any way* —
binary missing, crash, watchdog fired, weird exit code — step 6 still happens
identically.

## 6. How it's tested

The testing philosophy: **buffer exactness and fail-open behavior are the
product**, so the highest-value tests drive a real interactive zsh, not mocks.

- **Unit tests** live inside each `src/` module (`#[cfg(test)] mod tests`):
  the closed resolution vocabulary, concurrent log appends, structural-only
  serialization, executable-bit PATH lookup.
- **`tests/pty.rs`** — the PTY integration suite. Each test builds an
  isolated `ZDOTDIR` (a throwaway home for zsh config) whose `.zshrc` loads
  the plugin against the freshly-built debug binary, then runs a genuine
  interactive zsh inside a pseudo-terminal via util-linux
  `script -qec "zsh -i" /dev/null`, feeds it keystrokes, and asserts on what
  the terminal displayed. Covered: ordinary passthrough, unicode/quoting
  survival, PS2 multiline continuation, missing binary fails open, hostile
  escape sequences in the load diagnostic are neutralized, hanging binary is
  killed by the watchdog within deadline, secrets never reach the event log,
  resolution kinds are extracted correctly (including the single-word
  regression), double-sourcing is harmless, and Vi keymap accepts work.
- **`tests/uninstall.rs`** — pinned audit finding: `uninstall.zsh` against a
  `~/.zshrc` with damaged marker blocks must refuse to edit, never
  delete-to-end-of-file.
- **`scripts/pty-gate.zsh`** — the volume acceptance gate: N unique
  submissions (default 10,000) through a PTY shell; every output must appear,
  nothing may hang. M1's run: 10,000/10,000, zero altered buffers, in 128 s.
- **`eval/golden/`** — empty today; fills in M3 with paired counterfactual
  cases (same command, different context, different expected decision — SPEC
  §11). Project rule: every danger rule ships with a case where the same
  command is silently allowed, so the tool can't decay into a blanket
  dangerous-command blocker.

Testing rules that bind every change: each layer lands with tests in the same
commit; bug fixes ship a regression fixture; anything touching the zsh plugin
gets PTY tests; fixtures never contain real shell history or personal data.

## 7. Decisions and constraints (why it's shaped this way)

Short versions — SPEC has the full arguments:

- **Per-command spawn, no daemon** (SPEC §6). A static Rust binary spawns in
  ~1–5 ms, well inside budget. Persistent state lives in the shell (aliases,
  history — the plugin's job) or in files. A daemon is a v2 option only if
  measurement ever demands it.
- **Fail open, enforced in-binary** (SPEC §6). The watchdog lives inside the
  binary rather than as a plugin-side timer: the binary is ours and arms it
  before doing anything else, and zsh's job control independently returns the
  prompt if the process is stopped or killed (verified by test).
- **Never execute what you analyze** (SPEC §9.1). No shell invocation during
  analysis, ever — even `doctor`'s PATH lookup walks the filesystem itself.
- **Sync only, lean style** (CLAUDE.md). No async runtime, plain data +
  free functions, no `unwrap` outside tests, no `unsafe` without discussion.
- **Shadow first** (SPEC §8). Nothing becomes visible to users until logged
  evidence says a category earns it. The decisive metric is false
  interventions per 1,000 commands — missing a real mistake is acceptable;
  crying wolf is fatal (SPEC §2).
- **Honest threat boundary** (SPEC §9). oopsinput is an assistance layer for
  the user's own mistakes. It does not resist malware running as the user, a
  compromised kernel, or a determined bypass — and its docs never claim
  otherwise.

## 8. Where things stand

See [PLAN.md](PLAN.md) — M0 (skeleton) and M1 (zsh capture + shadow
passthrough, plus a same-day hardening pass) are complete; M2 (lexer + typo
layer, the first user-visible value) is underway: the lexer and the
UTF-8-safe input cap have landed, the typo layer and intervention UI have
not. Files SPEC §16 lists for later milestones (`layers/`, `policy.rs`,
`ui.rs`, `model.rs`) don't exist yet by design — modules are created when
their milestone starts.
