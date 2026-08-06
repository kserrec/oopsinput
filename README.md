# oopsinput

**Catches commands that probably aren't what you meant — before they run.**

> **Pre-alpha — read the labels.** Of the three things below, one works today
> and two are designed but not built. Each is marked. Nothing on this page is
> a plan described as if it already shipped.

You press Enter. In a few milliseconds, oopsinput checks what you typed:

- **Works today.** `gti pull` → `'gti' not found — did you mean 'git pull'?
  [y/n]` — before the error, not after.
- **Not built yet.** `rm -rf .` in a dirty repo root right after you were
  inspecting `./build` → a pause that names exactly what's about to be
  deleted and why it looks off.
- **Not built yet.** `git reset --hard` on a clean scratch branch → nothing.
  Silence. That's the point: the same command is fine in one context and a
  mistake in another, and oopsinput should judge the context, not just the
  command.

What that means concretely today: **no command is ever flagged for being
dangerous.** Mistyped command names get a suggestion; everything else runs
exactly as you typed it and is recorded locally, so the judgment rules above
can be built and tuned against real data instead of guesses.

**You always run the show.** It never executes anything you didn't explicitly
consent to, never silently rewrites a command, and never blocks you — every
prompt has a "run it anyway" key. If anything inside it fails, your command
runs untouched.

Local-only: no telemetry, and today **no network access of any kind** — all
analysis is deterministic and runs in milliseconds. (The design reserves one
future exception: a loopback call to an optional local model via
[Ollama](https://ollama.com) for the rare genuinely ambiguous command. That
is not built, and it will be opt-in when it is.)

## Status

**Pre-alpha, under active development.** Linux + interactive Zsh only.

Built and working: command capture that provably never alters your buffer,
a conservative command lexer, the typo layer with its single-key prompt, and
local event recording. Not built: the danger, context, and local-model
layers, and the warning interface they feed.

See [SPEC.md](SPEC.md) for the full design, [PLAN.md](PLAN.md) for
milestone-by-milestone progress, and
[ARCHITECTURE.md](ARCHITECTURE.md#8-known-limitations) for a plain list of
what today's code does not do.

## Design in one breath

Zsh widget captures the buffer at Enter → a single Rust binary (no daemon)
runs up to four layers — typo, danger, context, and optional local-model
inference — → deterministic policy decides: allow silently, suggest, warn,
or ask. Only the typo layer is built so far. Shadow mode (observe, never
interrupt) is the default for everything until logged data says a category
has earned the right to speak; typo suggestions are the one exception,
enabled from the start because they only ever fire on a command that
couldn't have run anyway.

## Developing

[ARCHITECTURE.md](ARCHITECTURE.md) is the developer guide: what each piece
does, how a command flows through, and how it's all tested. Quick start
(needs [rustup](https://rustup.rs), zsh, and util-linux `script`):

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

## License

Apache-2.0
