# oopsinput — Architecture and developer guide

This document explains the implementation as it exists today, ground-up, so a
new developer can build it, test it, and change it with confidence.

How it relates to the other documents:

- **[SPEC.md](SPEC.md)** is canonical for *design*: scope, principles, security
  invariants, interfaces, and the full four-layer vision. When this document
  and SPEC disagree, SPEC wins.
- **[PLAN.md](PLAN.md)** tracks *progress*: which milestones are done and what
  each one covered.
- **This document** covers *how the code works right now*, in depth through
  the M2 state: command capture in zsh, the conservative lexer, the typo
  layer with its single-key prompt and correction channel, event logging, and
  the test harness that proves buffers survive intact. The M3 core has since
  landed and is summarized in §9 pending this document's next full refresh:
  the danger layer, the deterministic half of the context layer, the policy
  engine, and the L2+ warning UI (e/edit, c/cancel, r/run-once — live in
  warn/confirm modes). The model layer and the recency relation remain
  unbuilt.

## 1. The pieces

oopsinput is three things working together:

1. **A zsh plugin** (`zsh/oopsinput.zsh`) — a small script sourced by your
   `~/.zshrc`. It hooks the moment you press Enter, hands the typed command to
   the binary, and interprets the binary's answer. It contains no analysis
   logic at all.
2. **A Rust binary** (`src/`, built to a single executable named `oopsinput`)
   — spawned fresh for every command. It reads the command, analyzes it,
   decides what to do, writes one line to an event log, and exits. When it
   decides to intervene it talks to your terminal directly (see §4.6) rather
   than through the plugin. There is no daemon, no background process, no
   state held in memory between commands.
3. **Install/uninstall scripts** (`zsh/install.zsh`, `zsh/uninstall.zsh`) —
   copy the binary into `~/.local/bin`, add/remove one clearly-marked block
   in `~/.zshrc`, and write a default config file on first install.
   Everything they touch is marked, backed up, and reversible.

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

To install on your own machine:

```
zsh/install.zsh
```

This installs in **suggest** mode: the typo layer may ask you a question
(only ever about a command that could not have run anyway), and everything
else is silently observed and recorded. See §4.5 for what modes exist and
§4.7 for how the mode is chosen.

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
3. **Builds the payload and invokes the binary.** The buffer's exact bytes go
   in first. Then, *only* when the resolution kind is `none` — the case the
   typo layer handles — the plugin appends a NUL byte followed by every
   command name the live shell can see that the binary cannot: alias,
   function, builtin, and reserved-word names. (A NUL is a safe separator
   because zsh strings can never contain one.) That name list is the typo
   layer's candidate pool alongside the executables it finds on PATH itself.
   Only this already-failing path pays the cost of collecting it.

   The invocation routes three streams: stdout (the decision JSON) and
   stderr are discarded, while **file descriptor 3 is captured** into a shell
   variable. Descriptor 3 is the channel the binary uses to hand back a
   corrected command (see §4.6); nothing user-facing travels on it, because
   prompts go straight to the terminal instead.
4. **Interprets the exit code** and nothing else:

   | Exit code | Meaning | Plugin action |
   |---|---|---|
   | `0` | allow | run the original buffer unchanged |
   | `10` | replace | run the corrected buffer received on descriptor 3 |
   | `11` | edit | restore the original buffer to ZLE for editing, don't run |
   | `12` | cancel | clear the buffer, run nothing |
   | anything else | failure | **fail open: run the original buffer unchanged** |

   Exit code `10` is the only path where something the binary produced gets
   executed, so it carries an integrity check. The binary terminates the
   replacement with a single NUL byte; the plugin runs the replacement *only*
   if that byte is present, and otherwise falls back to the original. A
   truncated or absent write therefore can never execute a truncated command.
   (The NUL does double duty: zsh's `$(...)` strips trailing newlines, and
   the sentinel protects a replacement that legitimately ends in one.)

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

Six modules today (SPEC §16 lists the files future milestones add):

### 4.1 `src/main.rs` — dispatch, watchdog, the `check` path

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

The watchdog **retires** once a prompt is on screen (a flag named
`PROMPT_ACTIVE`): a question waiting on a human legitimately outlives an
analysis deadline, and killing the process mid-prompt would leave the
terminal in the wrong mode. What makes retiring safe is that everything past
that point is bounded by construction — the prompt's own read has a timeout
enforced by the terminal, and every external helper it runs is killed at a
deadline (§4.6).

After the watchdog: read the proposal, lex it, run the typo layer, assemble
the evidence codes, and — if the mode allows intervening and the layer found
a candidate — prompt. Then append an event, print a `Decision` as one JSON
line on stdout, and exit with the code the decision implies.

One ordering detail matters for measurement: the duration recorded in the
event is captured *before* any prompt, so latency percentiles measure
analysis, not how long a human took to answer.

Two test hooks — `OOPSINPUT_TEST_DEADLINE_MS` (shorten the deadline) and
`OOPSINPUT_TEST_HANG` (sleep 30 s inside `check`) — exist **only in debug
builds** (`#[cfg(debug_assertions)]`, meaning the code is compiled out of
release binaries entirely). The PTY suite uses them to prove the watchdog
works end-to-end; a release binary has a fixed deadline and no hang hook.

`doctor` prints environment sanity checks: version, whether `zsh` is on PATH
(via direct file-metadata lookup that requires the executable bit — never by
asking a shell, per SPEC §9), which config file is in effect and whether it
exists, the resulting mode, and whether the plugin block is present in
`~/.zshrc`. The config line and the mode line resolve through the same
function, so they can never contradict each other (they once did — a bug
found by review and now pinned by `tests/doctor.rs`).

### 4.2 `src/proposal.rs` — input parsing

A `Proposal` is what arrived on stdin plus the resolution kind from argv.

The stdin payload is the buffer, optionally followed by a NUL byte and the
newline-separated candidate names described in §3. Parsing is deliberately
defensive, because this is attacker-adjacent input in the sense that matters
(a corrupted or hostile payload must degrade, never mislead): the whole read
is capped at 1 MB; oversized input is truncated at a UTF-8 character
boundary and flagged `capped` so it still gets analyzed rather than erroring
out; the name list is count-capped; a single name that isn't valid UTF-8 is
skipped rather than failing the whole check; and if the size cap lands
inside the name list, only the possibly-truncated final name is dropped
while the buffer itself stays intact.

`ResolutionKind` is a closed enum; `parse` maps the eight known tokens and
collapses *anything* else to `Other`, so even if the plugin-side whitelist
failed, raw text still couldn't become a stored value. Unknown `check` flags
are ignored — forward compatibility for the future agent-adapter seam
(SPEC §6).

### 4.3 `src/lexer.rs` — the conservative lexer

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
typo layer checks. The uncertainty codes flow into `check`'s decision
evidence and the shadow event log, so shadow data can measure the
unsupported-syntax rate (SPEC §11).

One subtlety worth knowing before you touch this file: **word separators are
space, tab, and newline only** (`is_shell_whitespace`), deliberately *not*
Rust's general "is this whitespace" test. Unicode blanks such as the
no-break space are ordinary word characters to the shell, and this lexer
must split words the way zsh will. When it didn't, a pasted no-break space
made the lexer analyze `gti` while the shell had actually resolved
`<no-break space>gti`, so accepting the correction produced another
command-not-found. Any code that decides where a word starts or ends must
use this same function.

### 4.4 `src/events.rs` — the shadow event log

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

### 4.5 `src/layers/typo.rs` — the first analysis layer

This is layer 1 of the four in SPEC §5. It answers one question: *you typed
a command word that doesn't exist — did you mean this other one?*

It only ever runs when the shell reported the resolution kind as exactly
`none`. That single condition is what makes the layer safe to enable by
default: the command could not have run, so the alternative to a suggestion
was an error message. `unknown` is not `none` — not knowing never prompts.

Before matching, the layer insists on knowing exactly what the user typed.
It takes the word only if it is the buffer's *first* token (the same word
the plugin resolved), unquoted, free of expansions, free of `/`, and between
2 and 64 characters. A single character is never corrected, because nearly
every short name is one edit away from it.

Matching is a self-written bounded **optimal string alignment** distance:
insertion, deletion, substitution, and swapping two adjacent characters each
cost one edit. Words of four characters or fewer allow one edit; longer
words allow two. (`gti` → `git` is a swap, so it costs one.) Candidates come
from two places: the names the plugin supplied, and the executables the
layer finds by reading each PATH directory itself — never by asking a shell,
and only counting entries that are real files with an executable bit. Hard
caps bound the work: entries per directory, total permission checks, and
candidate name length.

Three refusals protect the "never a false positive" rule:

- **An exact match anywhere suppresses everything.** If some name is
  identical to what the user typed, then our scan and the shell's
  resolution disagree, and disagreement means silence. This check runs
  outside the permission-check budget, so a pathological directory can't
  starve it.
- **Implausible candidate names are rejected** — anything containing
  whitespace, a shell metacharacter, or a control character. zsh permits an
  alias named `foo bar`, but splicing that into a command line would run
  `foo` with an argument `bar`, which is not what the prompt promised.
- **`replacement_buffer` refuses unless it is certain.** It rebuilds the
  command by replacing only the typed word, preserving every other byte
  exactly, and returns nothing at all unless the buffer's first word is
  byte-identical to the analyzed word and ends on a real boundary.

### 4.6 `src/ui.rs` — prompts and the display escaper

Two responsibilities, both security-relevant.

**Escaping.** `escape_for_display` is what every piece of untrusted text
passes through before it can reach your terminal. Terminals interpret
certain byte sequences as commands rather than text — changing colors,
rewriting the window title, moving the cursor — and some Unicode characters
reverse the display order of text, so a file named to exploit that can
appear as a completely different name than it is. The escaper renders all of
that inert: control characters become visible caret notation (`^[`), and
bidirectional-text and invisible formatting characters become visible
`\u{...}` escapes. A 20,000-case fuzz test asserts that nothing active ever
survives it and that escaping twice changes nothing.

**Prompting.** `prompt_typo` asks its question on `/dev/tty` — the terminal
itself — rather than stdout, which carries the decision JSON and is
discarded by the plugin. It reads a single keypress: `y` accepts, Ctrl-C
cancels, and everything else (`n`, any other key, a ten-second timeout, a
missing terminal, any internal failure) runs the original command, which is
the safe outcome since that command could not have run anyway.

Switching the terminal into single-keypress mode requires the `stty`
program, because Rust's standard library exposes no terminal-mode control
and the dependency policy allows no crate for it. Two rules govern that
call, both learned from a security review:

- **`stty` is invoked by absolute path**, never resolved through PATH. When
  it was resolved by name, any directory leading PATH could supply the
  program that ran — and since this layer fires on *mistyped* commands, any
  typo at all became the trigger.
- **Every external helper is bounded** (`run_bounded`): a child that
  overruns its deadline is killed and reaped, and the call reports failure.
  This runs after the watchdog retires, so an unbounded child would hang the
  shell with nothing left to recover it.

The saved terminal settings are restored on every exit path by a guard
value, so no path out of the prompt leaves your terminal in raw mode.

### 4.7 Modes and configuration

SPEC §8 defines four modes: **shadow** (analyze and record, never visible),
**suggest** (adds typo prompts), **warn**, and **confirm** (both belong to
later milestones). Because warn and confirm include typo prompts, all three
of suggest/warn/confirm currently behave as suggest.

The mode is resolved in this order: the `OOPSINPUT_MODE` environment
variable, then a `mode = ...` line in the config file, then shadow. The
config file lives at `$XDG_CONFIG_HOME/oopsinput/config` if that variable is
set, otherwise `~/.config/oopsinput/config`. Any unrecognized value resolves
to shadow — the silent mode is the safe default. The config reader is a
handful of lines (`key = value`, `#` starts a comment); the fuller surface
in SPEC §15 arrives with the policy work in M3.

`zsh/install.zsh` writes `mode = suggest` on a fresh install, with
user-only permissions, and never touches a config that already exists —
including a path occupied by a symlink, which it leaves alone rather than
writing through.

### 4.8 Dependencies

Exactly two: `serde` and `serde_json` (JSON is a correctness/security surface
with real spec depth). Everything else — CLI dispatch, PATH lookup, the edit
distance, the lexer, config parsing, and eventually the HTTP client — is
self-written per the policy in SPEC §12. Adding a dependency requires
updating SPEC §12 first.

## 5. Two Enters, end to end

### 5.1 The common path: a command that resolves

You type `git status` and press Enter:

1. ZLE runs the wrapped `accept-line`, which calls
   `_oopsinput_handle accept-line` (`zsh/oopsinput.zsh:45`).
2. The buffer is non-empty, not a continuation line, not recursive — so the
   plugin resolves `git` via `whence -w` → `command`. Because that is not
   `none`, no candidate names are collected.
3. It pipes the exact bytes `git status` to
   `~/.local/bin/oopsinput check --res command`, capturing descriptor 3.
4. The binary arms the watchdog (`src/main.rs:87`), reads the payload into a
   `Proposal` (`src/proposal.rs:85`), lexes it for structure and uncertainty
   evidence (`src/lexer.rs`), and skips the typo layer immediately — the
   resolution kind is not `none` (`src/layers/typo.rs:49`). Decision:
   `allow` / `shadow.observed`.
5. It appends `{"ts_ms":…,"decision":"allow","reason_code":"shadow.observed",
   "evidence":[],"res_kind":"command","cmd_expands":false,"buffer_bytes":10,
   "word_count":2,"duration_us":…}` to
   `~/.local/state/oopsinput/events.jsonl` (`src/events.rs:57`), prints the
   decision JSON on stdout (discarded by the plugin), exits 0.
6. The plugin sees exit 0, restores `BUFFER=$original`, and delegates to the
   real `accept-line`. zsh executes `git status` exactly as typed.

Measured cost of that round trip on the dev machine, release build,
including process spawn: **p50 3.6 ms, p95 4.4 ms** — against a 25 ms p95
budget (SPEC §10). If steps 3–5 fail *in any way* — binary missing, crash,
watchdog fired, weird exit code — step 6 still happens identically.

### 5.2 The typo path: a command that doesn't resolve

You type `gti status` and press Enter, in suggest mode:

1–2. As above, except `whence -w` reports `none`.

3. Because the kind is `none`, the plugin appends a NUL and every alias,
   function, builtin, and reserved-word name to the payload, then invokes
   the binary the same way.
4. The binary lexes, then runs the typo layer
   (`src/layers/typo.rs:38`). The word `gti` is the first token, literal,
   and 3 characters, so it qualifies. Scanning the supplied names and PATH
   finds `git` at distance 1 (one adjacent swap). No exact match for `gti`
   exists, so nothing suppresses the result. Evidence:
   `typo.candidate_d1`.
5. The mode is not shadow, so the binary builds the corrected buffer first
   (`src/layers/typo.rs:159`) — `git status`, with every byte after the
   command word preserved. Only if that succeeds does it mark the prompt
   active, retiring the watchdog, and ask on `/dev/tty`:
   `oopsinput: 'gti' not found — did you mean 'git'? [y/n]`
6. You press `y`. The binary writes `git status` plus one NUL byte to
   descriptor 3 (`src/main.rs:309`), records the outcome as
   `replace` / `typo.accepted`, and exits **10**.
7. The plugin sees exit 10, confirms the trailing NUL is present, strips it,
   sets `BUFFER` to the corrected text, and delegates. zsh runs `git status`.

Pressing `n` instead produces exit 0 and your original `gti status` runs and
fails naturally; Ctrl-C produces exit 12 and nothing runs at all. Measured
cost of the analysis on this path (release, including spawn, with a
2,000-name pool and a full PATH scan): **p50 16.3 ms, p95 19.5 ms** against
a 75 ms p95 budget. The prompt itself is unbounded by design — it waits for
a person.

## 6. How it's tested

The testing philosophy: **buffer exactness and fail-open behavior are the
product**, so the highest-value tests drive a real interactive zsh, not mocks.

- **Unit tests** live inside each `src/` module (`#[cfg(test)] mod tests`):
  the closed resolution vocabulary, payload parsing edge cases, concurrent
  log appends, structural-only serialization, executable-bit PATH lookup,
  the edit-distance function, the typo layer's refusal rules, byte-exact
  replacement construction, the display escaper (including its fuzz test),
  the prompt's key protocol against a scripted fake terminal, and the
  bounded external-helper runner.
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
  M2 added the full typo flow through a real shell: `y` runs the correction
  with arguments preserved byte-for-byte, `n` runs the original, Ctrl-C runs
  nothing, a command word that resolves never prompts, and the installed
  config file alone (no environment override) is enough to enable prompts.

  Some of these need a **staged** runner (`Session::run_staged`) that waits
  for expected text to appear on the terminal before sending the next keys.
  That is not a convenience: a Ctrl-C byte sent before the binary switches
  the terminal into single-key mode becomes an interrupt signal instead of a
  keypress, which is also why a real user's Ctrl-C is safe — they cannot
  press it before the prompt exists.
- **`tests/uninstall.rs`** — pinned audit finding: `uninstall.zsh` against a
  `~/.zshrc` with damaged marker blocks must refuse to edit, never
  delete-to-end-of-file.
- **`tests/install.rs`** — the installed defaults: a fresh install writes
  `mode = suggest` with user-only permissions, an existing config is left
  byte-identical, a symlink at the config path is not written through, and
  installing twice doesn't duplicate the `~/.zshrc` block.
- **`tests/doctor.rs`** — `doctor`'s config line and mode line must agree,
  including when `XDG_CONFIG_HOME` redirects the config elsewhere.
- **`scripts/pty-gate.zsh`** — the volume acceptance gate: N unique
  submissions (default 10,000) through a PTY shell; every output must appear,
  nothing may hang. M1's run: 10,000/10,000, zero altered buffers, in 128 s.
- **`eval/golden/`** — the golden corpus (SPEC §11). `typo.json` holds 20
  cases today: a command buffer, a resolution kind, a candidate name list,
  and the expected suggestion plus exact evidence codes. Half are
  **counterfactual pairs** — the identical command in a context where
  nothing should be suggested — and the runner asserts that at least 30% of
  cases are paired, so the discipline can't quietly erode. Project rule:
  every rule ships with a case where the same command is silently allowed,
  so the tool can't decay into a blanket dangerous-command blocker. Cases
  run hermetically (candidates come only from the fixture, never the
  developer's real PATH) and go through the same evidence-assembly function
  the binary uses, so they pin real behavior rather than a parallel
  implementation.

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
  analysis, ever — even `doctor`'s PATH lookup and the typo layer's PATH
  scan walk the filesystem themselves. The one external program the code
  runs at all is `stty`, and it obeys the same rule set: absolute path,
  fixed arguments, no shell, hard timeout.
- **The correction channel is treated like the event log** (SPEC §9.2).
  Descriptor 3 is the only place binary output becomes an executed command,
  so it gets exact bytes, no interpretation, an integrity sentinel, and
  pinned tests. Any doubt anywhere in that chain produces no replacement at
  all rather than a best guess.
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

## 8. Known limitations

Honest about what today's code does *not* do:

- **Danger warnings are opt-in.** The default modes (shadow, suggest) never
  show L2+ warnings — the policy verdict is recorded as shadow data instead.
  Visible warnings/confirmations require `mode = warn` or `confirm`, and no
  rule category is enabled that way by default until the M5 pilot supplies
  evidence (SPEC §8 graduation). The model layer (L4) is unbuilt.
- **Only the first line of a multi-line command is analyzed.**
  Continuation lines typed at the `PS2` prompt pass through untouched.
- **Linux and interactive zsh only.** The `/dev/fd/3` mechanism works on the
  BSDs too, but nothing else is tested there, and there is no bash adapter.
- **The candidate scan is bounded, not exhaustive.** Directory entries and
  permission checks are capped, so on a pathological PATH the layer may miss
  a candidate. It fails toward silence, which is the safe direction.

## 9. Where things stand

See [PLAN.md](PLAN.md) — M0 (skeleton), M1 (zsh capture + shadow
passthrough), and M2 (lexer + typo layer, the first user-visible value) are
complete. M3 is underway; landed 2026-08-06, documented so far only here and
in their module headers:

- **`src/layers/danger.rs`** (L2) — curated rule tables recognizing
  high-consequence command shapes (recursive/forced deletes with target
  classification, git history rewrites, block-device writes, service stops,
  package removal, sudo escalation). Emits stable evidence codes plus a
  direct-catastrophic flag (recursive delete of `/` or home); never
  intervenes on its own. It also hands the literal targets of fired rules to
  L3.
- **`src/layers/context.rs`** (L3, deterministic half) — fresh facts
  collected only when L2 marked a candidate: git branch/detached/dirty/
  untracked (dirty counts via `git status` as a bounded external helper —
  absolute path, fixed argv, hard timeout), and per-target stats (exists,
  symlink, capped entry count, canonicalized cwd/parent detection, near-miss
  siblings). Unavailable evidence is reported as unavailable, never guessed.
- **`src/distance.rs`** — the bounded edit distance, moved out of the typo
  layer so context's near-miss check shares one implementation.
- **`src/policy.rs`** — the decision engine: `warranted` (the mode-blind
  evidence → decision matrix pinned by `eval/golden/policy.json`),
  `cap_for_mode` (the mode is a ceiling; downgrades preserve the policy
  reason, which is what makes shadow data reportable), the intervention
  budget and per-rule cooldown (built and tested; consumed once the warning
  UI can show something), and the full SPEC §15 config surface with
  warn-once diagnostics. The watchdog deadline now comes from
  `det_timeout_ms`.

- **The L2+ warning UI** (in `src/ui.rs` and `src/main.rs`) — the SPEC §7
  warning anatomy on /dev/tty with e/edit (exit 11, exact buffer restored to
  ZLE), c/cancel (exit 12, nothing runs), r/run-once. Warnings are advisory
  (timeout runs the command); confirmations pause (timeout cancels).
  Outcomes are recorded in the event log and feed the per-rule cooldown; the
  intervention budget is spent only when a prompt is actually shown. The
  prompt key reader now consumes complete escape sequences, fixing the
  stray-bytes-after-arrow-keys bug (bughunt 2026-08-06).

Still ahead in M3: the recency relation (plugin-supplied). Files SPEC §16
lists for later milestones (`model.rs`, `layers/infer.rs`) don't exist yet
by design — modules are created when their milestone starts.
