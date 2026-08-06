# oopsinput

**Catches commands that probably aren't what you meant — before they run.**

You press Enter. In a few milliseconds, oopsinput checks what you typed:

- `gti pull` → `'gti' not found — did you mean 'git pull'? [y/n]` — before the
  error, not after
- `rm -rf .` in a dirty repo root right after you were inspecting `./build` →
  a pause that names exactly what's about to be deleted and why it looks off
- `git reset --hard` on a clean scratch branch → nothing. Silence. That's the
  point: the same command is fine in one context and a mistake in another, and
  oopsinput judges the context, not just the command.

**You always run the show.** It never executes anything you didn't explicitly
consent to, never silently rewrites a command, and never blocks you — every
prompt has a "run it anyway" key. If anything inside it fails, your command
runs untouched.

Local-only. No telemetry, no network (except loopback to an optional local
model via [Ollama](https://ollama.com) for the rare genuinely ambiguous case).
Deterministic analysis handles ~99% of commands in milliseconds.

## Status

**Pre-alpha, under active development.** Linux + interactive Zsh only.
See [SPEC.md](SPEC.md) for the full design and [PLAN.md](PLAN.md) for progress.
Not ready to install yet — watch the repo if you're curious.

## Design in one breath

Zsh widget captures the buffer at Enter → a single Rust binary (no daemon)
runs four layers — typo, danger, context, and optional local-model inference —
→ deterministic policy decides: allow silently, suggest, warn, or ask.
Shadow mode (observe, never interrupt) is the default until the data says a
category has earned the right to speak.

## Developing

[ARCHITECTURE.md](ARCHITECTURE.md) is the developer guide: what each piece
does, how a command flows through, and how it's all tested. Quick start
(needs [rustup](https://rustup.rs), zsh, and util-linux `script`):

```
cargo build --release
cargo test
```

## License

Apache-2.0
