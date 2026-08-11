# oopsinput — Architecture and developer guide

This document explains the implementation as it exists today, ground-up, so a
new developer can build it, test it, and change it with confidence.

How it relates to the other documents:

- **[SPEC.md](SPEC.md)** is canonical for *design*: scope, principles, security
  invariants, interfaces, and the full four-layer vision. When this document
  and SPEC disagree, SPEC wins.
- **[PLAN.md](PLAN.md)** tracks *progress*: which milestones are done and what
  each one covered.
- **[SECURITY.md](SECURITY.md)** states the threat boundary, residual risks,
  and private vulnerability-reporting channel.
- **This document** covers *how the code works right now*. That is the whole
  deterministic product: command capture in zsh, the lexer, all three
  deterministic analysis layers (typo, danger, context), the policy engine,
  both visible prompts, event logging and reporting, and the test harness that
  proves your buffer survives intact. The optional local-model layer (L4) is
  fully wired: with a model configured (default: none — deterministic-only),
  `check` consults loopback Ollama on the rare ambiguous danger candidate,
  measured at 0.27% of replayed natural commands.

## 1. The pieces

oopsinput is three things working together:

1. **A zsh plugin** (`zsh/oopsinput.zsh`) — a small script sourced by your
   `~/.zshrc`. It hooks the moment you press Enter, hands the typed command to
   the binary, and interprets the binary's answer. It contains no analysis
   logic — with one deliberate exception, the recency summary described in
   §3, which exists precisely so that raw history text never leaves the shell.
2. **A Rust binary** (`src/`, built to a single executable named `oopsinput`)
   — spawned fresh for every command. It reads the command, analyzes it,
   decides what to do, writes one line to an event log, and exits. When it
   decides to intervene it talks to your terminal directly (see §4.9) rather
   than through the plugin. There is no daemon, no background process, no
   state held in memory between commands.
3. **Guided install/uninstall scripts** (`zsh/install.zsh`,
   `zsh/uninstall.zsh`) — copy the binary into `~/.local/bin` and the plugin
   plus a stable uninstaller into `~/.local/share/oopsinput`, then add/remove
   one clearly marked block in `~/.zshrc`. A fresh install has no default mode:
   it requires an unfocused terminal choice, or an explicit public `--mode`
   argument for automation, before creating its config. The shell edit is
   backed up byte-for-byte (including a missing final newline), and the
   original backup is retained across updates and uninstall. The marker block
   is the receipt that authorizes later updates and removal of the three
   installed runtime files. Every new output and old rollback copy is staged
   before commit; a handled fresh failure removes only newly created files,
   while a failed update restores the complete previous runtime set.

The trust and failure model in one sentence: the plugin treats the binary as
something that can crash, hang, or be missing at any moment, and in every one
of those cases the user's original command runs unchanged ("fail open").

## 2. From a fresh clone

Prerequisites:

- **Rust 1.89 or newer** via [rustup](https://rustup.rs) (user-level install,
  no root). The minimum is where standard-library cross-process file locking
  stabilized. If
  `cargo` isn't found in a fresh shell:

```
. "$HOME/.cargo/env"
```

- **zsh** — the target shell, and what the PTY tests drive.
- **`script`** from util-linux — the PTY tests use it to run a real
  interactive zsh inside a pseudo-terminal. Preinstalled on essentially every
  Linux distribution.
- **git** — not just for source control: the context layer runs `git status`
  as a bounded helper, and several tests build throwaway repositories.

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

The clean-machine lifecycle gate — install the real release artifacts under a
temporary isolated home, exercise `doctor`, Shadow recording, `report`,
`purge`, and uninstall, then enforce the exact ownership residue:

```
scripts/lifecycle-gate.zsh
```

The volume acceptance gate — thousands of scripted submissions through a real
interactive zsh, verifying that every command's output appears and nothing
hangs:

```
scripts/pty-gate.zsh
```

(Default 10,000 submissions; pass a number for a quicker run, e.g.
`scripts/pty-gate.zsh 500`. It also enforces a coarse per-submission time
ceiling, which is the only check covering plugin-side cost.)

The performance gate — the SPEC §10 latency budgets, actually enforced
against a release build:

```
scripts/perf-gate.zsh
```

These three runtime gates run separately from `cargo test`: they need a release
build and real process spawns, and keeping them separate keeps the test suite
fast. Run all three before release; run both timing gates before claiming a
performance number.

The official artifact builder uses the pinned Rust 1.89.0 toolchain and its
musl target to create the versioned static archive plus `SHA256SUMS`. The
archive gate rejects extra or missing members, wrong types or modes, a version
mismatch, dynamic linkage, or a failed checksum, then passes the extracted
files—not repository-private source overrides—to both lifecycle gates:

```
rustup target add --toolchain 1.89.0 x86_64-unknown-linux-musl
scripts/build-release-bundle.zsh
scripts/release-bundle-gate.zsh dist/oopsinput-0.1.0-x86_64-unknown-linux-musl.tar.gz
```

The tag-triggered `.github/workflows/release.yml` performs those same steps,
generates GitHub build provenance for the archive, and publishes the archive
and checksum receipt only after every gate succeeds.

To install a source build on your own machine:

```
zsh zsh/install.zsh
```

On a fresh install, the script explains Shadow, Suggest, Warn, and Confirm;
nothing begins focused, and a direct digit or Tab followed by Enter is required.
For non-interactive use, `--mode shadow` (or one of the other three exact mode
names) is mandatory. The release binary lands at `~/.local/bin/oopsinput`; the
plugin and stable uninstaller land at `~/.local/share/oopsinput/`, and the
marked `.zshrc` block sources only the installed plugin. Moving or deleting the
checkout therefore does not break a new shell or removal path. Rerunning the
installer atomically updates the complete three-file runtime set, while
preserving both the original pre-install shell backup and every existing config
byte-for-byte.

Before editing, the installer requires one exact, standalone marker boundary
(marker text joined to a user line is damaged, never an ownership receipt) and
refuses symbolic-link or non-regular destinations. On a fresh install it also
refuses to overwrite same-named regular files: only an existing healthy marker
block authorizes update behavior.

To remove without the checkout or archive:

```
zsh "$HOME/.local/share/oopsinput/uninstall.zsh"
```

The uninstaller uses the same healthy marker block as its ownership receipt.
It removes that block, the binary, plugin, and its own installed copy; restores
whether the preceding shell line originally lacked a final newline; and
preserves any unrecognized file in the plugin directory. Configuration, the
original shell backup, and recorded state remain; run the installed binary's
`purge` command first when recorded state should be removed.

## 3. The zsh side, ground-up

### What ZLE and widgets are

When zsh is interactive, the line you're typing lives in the **Zsh Line
Editor (ZLE)**. Every keypress runs a **widget** — a named editing function.
Typing `a` runs the `self-insert` widget; pressing Enter runs the
`accept-line` widget, which submits the buffer for execution. Crucially, zsh
lets you *replace* a widget with your own shell function. That is the entire
command-interception mechanism: no patched zsh and no trap drives analysis,
just widgets swapped for wrappers. A separate `preexec` hook refreshes only
the read-only `doctor` status described below; it never sees the analysis
payload or decides whether a command runs.

Enter is not the only way to submit a buffer. The plugin wraps all four
"accept" widgets (`accept-line`, `accept-line-and-down-history`,
`accept-and-hold`, `accept-and-infer-next-history`), and because widgets are
keymap-independent, the same wrappers cover both Emacs and Vi modes.

A child process cannot inspect the ZLE widgets in its parent shell. After
wrapping, the plugin therefore publishes only a closed status vocabulary:
`OOPSINPUT_PLUGIN_ACTIVE=1` and a comma-separated subset of those four static
widget names in `OOPSINPUT_WRAPPED_WIDGETS`. A load-time value is explicitly
stale: a later plugin could replace the wrappers. The independent `preexec`
hook refreshes the list immediately before an interactively entered
`oopsinput doctor` process and marks that snapshot fresh; `precmd` invalidates
it again at the next prompt. Doctor therefore refuses an old snapshot without
receiving command text or user-defined widget names.

### What the wrapper does (`_oopsinput_handle` in `zsh/oopsinput.zsh`)

On each accepted buffer, the wrapper:

1. **Passes through untouched** — without invoking the binary at all — if any
   of these hold: we're already inside a wrapped call (recursion guard via a
   dynamically-scoped `_OOPSINPUT_ACTIVE` variable), the buffer is empty or
   whitespace-only, or this is a continuation buffer (`$CONTEXT != start` —
   e.g. more text for an unclosed quote, typed at the `PS2` prompt). The
   initial ZLE buffer is analyzed in full, including embedded newlines from a
   paste; only later PS2 submissions bypass analysis.
2. **Resolves the command word.** Only the live shell knows the user's
   aliases and functions, so the plugin asks `whence -w` what the first word
   is (`alias`, `function`, `builtin`, `command`, `hashed`, `reserved`, or
   `none`) and passes that single token to the binary as `--res <kind>`. The
   vocabulary is enforced with a `case` whitelist *in the plugin*: anything
   unexpected collapses to `unknown`, so raw user text can never ride into
   argv. (Argv matters because `/proc/<pid>/cmdline` is world-readable —
   which is also why the buffer itself travels over stdin, never argv.)
3. **Summarizes recent commands** — the *recency relation* (SPEC §5-L3), the
   evidence that answers "did you just reference this thing?". This is
   computed in the shell rather than in the binary, and that placement is the
   security design: the binary never receives history text at all. Per
   remembered command (the last five) the plugin sends only three things: how
   many commands back it was, one bit for "shares a word with what you just
   typed", and its first two words — each constrained to
   `[A-Za-z0-9_-]{1,32}`, with anything else (quoted strings, URLs,
   `--flag=value` pairs, paths with slashes) replaced by a single `_`. A
   password or token cannot survive that shape, so there is nothing to strip.

   History is read by direct event number (`$history[$((HISTCMD - age))]`).
   Sorting all history keys instead cost about 7.5 ms per command on a
   10,000-entry history — most of the entire latency budget — and was
   replaced with these lookups at about 0.1 ms.
4. **Builds the payload and invokes the binary.** The payload has three
   sections separated by NUL bytes (a safe separator, because zsh strings can
   never contain one): the buffer's exact bytes; the candidate name pool; and
   the recency summaries. The middle section is filled *only* when the
   resolution kind is `none` — the case the typo layer handles — and then it
   carries every command name the live shell can see that the binary cannot:
   alias, function, builtin, and reserved-word names. Only that
   already-failing path pays the cost of collecting them.

   The invocation routes three streams: stdout (the decision JSON) and
   stderr are discarded, while **file descriptor 3 is captured** into a shell
   variable. Descriptor 3 is the channel the binary uses to hand back a
   corrected command (see §5.2); nothing user-facing travels on it, because
   prompts go straight to the terminal instead.
5. **Interprets the exit code** and nothing else:

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
the session. That diagnostic maps bidirectional and invisible Unicode format
characters to explicit code points, then renders control characters visibly
with zsh's `(V)` flag (`^[`). The byte-based mapping works under both UTF-8 and
`LC_ALL=C`, so a hostile `OOPSINPUT_BIN` value cannot smuggle terminal controls
or misleading text direction to the terminal. (`(qqqq)` quoting is *not*
sufficient for this; it leaves control bytes raw.)

### Three zsh traps, regression-locked

All were real bugs, now pinned by tests:

- `${${(z)BUFFER}[1]}` *string*-indexes (first character, not first word) when
  the split yields a single word — the plugin uses an explicit array
  assignment instead.
- Nested `${$(whence -w ...)##*: }` doesn't strip as expected — extraction is
  done in two steps, then whitelisted.
- Array subscript search comes in two directions, and the reverse form
  `(Ie)` returns `0` when the element is *absent*, while the forward form
  `(ie)` returns "length + 1". A containment test written with `(Ie)` is
  therefore always true. That mistake made every recency entry claim it
  shared a word with the current command until it was found by dumping the
  real payloads through a fake binary.

## 4. The Rust side, ground-up

The Rust modules run analysis strictly cheapest-first, and each layer can be
read on its own:

### 4.1 `src/main.rs` + `src/doctor.rs` — dispatch, checking, and diagnosis

Hand-rolled subcommand dispatch (no CLI-parsing dependency; SPEC §12):
`version`, `check`, `report`, `purge`, `doctor`, `help`.

`check` is the command the plugin runs. It reads the config file first (a
small, capped, local read), then immediately calls `arm_watchdog()`: a thread
that sleeps for the deadline — `det_timeout_ms`, 150 ms by default — and then
force-exits the whole process with code 1. If analysis ever wedges, the
process dies, the plugin sees a nonzero exit, and fails open — the user's
prompt cannot be held hostage. A blunt `exit` is safe precisely because the
process is per-command: there's nothing to clean up that matters more than
the user's prompt. (SPEC §6 records the one honest residual: a process stuck
in uninterruptible disk I/O can't even do that; documented, not defended.)

The watchdog grants one bounded **extension** when a model consultation is
in flight (SPEC §6: the model path gets its own longer deadline): before
the first socket call, the check path stores `model_timeout_ms` plus margin
into an atomic the watchdog reads at the deterministic deadline. The
consultation's own socket deadline is strictly shorter, so the extension is
a backstop, not the bound.

The watchdog **retires** once a prompt is on screen (a flag named
`PROMPT_ACTIVE`): a question waiting on a human legitimately outlives an
analysis deadline, and killing the process mid-prompt would leave the
terminal in the wrong mode. What makes retiring safe is that everything past
that point is bounded by construction — the prompt's own read has a timeout
enforced by the terminal, and every external helper it runs is killed at a
deadline (§4.12).

The analysis sequence, in order: read the proposal, lex it, run the typo
layer (L1), run the danger layer (L2), and — only if danger found something —
collect context facts (L3). Then assemble the evidence codes, ask policy for
a decision, and possibly prompt.

**Which prompt wins.** A buffer can qualify for both prompts at once:
`gti status; git reset --hard` has an unresolvable first word *and* a
dangerous second segment. A semicolon does not short-circuit, so the reset
runs whichever way the typo question is answered — meaning the typo prompt
would have been actively misleading. The stronger intervention therefore
takes precedence: a warn or confirm verdict is shown, and the typo prompt is
only reached when policy has nothing to say.

One ordering detail matters for measurement: the duration recorded in the
event is captured *before* any prompt, so latency percentiles measure
analysis, not how long a human took to answer.

Two test hooks — `OOPSINPUT_TEST_DEADLINE_MS` (shorten the deadline) and
`OOPSINPUT_TEST_HANG` (sleep 30 s inside `check`) — exist **only in debug
builds** (`#[cfg(debug_assertions)]`, meaning the code is compiled out of
release binaries entirely). The PTY suite uses them to prove the watchdog
works end-to-end; a release binary has a fixed deadline and no hang hook.

`src/doctor.rs` owns the read-only `doctor` setup diagnosis. It checks the
version and whether a real executable `zsh` is on PATH (via direct
file-metadata lookup — never by asking a shell, per SPEC §9); the unique
marked block in regular `~/.zshrc` and the regular installed plugin file; all
four accept-widget wrappers from a snapshot refreshed immediately before this
doctor process (a stale load-time snapshot is a problem); the config file,
every parser issue, any
`OOPSINPUT_MODE` override, and the effective mode; the optional Ollama peer
and configured model; and exact `0700`/`0600` modes on the state directory and
every owned state file. A missing state directory is healthy because the
first write creates it. The command never creates, repairs, or rewrites
anything: it prints `result: ready` and exits zero only when every required
check passes, otherwise it prints `result: problems found` and exits one.

The model check is a POST to `/api/show` through §4.10's client; it loads
nothing and runs no inference. The config line and the mode line resolve
through the same inspection, so they cannot contradict each other (they once
did — a bug found by review and now pinned by `tests/doctor.rs`). Untrusted
paths, config diagnostics, model names, and state paths are escaped before
display.

### 4.2 `src/proposal.rs` — input parsing

A `Proposal` is what arrived on stdin plus the resolution kind from argv.

The stdin payload is the three NUL-separated sections described in §3.
Parsing is deliberately defensive, because this is attacker-adjacent input in
the sense that matters (a corrupted or hostile payload must degrade, never
mislead): the whole read is capped at 1 MB; oversized input is truncated at a
UTF-8 character boundary and flagged `capped` so it still gets analyzed
rather than erroring out; the name list is count-capped; a single name that
isn't valid UTF-8 is skipped rather than failing the whole check; and if the
size cap lands inside a later section, only the possibly-truncated final
entry is dropped while the buffer itself stays intact.

Recency entries are **re-sanitized here**, against the same
`[A-Za-z0-9_-]{1,32}` rule the plugin applies. That duplication is
deliberate: the binary does not trust its adapter, so even a compromised or
out-of-date plugin cannot push arbitrary text into a prompt or a log. A line
whose numeric fields are malformed is dropped rather than guessed at.

`ResolutionKind` is a closed enum; `parse` maps the eight known tokens and
collapses *anything* else to `Other`, so even if the plugin-side whitelist
failed, raw text still couldn't become a stored value. Unknown `check` flags
are ignored — forward compatibility for the future agent-adapter seam
(SPEC §6).

That seam is only a foundation today: `check` writes a structured JSON
decision, but its request is still this Zsh-specific stdin/flag format. There
is no JSON request, serialized origin, task goal, or provenance yet, and
non-interactive coding-agent subprocesses bypass ZLE entirely. Agent support
therefore requires a real request contract plus an adapter and approval path;
it is not activated by installing the current plugin.

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
skipping `VAR=value` assignments and redirect targets. The uncertainty codes
flow into `check`'s decision evidence and the shadow event log, so shadow
data can measure the unsupported-syntax rate (SPEC §11).

At top level, an embedded newline is a control operator like `;`, not ordinary
spacing. That matters when a paste or prefilled ZLE buffer contains multiple
commands: every segment is analyzed before any of them run. Newlines inside
quotes and substitutions remain part of their owning word. A heredoc makes
the remainder opaque at its first body newline, so command-looking data fed to
stdin is never mistaken for another executable segment.

One subtlety worth knowing before you touch this file: **word separators are
space, tab, and newline only** (`is_shell_whitespace`), deliberately *not*
Rust's general "is this whitespace" test. Unicode blanks such as the
no-break space are ordinary word characters to the shell, and this lexer
must split words the way zsh will. When it didn't, a pasted no-break space
made the lexer analyze `gti` while the shell had actually resolved
`<no-break space>gti`, so accepting the correction produced another
command-not-found. Any code that decides where a word starts or ends must
use this same function.

### 4.4 `src/events.rs` + `src/state.rs` — logs, report, retention, purge

Appends one JSON line per command to `events.jsonl` in the state directory
(resolution order: `$OOPSINPUT_STATE_DIR` override — used by tests — then
`$XDG_STATE_HOME/oopsinput`, then `~/.local/state/oopsinput`). Every resolved
path must be absolute. A nonempty relative explicit override disables state; a
relative XDG root falls back to an absolute HOME; a relative, unset, or empty
HOME leaves state unavailable. No form can claim the current directory.

The invariants, all test-pinned:

- **Structural fields only.** Timestamp, decision, reason code, evidence
  codes, resolution kind, buffer byte count, word count, duration, plus
  optional context *counts* (dirty file count, largest directory entry
  count), model state immediately before L4 inference (`warm`, `cold`, or
  `unknown`), the mode-blind policy reason when Shadow/Suggest suppressed a
  Warn/Confirm, and the user's outcome at a visible warning (`edited`,
  `cancelled`, `ran_unchanged`). The `Event` struct has no field that *could*
  carry command text — the type system is the redaction.
- **User-only permissions**: directory `0700`, file `0600`.
- **One locked, buffered append per event.** The line and its trailing newline
  are joined before writing. Every event and policy writer takes the same
  stable cross-process lock, so two shells cannot interleave a line or race a
  retention rewrite (pinned by 16-thread event and 8-thread policy hammers).
- **Never written through a symlink.** Existing paths are rejected unless they
  are regular before open and still the same nonsymlink inode immediately
  afterwards. A missing lock or log uses atomic `create_new`, never a combined
  create-and-follow open, so a dangling symlink raced into the path cannot
  create its target.
- **Failures are swallowed.** Logging must never cost the user their command
  or their prompt.

`oopsinput report` streams that JSONL file and summarizes the data without
recomputing policy. It reports decision and intervention rates, ranked evidence
codes and hypothetical policy reasons, visible-warning outcomes, and nearest-
rank p50/p95/p99 analysis latency. New events persist the pre-mode hypothetical
reason explicitly; intrinsic Observe outcomes such as unavailable evidence or
an ungraduated candidate therefore cannot be mistaken for interventions. For
legacy M5 lines, the report recognizes only the closed set of reasons that
actually represented Warn/Confirm at that time. Any `model.*` evidence code
keeps that event out of the deterministic latency bucket; the event's model
state then chooses warm, cold, or unknown. That last bucket is important for
old logs and failed status queries: absent metadata must not quietly become
either a deterministic or a cold measurement. A torn or malformed line is
counted and skipped, while valid neighboring events remain usable. One record
is capped at 64 KiB and an oversized record is drained through its newline, so
it cannot allocate without bound or hide a valid following event. Codes read
from disk pass through the display escaper before reaching the terminal.

`state.rs` owns the shared mechanics. On each analysis-time append it reads a
tiny marker; at most once per 24 hours per log, it streams valid records at or
inside the 30-day window to a private `0600` temporary file and atomically
renames it over the old log. Expired and malformed/torn records are dropped.
That same 64 KiB per-record cap applies during the sweep.
The stable lock anchor matters: locking the log itself would stop protecting
writers the moment atomic rename created a new inode. Readers need no lock
because they see one complete inode or the other. A sweep can lag the exact
cutoff by less than a day, and post-prompt-only or no writes mean no sweep.

Analysis-time state writes wait for that lock under the existing process
watchdog, so normal concurrent shells lose no records without adding an
unbounded hold on the command. Once prompt setup begins, each remaining state
write waits no more than 25 ms and only appends; if contention outlives the
bound, the record is omitted, and a due retention sweep waits for the next
analysis-time write. Neither a busy lock nor a large log can therefore delay
the user's edit/cancel/run choice. Explicit `purge` is different—it blocks for
the lock because deletion is the only effect the user asked that command to
perform.

`oopsinput purge` takes that same lock and removes only oopsinput's named data,
markers, abandoned temp files, and the lock itself; configuration is outside
the state directory and stays. It refuses to enter a symlinked state directory
or recursively delete a directory found at a file name. Known symlink entries
are unlinked without following them—including a corrupted lock anchor—and a
regular lock anchor's private permissions are restored before it is acquired.
Other non-regular lock objects are refused. Unknown entries stay, and keep the
state directory from being removed. In the normal, uncorrupted-anchor path, a
waiting writer compares the inode it locked with the current anchor and retries
if purge replaced it, so concurrent shells cannot split into two
unsynchronized groups.

The same module supplies `doctor` with a strictly read-only inspection. It
accepts an absent state directory, but when state exists it requires the real
directory to be `0700` and every recognized lock, log, marker, key, policy, or
temporary state file to be a regular `0600` file. Unknown entries are ignored
because oopsinput does not claim them. Inspection reports damage but never
creates state or repairs permissions.

### 4.5 `src/layers/typo.rs` — L1, the typo layer

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

Matching uses the shared bounded edit distance (§4.12). Words of four
characters or fewer allow one edit; longer words allow two. (`gti` → `git`
is a swap of two adjacent characters, so it costs one.) Candidates come from
two places: the names the plugin supplied, and the executables the layer
finds by reading each PATH directory itself — never by asking a shell, and
only counting entries that are real files with an executable bit. Hard caps
bound the work: entries per directory, total permission checks, and
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

### 4.6 `src/layers/danger.rs` — L2, the danger layer

Recognizes high-consequence command *shapes* from curated rule tables —
plain Rust data and `match` arms, never a scripting language, never
execution. It reads the lexer's tokens, splits them into simple-command
segments at every control operator, and applies one rule per recognized
command name:

- **filesystem** — recursive or forced `rm` (with the target classified as
  `/`, the current directory, its parent, the home directory, or a block
  device), recursive `chmod`/`chown`, forced `cp`/`mv`, truncating
  redirections, writes to block devices by name shape.
- **git** — `reset --hard`, `clean -f`, `push --force`, remote and local
  branch deletion, `filter-branch`. Plain `git rebase` is deliberately
  absent: it is routine and intentional, and flagging it would spend the
  intervention budget on noise.
- **system** — `dd of=`, `mkfs*`, `kill -9 -1`, broad `pkill`, service
  stops, package removal.
- **privilege** — `sudo`/`doas` adds `priv.sudo`, but *only* when the
  wrapped command itself tripped a rule, so `sudo apt update` stays
  evidence-free.

The layer never intervenes on its own. It produces three things: stable
evidence codes, a **direct-catastrophic** flag (a recursive delete aimed at
`/` or your home directory), and the literal target words of rules that
fired, which L3 then inspects on disk.

Three honesty rules run through the whole file, and each exists because
violating it produced a wrong answer:

- **A word the shell would expand is unknowable**, so it never matches a
  table — with the curated exception of `$HOME`/`${HOME}`, whose meaning is
  exactly the point. When an `rm` operand is unknowable, the layer says so
  with its own evidence code, because otherwise a *different* rule's target
  could vouch for it downstream.
- **Quoting is the shell's business.** `'rm' -rf /` runs rm, so quoted text
  still matches; `'~'` is a literal file named `~`, so it doesn't.
- **A flag cluster only counts if every letter in it is real.** To `rm`,
  `-force` is not "recursive and force" — it is `invalid option -- 'o'`, and
  the command never runs. Reading it as flags produced a full catastrophic
  confirmation for a command that could not do anything, so each tool now
  carries its own short-flag alphabet.

### 4.7 `src/layers/context.rs` — L3, the context layer (deterministic half)

The differentiator: the facts that separate "dangerous and intended" from
"probably not what you meant". It runs **only when L2 marked a candidate**,
so the common path spends no syscalls here at all.

**Git facts.** It finds the repository by walking up to a `.git` (handling
both a directory and the one-line `gitdir:` file used by worktrees), reads
the branch and detached state straight out of `HEAD`, and gets the dirty
file count and untracked presence by running `git status --porcelain` as a
bounded helper. If git is missing or overruns, the facts are reported as
*unavailable* rather than guessed — and policy treats unavailable evidence
as a reason to stay quiet, never as "clean".

That helper invocation carries a security lesson worth understanding before
you touch it. Git executes programs named in its own configuration, and the
repository's `.git/config` is not necessarily yours: a directory extracted
from an archive, or a fixture repository committed inside another project,
brings its own. Because oopsinput runs `git status` wherever you happen to
be standing, typing `rm -rf ./build` in such a directory was enough to
execute a stranger's program — analysis causing execution, which SPEC §9-1
forbids outright. Every configuration key that can spawn something is now
neutralized on the command line, where `-c` outranks repository config. The
regression test arms the trap first (asserting that a plain `git status`
*does* fire it) so it can never silently stop proving anything.

**Target facts.** For each target the danger layer handed over: does it
exist, is it a directory, is it a symlink, roughly how many entries does it
hold (capped), does it resolve to the current directory or its parent
(catching `rm -rf ../myproject` typed from inside it), and — when it does
*not* exist — is there a similarly-named neighbor, the "near-miss" signal
that catches `./buidl` beside `./build`.

Every collector is hard-capped: bounded upward walks, bounded directory
reads, bounded child runtime. A pathological environment degrades to honest
"unavailable", never a hang.

### 4.8 `src/policy.rs` — decisions, modes, budgets, configuration

Three deliberately separate pieces:

**`warranted`** is the mode-blind decision matrix: given the evidence, what
does this *deserve*? Catastrophic deletes ask for confirmation, always.
`git reset --hard` warns only for tracked/staged changes, while `git clean -f`
warns only for untracked files; evidence about the file class that command
cannot delete does not justify an intervention. Each is silently allowed when
its own affected class is clear. `push --force` warns only on a main-like
branch. A recursive delete warns when the target is the
current directory, its parent, or a near-miss of a real neighbor, and is
allowed when every target plainly exists. Everything else recognized records
as `observe` — recognized but not yet graduated to speaking. Each arm exists
to make a golden counterfactual pair pass; a rule with no context in which
it stays silent does not belong here.

**`cap_for_mode`** applies the mode as a *ceiling, never a floor*. Before that
cap is applied, the event path records the mode-blind Warn/Confirm reason in a
dedicated optional field. That explicit field is the shadow-mode measurement:
it lets the M5 report ask "how often would this have spoken?" without
misclassifying intrinsic Observe reasons or depending on the final reason a
Suggest-mode typo prompt may produce.

The four modes (SPEC §8): **shadow** (analyze and record, never visible),
**suggest** (adds typo prompts), **warn** (adds nonblocking danger warnings),
and **confirm** (danger warnings pause for an answer). Shadow is the config
parser's conservative fallback when mode is missing or invalid; it is not a
fresh-install default, because the installer requires an explicit choice.

**`apply_gates`** is habituation control (SPEC §7): at most three visible
interventions per rolling hour, and a per-rule cooldown — three consecutive
"I meant it, run unchanged" outcomes puts that rule to sleep for a day,
while any edit or cancel wakes it. Direct-catastrophic findings are exempt
from both. Exhausting the budget degrades to silent recording; it never
degrades to nagging.

`apply_gates` itself is a pure decision, but the live admission path surrounds
the history load, that decision, and a reservation append with one shared
cross-process lock. A visible non-catastrophic prompt therefore claims its
budget slot before the lock is released; simultaneous shells cannot all claim
the same last slot. The outcome later completes that reservation, a terminal
setup failure releases it, and an abandoned reservation expires after ten
minutes. The displayed prompt still spends exactly one slot, while a prompt
nobody saw spends none.

The module also owns the full SPEC §15 **config surface**: `mode`, `model`,
`model_timeout_ms`, `det_timeout_ms`, `budget_per_hour`, `log_raw`. A config
over 64 KiB is rejected whole and `doctor` reports it invalid; no valid prefix
is silently accepted. Invalid values fall back to the documented default and
say so; unknown keys are reported **by line number only**, never by echoing the
key — config text is untrusted input and must not reach a terminal. Complaints
are printed once per distinct set, tracked by a fingerprint file in the state
directory. The fingerprint comparison, display, and marker replacement hold
the shared state
lock as one transaction, so simultaneous shells cannot all print the same
complaint. The plugin opts into direct `/dev/tty` delivery only on this warning
path—ordinary commands open no extra terminal descriptor—and the marker is
committed only after the whole diagnostic is written successfully.
`$OOPSINPUT_MODE` overrides the file's mode.

Cooldown and budget state live in `policy.jsonl` (user-only): append-only
reservation, release, and shown-outcome records say when an admission happened,
which rule it belongs to, and what the user did. The reader folds each
reservation/outcome pair into one logical intervention. It takes only a bounded
tail — the budget looks back an hour and a cooldown a day — so a long-lived
file has bounded cost; a torn or partial line simply fails to parse and is
skipped, and the path is never written through a symlink.

**Append and admission semantics are load-bearing, not style choices.** The
first version was a JSON blob whose concurrent writers lost outcomes. The
append-only replacement fixed that but still let concurrent shells pass one
remaining budget slot before any prompt finished. Admissions now append their
reservation inside the same locked history transaction; outcomes still append,
and retention's locked atomic replacement remains the only rewrite. Focused
contention tests fail if either lost appends or over-admission returns.

### 4.9 `src/ui.rs` — prompts, message building, and the display escaper

**Escaping.** `escape_for_display` is what every piece of untrusted text
passes through before it can reach your terminal. Terminals interpret
certain byte sequences as commands rather than text — changing colors,
rewriting the window title, moving the cursor — and some Unicode characters
reverse the display order of text, so a file named to exploit that can
appear as a completely different name than it is. The escaper renders all of
that inert: control characters become visible caret notation (`^[`), and
bidirectional-text and invisible formatting characters become visible
`\u{...}` escapes. A 20,000-case fuzz test asserts that nothing active ever
survives it and that escaping twice changes nothing. It is applied
unconditionally — including to text that a charset check elsewhere has
already restricted, because a rule that holds only while a distant check
stays correct is a rule that breaks silently when that check is edited.

The plugin's missing-binary diagnostic and the install/uninstall scripts run
before or outside the Rust binary, so they use a small Zsh equivalent: Zsh's
visible representation for C0/C1 controls plus an explicit UTF-8 table for the
same bidi and invisible formatting characters. The table uses byte spellings
so it remains valid even under the C locale. Regression fixtures cover hostile
environment-derived paths in all three surfaces.

**Two prompts**, both on `/dev/tty` — the terminal itself — rather than
stdout, which carries the decision JSON and is discarded by the plugin:

- `prompt_typo` shows the complete original and corrected buffers in a bounded,
  escaped `*** oops? ***` block. `y` accepts and `n` keeps the original.
  The compact choice row starts with the original visibly focused; Tab switches
  focus and Enter activates it, so a bare Enter can never consent to a
  correction. Ctrl-C still cancels but is not advertised in the row. Any other
  key, a ten-second timeout, a missing terminal, or an internal failure runs
  the original command, which is the safe outcome since that command could not
  have run anyway. Timeout is still recorded as `typo.timed_out`, not as a
  deliberate `n`, and is named on the terminal before the original runs so it
  cannot look like correction consent.
- `prompt_warning` shows the L2+ warning, whose anatomy is fixed by SPEC §7:
  what the command does, the concrete facts, why the context is unusual,
  then the keys. `e` restores your exact buffer to ZLE for editing, `c`
  cancels and runs nothing, `r` runs the original unchanged once. The
  timeout default depends on the tier: an advisory warning runs the command,
  a pausing confirmation cancels it — running is never the default for a
  command whose consequences are predicted to be irreversible. In either
  tier the recorded outcome is `timed_out`, distinct from the physical default
  and therefore unable to manufacture a deliberate run-unchanged cooldown.
  Any failure to display fails open to running unchanged, but returns a
  distinct `NotShown` result: it is absent from the visible-outcome report and
  never spends the hourly budget or advances a cooldown.

`warning_lines` builds those message lines from evidence codes and context
counts, with every untrusted fragment escaped and every line framed by the
fixed `oopsinput:` prefix. The warning writes a terminal line break and the
typo block begins after a blank line: ZLE has intercepted Enter but has not
delegated the accept widget yet, so without that separation the question would
be joined directly to the submitted
buffer on the same row.

**Reading a keypress** is subtler than it looks: an arrow key is not one
byte but a short escape sequence (`ESC [ A`). Reading a single byte treated
that as an answer and left the remaining bytes behind, where they leaked
into the next command line as stray characters. The reader now consumes
complete sequences (CSI, SS3, alt-chords) and ignores them, while a lone
`ESC` still means "dismiss". Unrecognized keys are ignored rather than
guessed at. Long CSI sequences receive a second bounded drain; an over-cap or
incomplete sequence ends the prompt with its non-consent timeout outcome, so
its final `y` or `r` can never be reinterpreted as a fresh answer. The whole
loop remains bounded so hostile input cannot hold the prompt open.

Switching the terminal into single-keypress mode requires the `stty`
program, because Rust's standard library exposes no terminal-mode control
and the dependency policy allows no crate for it. Two rules govern that
call, both learned from a security review:

- **`stty` is invoked by absolute path**, never resolved through PATH. When
  it was resolved by name, any directory leading PATH could supply the
  program that ran — and since this layer fires on *mistyped* commands, any
  typo at all became the trigger.
- **Every external helper is bounded**: a child that overruns its deadline
  is killed and reaped, and the call reports failure. This runs after the
  watchdog retires, so an unbounded child would hang the shell with nothing
  left to recover it.

The saved terminal settings are restored on every exit path by a guard
value, so no path out of the prompt leaves your terminal in raw mode.

### 4.10 `src/model.rs` — the loopback HTTP client (L4 transport)

The first piece of the inference layer: a self-written HTTP/1.1 GET/POST over
`std::net::TcpStream` (SPEC §12 rejects an HTTP crate for this — loopback
needs no TLS, no redirects, no connection reuse). It refuses any non-loopback
address before opening a socket, and the target is fixed at Ollama's default
`127.0.0.1:11434` — deliberately not configurable, because SPEC §14 allows no
network beyond loopback and an address knob would invite pointing it
elsewhere. Loopback is not blindly trusted either (audit 2026-08-06): any
local account can bind the port while Ollama is down, so after connecting
and before sending a byte, the client reads the peer's uid from
/proc/net/tcp — the established connection's own row, so the listener can't
be swapped — and refuses anything not owned by the user or a system
account. A refusal, like every other model failure, means
deterministic-only.

Its two disciplines mirror the external-helper rules:

- **One hard deadline covers the whole exchange.** Connect, every write, and
  every read recompute the time remaining and use that as the socket timeout.
  The recomputation is the point: a server dripping one byte per poll resets
  a naive per-read timeout forever (probed and pinned by test) but cannot
  stretch a shrinking one.
- **Size caps on everything buffered**: the response head, the decoded body
  (caller-chosen cap), and the still-encoded chunked stream, so a hostile
  peer can't balloon memory with framing overhead.

It speaks enough HTTP to talk to Ollama and nothing more: status line,
`Content-Length` / chunked / connection-close framing. Every failure —
unreachable, timeout, oversized, malformed, non-2xx — is a distinct error the
caller maps to "model evidence unavailable" (SPEC §9-6); nothing in this
module can block a command. `doctor` uses `POST /api/show`; the inference layer
uses `GET /api/ps` for pre-chat model state and `POST /api/chat` for advisory
evidence. Every connection independently verifies the peer before sending.

### 4.11 `src/layers/infer.rs` — L4, the inference layer

The brain above §4.10's transport: prompt assembly, the response schema, and
validation. `check` consults it only when **all** of these hold: a model is
configured (`model =` in config; the default is none, so an ordinary install
never enables model network access), the danger layer marked a candidate that
is *not* direct-catastrophic, and policy's mode-blind verdict came back Observe
with an ambiguity reason — L3 neither cleared the command nor decided against
it (`policy::l4_gate`). Replaying 1,107 natural commands from this
machine's real history put the gate-eligible rate at 0.27%, inside SPEC
§5-L4's <1% target. What the model says is consumed deterministically
(`policy::apply_model_evidence`): exactly two arms, both capped at Warn —
`probable_mismatch` and `adversarial_or_untrusted_instruction` — and no
downgrade arm at all, so a lying model can never clear a command and
Confirm stays reachable only through deterministic rules. Because a
consultation legitimately outlives the 150 ms deterministic deadline, the
check path arms a one-shot watchdog extension (`model_timeout_ms` + margin)
before the first socket call; the probe for that test showed the process
watchdog-killed mid-consultation without it.

Immediately before chat, the product queries Ollama's read-only `/api/ps`
endpoint to record whether the configured model was already loaded. The query
carries no proposal or command text, has a small slice of the existing model
deadline, and the following chat shares that original overall deadline. A
failed or malformed status response yields `unknown` but does not prevent the
chat; a status query can classify performance, never decide whether model
evidence is available.

The prompt keeps computed facts and human text strictly apart (SPEC §5-L4).
The request's user message is one JSON document: everything under
`"evidence"` was computed by the deterministic layers (danger codes,
git/target facts, structural recency — closed vocabularies and numbers, no
free text); every human-originated string — the command buffer, target
words, recency words — sits under a key starting with `untrusted_`. serde
does the serialization, so hostile buffer content cannot escape its JSON
string, and a test walks the whole evidence subtree proving no free text
appears outside `untrusted_` keys. The system prompt tells the model
untrusted text is inert data, and that text instructing the model is itself
evidence (`adversarial_or_untrusted_instruction`).

The response is doubly constrained: Ollama's structured outputs (`format`:
a JSON schema) constrain sampling server-side, and our own validator
re-checks everything on arrival — closed assessment vocabulary, closed
mismatch-kind vocabulary, reason ≤ 240 characters, no unknown fields.
Anything else — including a well-formed answer with one extra key — is
discarded whole as unavailable evidence (SPEC §9-6), recorded under a
stable code (`model.unreachable` / `model.timeout` / `model.invalid` /
`model.error`) so evaluation can tell fallback from success. The reason
text stays untrusted and passes through the display escaper if it is ever
shown. Verified live against a local Ollama: schema-valid structured output
end-to-end through §4.10's client.

### 4.12 The two shared helpers

- **`src/distance.rs`** — the bounded **optimal string alignment** distance:
  insertion, deletion, substitution, and swapping two adjacent characters
  each cost one edit, and the computation abandons early once the distance
  provably exceeds the budget. Shared by the typo layer's candidate search
  and the context layer's near-miss check, so "how close is close?" has one
  answer.
- **`src/proc.rs`** — the single wait-or-kill loop every bounded helper uses.
  Having one copy is the point: "no path outlives the deadline" is a claim
  that should be provable by reading one function.

### 4.13 Dependencies

Exactly two: `serde` and `serde_json` (JSON is a correctness/security surface
with real spec depth). Everything else — CLI dispatch, PATH lookup, the edit
distance, the lexer, config parsing, and the loopback HTTP client — is
self-written per the policy in SPEC §12. Adding a dependency requires
updating SPEC §12 first. `deny.toml` independently pins every resolved
transitive crate and the four licenses currently present; the checksum-pinned
`.github/workflows/dependency-policy.yml` runs cargo-deny on every push and
pull request and weekly, so a newly published advisory is found even when the
repository is idle.

## 5. Three Enters, end to end

### 5.1 The common path: an ordinary command

You type `git status` and press Enter:

1. ZLE runs the wrapped `accept-line`, which calls `_oopsinput_handle` in
   `zsh/oopsinput.zsh`.
2. The buffer is non-empty, not a continuation line, not recursive — so the
   plugin resolves `git` via `whence -w` → `command`. Because that is not
   `none`, no candidate names are collected; five recency summaries are.
3. It pipes the payload to `~/.local/bin/oopsinput check --res command`,
   capturing descriptor 3.
4. The binary reads config, arms the watchdog (`src/main.rs`), reads the
   payload into a `Proposal` (`src/proposal.rs`), lexes it, skips the
   typo layer immediately (the resolution kind is not `none`), and runs the
   danger layer (`src/layers/danger.rs`), which finds nothing. Because
   there is no candidate, the context layer never runs. Decision: `allow` /
   `shadow.observed`.
5. It appends one structural line to `~/.local/state/oopsinput/events.jsonl`
   (`events::append` through `state::append_jsonl`), prints the decision JSON on stdout (discarded by the
   plugin), and exits 0.
6. The plugin sees exit 0, restores `BUFFER=$original`, and delegates to the
   real `accept-line`. zsh executes `git status` exactly as typed.

Measured on the dev machine, release build, including process spawn:
**p50 6.78 ms, p95 7.80 ms** — against a 25 ms p95 budget (SPEC §10). If steps
3–5 fail *in any way* — binary missing, crash, watchdog fired, weird exit
code — step 6 still happens identically.

### 5.2 The typo path: a command that doesn't resolve

You type `gti status` and press Enter, in suggest mode:

1–2. As above, except `whence -w` reports `none`.

3. Because the kind is `none`, the plugin fills the candidate section with
   every alias, function, builtin, and reserved-word name.
4. The binary runs the typo layer (`src/layers/typo.rs`). The word `gti`
   is the first token, literal, and 3 characters, so it qualifies. Scanning
   the supplied names and PATH finds `git` at distance 1. No exact match for
   `gti` exists, so nothing suppresses the result. Evidence:
   `typo.candidate_d1`. The danger layer finds nothing, so policy has nothing
   stronger to say and the typo prompt proceeds.
5. The binary builds the corrected buffer *first*
   (`src/layers/typo.rs`) — `git status`, with every byte after the
   command word preserved. Only if that succeeds does it mark the prompt
   active, retiring the watchdog, and show on `/dev/tty`:

   ```text
   *** oops? ***
   You typed 'gti status'.
   Did you mean 'git status'?
   [y] run correction  [n] run original
   ```

   The original choice begins highlighted. Tab switches the highlight and
   Enter activates it; `y` and `n` remain immediate shortcuts.
6. You press `y`. The binary writes `git status` plus one NUL byte to
   descriptor 3 (`src/main.rs`), records `replace` / `typo.accepted`,
   and exits **10**.
7. The plugin sees exit 10, confirms the trailing NUL, strips it, sets
   `BUFFER` to the corrected text, and delegates. zsh runs `git status`.

Pressing `n` produces exit 0 and your original `gti status` runs and fails
naturally; Ctrl-C produces exit 12 and nothing runs. Measured on this path
(release, including spawn, with a 2,000-name pool and a full PATH scan):
**p50 16.3 ms, p95 19.5 ms** against a 75 ms p95 budget.

### 5.3 The warning path: the flagship pair

This is the behavior the product exists for, and it is a *pair* — the same
command, two contexts, two different answers. Both halves are proven by PTY
tests. In `warn` mode you type `git reset --hard`:

**Half one — 17 modified files.**

1–3. As the common path; `git` resolves, so no candidate names.
4. The danger layer matches the `reset --hard` rule and emits
   `git.reset_hard`. Because there is now a candidate, the context layer runs
   (`src/layers/context.rs`): it finds the repository, reads `HEAD`, and
   runs the hardened `git status`, which reports 17 dirty files.
5. Policy (`src/policy.rs`) sees a work-loss command with work to lose and
   returns `warn` / `policy.dirty_work_at_risk`. The mode is `warn`, so the
   ceiling doesn't lower it. Under the shared state lock, the gates pass
   (budget available, rule not in cooldown) and atomically reserve one budget
   slot before another shell can inspect the same history.
6. `warning_intervention` (`src/main.rs`) marks the prompt active and
   displays:

   ```
   oopsinput: git reset --hard will discard uncommitted changes in tracked files
   oopsinput: right now: 17 modified tracked files
   oopsinput: previous command: git diff
   oopsinput: [e]dit  [c]ancel  [r]un unchanged
   ```

7. You press `c`. The binary appends outcome `cancelled`, which completes the
   reservation without creating a cooldown, and exits **12**. The plugin clears the buffer.
   Nothing runs; your 17 files are untouched. (`e` would exit 11 and hand the
   exact command back to ZLE for editing; `r` would exit 0 and run it.)

**Half two — a clean tree.** Steps 1–4 are identical, but `git status`
reports nothing dirty. Policy returns `allow` / `policy.context_clear`, no
prompt is shown at all, and the command runs in silence. The event log still
records the reason, which is what makes the decision auditable later.

Measured on the candidate path (release, including both our spawn and git's):
**p50 17.03 ms, p95 19.79 ms**, against the same 75 ms p95 budget.

## 6. How it's tested

The testing philosophy: **buffer exactness and fail-open behavior are the
product**, so the highest-value tests drive a real interactive zsh, not mocks.
312 automated tests today (311 passing by default plus one ignored live-model
harness) across unit and nine integration suites, plus five standalone gates.

- **Unit tests** live inside each `src/` module (`#[cfg(test)] mod tests`):
  the closed resolution vocabulary, payload parsing edge cases (including the
  recency section's re-sanitization and cap handling), concurrent log
  appends, structural-only serialization, the danger layer's rule tables and
  refusals, the context layer's git and target facts, the policy matrix,
  budget and cooldown behavior, config validation, the display escaper
  (including its fuzz test), both prompts' key protocols against a scripted
  fake terminal, and the bounded external-helper runner.
- **`tests/pty.rs`** — the PTY integration suite. Each test builds an isolated
  throwaway home, copies the current plugin to its installed location, and
  writes a real marked `.zshrc` block that loads it against the freshly-built
  debug binary. It then runs a genuine
  interactive zsh inside a pseudo-terminal via util-linux
  `script -qec "zsh -i" /dev/null`, feeds it keystrokes, and asserts on what
  the terminal displayed. Covered: ordinary passthrough, unicode and quoting
  survival, PS2 multiline continuation, every command in a pasted initial
  multiline buffer, missing binary fails open, hostile
  terminal and bidi controls in the load diagnostic are neutralized under the
  C locale, a hanging binary is killed by the watchdog within deadline, secrets
  never reach the event log, resolution kinds are extracted correctly,
  double-sourcing is harmless, Vi keymap accepts work; the full typo flow (`y`
  runs the correction with arguments preserved byte-for-byte, prompts begin on
  a clean blank line with the complete escaped comparison, Tab moves focus,
  Enter activates the focused choice without defaulting to correction, a
  timeout names its unchanged-original outcome, `n` runs the original, Ctrl-C
  runs nothing, resolving words never prompt, the config file alone enables
  prompts); and the full warning flow (both halves of the
  flagship pair, edit restoring the exact buffer to a live ZLE, run-once
  executing unchanged, cancel leaving the dirty bytes untouched *on disk*,
  warnings outranking the typo prompt, arrow keys leaving no stray bytes,
  long CSI final bytes never becoming consent, and
  recency overlap counting shared targets but not shared flags). It also proves
  a config warning reaches the actual terminal exactly once through the plugin,
  and that an interactively invoked `doctor` sees the installed plugin plus all
  four live wrappers and reports `ready`; replacing every wrapper after plugin
  load makes that same interactive doctor report the live 0/4 problem rather
  than trusting its stale load-time snapshot.

  Some of these need a **staged** runner (`Session::run_staged`) that waits
  for expected text to appear on the terminal before sending the next keys.
  That is not a convenience: a Ctrl-C byte sent before the binary switches
  the terminal into single-key mode becomes an interrupt signal instead of a
  keypress, which is also why a real user's Ctrl-C is safe — they cannot
  press it before the prompt exists. The runner is bounded: a marker that
  never appears fails with the terminal transcript instead of hanging the
  suite.
- **`tests/uninstall.rs`** — damaged or multiple marker blocks refuse without
  editing; a healthy install removes only its marker and all three runtime
  files, including the installed uninstaller itself, while preserving config,
  state, and unrecognized files; no marker means no authority to remove
  same-named files. A no-final-newline shell file survives install, update, and
  uninstall byte-exact with its original backup intact; a later user suffix
  stays on its own line, and old marker text joined to a user line is refused
  rather than treated as ownership; displayed paths cannot inject terminal or
  bidi controls.
- **`tests/install.rs`** — the guided installation contract: all four explicit
  promptless choices, missing and invalid choices, no initial terminal focus,
  digit selection, Tab/Enter selection, bare Enter, Ctrl-C cancellation, and
  existing-config preservation. It also pins private modes for every installed
  file, symlink and marker refusal, checkout independence, stable-uninstaller
  updates, path-display escaping, byte-exact shell handling, retry after an
  early backup failure, complete fresh cleanup after a late failure, and
  restoration of one coherent old runtime set after a failed update.
- **`tests/doctor.rs`** — a complete healthy installation reports `ready`;
  missing runtime files, partial wrapper coverage, unsafe state permissions,
  invalid or over-cap config, a stale wrapper snapshot, and an unreachable
  configured model each report problems and exit nonzero. The state failure
  proves diagnosis does not repair permissions.
  Config and mode lines must agree under XDG redirection and when a symlinked
  config is ignored; invalid config reports only safe line-level diagnostics,
  and environment-derived config and PATH results are terminal-escaped.
- **`tests/report.rs`** — the shipped `report` command honors the selected
  state directory and exposes its summary through the real CLI dispatch.
- **`tests/state_paths.rs`** — real `check` processes prove relative explicit
  state overrides and relative HOME values cannot claim the working directory,
  while a relative XDG state root falls back to an absolute HOME. They also
  prove a FIFO log is rejected before retention can block on it and the process
  watchdog bounds analysis-time lock contention.
- **`tests/config_warnings.rs`** — simultaneous processes emit one warning set,
  a changed warning set re-arms exactly once, and a failed terminal display
  leaves the marker absent so delivery retries.
- **`tests/purge.rs`** — exact destructive-command argv, empty-state success,
  configuration/unknown-file preservation, known-symlink unlink safety,
  retention-marker and abandoned-temp cleanup, and refusal to enter a
  symlinked directory or recurse into an unexpected one.
- **`scripts/lifecycle-gate.zsh`** — the complete release lifecycle under one
  `mktemp`-owned home: install the binary, plugin, and stable uninstaller; load
  them in a real interactive ZLE shell; require `doctor` to report all four
  wrappers ready in explicitly selected Shadow mode; record and report three
  commands; purge state; then remove the runtime through the installed
  uninstaller. It proves the original `.zshrc` bytes return exactly, runtime
  and state disappear, and only the deliberately retained byte-exact config
  plus `.zshrc` backup remain. Its optional argument switches it from private
  source artifacts to an extracted public release directory.
- **`scripts/install-experience-gate.zsh`** — accepts only an extracted public
  release directory and drives its bundled installer through staged real PTYs.
  It selects all four modes (including Tab/Enter and ignored input), requires
  `doctor` to report each installed shell ready, proves Ctrl-C and a real TERM
  leave a fresh home uninstalled, rejects an invalid promptless mode, updates
  poisoned runtime files while preserving every config and shell byte, and
  removes each successful install through the stable installed uninstaller.
- **`scripts/build-release-bundle.zsh` and
  `scripts/release-bundle-gate.zsh`** — build the pinned, reproducible
  x86_64-musl archive and checksum receipt, then enforce the exact archive
  boundary, modes, version, static linkage, repository-source identity, basic
  lifecycle, and complete install-experience gate before those files can be
  published.
- **`scripts/pty-gate.zsh`** — the volume acceptance gate: N unique
  submissions (default 10,000) through a PTY shell; every output must appear,
  nothing may hang. M1's run: 10,000/10,000, zero altered buffers, in 128 s.
  It also fails if the average round trip exceeds a coarse per-submission
  ceiling (override with `OOPSINPUT_GATE_MS`) — deliberately loose, because
  it includes zsh startup and the submitted command's own execution, but it
  is the only check that sees *plugin-side* cost at all.
- **`scripts/perf-gate.zsh`** — the SPEC §10 latency budgets enforced against
  a release build, common and candidate paths, exiting nonzero when a
  percentile is over budget. It exists because a change that cost ~7.5 ms per
  command once shipped and was caught by a hand-run probe rather than by any
  test (test-audit 2026-08-06). It writes to a temporary state directory, so
  running it never pollutes the shadow-mode event log the pilot depends on.
- **`.github/workflows/ci.yml`** — on every push and pull request, one job
  checks formatting, runs Clippy with warnings denied, and runs the complete
  test suite; an independent job builds the release binary and runs the
  lifecycle, latency, and default 10,000-submission PTY gates above. Both jobs
  run against the declared minimum Rust 1.89.0. Manual dispatch is available
  too.
- **`.github/workflows/dependency-policy.yml`** — installs cargo-deny 0.20.2
  only after its release archive matches the repository-pinned SHA-256, then
  rejects advisories, yanked crates, unreviewed crate versions, unacceptable
  licenses, duplicate/wildcard dependencies, and non-crates.io sources. It
  runs on pushes, pull requests, manual dispatch, and a weekly schedule.
- **`.github/workflows/release.yml`** — on a matching `vVERSION` tag, builds
  the musl artifact with Rust 1.89.0, runs the release-bundle gate, generates a
  GitHub artifact attestation through a commit-pinned official action, and
  creates the prerelease with only the verified archive and `SHA256SUMS`.
- **`eval/golden/`** — the golden corpus (SPEC §11), three files run as
  ordinary tests: `typo.json` (20 cases), `danger.json` (41 cases, command
  shapes) and `policy.json` (19 cases, context flips — the same command in
  a dirty versus clean repository). Each case is a command plus a context
  fixture plus the exact expected evidence and decision. A large share are
  **counterfactual pairs** — the identical command in a context where
  nothing should happen — and every corpus runner asserts at least 30% are
  paired, so the discipline can't quietly erode. Project rule: every danger
  rule ships with a case where the same command is silently allowed, so the
  tool can't decay into a blanket dangerous-command blocker. Cases run
  hermetically (candidates and `$HOME` come from the fixture, never the
  developer's machine) and go through the same functions the binary uses.

Testing rules that bind every change: tests are derived from failure modes
that were actually proven, never written as ritual; each layer lands with
tests in the same commit; bug fixes ship a regression fixture; anything
touching the zsh plugin gets PTY tests; fixtures never contain real shell
history or personal data.

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
  analysis, ever — even `doctor`'s PATH lookup and the typo layer's PATH scan
  walk the filesystem themselves. Two external programs are run at all,
  `stty` and `git status`, and both obey the same rules: absolute path, fixed
  arguments, no shell, hard timeout. A helper's *own configuration* counts as
  untrusted input too — the repository you are standing in can tell git to
  execute something, and every such key is neutralized explicitly.
- **Evidence and decisions are separate things.** Layers produce typed
  evidence codes; policy alone turns evidence into a verdict; the mode alone
  decides whether a verdict becomes visible. That separation is what makes
  shadow mode meaningful — the decision is fully computed and recorded even
  when nothing is shown.
- **The correction channel is treated like the event log** (SPEC §9.2).
  Descriptor 3 is the only place binary output becomes an executed command,
  so it gets exact bytes, no interpretation, an integrity sentinel, and
  pinned tests. Any doubt anywhere in that chain produces no replacement at
  all rather than a best guess.
- **Sanitize at the source, then distrust it anyway.** The recency summary is
  built in the shell so raw history never crosses the boundary, and the
  binary re-checks the same restriction on arrival, because an adapter can be
  compromised or simply out of date.
- **Sync only, lean style** (AGENTS.md and CLAUDE.md). No async runtime, plain data +
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

- **Danger warnings are mode-controlled, not comprehensive.** Shadow and
  Suggest never show them—the verdict is recorded as shadow data instead.
  Visible warnings require the user to select `mode = warn` or `confirm`, and
  no unrecognized category becomes visible at any mode. This is deliberate
  sequencing, not a safety guarantee.
- **The rule tables are curated, not exhaustive.** They recognize the command
  shapes we chose; an unusual tool or an unfamiliar flag spelling is simply
  not recognized. The layer fails toward silence.
- **Rules match shapes, not semantics.** There is no model of how many
  arguments a command requires, so a malformed command can still produce
  evidence (recorded, never shown at today's tiers).
- **The local-model layer (L4) is opt-in and advisory.** With no `model`
  configured (the default), `check` never touches the network. With one, the
  model is consulted on ambiguous danger candidates only, its answer can at
  most raise a Warn, and every failure falls back to the deterministic
  verdict silently. The paired-corpus comparison (eval/model-comparison-
  2026-08-06.md) decided the model does NOT join the default config: zero
  categories improved, and reference-class local models miss the latency
  budget on this hardware by ~35×.
- **PS2 continuation submissions are not re-analyzed.** A pasted or prefilled
  initial ZLE buffer is analyzed in full, with top-level newlines separating
  commands. Text entered later after Zsh has moved to its `PS2` continuation
  prompt passes through untouched.
- **Linux and interactive zsh only.** The `/dev/fd/3` mechanism works on the
  BSDs too, but nothing else is tested there, and there is no bash adapter.
- **The candidate scan is bounded, not exhaustive.** Directory entries and
  permission checks are capped, so on a pathological PATH the layer may miss
  a candidate — again failing toward silence.
- **Git-helper hardening is a curated key list.** We disable the git
  configuration keys that can execute programs; a future git release could
  add a new one. The structural fix — reading the index ourselves and never
  spawning git — is a v2 candidate, recorded in PLAN.

## 9. Where things stand

See **[PLAN.md](PLAN.md)** for milestone-by-milestone status and what comes
next; completed milestones are archived verbatim in
[PLAN-ARCHIVE.md](PLAN-ARCHIVE.md), including the findings from each
refactor, bug-hunt, and security-audit pass.

In short: the deterministic product, optional local-model layer, reporting,
purge, retention, security hardening, guided mode-selecting installer, stable
uninstaller, and static release-archive pipeline are implemented and tested.
The published `v0.1.0` remains the first source-only public alpha. The active
installation feature's remaining acceptance and publication work—and all later
priorities—live in [PLAN.md](PLAN.md); this document deliberately does not
duplicate their order.
