//! oopsinput — catches commands that probably aren't what you meant, before they run.
//!
//! The full deterministic pipeline: proposal intake from the zsh plugin,
//! lexer, typo/danger/context layers, policy decision, and the two visible
//! interventions — the L1 typo prompt and the L2+ warning prompt — all under
//! a self-watchdog deadline so a wedged process can never hold the user's
//! shell. See SPEC.md (canonical) and PLAN.md.
//!
//! Exit code contract (the zsh plugin treats anything unexpected as fail-open allow):
//!   0  = allow, run the original buffer unchanged
//!   10 = replace buffer with text from fd 3 and run it   (typo `y`)
//!   11 = restore original buffer to ZLE for editing       (warning `e`)
//!   12 = cancel, run nothing                              (warning `c`, typo Ctrl-C)
//!   2  = usage error
//!   1  = internal error (plugin fails open)

mod distance;
mod doctor;
mod events;
mod layers;
mod lexer;
mod model;
mod policy;
mod proc;
mod proposal;
mod state;
mod ui;

use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use serde::Serialize;

use proposal::Proposal;

#[derive(Serialize)]
struct Decision {
    decision: &'static str,
    reason_code: &'static str,
    /// Stable evidence codes only (lexer uncertainty, input caps) — never
    /// raw command text.
    evidence: Vec<&'static str>,
    timings_us: Timings,
}

#[derive(Serialize)]
struct Timings {
    total: u128,
}

struct CheckAction {
    decision: &'static str,
    reason_code: &'static str,
    exit_code: u8,
    outcome: Option<&'static str>,
}

struct WarningChoiceAction {
    outcome: &'static str,
    ran_unchanged: bool,
    timed_out: bool,
    exit_code: u8,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("version") | Some("--version") => {
            println!("oopsinput {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("check") => check(&args[1..]),
        Some("report") => report(),
        Some("purge") => purge(&args[1..]),
        Some("doctor") => doctor::run(),
        // Test seam (debug builds only): exercise the real /dev/tty + stty
        // prompt path under a PTY, without needing the full suggest-mode flow.
        #[cfg(debug_assertions)]
        Some("__prompt-typo-test") => {
            let choice = ui::prompt_typo("gti pull", "git pull");
            println!("choice={choice:?}");
            ExitCode::SUCCESS
        }
        None | Some("help") | Some("--help") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(other) => {
            let shown = ui::escape_for_display(other);
            eprintln!("oopsinput: unknown command '{shown}' (try 'oopsinput help')");
            ExitCode::from(2)
        }
    }
}

/// Set once analysis is over and prompt setup begins: the analysis deadline no
/// longer applies (ui.rs caller contract). Terminal helpers/reads and trailing
/// state-lock contention carry their own bounds.
static PROMPT_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn prompt_is_active() -> bool {
    PROMPT_ACTIVE.load(Ordering::SeqCst)
}

/// One-shot deadline extension for the model path (SPEC §6: "the model path
/// gets its own longer deadline"). Zero until the L4 gate opens; the check
/// path stores `model_timeout_ms` + margin here *before* connecting, so a
/// consultation legitimately outlives the deterministic deadline without
/// disarming the watchdog. If the store loses the race with the watchdog's
/// read — analysis reached the gate at the very moment the deterministic
/// budget expired — the process fail-opens, which is the correct answer for
/// analysis that slow.
static MODEL_EXTENSION_MS: AtomicU64 = AtomicU64::new(0);

/// If analysis wedges for any reason, exit with the fail-open code before the
/// shell-side wait becomes perceptible. The process is per-command, so a blunt
/// exit is safe: there is nothing to clean up that matters more than the
/// user's prompt.
fn arm_watchdog(deadline_ms: u64) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(deadline_ms));
        if PROMPT_ACTIVE.load(Ordering::SeqCst) {
            // Prompt path active: the watchdog retires. Trailing state writes
            // use bounded, append-only coordination and never run retention.
            return;
        }
        let extension = MODEL_EXTENSION_MS.load(Ordering::SeqCst);
        if extension > 0 {
            // Model consultation in flight — grant its bounded window (the
            // consult's own socket deadline is strictly shorter), then
            // enforce as usual.
            std::thread::sleep(std::time::Duration::from_millis(extension));
            if PROMPT_ACTIVE.load(Ordering::SeqCst) {
                return;
            }
        }
        std::process::exit(1);
    });
}

/// Test hooks exist in debug builds only (the PTY suite runs against the
/// debug profile); release binaries take the deadline from config (SPEC §15
/// det_timeout_ms, validated and range-clamped in policy.rs).
fn deadline_ms(configured: u64) -> u64 {
    #[cfg(debug_assertions)]
    if let Ok(v) = std::env::var("OOPSINPUT_TEST_DEADLINE_MS") {
        return v.parse().unwrap_or(policy::DET_TIMEOUT_DEFAULT_MS);
    }
    configured
}

/// Read one proposal from stdin (+ adapter flags), analyze, record a shadow
/// event, print a Decision as JSON on stdout, signal via exit code.
fn check(args: &[String]) -> ExitCode {
    let started = Instant::now();
    // Config first, watchdog immediately after: the read is a bounded open +
    // capped read of one small local file — the only thing that could hang it
    // is a hung home filesystem, the same documented residual boundary that
    // already applies to every state write. Everything with real work in it
    // runs under the watchdog.
    let cfg = policy::load_config();
    arm_watchdog(deadline_ms(cfg.det_timeout_ms));
    policy::emit_config_warnings_once(&cfg);

    // Test hook (debug builds only): prove the watchdog end-to-end — a plugin
    // pointed at a hanging binary must still run the user's command.
    #[cfg(debug_assertions)]
    if std::env::var("OOPSINPUT_TEST_HANG").is_ok() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    let Ok(proposal) = Proposal::from_check_invocation(args) else {
        return ExitCode::from(1);
    };

    // Lex for structure and honest uncertainty (SPEC §13).
    let lexed = lexer::lex(&proposal.buffer);
    let cmd_expands = lexer::command_words(&lexed)
        .first()
        .is_some_and(|w| w.expands);
    let word_count = lexed
        .tokens
        .iter()
        .filter(|t| matches!(t, lexer::Token::Word(_)))
        .count();
    // L1 typo layer (SPEC §5-L1): only meaningful when the command word
    // resolves to nothing. Distance is structural, never the names.
    let suggestion = layers::typo::analyze(proposal.res_kind, &lexed, &proposal.names);

    // L2 danger layer (SPEC §5-L2): deterministic candidate marking. It never
    // intervenes on its own — policy consumes it below.
    let danger = layers::danger::analyze(&lexed);

    // L3 context layer (SPEC §5-L3, deterministic half): fresh git and
    // filesystem facts, collected only when L2 marked a candidate — the
    // common path stays syscall-free.
    let context = (!danger.codes.is_empty()).then(|| layers::context::collect(&danger.targets));

    let warranted = policy::warranted(&danger, context.as_ref());
    let consulted = consult_model_if_gated(&cfg, warranted, &proposal, &danger, context.as_ref());

    let evidence = build_evidence(
        lexed.uncertainty,
        proposal.capped,
        proposal.names_capped,
        suggestion.as_ref(),
        &danger,
        context.as_ref(),
        &proposal.recency,
        consulted.as_ref().map(|c| &c.outcome),
    );

    // Analysis-only duration: the prompt below waits on a human and must not
    // pollute the latency percentiles the budgets are measured against.
    let duration_us = started.elapsed().as_micros();

    // The strongest intervention wins: a gated warn/confirm outranks the L1
    // typo prompt. `gti status; rm -rf /` has an unresolvable first word AND
    // a catastrophic later segment — `;` does not short-circuit, so the rm
    // runs either way the typo question is answered; the user must see the
    // warning, not a chat about gti (bughunt 2026-08-06). Otherwise: the
    // typo prompt in suggest mode and up, else record silently with the
    // policy reason preserved (the shadow conversion).
    let assessed = policy::apply_model_evidence(warranted, consulted.as_ref().map(|c| &c.outcome));
    let hypothetical_reason = (matches!(cfg.mode, policy::Mode::Shadow | policy::Mode::Suggest)
        && matches!(
            assessed.verdict,
            policy::Verdict::Warn | policy::Verdict::Confirm
        ))
    .then_some(assessed.reason);
    let capped = policy::cap_for_mode(assessed, cfg.mode);
    let action = match capped.verdict {
        policy::Verdict::Warn | policy::Verdict::Confirm => warning_intervention(
            capped,
            &danger,
            context.as_ref(),
            &proposal.recency,
            consulted.as_ref().and_then(|c| c.outcome.evidence()),
            &cfg,
        ),
        _ => match &suggestion {
            Some(s) if cfg.mode != policy::Mode::Shadow => typo_intervention(&proposal.buffer, s),
            _ => CheckAction {
                decision: capped.verdict.as_str(),
                reason_code: capped.reason,
                exit_code: 0,
                outcome: None,
            },
        },
    };

    let decision = Decision {
        decision: action.decision,
        reason_code: action.reason_code,
        evidence,
        timings_us: Timings { total: duration_us },
    };

    events::append(&events::Event {
        ts_ms: events::now_ms(),
        decision: decision.decision,
        reason_code: decision.reason_code,
        evidence: decision.evidence.clone(),
        res_kind: proposal.res_kind.as_str(),
        cmd_expands,
        buffer_bytes: proposal.buffer.len(),
        word_count,
        duration_us,
        hypothetical_reason,
        model_state: consulted.as_ref().map(|c| c.state.as_str()),
        ctx_git_dirty: context
            .as_ref()
            .and_then(|c| c.git.as_ref())
            .and_then(|g| g.dirty),
        ctx_target_entries: context
            .as_ref()
            .and_then(|c| c.targets.iter().filter_map(|t| t.entries).max()),
        outcome: action.outcome,
    });

    // The decision JSON is diagnostics; the exit code is the contract. Once
    // the replacement is on fd 3 the code MUST be 10 — a print failure here
    // must not flip a consented correction back into fail-open (bughunt
    // 2026-08-06; unreachable today with all-static fields, pinned anyway).
    if let Ok(json) = serde_json::to_string(&decision) {
        println!("{json}");
    }
    ExitCode::from(action.exit_code)
}

/// Assemble the decision's evidence codes in stable order: lexer uncertainty
/// (first-seen), input caps, typo findings, danger findings, then context
/// facts. The golden corpus pins this exact assembly — static strings only,
/// never raw text (context *counts* travel as event fields, not codes).
#[allow(clippy::too_many_arguments)] // one flat assembly point, by design
fn build_evidence(
    lexer_uncertainty: Vec<&'static str>,
    capped: bool,
    names_capped: bool,
    suggestion: Option<&layers::typo::Suggestion>,
    danger: &layers::danger::Analysis,
    context: Option<&layers::context::Context>,
    recency: &[proposal::RecencyEntry],
    consulted: Option<&layers::infer::Consult>,
) -> Vec<&'static str> {
    let mut evidence = lexer_uncertainty;
    if capped {
        evidence.push("input.capped");
    }
    if names_capped {
        evidence.push("input.names_capped");
    }
    if let Some(s) = suggestion {
        evidence.push(if s.distance == 1 {
            "typo.candidate_d1"
        } else {
            "typo.candidate_d2"
        });
    }
    evidence.extend(danger.codes.iter().copied());
    if danger.catastrophic {
        evidence.push("danger.direct_catastrophic");
    }
    if let Some(ctx) = context {
        if let Some(g) = &ctx.git {
            evidence.push("ctx.git_repo");
            if g.detached {
                evidence.push("ctx.git_detached");
            }
            if g.branch_main_like {
                evidence.push("ctx.git_main_branch");
            }
            match g.dirty {
                Some(0) => {}
                Some(_) => evidence.push("ctx.git_dirty"),
                None => evidence.push("ctx.git_unavailable"),
            }
            if g.untracked == Some(true) {
                evidence.push("ctx.git_untracked");
            }
        }
        let t = &ctx.targets;
        for (present, code) in [
            (t.iter().any(|t| !t.exists), "ctx.target_missing"),
            (t.iter().any(|t| t.is_symlink), "ctx.target_symlink"),
            (t.iter().any(|t| t.is_cwd), "ctx.target_is_cwd"),
            (t.iter().any(|t| t.is_parent), "ctx.target_is_parent"),
            (t.iter().any(|t| t.near_miss), "ctx.near_miss_target"),
        ] {
            if present {
                evidence.push(code);
            }
        }
    }
    // Recency (SPEC §5-L3): only candidate events carry it — on benign
    // commands word overlap with recent history is routine, not evidence.
    if !danger.codes.is_empty() && recency.iter().any(|r| r.shares_word) {
        evidence.push("recency.target_overlap");
    }
    // Model consultation outcome (SPEC §5-L4): the assessment when one
    // arrived, or the stable unavailability code — recorded either way so
    // evaluation can tell deterministic fallback from model agreement.
    match consulted {
        Some(layers::infer::Consult::Evidence(e)) => evidence.push(e.assessment.evidence_code()),
        Some(layers::infer::Consult::Unavailable(code)) => evidence.push(code),
        None => {}
    }
    evidence
}

/// L4 inference layer (SPEC §5-L4): consult only when a model is configured
/// (default: none) AND the candidate gate opens — danger marked it, context
/// left it genuinely ambiguous, and it is not direct-catastrophic. Owns the
/// watchdog's one-shot deadline extension, armed before the first socket
/// call; consult() itself is bounded strictly tighter.
fn consult_model_if_gated(
    cfg: &policy::Config,
    warranted: policy::Assessment,
    proposal: &Proposal,
    danger: &layers::danger::Analysis,
    context: Option<&layers::context::Context>,
) -> Option<layers::infer::Consultation> {
    let name = cfg.model.as_ref()?;
    if !policy::l4_gate(danger, warranted) {
        return None;
    }
    MODEL_EXTENSION_MS.store(cfg.model_timeout_ms + 1_000, Ordering::SeqCst);
    Some(layers::infer::consult_with_state(
        name,
        cfg.model_timeout_ms,
        proposal,
        danger,
        context,
    ))
}

/// The L1 prompt flow. Exit 10 means "replacement delivered on fd 3", and is
/// only ever returned after the full replacement (sentinel included) was
/// written successfully; every failure degrades to exit 0, running the
/// original unchanged (SPEC §9-8).
fn typo_intervention(buffer: &str, s: &layers::typo::Suggestion) -> CheckAction {
    // Construct the replacement BEFORE asking: if it cannot be built with
    // byte-exact certainty there is nothing to offer.
    let Some(replacement) = layers::typo::replacement_buffer(buffer, &s.typed, &s.candidate) else {
        return CheckAction {
            decision: "allow",
            reason_code: "shadow.observed",
            exit_code: 0,
            outcome: None,
        };
    };
    PROMPT_ACTIVE.store(true, Ordering::SeqCst);
    let (decision, reason_code, exit_code) = match ui::prompt_typo(buffer, &replacement) {
        ui::TypoChoice::Correct => {
            if write_replacement_fd3(&replacement).is_ok() {
                ("replace", "typo.accepted", 10)
            } else {
                ("allow", "typo.delivery_failed", 0)
            }
        }
        ui::TypoChoice::Original => ("allow", "typo.declined", 0),
        ui::TypoChoice::Cancel => ("cancel", "typo.cancelled", 12),
        ui::TypoChoice::Timeout => ("allow", "typo.timed_out", 0),
    };
    CheckAction {
        decision,
        reason_code,
        exit_code,
        outcome: None,
    }
}

/// The L2+ warning flow (SPEC §7): gate through budget and cooldown, show
/// the prompt, act on the answer, record the outcome. Exit codes: 11 =
/// restore buffer for editing, 12 = cancel, 0 = run unchanged — the plugin
/// holds up its end of each.
fn warning_intervention(
    assessment: policy::Assessment,
    danger: &layers::danger::Analysis,
    context: Option<&layers::context::Context>,
    recency: &[proposal::RecencyEntry],
    model: Option<&layers::infer::ModelEvidence>,
    cfg: &policy::Config,
) -> CheckAction {
    let rule = policy::primary_code(danger);
    let admission = policy::admit_intervention(
        assessment,
        rule,
        danger.catastrophic,
        events::now_ms(),
        cfg.budget_per_hour,
    );
    let gated = admission.assessment;
    let reservation = admission.reservation;
    if !matches!(
        gated.verdict,
        policy::Verdict::Warn | policy::Verdict::Confirm
    ) {
        // budget exhausted or rule in cooldown: degrade to shadow recording
        return CheckAction {
            decision: gated.verdict.as_str(),
            reason_code: gated.reason,
            exit_code: 0,
            outcome: None,
        };
    }

    let lines = ui::warning_lines(gated.reason, danger, context, recency, model);
    let pausing = gated.verdict == policy::Verdict::Confirm;
    PROMPT_ACTIVE.store(true, Ordering::SeqCst);
    let choice = match ui::prompt_warning(&lines) {
        ui::WarningPrompt::NotShown => {
            policy::release_admission(reservation, events::now_ms());
            return CheckAction {
                decision: gated.verdict.as_str(),
                reason_code: gated.reason,
                exit_code: 0,
                outcome: None,
            };
        }
        ui::WarningPrompt::Shown(choice) => choice,
    };
    let action = warning_choice_action(choice, pausing);
    // The admission already reserved exactly one budget slot. Completing it
    // swaps that short-lived reservation for the shown prompt's outcome.
    if let Some(code) = rule {
        policy::record_admitted_outcome(
            reservation,
            code,
            action.ran_unchanged,
            action.timed_out,
            events::now_ms(),
        );
    }
    CheckAction {
        decision: gated.verdict.as_str(),
        reason_code: gated.reason,
        exit_code: action.exit_code,
        outcome: Some(action.outcome),
    }
}

/// Keep what physically happens after a timeout separate from what the user
/// did. Advisory Warn still runs unchanged and pausing Confirm still cancels,
/// but both are recorded as `timed_out`, never as a deliberate `r` or `c`.
fn warning_choice_action(choice: ui::WarnChoice, pausing: bool) -> WarningChoiceAction {
    match choice {
        ui::WarnChoice::Edit => WarningChoiceAction {
            outcome: "edited",
            ran_unchanged: false,
            timed_out: false,
            exit_code: 11,
        },
        ui::WarnChoice::Cancel => WarningChoiceAction {
            outcome: "cancelled",
            ran_unchanged: false,
            timed_out: false,
            exit_code: 12,
        },
        ui::WarnChoice::RunOnce => WarningChoiceAction {
            outcome: "ran_unchanged",
            ran_unchanged: true,
            timed_out: false,
            exit_code: 0,
        },
        ui::WarnChoice::Timeout => WarningChoiceAction {
            outcome: "timed_out",
            ran_unchanged: !pausing,
            timed_out: true,
            exit_code: if pausing { 12 } else { 0 },
        },
    }
}

/// Deliver the replacement to the plugin: exact bytes on fd 3, terminated by
/// a single NUL sentinel. The sentinel survives zsh's command-substitution
/// trailing-newline stripping and doubles as an integrity mark — the plugin
/// runs the replacement only if the NUL is present, so a truncated write can
/// never execute a truncated command. NUL itself cannot occur in the
/// replacement (zsh strings and filenames cannot contain it).
///
/// fd 3 is re-opened via /dev/fd/3: std exposes no safe handle to an
/// inherited raw fd, and `from_raw_fd` is unsafe (banned here without prior
/// discussion — CLAUDE.md). /dev/fd works on Linux and the BSDs.
fn write_replacement_fd3(replacement: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().write(true).open("/dev/fd/3")?;
    f.write_all(replacement.as_bytes())?;
    f.write_all(b"\0")?;
    f.flush()
}

fn report() -> ExitCode {
    match events::report_text() {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!(
                "oopsinput report: {}",
                ui::escape_for_display(&error.to_string())
            );
            ExitCode::from(1)
        }
    }
}

fn purge(args: &[String]) -> ExitCode {
    if !args.is_empty() {
        eprintln!("oopsinput purge: this command takes no arguments");
        return ExitCode::from(2);
    }
    match state::purge() {
        Ok(result) => {
            println!("oopsinput purge");
            if result.removed_files == 0 {
                println!("  nothing to purge");
            } else {
                let noun = if result.removed_files == 1 {
                    "file"
                } else {
                    "files"
                };
                println!("  removed {} state {noun}", result.removed_files);
            }
            if result.directory_retained {
                println!(
                    "  kept the state directory because it contains new or unrecognized entries"
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!(
                "oopsinput purge: {}",
                ui::escape_for_display(&error.to_string())
            );
            ExitCode::from(1)
        }
    }
}

fn print_help() {
    println!(
        "oopsinput {} — catches commands that probably aren't what you meant\n\n\
         usage: oopsinput <command>\n\n\
         commands:\n\
         \x20 check    read a command proposal on stdin, print a decision (used by the zsh plugin)\n\
         \x20 report   summarize recorded decisions, latency, and evidence\n\
         \x20 purge    delete all recorded state (configuration is kept)\n\
         \x20 doctor   check the installation and environment\n\
         \x20 version  print version\n\
         \x20 help     this text",
        env!("CARGO_PKG_VERSION")
    );
}

/// Test support: load a golden corpus and enforce the SPEC §11 paired-case
/// discipline (≥30% counterfactual pairs) — the one home for that threshold,
/// shared by every corpus runner.
#[cfg(test)]
pub(crate) fn golden_cases<T: serde::de::DeserializeOwned>(
    file: &str,
    is_paired: impl Fn(&T) -> bool,
) -> Vec<T> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("eval/golden")
        .join(file);
    let text = std::fs::read_to_string(&path).expect("read golden corpus");
    let cases: Vec<T> = serde_json::from_str(&text).expect("parse golden corpus");
    assert!(!cases.is_empty());
    let paired = cases.iter().filter(|c| is_paired(c)).count();
    assert!(
        paired * 100 >= cases.len() * 30,
        "SPEC §11: ≥30% of golden cases must be counterfactual pairs ({paired}/{} are)",
        cases.len()
    );
    cases
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_timeout_keeps_outcome_distinct_from_physical_default() {
        // SPEC §4 requires `timed_out` as its own outcome. Warn remains
        // fail-open while Confirm remains pausing, but neither default is a
        // deliberate run/cancel choice.
        let warn = warning_choice_action(ui::WarnChoice::Timeout, false);
        assert_eq!(warn.outcome, "timed_out");
        assert!(warn.ran_unchanged && warn.timed_out);
        assert_eq!(warn.exit_code, 0);

        let confirm = warning_choice_action(ui::WarnChoice::Timeout, true);
        assert_eq!(confirm.outcome, "timed_out");
        assert!(!confirm.ran_unchanged && confirm.timed_out);
        assert_eq!(confirm.exit_code, 12);

        let deliberate = warning_choice_action(ui::WarnChoice::RunOnce, true);
        assert_eq!(deliberate.outcome, "ran_unchanged");
        assert!(deliberate.ran_unchanged && !deliberate.timed_out);
    }

    /// SPEC §11 golden corpus, L1 slice: each case is buffer + resolution
    /// context + candidate names → expected suggestion and evidence codes.
    /// Hermetic — candidates come only from the fixture, never real PATH.
    #[test]
    fn golden_typo_corpus() {
        #[derive(serde::Deserialize)]
        struct Case {
            name: String,
            /// Set on counterfactual pairs: same buffer, different context,
            /// different expectation (SPEC §11 paired-case discipline).
            #[serde(default)]
            pair: Option<String>,
            buffer: String,
            res: String,
            #[serde(default)]
            names: Vec<String>,
            expect_candidate: Option<String>,
            expect_evidence: Vec<String>,
        }

        let cases: Vec<Case> = golden_cases("typo.json", |c: &Case| c.pair.is_some());

        for c in &cases {
            let lexed = lexer::lex(&c.buffer);
            let res = proposal::ResolutionKind::parse(&c.res);
            let suggestion = layers::typo::analyze_with_path(res, &lexed, &c.names, "");
            assert_eq!(
                suggestion.as_ref().map(|s| s.candidate.as_str()),
                c.expect_candidate.as_deref(),
                "case '{}': wrong candidate",
                c.name
            );
            let danger = layers::danger::analyze_with_home(&lexed, None);
            let evidence = build_evidence(
                lexed.uncertainty,
                false,
                false,
                suggestion.as_ref(),
                &danger,
                None,
                &[],
                None,
            );
            assert_eq!(
                evidence,
                c.expect_evidence
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                "case '{}': wrong evidence",
                c.name
            );
        }
    }

    /// SPEC §11 golden corpus, L2 slice: buffer → expected danger codes and
    /// direct-catastrophic flag. Hermetic — "/home/u" stands in for $HOME.
    /// Pairs here are command-shape counterfactuals (same command family,
    /// benign variant, no evidence). The context-flip pairs (same command,
    /// clean-vs-dirty repo) land with policy + L3 in M3, where context can
    /// actually change the outcome.
    #[test]
    fn golden_danger_corpus() {
        #[derive(serde::Deserialize)]
        struct Case {
            name: String,
            #[serde(default)]
            pair: Option<String>,
            buffer: String,
            expect_evidence: Vec<String>,
            #[serde(default)]
            expect_catastrophic: bool,
        }

        let cases: Vec<Case> = golden_cases("danger.json", |c: &Case| c.pair.is_some());

        for c in &cases {
            let lexed = lexer::lex(&c.buffer);
            let analysis = layers::danger::analyze_with_home(&lexed, Some("/home/u"));
            assert_eq!(
                analysis.codes,
                c.expect_evidence
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                "case '{}': wrong danger codes",
                c.name
            );
            assert_eq!(
                analysis.catastrophic, c.expect_catastrophic,
                "case '{}': wrong catastrophic flag",
                c.name
            );
        }
    }

    // Mode vocabulary and the SPEC §15 config surface are policy.rs's
    // domain now — their tests moved there with the code.

    /// CLAUDE.md: "Every danger rule ships with a counterfactual pair (same
    /// command, context where it's silently allowed)." The ≥30% pair ratio
    /// checks the corpus in aggregate, so it cannot notice a *new* rule that
    /// arrives with no cases at all — test-audit 2026-08-06 added a `shred`
    /// rule to the danger layer and watched all 164 tests pass anyway. This
    /// closes that hole: every evidence code the layer can emit must appear
    /// in the corpus, and the emittable set is read from the source itself,
    /// so a rule added without a case fails here instead of shipping unseen.
    #[test]
    fn every_danger_rule_appears_in_the_golden_corpus() {
        let src =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/layers/danger.rs"))
                .expect("read danger layer source");

        // The codes the rules emit: `.note("code")` before the test module.
        let rules_only = src.split("mod tests").next().unwrap_or(&src);
        let mut emitted: Vec<&str> = Vec::new();
        for (idx, _) in rules_only.match_indices(".note(\"") {
            let rest = &rules_only[idx + ".note(\"".len()..];
            if let Some(end) = rest.find('"')
                && !emitted.contains(&&rest[..end])
            {
                emitted.push(&rest[..end]);
            }
        }
        assert!(
            emitted.len() > 10,
            "scan found only {} codes — the extraction broke, not the corpus",
            emitted.len()
        );

        let corpus = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/eval/golden/danger.json"
        ))
        .expect("read danger corpus");
        let uncovered: Vec<&&str> = emitted
            .iter()
            .filter(|code| !corpus.contains(**code))
            .collect();
        assert!(
            uncovered.is_empty(),
            "danger rules with no golden case: {uncovered:?} — every rule needs \
             a case, and a counterfactual pair per CLAUDE.md"
        );
    }
}
