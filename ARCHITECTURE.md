# oopsinput — Architecture and developer guide

This document explains the implementation as it exists today, ground-up, so a
new developer can build it, test it, and change it with confidence.

How it relates to the other documents:

- **[SPEC.md](SPEC.md)** is canonical for *design*: scope, principles, security
  invariants, interfaces, and the full four-layer vision. When this document
  and SPEC disagree, SPEC wins.
- **[PLAN.md](PLAN.md)** tracks *progress*: which milestones are done and what
  each one covered.
- **This document** covers *how the code works right now*. That is the whole
  deterministic product: command capture in zsh, the lexer, all three
  deterministic analysis layers (typo, danger, context), the policy engine,
  both visible prompts, event logging, and the test harness that proves your
  buffer survives intact. The optional local-model layer (L4) is fully
  wired: with a model configured (default: none — deterministic-only),
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

Neither gate runs under `cargo test`: they need a release build and real
process spawns, and keeping them separate keeps the test suite fast. Run
them before claiming a performance number.

To install on your own machine:

```
zsh/install.zsh
```

This installs in **suggest** mode: the typo layer may ask you a question
(only ever about a command that could not have run anyway), and everything
else is silently observed and recorded. Danger warnings are off by default —
see §4.8 for the modes and how one is chosen.

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

### What the wrapper does (`_oopsinput_handle`, `zsh/oopsinput.zsh:48`)

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
the session. That diagnostic renders the configured path with zsh's `(V)`
flag so control characters appear visibly (`^[`) — a hostile `OOPSINPUT_BIN`
value can't smuggle terminal escape sequences to your terminal. (`(qqqq)`
quoting is *not* sufficient for this; it leaves control bytes raw.)

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

Twelve modules. Analysis runs strictly cheapest-first, and each layer can be
read on its own:

### 4.1 `src/main.rs` — dispatch, watchdog, the `check` path

Hand-rolled subcommand dispatch (no CLI-parsing dependency; SPEC §12):
`version`, `check`, `doctor`, `help`.

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

`doctor` prints environment sanity checks: version, whether `zsh` is on PATH
(via direct file-metadata lookup that requires the executable bit — never by
asking a shell, per SPEC §9), which config file is in effect and whether it
exists, the resulting mode, how many config problems were found, whether
the plugin block is present in `~/.zshrc`, and — when a model is configured —
whether Ollama answers on loopback and has that model pulled (a POST to
`/api/show` through §4.10's client; it loads nothing and runs no inference).
The config line and the mode line resolve through the same function, so they
can never contradict each other (they once did — a bug found by review and
now pinned by `tests/doctor.rs`).

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

- **Structural fields only.** Timestamp, decision, reason code, evidence
  codes, resolution kind, buffer byte count, word count, duration, plus
  optional context *counts* (dirty file count, largest directory entry
  count) and the user's outcome at a visible warning (`edited`, `cancelled`,
  `ran_unchanged`). The `Event` struct has no field that *could* carry
  command text — the type system is the redaction.
- **User-only permissions**: directory `0700`, file `0600`.
- **One write syscall per event.** The line and its trailing newline are
  joined into a single buffer before writing, because `O_APPEND` atomicity
  holds per write call — two concurrent shells appending with separate
  newline writes would interleave and corrupt the JSONL stream (a real bug
  found by review, now pinned by a 16-thread hammer test).
- **Never written through a symlink.** If the log path is a symbolic link,
  the append is refused rather than growing onto whatever it points at.
- **Failures are swallowed.** Logging must never cost the user their command
  or their prompt.

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
`git reset --hard` and `git clean -f` warn only when there is actually work
to lose, and are silently allowed on a clean tree. `push --force` warns only
on a main-like branch. A recursive delete warns when the target is the
current directory, its parent, or a near-miss of a real neighbor, and is
allowed when every target plainly exists. Everything else recognized records
as `observe` — recognized but not yet graduated to speaking. Each arm exists
to make a golden counterfactual pair pass; a rule with no context in which
it stays silent does not belong here.

**`cap_for_mode`** applies the mode as a *ceiling, never a floor* — and
downgrades preserve the policy reason. That preserved reason is the whole
shadow-mode mechanism: an event recorded as `observe` with the reason
`policy.dirty_work_at_risk` is a hypothetical intervention, which is what
lets the M5 pilot measure "how often would this have spoken, and would it
have been right?" before anything is enabled for real users.

The four modes (SPEC §8): **shadow** (analyze and record, never visible —
the default), **suggest** (adds typo prompts), **warn** (adds nonblocking
danger warnings), **confirm** (danger warnings pause for an answer).

**`apply_gates`** is habituation control (SPEC §7): at most three visible
interventions per rolling hour, and a per-rule cooldown — three consecutive
"I meant it, run unchanged" outcomes puts that rule to sleep for a day,
while any edit or cancel wakes it. Direct-catastrophic findings are exempt
from both. Exhausting the budget degrades to silent recording; it never
degrades to nagging.

It is a *pure read* over history: checking the gates writes nothing, and only
a prompt the user actually saw is recorded afterwards. That ordering is what
makes "an intervention nobody saw cannot spend budget" structural rather than
a flag somebody has to remember to pass.

The module also owns the full SPEC §15 **config surface**: `mode`, `model`,
`model_timeout_ms`, `det_timeout_ms`, `budget_per_hour`, `log_raw`. Invalid
values fall back to the documented default and say so; unknown keys are
reported **by line number only**, never by echoing the key — config text is
untrusted input and must not reach a terminal. Complaints are printed once
per distinct set, tracked by a fingerprint file in the state directory.
`$OOPSINPUT_MODE` overrides the file's mode.

Cooldown and budget state live in `policy.jsonl` (user-only): one appended
line per shown intervention, recording when it happened, which rule, and what
the user did. Reading takes only the tail — the budget looks back an hour and
a cooldown a day — so a long-lived file costs a bounded read; a torn or
partial line simply fails to parse and is skipped, and the path is never
written through a symlink.

**Append-only is load-bearing, not a style choice.** The first version was a
JSON blob loaded, modified and written back, which meant two shells finishing
warnings in the same instant each recorded a spend and the second write
dropped the first: the hourly cap silently under-counted and a cooldown could
disappear. The event log had already solved that exact problem with one
atomic append per line, and this is the same fix — the race is gone by
construction rather than held off by a lock. An 8-thread test fails if anyone
reintroduces read-modify-write here.

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

**Two prompts**, both on `/dev/tty` — the terminal itself — rather than
stdout, which carries the decision JSON and is discarded by the plugin:

- `prompt_typo` asks the L1 question. `y` accepts, Ctrl-C cancels, and
  everything else (`n`, any other key, a ten-second timeout, a missing
  terminal, any internal failure) runs the original command, which is the
  safe outcome since that command could not have run anyway.
- `prompt_warning` shows the L2+ warning, whose anatomy is fixed by SPEC §7:
  what the command does, the concrete facts, why the context is unusual,
  then the keys. `e` restores your exact buffer to ZLE for editing, `c`
  cancels and runs nothing, `r` runs the original unchanged once. The
  timeout default depends on the tier: an advisory warning runs the command,
  a pausing confirmation cancels it — running is never the default for a
  command whose consequences are predicted to be irreversible. Any failure
  to display fails open to running unchanged.

`warning_lines` builds those message lines from evidence codes and context
counts, with every untrusted fragment escaped and every line framed by the
fixed `oopsinput:` prefix.

**Reading a keypress** is subtler than it looks: an arrow key is not one
byte but a short escape sequence (`ESC [ A`). Reading a single byte treated
that as an answer and left the remaining bytes behind, where they leaked
into the next command line as stray characters. The reader now consumes
complete sequences (CSI, SS3, alt-chords) and ignores them, while a lone
`ESC` still means "dismiss". Unrecognized keys are ignored rather than
guessed at, and the whole loop is bounded so hostile input cannot hold the
prompt open.

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

The first piece of the inference layer: a self-written HTTP/1.1 POST over
`std::net::TcpStream` (SPEC §12 rejects an HTTP crate for this — loopback
needs no TLS, no redirects, no connection reuse). It refuses any non-loopback
address before opening a socket, and the target is fixed at Ollama's default
`127.0.0.1:11434` — deliberately not configurable, because SPEC §14 allows no
network beyond loopback and an address knob would invite pointing it
elsewhere.

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
module can block a command. Today `doctor` is its only caller; the inference
layer (prompt assembly, schema validation, the candidate gate) comes next.

### 4.11 `src/layers/infer.rs` — L4, the inference layer

The brain above §4.10's transport: prompt assembly, the response schema, and
validation. `check` consults it only when **all** of these hold: a model is
configured (`model =` in config; the default is none, so default installs
never touch the network), the danger layer marked a candidate that is *not*
direct-catastrophic, and policy's mode-blind verdict came back Observe with
an ambiguity reason — L3 neither cleared the command nor decided against
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
updating SPEC §12 first.

## 5. Three Enters, end to end

### 5.1 The common path: an ordinary command

You type `git status` and press Enter:

1. ZLE runs the wrapped `accept-line`, which calls `_oopsinput_handle`
   (`zsh/oopsinput.zsh:48`).
2. The buffer is non-empty, not a continuation line, not recursive — so the
   plugin resolves `git` via `whence -w` → `command`. Because that is not
   `none`, no candidate names are collected; five recency summaries are.
3. It pipes the payload to `~/.local/bin/oopsinput check --res command`,
   capturing descriptor 3.
4. The binary reads config, arms the watchdog (`src/main.rs:87`), reads the
   payload into a `Proposal` (`src/proposal.rs:107`), lexes it, skips the
   typo layer immediately (the resolution kind is not `none`), and runs the
   danger layer (`src/layers/danger.rs:60`), which finds nothing. Because
   there is no candidate, the context layer never runs. Decision: `allow` /
   `shadow.observed`.
5. It appends one structural line to `~/.local/state/oopsinput/events.jsonl`
   (`src/events.rs:69`), prints the decision JSON on stdout (discarded by the
   plugin), and exits 0.
6. The plugin sees exit 0, restores `BUFFER=$original`, and delegates to the
   real `accept-line`. zsh executes `git status` exactly as typed.

Measured on the dev machine, release build, including process spawn:
**p50 2.4 ms, p95 4.4 ms** — against a 25 ms p95 budget (SPEC §10). If steps
3–5 fail *in any way* — binary missing, crash, watchdog fired, weird exit
code — step 6 still happens identically.

### 5.2 The typo path: a command that doesn't resolve

You type `gti status` and press Enter, in suggest mode:

1–2. As above, except `whence -w` reports `none`.

3. Because the kind is `none`, the plugin fills the candidate section with
   every alias, function, builtin, and reserved-word name.
4. The binary runs the typo layer (`src/layers/typo.rs:39`). The word `gti`
   is the first token, literal, and 3 characters, so it qualifies. Scanning
   the supplied names and PATH finds `git` at distance 1. No exact match for
   `gti` exists, so nothing suppresses the result. Evidence:
   `typo.candidate_d1`. The danger layer finds nothing, so policy has nothing
   stronger to say and the typo prompt proceeds.
5. The binary builds the corrected buffer *first*
   (`src/layers/typo.rs:160`) — `git status`, with every byte after the
   command word preserved. Only if that succeeds does it mark the prompt
   active, retiring the watchdog, and ask on `/dev/tty`:
   `oopsinput: 'gti' not found — did you mean 'git'? [y/n]`
6. You press `y`. The binary writes `git status` plus one NUL byte to
   descriptor 3 (`src/main.rs:387`), records `replace` / `typo.accepted`,
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
   (`src/layers/context.rs:75`): it finds the repository, reads `HEAD`, and
   runs the hardened `git status`, which reports 17 dirty files.
5. Policy (`src/policy.rs:57`) sees a work-loss command with work to lose and
   returns `warn` / `policy.dirty_work_at_risk`. The mode is `warn`, so the
   ceiling doesn't lower it. The gates pass (budget available, rule not in
   cooldown).
6. `warning_intervention` (`src/main.rs:329`) marks the prompt active and
   displays:

   ```
   oopsinput: git reset --hard will discard uncommitted changes in tracked files
   oopsinput: right now: 17 modified tracked files
   oopsinput: previous command: git diff
   oopsinput: [e]dit  [c]ancel  [r]un unchanged
   ```

7. You press `c`. The binary records the outcome `cancelled`, saves the
   updated cooldown state, and exits **12**. The plugin clears the buffer.
   Nothing runs; your 17 files are untouched. (`e` would exit 11 and hand the
   exact command back to ZLE for editing; `r` would exit 0 and run it.)

**Half two — a clean tree.** Steps 1–4 are identical, but `git status`
reports nothing dirty. Policy returns `allow` / `policy.context_clear`, no
prompt is shown at all, and the command runs in silence. The event log still
records the reason, which is what makes the decision auditable later.

Measured on the candidate path (release, including both our spawn and git's):
**p50 15.0 ms, p95 18.1 ms**, against the same 75 ms p95 budget.

## 6. How it's tested

The testing philosophy: **buffer exactness and fail-open behavior are the
product**, so the highest-value tests drive a real interactive zsh, not mocks.
165 tests across five suites today, plus two gates that run separately.

- **Unit tests** live inside each `src/` module (`#[cfg(test)] mod tests`):
  the closed resolution vocabulary, payload parsing edge cases (including the
  recency section's re-sanitization and cap handling), concurrent log
  appends, structural-only serialization, the danger layer's rule tables and
  refusals, the context layer's git and target facts, the policy matrix,
  budget and cooldown behavior, config validation, the display escaper
  (including its fuzz test), both prompts' key protocols against a scripted
  fake terminal, and the bounded external-helper runner.
- **`tests/pty.rs`** — the PTY integration suite. Each test builds an
  isolated `ZDOTDIR` (a throwaway home for zsh config) whose `.zshrc` loads
  the plugin against the freshly-built debug binary, then runs a genuine
  interactive zsh inside a pseudo-terminal via util-linux
  `script -qec "zsh -i" /dev/null`, feeds it keystrokes, and asserts on what
  the terminal displayed. Covered: ordinary passthrough, unicode and quoting
  survival, PS2 multiline continuation, missing binary fails open, hostile
  escape sequences in the load diagnostic are neutralized, a hanging binary
  is killed by the watchdog within deadline, secrets never reach the event
  log, resolution kinds are extracted correctly, double-sourcing is harmless,
  Vi keymap accepts work; the full typo flow (`y` runs the correction with
  arguments preserved byte-for-byte, `n` runs the original, Ctrl-C runs
  nothing, resolving words never prompt, the config file alone enables
  prompts); and the full warning flow (both halves of the flagship pair, edit
  restoring the exact buffer to a live ZLE, run-once executing unchanged,
  cancel leaving the dirty bytes untouched *on disk*, warnings outranking the
  typo prompt, arrow keys leaving no stray bytes, and recency overlap
  counting shared targets but not shared flags).

  Some of these need a **staged** runner (`Session::run_staged`) that waits
  for expected text to appear on the terminal before sending the next keys.
  That is not a convenience: a Ctrl-C byte sent before the binary switches
  the terminal into single-key mode becomes an interrupt signal instead of a
  keypress, which is also why a real user's Ctrl-C is safe — they cannot
  press it before the prompt exists. The runner is bounded: a marker that
  never appears fails with the terminal transcript instead of hanging the
  suite.
- **`tests/uninstall.rs`** — `uninstall.zsh` against a `~/.zshrc` with
  damaged marker blocks must refuse to edit, never delete-to-end-of-file.
- **`tests/install.rs`** — the installed defaults: a fresh install writes
  `mode = suggest` with user-only permissions, an existing config is left
  byte-identical, a symlink at the config path is not written through, and
  installing twice doesn't duplicate the `~/.zshrc` block.
- **`tests/doctor.rs`** — `doctor`'s config line and mode line must agree,
  including when `XDG_CONFIG_HOME` redirects the config elsewhere.
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

- **Danger warnings are off by default.** The default modes (shadow, and the
  installed default suggest) never show them — the verdict is recorded as
  shadow data instead. Visible warnings require `mode = warn` or `confirm`,
  and no category is enabled by default until the pilot supplies evidence
  (SPEC §8 graduation). This is deliberate sequencing, not an oversight.
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
- **Only the first line of a multi-line command is analyzed.** Continuation
  lines typed at the `PS2` prompt pass through untouched.
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

In short: the deterministic product is complete and tested — capture, lexing,
all three deterministic layers, policy, and both prompts. What remains before
a first release is the optional local-model layer, a shadow-mode pilot on
real usage to decide which rule categories have earned the right to speak,
and release engineering (continuous integration, `SECURITY.md`, a `report`
command, and a clean-machine install test). Files SPEC §16 lists for later
milestones (`model.rs`, `layers/infer.rs`) don't exist yet by design —
modules are created when their milestone starts.
