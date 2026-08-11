# oopsinput

**Catches commands that probably are not what you meant — before they run.**

oopsinput sits between pressing Enter and command execution in an interactive
Zsh shell. It catches misspelled command names, recognizes a curated set of
high-consequence command shapes, checks the current context, and intervenes
only when the evidence warrants it.

> **Public alpha (`v0.1.0`):** Linux and interactive Zsh only. The published
> `v0.1.0` release is source-only; the guided binary-release path described
> below is implemented on `main` but will not be published until its user
> acceptance phase passes. oopsinput is an assistance layer, not a safety
> boundary: it deliberately fails open, so an internal failure or an
> unrecognized command shape lets the original command run unchanged. Never
> test it with a destructive command you would not otherwise run.

## What it does today

- In **Suggest mode**, `gti pull` can prompt:

  ```text
  *** oops? ***
  You typed 'gti pull'.
  Did you mean 'git pull'?
  [y] run correction  [n] run original
  ```

  This only happens when the first command name does not resolve in the live
  shell. The block starts after a clean blank line, and the complete original
  and corrected commands are escaped and display-bounded. `run original`
  starts highlighted; Tab switches focus and Enter activates it. `y` and `n`
  are immediate shortcuts. Ctrl-C still cancels but is not shown in the choice
  row. With no answer for ten seconds, oopsinput explicitly says it timed out
  and runs the original typo unchanged—never the correction.
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
- re-analyze continuation lines entered later at Zsh's `PS2` prompt (a pasted
  or prefilled initial multi-line ZLE buffer is analyzed in full);
- execute, expand, source, or evaluate the command during analysis;
- send telemetry or use a cloud service;
- show danger warnings unless the user explicitly chooses Warn or Confirm.

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
- installed plugin: `~/.local/share/oopsinput/oopsinput.zsh`;
- installed uninstaller: `~/.local/share/oopsinput/uninstall.zsh`.

The standard XDG configuration and state environment variables take precedence
when set. State files are user-only, and retention removes records older than
30 days during later analysis-time writes. No background cleanup process runs.

## Install

Installation is user-level: it asks for no password or root access, makes no
network connection, starts no daemon, never sources `.zshrc`, and does not
change `PATH`.

### Release archive

This is the ordinary-user path for a release that has these two assets:

- `oopsinput-VERSION-x86_64-unknown-linux-musl.tar.gz`;
- `SHA256SUMS`.

The current published `v0.1.0` does not have them yet. When a later release
does, its only prerequisites are x86_64 Linux, interactive Zsh, `tar`, and
`sha256sum`; Rust and Git are not required.

Download both files from the same [GitHub release](https://github.com/kserrec/oopsinput/releases)
into an otherwise empty directory, open a terminal in that directory, and
check that the downloaded archive matches the release's checksum receipt:

```sh
sha256sum --check SHA256SUMS
```

An `OK` result proves that the two downloaded files agree; it does not prove
that the software is safe. If the GitHub CLI is already installed, the release
also supports an optional provenance check showing that GitHub Actions built
the archive from this repository:

```sh
gh attestation verify oopsinput-*-x86_64-unknown-linux-musl.tar.gz --repo kserrec/oopsinput
```

The GitHub CLI is not otherwise needed. After the integrity check succeeds,
extract the archive, enter its single versioned directory, and start the local
installer:

```sh
tar -xzf oopsinput-*-x86_64-unknown-linux-musl.tar.gz
cd oopsinput-*/
zsh install.zsh
```

### Current source build

Until the verified archive is published, prerequisites are Linux, interactive
Zsh, Git, and Rust 1.89 or newer through [rustup](https://rustup.rs). From a
directory where you want the source checkout:

```sh
git clone https://github.com/kserrec/oopsinput.git
cd oopsinput
cargo build --release
zsh zsh/install.zsh
```

Both entry points use the same installer. On a fresh install it explains all
four modes, starts with nothing focused, and requires a direct `1`–`4` choice
or Tab followed by Enter. Ctrl-C or terminal EOF cancels before any write. For
deliberate promptless automation, the equivalent source-checkout form is:

```sh
zsh zsh/install.zsh --mode shadow
```

`shadow` may be replaced by `suggest`, `warn`, or `confirm`; there is no
implicit starting mode.

Before committing, the installer lists every effect. It:

- copies the release binary, plugin, and stable uninstaller to the paths listed
  above;
- creates a user-only config containing the mode the user selected, but only
  when no config path exists;
- backs up an existing `~/.zshrc` byte-for-byte to
  `~/.zshrc.oopsinput-backup` and keeps that original backup across updates and
  uninstall;
- adds one clearly marked source block to `~/.zshrc`.

It refuses symbolic-link and non-regular destinations. A healthy marked
`.zshrc` block is the ownership receipt that permits later updates; without it,
the installer refuses to overwrite same-named runtime files. It stages every
complete output before committing. A handled fresh-install failure restores
the shell and removes only files that invocation created; a failed update
restores the complete previous binary, plugin, and uninstaller set. Every
existing config is user-owned and remains byte-for-byte unchanged.

Open a new terminal, then run the read-only readiness check:

```sh
"$HOME/.local/bin/oopsinput" doctor
```

`doctor` checks the marked shell block and installed plugin file, all four
accept-widget wrappers from a snapshot refreshed immediately before the
doctor process in that terminal (stale snapshots fail), config validity and
effective mode, the configured Ollama model when one is enabled, and
`0700`/`0600` state permissions. A state directory that has not been created
yet is valid. The
check is read-only: it never installs, creates state, or repairs permissions.
It prints `result: ready` and exits zero only when every required check passes;
otherwise it prints `result: problems found` and exits nonzero. The installer
has copied the files when it finishes, but the new shell is not considered
ready until this check succeeds.

To update a healthy installation, run the newer archive's `zsh install.zsh`,
or rebuild a newer checkout and run `zsh zsh/install.zsh`. An update does not
show the mode chooser and preserves the existing config byte-for-byte. Passing
`--mode` when a config exists is rejected; changing modes is a separate,
deliberate config edit.

## Choose or change a mode

Use oopsinput on normal commands you already intended to run. Do not manufacture
dangerous probes: because the tool fails open and its rules are deliberately
incomplete, a real destructive command can still execute.

The fresh installer requires one of all four modes. To change it later, edit
the config file at `~/.config/oopsinput/config` unless `$XDG_CONFIG_HOME` is
set. New commands read the change immediately; no daemon needs restarting.

- **Shadow** gives a completely silent trial. Set:

  ```text
  mode = shadow
  ```

  Decisions are recorded locally when the state directory is writable, but no
  typo or danger prompt is shown.

- **Suggest** enables typo prompts. Set:

  ```text
  mode = suggest
  ```

  Misspelled command names may produce the `y`/`n` prompt described above.
  Danger decisions remain invisible and are only recorded as hypothetical
  interventions.

Use the shell normally for as long as you find useful, then inspect the local
aggregate:

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

Run the installed stable uninstaller; the source checkout or downloaded archive
is not needed. It removes the marked `.zshrc` block and the installed binary,
plugin, and its own installed copy that the block proves it owns. It keeps
configuration, recorded state, and `~/.zshrc.oopsinput-backup`; uninstalling
therefore does not silently delete user data.

If you also want the recorded state deleted, purge it **before** removing the
binary:

```sh
"$HOME/.local/bin/oopsinput" purge
zsh "$HOME/.local/share/oopsinput/uninstall.zsh"
```

`purge` keeps the config. After uninstalling, open a new terminal. If you want
no remaining configuration or backup, inspect and remove those two files
yourself; the uninstaller will not claim that authority.

## Project status and development

[`v0.1.0`](https://github.com/kserrec/oopsinput/releases/tag/v0.1.0) is the
first public alpha. The deterministic product, optional local-model path, local
reporting, 30-day retention, purge, and clean-machine install-to-uninstall
lifecycle are implemented and continuously checked. [PLAN.md](PLAN.md) is the
live status record.

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
Release engineering additionally installs Rust 1.89.0's
`x86_64-unknown-linux-musl` target, then runs
`scripts/build-release-bundle.zsh` followed by
`scripts/release-bundle-gate.zsh` on the resulting archive. The pinned release
workflow performs those steps before attestation or publication.

## License

Apache-2.0
