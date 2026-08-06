# oopsinput

**Catches commands that probably aren't what you meant — before they run.**

> **Pre-alpha — read the labels.** Everything below is marked with what it
> actually does today: on by default, built but off by default, or not built.
> Nothing on this page is a plan described as if it already shipped.

You press Enter. In a few milliseconds, oopsinput checks what you typed:

- **On by default.** `gti pull` → `'gti' not found — did you mean 'git pull'?
  [y/n]` — before the error, not after.
- **Built, off by default.** `git reset --hard` with 17 uncommitted files →
  a warning that names what's about to be lost, and offers to edit, cancel,
  or run it anyway.
- **Built, off by default.** The same `git reset --hard` on a clean tree →
  nothing. Silence. That's the point: the same command is fine in one context
  and a mistake in another, so oopsinput judges the context, not just the
  command.
- **Not built.** The optional local-model check for the rare command that is
  genuinely ambiguous after all the deterministic checks have run.

"Off by default" is deliberate sequencing, not a half-finished feature. The
danger rules work and are tested, but until they've been measured against a
few thousand real commands, they run in **shadow mode**: every decision is
computed and recorded locally, and none of it is shown. Turning them on is a
one-line config change (`mode = warn`), and the recorded data is what will
eventually justify enabling a category by default.

**You always run the show.** It never executes anything you didn't explicitly
consent to, never silently rewrites a command, and never blocks you — every
prompt has a "run it anyway" key. If anything inside it fails, your command
runs untouched.

Local-only: no telemetry, and today **no network access of any kind**. (The
design reserves one future exception: a loopback call to an optional local
model via [Ollama](https://ollama.com) for genuinely ambiguous commands. That
is not built, and it will be opt-in when it is.)

## Status

**Pre-alpha, under active development.** Linux + interactive Zsh only.

Built and working: command capture that provably never alters your buffer, a
conservative command lexer, the typo layer with its single-key prompt, the
danger and context layers, the policy engine with its intervention budget,
the warning interface (edit / cancel / run-once), and local event recording.
Not built: the optional local-model layer. Not yet done: the shadow-mode
pilot that decides which warnings earn the right to appear by default, and
release engineering (CI, `SECURITY.md`, the `report` command).

See [SPEC.md](SPEC.md) for the full design, [PLAN.md](PLAN.md) for
milestone-by-milestone progress, and
[ARCHITECTURE.md](ARCHITECTURE.md#8-known-limitations) for a plain list of
what today's code does not do.

## Design in one breath

A zsh widget captures the buffer at Enter → a single Rust binary (no daemon)
runs the analysis layers — typo, danger, context, and eventually optional
local-model inference — → a deterministic policy decides: allow silently,
suggest, warn, or ask. The three deterministic layers are built; the model
layer is not. Shadow mode (observe, never interrupt) is the default for
everything until logged data says a category has earned the right to speak;
typo suggestions are the one exception, enabled from the start because they
only ever fire on a command that couldn't have run anyway.

## Developing

[ARCHITECTURE.md](ARCHITECTURE.md) is the developer guide: what each piece
does, how a command flows through, and how it's all tested. Quick start
(needs [rustup](https://rustup.rs), zsh, git, and util-linux `script`):

```
cargo build --release
cargo test
```

To try it on your own shell — installs in suggest mode, adds one marked
block to `~/.zshrc` (backed up first), and is undone by
`zsh/uninstall.zsh`:

```
zsh/install.zsh
```

To also see danger warnings, set this in `~/.config/oopsinput/config`:

```
mode = warn
```

## License

Apache-2.0
