# Security policy

oopsinput is a local assistance layer for catching likely command-line mistakes.
It is not a sandbox, an antivirus, an authorization system, or an enforcement
boundary. Its central failure rule is **fail open**: when analysis fails, the
original command normally runs unchanged. Never use oopsinput as permission to
run a command you would not otherwise run.

[SPEC.md](SPEC.md) is canonical for product behavior and invariants. This file
states the security posture in one place and explains how to report a problem.

## Supported versions

There is no tagged release yet. Until the first release, security fixes target
the current `main` branch only; earlier pre-alpha snapshots are not separately
supported. This section will be updated when versioned releases begin.

## Report a vulnerability privately

Email [kserrec@gmail.com](mailto:kserrec@gmail.com) with a subject beginning
`[oopsinput security]`. Do not open a public issue for an undisclosed
vulnerability.

As of 2026-08-08, the repository's GitHub private-vulnerability-reporting
button is not enabled. Ordinary email is not an encrypted channel, so do not
send passwords, tokens, private shell history, live exploit credentials, or
another person's data. If encrypted coordination is necessary, begin with a
non-sensitive summary and ask to arrange it.

Please include:

- the affected commit or `oopsinput version` output;
- Linux distribution and Zsh version;
- the security invariant you believe is broken and the practical impact;
- the smallest synthetic reproduction you can provide;
- whether any details are already public.

Replace real command text, paths, repository contents, history, and secrets
with synthetic equivalents. The pre-alpha project does not promise a response
time SLA, but coordinated disclosure is preferred: please allow time to verify
and fix a report before publishing exploit details.

Good security reports include analysis executing something, a command buffer
changing without the documented consent, raw private data reaching a default
log or an unintended peer, terminal control-sequence injection, a product-owned
write escaping its intended path, or a model response gaining authority that
policy does not grant it. A missed danger pattern or an unhelpful warning is
still valuable, but it is normally a correctness report rather than a security
boundary failure.

## Threat model

oopsinput protects a user from some of that user's own accidental input. It
does not try to contain a malicious program already running as that user.

| Boundary | How oopsinput treats it |
|---|---|
| Command buffer, current directory, repository, PATH entries, model output, and displayed paths | Untrusted input. They are parsed or inspected, never evaluated as shell code during analysis. |
| Other processes with the same user ID | Outside the adversarial boundary. Such a process can replace the user's binary, plugin, config, or shell startup files and can bypass oopsinput entirely. |
| Other unprivileged human accounts on a shared machine | Untrusted where the operating system exposes identity, notably for the optional Ollama loopback peer check. |
| Root, the kernel, and system accounts | Trusted platform. A hostile system account or compromised kernel means the machine is compromised; oopsinput does not resist it. |
| The user's final choice | Authoritative. Explicitly accepting a typo correction or choosing “run unchanged” permits the shell to execute it. |

The same-user boundary is not an excuse to trust everything in the user's
environment. In particular, a directory on `$PATH` may come from an extracted
project, `.` itself, or a repository's `./bin` added by a tool such as direnv.
Those locations are treated as untrusted even though they are visible to the
same shell session.

## Enforced security properties

### Analysis does not execute the command

The command buffer is tokenized by oopsinput's conservative lexer. Analysis
does not invoke a shell, expand globs, evaluate substitutions, source files, or
run the proposed command. The buffer travels to the binary through standard
input rather than process arguments, which keeps it out of
`/proc/<pid>/cmdline`.

On the production `check` path, the Rust binary launches at most two external
programs:

- `stty`, for single-key terminal input; and
- `git status --porcelain`, for bounded repository context on danger
  candidates.

Both are selected from fixed absolute system paths, bypass standard input where
appropriate, and have hard deadlines. Arguments are fixed or, for terminal
restoration, derived only from `stty`'s own saved mode string; proposed command
text never becomes helper arguments. The helpers are never resolved by name
through `$PATH`. This matters more than the usual PATH-hijack warning: the typo
prompt runs only for an unresolvable command, so a PATH-resolved `stty` would
let a repository supply a predictable program that any typo could trigger
before the user consented to anything.

`git status` has an additional boundary. The repository being inspected may
have come from someone else and its `.git/config` is untrusted. Git is invoked
with command-line overrides for the known execution-capable settings relevant
to status (`core.fsmonitor`, `core.hooksPath`, and `core.pager`), disables
system configuration and terminal prompts, avoids optional locks, and has
bounded output.

Residual risk: that Git hardening is a curated list. A future Git version could
introduce another configuration key that causes `git status` to spawn a
program. The structural fix is to read the index without launching Git; that is
a recorded v2 candidate, not a v1 promise. A new exec-capable key is a security
report even if oopsinput's current list was complete when released.

### Command bytes and consent are preserved

The Zsh plugin delegates the exact original buffer for allow and fail-open
paths. A typo replacement is the only binary-produced text that can become an
executed command, and it requires an explicit `y`. The replacement travels on
file descriptor 3 with a terminating NUL integrity sentinel; a missing or
truncated sentinel makes the plugin use the original buffer instead.

Danger interventions never execute a proposed rewrite. They offer:

- `e` — restore the exact original buffer for editing;
- `c` — clear it and run nothing;
- `r` — run the exact original once.

Every untrusted fragment in oopsinput-authored prompts and diagnostics is
passed through a terminal escaper. The Rust binary uses its fuzz-tested Unicode
escaper; the plugin and lifecycle scripts use a Zsh equivalent for their
environment-derived paths. Control characters, ANSI/OSC sequences,
bidirectional controls, and invisible formatting characters are rendered as
inert visible text.

### Fail-open behavior is bounded, but it is not safety

Missing binaries, malformed input, internal errors, unavailable state,
timeouts, and model failures normally lead to the original command running
unchanged. A watchdog bounds the deterministic analysis path, and each helper
and model request has its own deadline.

Fail open prevents oopsinput from holding the user's shell hostage; it also
means failure is not protection. A process stuck in uninterruptible kernel I/O
— for example, a state directory on a hung network filesystem — may not be
able to obey even the watchdog and can still hold the prompt. That residual is
documented rather than hidden behind a stronger claim.

### Default logs are structural and local

There is no telemetry. With no model configured, oopsinput makes no network
connection. The current event type has no field for a raw command or path; it
stores structural codes, counts, timings, outcomes, and keyed fingerprints.
Recent shell history is summarized inside the plugin and constrained again by
the binary, so raw history text never reaches the event pipeline.

State directories are `0700` and state files are `0600`. Existing state and
config leaves must be regular before opening and must still match the inspected
nonsymlink inode after opening. Missing state files use atomic `create_new`
rather than a create-and-follow open, and coordinated writers use a stable file
lock. Logging failures are swallowed so they cannot change command execution.
`oopsinput doctor` checks these paths and modes without creating state,
rewriting files, or repairing permissions.

`oopsinput purge` removes only named oopsinput state. It does not follow a
symlinked state directory, recursively remove unexpected directories, delete
configuration, or delete unknown entries.

Residual risk: safe standard-library operations cannot make inspection plus
open atomic for every existing file type. A malicious process running as the
same user could swap a checked path for a FIFO before open and block that open;
it could also directly alter oopsinput's installed files. That is inside the
accepted same-user boundary. The implementation rejects ordinary non-regular
paths, verifies opened inode identity, and keeps the watchdog as a latency
backstop, but it does not claim hostile same-user isolation.

### Installation claims only what it owns

The installer is unprivileged, resets its helper PATH to `/usr/bin:/bin`, and
does not ask for credentials. On a fresh install it leaves every existing
binary or plugin destination untouched, including dangling symbolic links and
same-named regular files. It also leaves any existing config path byte-exact.

A healthy marked block in `~/.zshrc` is the ownership receipt that authorizes
later atomic replacement of regular installed binary and plugin files. No
marked block means a fresh install and cannot authorize overwriting an existing
runtime destination. Duplicated, reversed, mismatched, or otherwise damaged
markers cause refusal before installation changes anything. The installer
refuses symbolic-link and non-regular shell, backup, binary, and plugin
destinations.

The uninstaller uses the same receipt. It removes the marked block and runtime
files it owns, but deliberately keeps configuration, state, the shell backup,
and unrecognized plugin-directory entries. See [README.md](README.md#remove-it)
for the exact lifecycle.

`oopsinput doctor` diagnoses both sides of the installation contract: the
healthy marked block plus regular installed plugin file, and the four live
accept widgets published by the current interactive shell. It is an
inspection command, not an installer or repair path.

### The optional model is untrusted and loopback-only

No model is configured by default. When a model is explicitly configured,
gate-eligible commands — including their raw command text — may be sent to
Ollama. Release builds use the fixed IPv4 address `127.0.0.1:11434`; there is
no host or address setting, no cloud provider, no TLS, and no redirect support.

Loopback alone is not identity. Any local account can bind an unused port. The
client therefore connects first but sends no request bytes until it finds the
peer side of that exact established connection in `/proc/net/tcp` and accepts
only:

- the current user's own UID; or
- a UID below 1000, representing root or a conventional system service
  account.

Unreadable or unmatched socket state and another human user's UID are refused;
analysis continues deterministically without the model, and `doctor` names the
peer refusal. Because the lookup is for the established connection's service
and ephemeral ports, another listener cannot be swapped in after verification.

The limits are deliberate and explicit:

- UID 1000 assumes the conventional Linux `SYS_UID_MAX` boundary;
- a hostile root or system account is considered a compromised machine and is
  out of scope;
- a same-UID malicious service is inside the same-user boundary;
- a nonstandard setup whose connection appears only in `/proc/net/tcp6` is not
  recognized and fails closed to deterministic-only.

Model replies are schema-constrained and then validated again locally. Unknown
fields or vocabulary, malformed JSON, oversized output, errors, and timeouts
become unavailable evidence. A model can at most add a Warn; it cannot clear a
deterministic concern, produce Confirm, execute text, or deny a command.

## What oopsinput does not defend against

- Malware or a determined bypass running as the user, root, a compromised
  kernel, or a hostile system service.
- Commands or flags outside the curated rule tables. Silence means “no
  intervention under current evidence,” never “safe.”
- Bash, Fish, non-interactive scripts, agent subprocesses, or continuation
  lines entered at Zsh's `PS2` prompt.
- A user explicitly accepting a correction or choosing to run the original.
- The integrity or intent of executables the user chooses to run from PATH.
- Every future behavior of the external Git helper; the current residual is
  described above.
- Availability when the operating system itself cannot schedule or release a
  blocked process.

Security invariants are regression-tested, dependency advisories and the exact
crate allowlist are checked continuously, and [PLAN.md](PLAN.md) records the
remaining release work and known deferred risks.
