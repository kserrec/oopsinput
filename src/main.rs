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
mod events;
mod layers;
mod lexer;
mod model;
mod policy;
mod proc;
mod proposal;
mod state;
mod ui;

use std::io::{Read, Write};
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
        Some("doctor") => doctor(),
        // Test seam (debug builds only): exercise the real /dev/tty + stty
        // prompt path under a PTY, without needing the full suggest-mode flow.
        #[cfg(debug_assertions)]
        Some("__prompt-typo-test") => {
            let choice = ui::prompt_typo("gti", "git");
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
    let (decision_str, reason_code, exit_code, outcome) = match capped.verdict {
        policy::Verdict::Warn | policy::Verdict::Confirm => warning_intervention(
            capped,
            &danger,
            context.as_ref(),
            &proposal.recency,
            consulted.as_ref().and_then(|c| c.outcome.evidence()),
            &cfg,
        ),
        _ => match &suggestion {
            Some(s) if cfg.mode != policy::Mode::Shadow => {
                let (d, r, e) = typo_intervention(&proposal.buffer, s);
                (d, r, e, None)
            }
            _ => (capped.verdict.as_str(), capped.reason, 0u8, None),
        },
    };

    let decision = Decision {
        decision: decision_str,
        reason_code,
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
        outcome,
    });

    // The decision JSON is diagnostics; the exit code is the contract. Once
    // the replacement is on fd 3 the code MUST be 10 — a print failure here
    // must not flip a consented correction back into fail-open (bughunt
    // 2026-08-06; unreachable today with all-static fields, pinned anyway).
    if let Ok(json) = serde_json::to_string(&decision) {
        println!("{json}");
    }
    ExitCode::from(exit_code)
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

/// The L1 prompt flow. Returns (decision, reason_code, exit code) — exit 10
/// means "replacement delivered on fd 3", and is only ever returned after the
/// full replacement (sentinel included) was written successfully; every
/// failure degrades to exit 0, running the original unchanged (SPEC §9-8).
fn typo_intervention(
    buffer: &str,
    s: &layers::typo::Suggestion,
) -> (&'static str, &'static str, u8) {
    // Construct the replacement BEFORE asking: if it cannot be built with
    // byte-exact certainty there is nothing to offer.
    let Some(replacement) = layers::typo::replacement_buffer(buffer, &s.typed, &s.candidate) else {
        return ("allow", "shadow.observed", 0);
    };
    PROMPT_ACTIVE.store(true, Ordering::SeqCst);
    match ui::prompt_typo(&s.typed, &s.candidate) {
        ui::TypoChoice::Correct => {
            if write_replacement_fd3(&replacement).is_ok() {
                ("replace", "typo.accepted", 10)
            } else {
                ("allow", "typo.delivery_failed", 0)
            }
        }
        ui::TypoChoice::Original => ("allow", "typo.declined", 0),
        ui::TypoChoice::Cancel => ("cancel", "typo.cancelled", 12),
    }
}

/// The L2+ warning flow (SPEC §7): gate through budget and cooldown, show
/// the prompt, act on the answer, record the outcome. Returns (decision,
/// reason, exit code, outcome). Exit codes: 11 = restore buffer for editing,
/// 12 = cancel, 0 = run unchanged — the plugin holds up its end of each.
fn warning_intervention(
    assessment: policy::Assessment,
    danger: &layers::danger::Analysis,
    context: Option<&layers::context::Context>,
    recency: &[proposal::RecencyEntry],
    model: Option<&layers::infer::ModelEvidence>,
    cfg: &policy::Config,
) -> (&'static str, &'static str, u8, Option<&'static str>) {
    let rule = policy::primary_code(danger);
    let history = policy::load_history();
    let gated = policy::apply_gates(
        assessment,
        rule,
        danger.catastrophic,
        &history,
        events::now_ms(),
        cfg.budget_per_hour,
    );
    if !matches!(
        gated.verdict,
        policy::Verdict::Warn | policy::Verdict::Confirm
    ) {
        // budget exhausted or rule in cooldown: degrade to shadow recording
        return (gated.verdict.as_str(), gated.reason, 0, None);
    }

    let lines = ui::warning_lines(gated.reason, danger, context, recency, model);
    let pausing = gated.verdict == policy::Verdict::Confirm;
    PROMPT_ACTIVE.store(true, Ordering::SeqCst);
    let choice = match ui::prompt_warning(&lines, pausing) {
        ui::WarningPrompt::NotShown => {
            return (gated.verdict.as_str(), gated.reason, 0, None);
        }
        ui::WarningPrompt::Shown(choice) => choice,
    };
    let (outcome, ran_unchanged, exit_code) = match choice {
        ui::WarnChoice::Edit => ("edited", false, 11u8),
        ui::WarnChoice::Cancel => ("cancelled", false, 12),
        ui::WarnChoice::RunOnce => ("ran_unchanged", true, 0),
    };
    // Recorded only now, because only a prompt the user actually saw spends
    // budget — and the locked append keeps concurrent shells honest.
    if let Some(code) = rule {
        policy::record_outcome(code, ran_unchanged, events::now_ms());
    }
    (
        gated.verdict.as_str(),
        gated.reason,
        exit_code,
        Some(outcome),
    )
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

/// Complete installed-environment diagnosis for the interactive Zsh adapter.
fn doctor() -> ExitCode {
    println!("oopsinput doctor");
    println!("  version:    {}", env!("CARGO_PKG_VERSION"));

    let mut healthy = true;

    let zsh = find_in_path("zsh");
    let shown_zsh = zsh
        .as_deref()
        .map(ui::escape_for_display)
        .unwrap_or_else(|| "NOT FOUND in PATH".to_string());
    println!("  zsh:        {}", shown_zsh);
    healthy &= zsh.is_some();

    let plugin_ok = print_plugin_line();
    let widgets_ok = print_widgets_line();
    healthy &= plugin_ok && widgets_ok;

    // Regression (bughunt 2026-08-06): this line once hardcoded
    // ~/.config, contradicting the mode line below whenever
    // XDG_CONFIG_HOME pointed elsewhere. Both must resolve identically.
    let config = policy::inspect_config();
    let config_ok = print_config_line(&config);
    println!(
        "  mode:       {}",
        match config.config.mode {
            policy::Mode::Shadow => "shadow",
            policy::Mode::Suggest => "suggest (L1 typo prompts)",
            policy::Mode::Warn => "warn (L1 prompts and visible warnings)",
            policy::Mode::Confirm => {
                "confirm (L1 prompts, warnings, and gated confirmations)"
            }
        }
    );
    let model_ok = print_model_line(&config.config);
    let state_ok = print_state_line();
    healthy &= config_ok && model_ok && state_ok;

    if healthy {
        println!("  result:     ready");
        ExitCode::SUCCESS
    } else {
        println!("  result:     problems found");
        ExitCode::from(1)
    }
}

fn print_config_line(inspection: &policy::ConfigInspection) -> bool {
    let path = inspection
        .path
        .as_deref()
        .map(|path| ui::escape_for_display(&path.to_string_lossy()));
    let file_ok = match (path.as_deref(), inspection.file_state) {
        (Some(path), policy::ConfigFileState::Regular) => {
            if inspection.config.warnings.is_empty() {
                println!("  config:     {path} (present) — valid");
                true
            } else {
                println!(
                    "  config:     {path} (present) — INVALID ({} issue(s))",
                    inspection.config.warnings.len()
                );
                false
            }
        }
        (Some(path), policy::ConfigFileState::Missing) => {
            println!("  config:     {path} (absent — defaults in effect) — valid");
            true
        }
        (Some(path), policy::ConfigFileState::NonRegular) => {
            println!(
                "  config:     {path} (ignored — not a regular file; defaults in effect) — INVALID"
            );
            false
        }
        (Some(path), policy::ConfigFileState::Unavailable) => {
            println!("  config:     {path} (unavailable — defaults in effect) — INVALID");
            false
        }
        (None, _) => {
            println!("  config:     unavailable — HOME/XDG_CONFIG_HOME did not resolve a path");
            false
        }
    };
    for warning in &inspection.config.warnings {
        println!("              {}", ui::escape_for_display(warning));
    }
    if !inspection.mode_override_valid {
        println!("              OOPSINPUT_MODE is invalid; using shadow");
    }
    file_ok && inspection.mode_override_valid
}

const ZSHRC_READ_CAP: u64 = 1024 * 1024;
const MARK_BEGIN: &[u8] = b"# >>> oopsinput >>>";
const MARK_END: &[u8] = b"# <<< oopsinput <<<";

enum PluginInstallStatus {
    Installed,
    HomeUnavailable,
    ZshrcMissing,
    ZshrcUnsafe,
    ZshrcUnreadable,
    ZshrcTooLarge,
    MarkerMissing,
    MarkerDamaged,
    PluginMissing,
    PluginUnsafe,
    PluginUnreadable,
}

fn print_plugin_line() -> bool {
    let (message, ok) = match inspect_plugin_install() {
        PluginInstallStatus::Installed => (
            "installed (marked ~/.zshrc block + regular installed file)",
            true,
        ),
        PluginInstallStatus::HomeUnavailable => {
            ("unavailable — HOME must be a nonempty absolute path", false)
        }
        PluginInstallStatus::ZshrcMissing => (
            "not installed — ~/.zshrc is absent (run zsh/install.zsh)",
            false,
        ),
        PluginInstallStatus::ZshrcUnsafe => (
            "invalid — ~/.zshrc is a symlink or not a regular file",
            false,
        ),
        PluginInstallStatus::ZshrcUnreadable => {
            ("invalid — ~/.zshrc could not be read safely", false)
        }
        PluginInstallStatus::ZshrcTooLarge => (
            "invalid — ~/.zshrc exceeds the 1 MiB diagnostic read cap",
            false,
        ),
        PluginInstallStatus::MarkerMissing => (
            "not installed — marked block absent from ~/.zshrc (run zsh/install.zsh)",
            false,
        ),
        PluginInstallStatus::MarkerDamaged => (
            "invalid — marked block in ~/.zshrc is duplicated, mismatched, or reversed",
            false,
        ),
        PluginInstallStatus::PluginMissing => (
            "incomplete — ~/.local/share/oopsinput/oopsinput.zsh is absent",
            false,
        ),
        PluginInstallStatus::PluginUnsafe => (
            "invalid — installed plugin path is a symlink or not a regular file",
            false,
        ),
        PluginInstallStatus::PluginUnreadable => {
            ("invalid — installed plugin file is unreadable", false)
        }
    };
    println!("  plugin:     {message}");
    ok
}

fn inspect_plugin_install() -> PluginInstallStatus {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return PluginInstallStatus::HomeUnavailable;
    };
    if !home.is_absolute() || home.as_os_str().is_empty() {
        return PluginInstallStatus::HomeUnavailable;
    }
    let zshrc = home.join(".zshrc");
    match std::fs::symlink_metadata(&zshrc) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return PluginInstallStatus::ZshrcMissing;
        }
        Err(_) => return PluginInstallStatus::ZshrcUnreadable,
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return PluginInstallStatus::ZshrcUnsafe;
        }
        Ok(_) => {}
    }
    let file = match std::fs::File::open(&zshrc) {
        Ok(file) => file,
        Err(_) => return PluginInstallStatus::ZshrcUnreadable,
    };
    if state::opened_regular_file_metadata(&zshrc, &file, "~/.zshrc").is_err() {
        return PluginInstallStatus::ZshrcUnreadable;
    }
    let mut text = Vec::new();
    if file
        .take(ZSHRC_READ_CAP + 1)
        .read_to_end(&mut text)
        .is_err()
    {
        return PluginInstallStatus::ZshrcUnreadable;
    }
    if text.len() as u64 > ZSHRC_READ_CAP {
        return PluginInstallStatus::ZshrcTooLarge;
    }
    let mut begins = Vec::new();
    let mut ends = Vec::new();
    for (line_no, line) in text.split(|byte| *byte == b'\n').enumerate() {
        if bytes_contain(line, MARK_BEGIN) {
            begins.push(line_no);
        }
        if bytes_contain(line, MARK_END) {
            ends.push(line_no);
        }
    }
    if begins.is_empty() && ends.is_empty() {
        return PluginInstallStatus::MarkerMissing;
    }
    if begins.len() != 1 || ends.len() != 1 || ends[0] < begins[0] {
        return PluginInstallStatus::MarkerDamaged;
    }

    let plugin = home.join(".local/share/oopsinput/oopsinput.zsh");
    match std::fs::symlink_metadata(&plugin) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return PluginInstallStatus::PluginMissing;
        }
        Err(_) => return PluginInstallStatus::PluginUnreadable,
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return PluginInstallStatus::PluginUnsafe;
        }
        Ok(_) => {}
    }
    let file = match std::fs::File::open(&plugin) {
        Ok(file) => file,
        Err(_) => return PluginInstallStatus::PluginUnreadable,
    };
    if state::opened_regular_file_metadata(&plugin, &file, "installed plugin").is_err() {
        PluginInstallStatus::PluginUnreadable
    } else {
        PluginInstallStatus::Installed
    }
}

fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

const ACCEPT_WIDGETS: [&str; 4] = [
    "accept-line",
    "accept-line-and-down-history",
    "accept-and-hold",
    "accept-and-infer-next-history",
];

enum WidgetStatus {
    Inactive,
    Invalid,
    Wrapped(usize),
}

fn print_widgets_line() -> bool {
    match inspect_widgets() {
        WidgetStatus::Wrapped(count) if count == ACCEPT_WIDGETS.len() => {
            println!(
                "  widgets:    {count}/{} wrapped in this shell",
                ACCEPT_WIDGETS.len()
            );
            true
        }
        WidgetStatus::Wrapped(count) => {
            println!(
                "  widgets:    {count}/{} wrapped — reload the plugin in this shell",
                ACCEPT_WIDGETS.len()
            );
            false
        }
        WidgetStatus::Inactive => {
            println!("  widgets:    plugin not active in this shell — open a new terminal");
            false
        }
        WidgetStatus::Invalid => {
            println!("  widgets:    plugin status is malformed — reload the plugin");
            false
        }
    }
}

fn inspect_widgets() -> WidgetStatus {
    match std::env::var("OOPSINPUT_PLUGIN_ACTIVE") {
        Ok(value) if value == "1" => {}
        Err(std::env::VarError::NotPresent) => return WidgetStatus::Inactive,
        Ok(_) | Err(std::env::VarError::NotUnicode(_)) => return WidgetStatus::Invalid,
    }
    let value = match std::env::var("OOPSINPUT_WRAPPED_WIDGETS") {
        Ok(value) => value,
        Err(_) => return WidgetStatus::Invalid,
    };
    let mut seen = [false; ACCEPT_WIDGETS.len()];
    if !value.is_empty() {
        for name in value.split(',') {
            let Some(index) = ACCEPT_WIDGETS.iter().position(|expected| *expected == name) else {
                return WidgetStatus::Invalid;
            };
            if seen[index] {
                return WidgetStatus::Invalid;
            }
            seen[index] = true;
        }
    }
    WidgetStatus::Wrapped(seen.into_iter().filter(|wrapped| *wrapped).count())
}

fn print_state_line() -> bool {
    let inspection = state::inspect_state();
    let Some(dir) = inspection.dir.as_deref() else {
        println!("  state:      unavailable — no absolute state directory resolves");
        return false;
    };
    let shown = ui::escape_for_display(&dir.to_string_lossy());
    if inspection.issues.is_empty() {
        if inspection.present {
            println!(
                "  state:      {shown} (0700; {} owned file(s) present at 0600)",
                inspection.checked_files
            );
        } else {
            println!("  state:      {shown} (not created yet — valid)");
        }
        return true;
    }
    println!(
        "  state:      {shown} — INVALID ({} issue(s))",
        inspection.issues.len()
    );
    for issue in inspection.issues {
        match issue {
            state::StateIssue::DirectoryUnavailable => {
                println!("              state directory metadata is unavailable")
            }
            state::StateIssue::DirectoryNotReal => {
                println!("              state path is a symlink or not a directory")
            }
            state::StateIssue::DirectoryUnreadable => {
                println!("              state directory cannot be enumerated")
            }
            state::StateIssue::DirectoryMode(mode) => {
                println!("              state directory mode is {mode:03o}; required 700")
            }
            state::StateIssue::EntryUnavailable(label) => {
                println!("              {label} metadata is unavailable")
            }
            state::StateIssue::EntryNotRegular(label) => {
                println!("              {label} is a symlink or not a regular file")
            }
            state::StateIssue::EntryMode(label, mode) => {
                println!("              {label} mode is {mode:03o}; required 600")
            }
        }
    }
    false
}

/// Ollama's /api/show response carries the modelfile, license text, and
/// tensor metadata — legitimately large. Generous cap; this is a reachability
/// check, not model I/O.
const SHOW_RESPONSE_CAP: usize = 4 * 1024 * 1024;

/// Doctor's model line: is Ollama up, and is the configured model pulled?
/// POST /api/show answers both without loading the model or running any
/// inference. The model name comes from the config file — untrusted display
/// text, so it goes through the escaper (SPEC §9-4, no exemptions).
fn print_model_line(cfg: &policy::Config) -> bool {
    let Some(name) = &cfg.model else {
        println!("  model:      disabled (deterministic-only)");
        return true;
    };
    let shown = ui::escape_for_display(name);
    let body = serde_json::json!({ "model": name }).to_string();
    let deadline = Instant::now() + std::time::Duration::from_millis(cfg.model_timeout_ms);
    let result = model::post_json(
        model::ollama_addr(),
        "/api/show",
        body.as_bytes(),
        deadline,
        SHOW_RESPONSE_CAP,
    );
    match result {
        Ok(_) => {
            println!("  model:      {shown} (Ollama reachable, model present)");
            true
        }
        Err(model::ModelError::Status(404)) => {
            println!(
                "  model:      {shown} — Ollama is up but this model isn't pulled (ollama pull {shown})"
            );
            false
        }
        Err(model::ModelError::Connect) => {
            println!(
                "  model:      {shown} — Ollama not reachable at 127.0.0.1:11434; runs deterministic-only"
            );
            false
        }
        Err(model::ModelError::UntrustedPeer) => {
            println!(
                "  model:      {shown} — the process on 127.0.0.1:11434 is not owned by you or a \
                 system account; refusing to talk to it (runs deterministic-only)"
            );
            false
        }
        Err(model::ModelError::Timeout) => {
            println!(
                "  model:      {shown} — Ollama didn't answer within {} ms; runs deterministic-only",
                cfg.model_timeout_ms
            );
            false
        }
        Err(_) => {
            println!(
                "  model:      {shown} — unexpected reply from 127.0.0.1:11434; runs deterministic-only"
            );
            false
        }
    }
}

/// PATH lookup via direct metadata checks — never through a shell (SPEC §9).
fn find_in_path(name: &str) -> Option<String> {
    find_in_path_list(&std::env::var("PATH").ok()?, name)
}

fn find_in_path_list(path: &str, name: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let candidate = format!("{dir}/{name}");
        if let Ok(meta) = std::fs::metadata(&candidate)
            && meta.is_file()
            && meta.permissions().mode() & 0o111 != 0
        {
            return Some(candidate);
        }
    }
    None
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

    #[test]
    fn find_in_path_finds_sh() {
        // Smoke test for the thin env wrapper: `find_in_path` must read $PATH
        // and hand it to the (hermetically tested) lookup below. Its premise
        // is environmental, so it says so when it fails rather than looking
        // like a product bug (test-audit 2026-08-06).
        assert!(
            find_in_path("sh").is_some(),
            "no `sh` on $PATH — this asserts the environment, not the code; \
             the real lookup logic is pinned by find_in_path_requires_executable_bit"
        );
    }

    #[test]
    fn find_in_path_misses_nonsense() {
        assert!(find_in_path("definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn find_in_path_requires_executable_bit() {
        // Regression (bughunt #3): a plain file named like the binary was
        // reported as found by doctor.
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("oopsinput-xbit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("zsh");
        std::fs::write(&file, "not a binary").unwrap();

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(find_in_path_list(dir.to_str().unwrap(), "zsh").is_none());

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(find_in_path_list(dir.to_str().unwrap(), "zsh").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
