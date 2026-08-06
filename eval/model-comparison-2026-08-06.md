# Paired-corpus comparison: deterministic-only vs +model (M4 item 5)

**Date:** 2026-08-06 · **Decision: the model does NOT join the default
config.** `model =` stays empty; default installs remain deterministic-only.

## Method

Harness: `policy::tests::model_paired_comparison` (ignored test, run by hand):

```
cargo test model_paired_comparison -- --ignored --nocapture
```

It replays every gate-eligible golden case — the five ambiguous `observe`
cases, the only ones a live run would consult — through the real
`infer::consult` against local Ollama, with the fixture context and a 60 s
per-case evaluation deadline (30× the product's 2 s default, to measure the
model's judgment separately from its speed), then reports what
`policy::apply_model_evidence` would have done.

SPEC §11 bar: the model joins the default config only if it beats
deterministic-only in ≥ 2 categories without raising the intervention rate.

## A structural fact, stated before the numbers

On this corpus the bar is unreachable by construction: every gate-eligible
case's expected decision is `observe`, so a model answer either changes
nothing or raises the intervention rate. The corpus's expected decisions are
the ground truth, and deterministic policy already scores 19/19. What this
measurement CAN establish is (a) whether the model at least does no harm on
the ambiguous cases, and (b) whether reference-class local models are usable
at all inside the product's latency budget. Showing the model *beating*
deterministic policy would need cases with ground truth beyond deterministic
reach — deliberate-mistake fixtures, which the M5 pilot is designed to
produce.

## Results — qwen3:1.7b (CPU)

| case | category | model said | verdict changed? |
|---|---|---|---|
| reset-hard-status-unavailable | git.reset_hard | no_mismatch_evidence | no |
| dd-to-image-file | system.dd_of | possible_mismatch | no (not an upgrade arm) |
| mkfs-image-file | system.mkfs | possible_mismatch | no |
| pkg-remove-not-graduated | system.pkg_remove | no_mismatch_evidence | no |
| redirect-truncate-not-graduated | fs.redirect_truncate | possible_mismatch | no |

- Schema compliance: **5/5 valid** structured output (the transport and
  validation layers work against a real model).
- Categories improved: **0**. Intervention rate: **unchanged** (0 upgrades).
- Reason quality: poor — mostly confabulation. Examples: *"command uses
  system.pkg_remove but git repository is not in git"*; *"target directory
  'results.csv' vs command writing to file 'results.csv'"*. Nothing a user
  should ever be shown as justification for an interruption.
- Latency: ~208 s for 5 cases including cold load; ~35 s per case warm, on
  CPU. The product's default `model_timeout_ms` is 2 000, and SPEC §10's
  warm target is p50 < 1 s: **this machine misses the budget by ~35×.** In
  a real session every consultation would time out and fall back.

## Results — qwen3:8b (CPU)

All 5 cases: `model.timeout` at the 60 s evaluation deadline. Unusable on
this hardware. (The silent-fallback path behaved exactly as designed in
all five.)

## Decision against the SPEC §11 bar

- Beats deterministic-only in ≥ 2 categories: **no** (0 categories).
- Without raising the intervention rate: satisfied trivially (nothing
  changed), but only because the model never produced an upgrade arm —
  three `possible_mismatch` answers on benign fixtures suggest it would
  false-positive if `possible` ever became an intervention tier.
- SPEC §10 model budget: **not met** on this hardware by either model.

**Model stays out of the default config.** The wiring, gate, fallback, and
schema validation are all proven against live models; the model itself has
not earned a default seat.

## What could reopen the question (v1.x / M5+)

1. Deliberate-mistake fixtures from the M5 pilot — corpus cases whose
   ground truth deterministic policy provably cannot reach (the only way
   the §11 bar becomes winnable at all).
2. GPU-backed Ollama or a faster ~1B-class instruct model bringing warm
   latency inside the 2 s timeout (SPEC's reference class was "a ~4B
   instruction model"; on this CPU that class is out of reach).
3. Reason quality good enough to show a user (SPEC §2-4 "specific beats
   generic" — today's reasons are worse than saying nothing).
