# oopsinput — working agreement

SPEC.md is canonical; PLAN.md tracks milestones. When implementation and SPEC
disagree, stop and update SPEC first (with Kyle), then code.

Documentation must stay faithful to reality at all times (Kyle, 2026-08-06).
Purely descriptive drift — SPEC §16's tree, module lists, "what exists
today" claims in any doc — is standing-approved: fix it on sight. Only
substantive SPEC changes (behavior, scope, security posture) need Kyle's
sign-off first.

## Build / test

Rust via rustup (user-level). If `cargo` isn't on PATH in a fresh shell:
`. "$HOME/.cargo/env"`.

- `cargo build --release` — the binary users run; perf claims are release-only
- `cargo test` — unit + golden corpus
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings` — must be
  clean before any commit

## Style: lean, mostly functional

- Plain data structs/enums + free functions. No trait until there are two real
  implementations. No generics for one concrete type. No builder patterns for
  three-field structs.
- **Sync only.** No async, no tokio. This is a per-command CLI binary.
- Errors: small enums per module, `?` propagation. No `unwrap`/`expect` outside
  tests and provably-infallible spots (comment the proof).
- No `unsafe` without prior discussion.
- Performance and accuracy are paramount, in that order of *visibility*:
  the common path must be imperceptible; interventions must be right.
- Prefer boring code. If a function needs a comment to explain flow, simplify
  the flow.

## Dependencies

Allowlist is SPEC §12 (currently: serde, serde_json — nothing else, dev-deps
included). Adding one = update SPEC §12 table with a one-line defense + ask
Kyle first. Self-write small slivers (HTTP loopback client, lexer, edit
distance, config parsing, CLI dispatch).

## Security invariants (SPEC §9 — never weaken to make a test pass)

1. Never execute/expand/source/evaluate any part of an analyzed command. No
   shell invocation during analysis; external read-only helpers use fixed
   argv + timeout only.
2. Original buffer bytes preserved exactly through every path.
3. No raw commands/paths/secrets in default logs; user-only file perms.
4. All displayed untrusted text goes through the escaper.
5. Model output is untrusted; schema-invalid ⇒ unavailable evidence; the model
   can never cause execution or denial.
6. Fail open: on any internal failure the user's original command runs
   unchanged.

## Testing rules

- Tests are derived from failure modes, never written as ritual. Build the
  feature first, then hunt for the ways it can actually go wrong; when
  reasonably possible, prove a suspected failure is real (probe it, watch it
  happen) before writing the test that pins it — and note the probe in the
  test comment. The only other source of tests is surprise: something broke
  that we didn't predict, now found and proven (bug fix ⇒ fixture, below).
  No speculative tests for failures nobody has named.
- Every layer lands with unit tests + golden cases in the same commit.
- Every danger rule ships with a counterfactual pair (same command, context
  where it's silently allowed) — CI enforces the ≥30% pair ratio.
- Bug fix ⇒ regression fixture.
- Zsh plugin changes ⇒ PTY tests (buffer exactness is the product).
- Never commit real shell history or personal data into fixtures.

## Workflow

- Work in milestone order from PLAN.md; check items off as they land.
- Small commits scoped to one milestone item.
- Kyle doesn't know Rust (yet): PR-sized explanations in commit messages;
  when he asks questions, answer — don't change code unless asked (see his
  global rules).
- The user runs the show — in the product and in this repo.
