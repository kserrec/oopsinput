# oopsinput

**Catches commands that probably are not what you meant — before they run.**

oopsinput sits between pressing Enter and command execution in an interactive
Zsh shell. It catches misspelled command names, recognizes a curated set of
high-consequence command shapes, checks the current context, and intervenes
only when the evidence warrants it.

> **Pre-alpha:** Linux and interactive Zsh only. There is no packaged release
> yet. oopsinput is an assistance layer, not a safety boundary: it deliberately
> fails open, so an internal failure or an unrecognized command shape lets the
> original command run unchanged. Never test it with a destructive command you
> would not otherwise run.

## What it does today

- In the installed default, **Suggest mode**, `gti pull` can prompt:
  `'gti' not found — did you mean 'git pull'? [y/n]`. This only happens when
  the first command name does not resolve in the live shell. `y` runs the
  correction; `n` runs the original unchanged; Ctrl-C cancels.
- It recognizes selected filesystem, Git, system, and privilege-related
  command shapes, then collects bounded context that can distinguish an
  unusual command from an ordinary one. For example, `git reset --hard` with
  dirty work is different from the same command on a clean scratch branch.
- Danger and context decisions are fully implemented but remain invisible in
  Shadow and Suggest modes. They are recorded locally as hypothetical
  interventions so their accuracy can be measured before any category earns a
  visible default.
- In the opt-in Warn and Confirm modes, a visible danger intervention names
  the reason and offers `e` to restore the exact command for editing, `c` to
  cancel, or `r` to run the original once. Nothing is silently rewritten, and
  there is no hard-deny decision.
- `oopsinput report` summarizes local decisions, hypothetical and visible
  intervention rates, outcomes, evidence-code rankings, and latency.
- `oopsinput purge` removes oopsinput-owned recorded state while keeping the
  configuration file.

The common path is one short-lived Rust process per submitted command; there
is no oopsinput daemon or background service. Current release-build
measurements on the development machine are in [PLAN.md](PLAN.md).

## What it does not do

oopsinput is not a sandbox, antivirus, command validator, or guarantee that an
allowed command is safe. “Allowed” means only that the implemented rules found
no reason to intervene under the current evidence.

It does not:

- comprehensively understand every command, flag spelling, shell construct, or
  possible consequence;
- intercept Bash, Fish, non-interactive scripts, or commands launched by agent
  subprocesses;
- analyze continuation lines entered at Zsh's `PS2` prompt;
- execute, expand, source, or evaluate the command during analysis;
- send telemetry or use a cloud service;
- enable danger warnings by default.

For selected Git-related candidates, oopsinput runs a bounded, read-only
`git status` helper with fixed arguments and a hard timeout. It never runs the
candidate command as part of analysis.

The optional local-model layer is also implemented, but no model is configured
by default because the evaluated models did not improve the paired corpus or
meet the latency target. If you explicitly configure one, rare ambiguous
danger candidates — including their raw command text — are sent to Ollama at
`127.0.0.1:11434`. oopsinput first verifies that the connected process belongs
to your user or a system account. Model output is untrusted, can only add a
warning, and can never clear or deny a command.

See [ARCHITECTURE.md](ARCHITECTURE.md#8-known-limitations) for the complete
current limitations, [SPEC.md](SPEC.md) for the canonical design, and
[SECURITY.md](SECURITY.md) for the exact threat boundary and private reporting
instructions.

## Privacy and local state

There is no telemetry. With the default deterministic configuration, oopsinput
makes no network connection. The default event log contains structural facts
such as decision and evidence codes, counts, timings, outcomes, and keyed
fingerprints; it does not contain raw commands, paths, or secrets.
`oopsinput report` reads that log locally and prints aggregates rather than
command text.

The default paths are:

- config: `~/.config/oopsinput/config`;
- recorded state: `~/.local/state/oopsinput/`;
- installed binary: `~/.local/bin/oopsinput`;
- installed plugin: `~/.local/share/oopsinput/oopsinput.zsh`.

The standard XDG configuration and state environment variables take precedence
when set. State files are user-only, and retention removes records older than
30 days during later analysis-time writes. No background cleanup process runs.

## Install from source

Prerequisites are Linux, interactive Zsh, Git, and Rust 1.89 or newer through
[rustup](https://rustup.rs). Installation is user-level and does not use root.

From a directory where you want the source checkout:

```sh
git clone https://github.com/kserrec/oopsinput.git
cd oopsinput
cargo build --release
zsh/install.zsh
```

Before you run the installer, know exactly what it changes:

- copies the release binary and plugin to the paths listed above;
- creates a user-only config with `mode = suggest` only when no config already
  exists;
- backs up an existing `~/.zshrc` to `~/.zshrc.oopsinput-backup`;
- adds one clearly marked source block to `~/.zshrc`.

It refuses symbolic-link and non-regular destinations. On a fresh install it
also refuses to overwrite same-named runtime files; a healthy marked
`~/.zshrc` block is the ownership receipt that permits later updates. Rerunning
the installer atomically updates the installed binary and plugin without
changing an existing config. The installed shell hook does not depend on the
checkout remaining in place.

Open a new terminal, then verify the pieces that `doctor` currently checks:

```sh
"$HOME/.local/bin/oopsinput" doctor
```

`doctor` checks the marked shell block and installed plugin file, all four
live accept-widget wrappers in that terminal, config validity and effective
mode, the configured Ollama model when one is enabled, and `0700`/`0600` state
permissions. A state directory that has not been created yet is valid. The
check is read-only: it never installs, creates state, or repairs permissions.
It prints `result: ready` and exits zero only when every required check passes;
otherwise it prints `result: problems found` and exits nonzero.

## Self-serve Shadow or Suggest trial

Use oopsinput on normal commands you already intended to run. Do not manufacture
dangerous probes: because the tool fails open and its rules are deliberately
incomplete, a real destructive command can still execute.

Choose one of these two trial modes in the config file. The installer creates
the file at `~/.config/oopsinput/config` unless `$XDG_CONFIG_HOME` is set.

- **Shadow** gives a completely silent trial. Set:

  ```text
  mode = shadow
  ```

  Decisions are recorded locally when the state directory is writable, but no
  typo or danger prompt is shown.

- **Suggest** is the installed default. Keep:

  ```text
  mode = suggest
  ```

  Misspelled command names may produce the `y`/`n` prompt described above.
  Danger decisions remain invisible and are only recorded as hypothetical
  interventions.

After changing the file, new commands use the selected mode immediately; no
daemon needs restarting. Use the shell normally for as long as you find useful,
then inspect the local aggregate:

```sh
"$HOME/.local/bin/oopsinput" report
```

The report is the whole self-serve feedback artifact. Nothing uploads it. Read
it before sharing it voluntarily, just as you would any diagnostic output. A
useful trial is natural usage, not a canned command list: the important
measurement is how often oopsinput would interrupt real work, especially when
it should remain silent.

Warn and Confirm modes are available for deliberate local experimentation, but
they are outside this low-risk trial protocol and are not recommended as a way
to test destructive commands.

## Remove it

Run the uninstaller from the source checkout. It removes the marked `~/.zshrc`
block and the installed binary/plugin that the block proves it owns. It keeps
configuration, recorded state, and `~/.zshrc.oopsinput-backup`; uninstalling
therefore does not silently delete user data.

If you also want the recorded state deleted, purge it **before** removing the
binary:

```sh
"$HOME/.local/bin/oopsinput" purge
zsh/uninstall.zsh
```

`purge` keeps the config. After uninstalling, open a new terminal. If you want
no remaining configuration or backup, inspect and remove those two files
yourself; the uninstaller will not claim that authority.

## Project status and development

The deterministic product, the optional local-model path, local reporting,
30-day retention, purge, and the clean-machine install-to-uninstall lifecycle
are implemented and continuously checked. Remaining release engineering is the
first tag and public-alpha launch. [PLAN.md](PLAN.md) is the live status record.

[ARCHITECTURE.md](ARCHITECTURE.md) is the developer guide. From a fresh clone,
the required local checks are:

```sh
cargo build --release
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
scripts/lifecycle-gate.zsh
scripts/pty-gate.zsh
scripts/perf-gate.zsh
```

The lifecycle and PTY gates need Zsh plus util-linux `script`; the Git context
tests also need Git. The lifecycle gate changes only a temporary isolated home
and deletes it afterward. Performance claims count only in release builds.

## License

Apache-2.0
