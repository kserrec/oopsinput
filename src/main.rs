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
mod policy;
mod proc;
mod proposal;
mod ui;

use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
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
            eprintln!("oopsinput: unknown command '{other}' (try 'oopsinput help')");
            ExitCode::from(2)
        }
    }
}

/// Set once analysis is over and a prompt is on screen: the user is in
/// control, so the analysis deadline no longer applies (ui.rs caller
/// contract). The prompt bounds itself via the terminal-level read timeout.
static PROMPT_ACTIVE: AtomicBool = AtomicBool::new(false);

/// If analysis wedges for any reason, exit with the fail-open code before the
/// shell-side wait becomes perceptible. The process is per-command, so a blunt
/// exit is safe: there is nothing to clean up that matters more than the
/// user's prompt.
fn arm_watchdog(deadline_ms: u64) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(deadline_ms));
        if !PROMPT_ACTIVE.load(Ordering::SeqCst) {
            std::process::exit(1);
        }
        // Prompt on screen: the watchdog retires. Post-prompt work is a
        // single fd write and exit.
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

    let evidence = build_evidence(
        lexed.uncertainty,
        proposal.capped,
        proposal.names_capped,
        suggestion.as_ref(),
        &danger,
        context.as_ref(),
        &proposal.recency,
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
    let capped = policy::cap_for_mode(policy::warranted(&danger, context.as_ref()), cfg.mode);
    let (decision_str, reason_code, exit_code, outcome) = match capped.verdict {
        policy::Verdict::Warn | policy::Verdict::Confirm => {
            warning_intervention(capped, &danger, context.as_ref(), &proposal.recency, &cfg)
        }
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
fn build_evidence(
    lexer_uncertainty: Vec<&'static str>,
    capped: bool,
    names_capped: bool,
    suggestion: Option<&layers::typo::Suggestion>,
    danger: &layers::danger::Analysis,
    context: Option<&layers::context::Context>,
    recency: &[proposal::RecencyEntry],
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
    evidence
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

    let lines = ui::warning_lines(gated.reason, danger, context, recency);
    let pausing = gated.verdict == policy::Verdict::Confirm;
    PROMPT_ACTIVE.store(true, Ordering::SeqCst);
    let choice = ui::prompt_warning(&lines, pausing);
    let (outcome, ran_unchanged, exit_code) = match choice {
        ui::WarnChoice::Edit => ("edited", false, 11u8),
        ui::WarnChoice::Cancel => ("cancelled", false, 12),
        ui::WarnChoice::RunOnce => ("ran_unchanged", true, 0),
    };
    // Recorded only now, because only a prompt the user actually saw spends
    // budget — and one atomic append keeps concurrent shells honest.
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

/// Environment sanity checks. Grows per milestone (config validity, model
/// reachability).
fn doctor() -> ExitCode {
    println!("oopsinput doctor");
    println!("  version:    {}", env!("CARGO_PKG_VERSION"));

    let zsh = find_in_path("zsh");
    println!(
        "  zsh:        {}",
        zsh.as_deref().unwrap_or("NOT FOUND in PATH")
    );

    // Regression (bughunt 2026-08-06): this line once hardcoded
    // ~/.config, contradicting the mode line below whenever
    // XDG_CONFIG_HOME pointed elsewhere. Both must resolve identically.
    match policy::config_path() {
        None => println!("  config:     HOME is unset — cannot locate config"),
        Some(config) => {
            let config_exists = std::fs::metadata(&config).is_ok();
            println!(
                "  config:     {} {}",
                config.display(),
                if config_exists {
                    "(present)"
                } else {
                    "(absent — defaults in effect)"
                }
            );
            let cfg = policy::load_config();
            println!(
                "  mode:       {}",
                match cfg.mode {
                    policy::Mode::Shadow => "shadow",
                    policy::Mode::Suggest => "suggest (L1 typo prompts)",
                    // honest until the M3 warning UI lands: these record
                    // would-be interventions but show only L1 prompts
                    policy::Mode::Warn => "warn (L1 prompts; visible warnings pending the M3 UI)",
                    policy::Mode::Confirm =>
                        "confirm (L1 prompts; visible confirmations pending the M3 UI)",
                }
            );
            if !cfg.warnings.is_empty() {
                println!(
                    "  config:     {} issue(s) — shown in detail once on next check",
                    cfg.warnings.len()
                );
            }
        }
    }

    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        println!("  plugin:     HOME is unset — cannot locate ~/.zshrc");
    } else {
        let zshrc = format!("{home}/.zshrc");
        let plugin_installed = std::fs::read_to_string(&zshrc)
            .map(|s| s.contains(">>> oopsinput >>>"))
            .unwrap_or(false);
        println!(
            "  plugin:     {}",
            if plugin_installed {
                "installed (block present in ~/.zshrc)"
            } else {
                "not installed (run zsh/install.zsh)"
            }
        );
    }

    if zsh.is_some() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
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
