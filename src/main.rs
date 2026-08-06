//! oopsinput — catches commands that probably aren't what you meant, before they run.
//!
//! M1: real proposal intake from the zsh plugin, shadow-mode analysis (always
//! allow, record structural event), self-watchdog deadline so a wedged process
//! can never hold the user's shell. See SPEC.md (canonical) and PLAN.md.
//!
//! Exit code contract (the zsh plugin treats anything unexpected as fail-open allow):
//!   0  = allow, run the original buffer unchanged
//!   10 = replace buffer with text from fd 3 and run it   (M2, typo `y`)
//!   11 = restore original buffer to ZLE for editing       (M3)
//!   12 = cancel, run nothing                              (M3)
//!   2  = usage error
//!   1  = internal error (plugin fails open)

mod events;
mod layers;
mod lexer;
mod proposal;
mod ui;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde::Serialize;

use proposal::Proposal;

/// Deterministic-path deadline (SPEC §10). The watchdog force-exits with the
/// fail-open code if analysis ever exceeds it; config surface arrives later.
const DET_DEADLINE_MS: u64 = 150;

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
fn arm_watchdog() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(deadline_ms()));
        if !PROMPT_ACTIVE.load(Ordering::SeqCst) {
            std::process::exit(1);
        }
        // Prompt on screen: the watchdog retires. Post-prompt work is a
        // single fd write and exit.
    });
}

/// Test hooks exist in debug builds only (the PTY suite runs against the
/// debug profile); release binaries have a fixed deadline and no hang hook.
fn deadline_ms() -> u64 {
    #[cfg(debug_assertions)]
    if let Ok(v) = std::env::var("OOPSINPUT_TEST_DEADLINE_MS") {
        return v.parse().unwrap_or(DET_DEADLINE_MS);
    }
    DET_DEADLINE_MS
}

/// Operating mode (SPEC §8/§15). Only the L1-relevant split exists so far:
/// warn/confirm include L1 prompts per §8, so they resolve to Suggest until
/// their own layers land (M3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Shadow,
    Suggest,
}

/// Precedence: $OOPSINPUT_MODE, then the config file's `mode` key, then
/// shadow. Unknown values collapse to shadow — the silent mode is the safe
/// one (SPEC §15: invalid values fall back to the default).
fn resolve_mode() -> Mode {
    if let Ok(v) = std::env::var("OOPSINPUT_MODE") {
        return parse_mode(&v);
    }
    let Some(path) = config_path() else {
        return Mode::Shadow;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Mode::Shadow;
    };
    parse_mode(config_value(&text, "mode").unwrap_or(""))
}

fn parse_mode(s: &str) -> Mode {
    match s {
        "suggest" | "warn" | "confirm" => Mode::Suggest,
        _ => Mode::Shadow,
    }
}

/// $XDG_CONFIG_HOME/oopsinput/config else ~/.config/oopsinput/config.
fn config_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("oopsinput/config"));
    }
    let home = std::env::var("HOME").ok()?;
    (!home.is_empty()).then(|| PathBuf::from(home).join(".config/oopsinput/config"))
}

/// Minimal `key = value` reader for the SPEC §15 surface: first match wins,
/// `#` starts a comment, whitespace is trimmed. The full surface (warn-once
/// on unknown keys) arrives with policy.rs in M3.
fn config_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == key {
            return Some(v.trim());
        }
    }
    None
}

/// Read one proposal from stdin (+ adapter flags), analyze, record a shadow
/// event, print a Decision as JSON on stdout, signal via exit code.
fn check(args: &[String]) -> ExitCode {
    let started = Instant::now();
    arm_watchdog();

    // Test hook (debug builds only): prove the watchdog end-to-end — a plugin
    // pointed at a hanging binary must still run the user's command.
    #[cfg(debug_assertions)]
    if std::env::var("OOPSINPUT_TEST_HANG").is_ok() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    let Ok(proposal) = Proposal::from_check_invocation(args) else {
        return ExitCode::from(1);
    };

    // Shadow analysis: lex for structure and honest uncertainty (SPEC §13);
    // the decision layers land next — for now always allow, always record.
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

    let evidence = build_evidence(
        lexed.uncertainty,
        proposal.capped,
        proposal.names_capped,
        suggestion.as_ref(),
    );

    // Analysis-only duration: the prompt below waits on a human and must not
    // pollute the latency percentiles the budgets are measured against.
    let duration_us = started.elapsed().as_micros();

    // Visible intervention (suggest mode and up): ask, then act on the
    // answer. Shadow stays silent and records what would have happened.
    let (decision_str, reason_code, exit_code) = match &suggestion {
        Some(s) if resolve_mode() != Mode::Shadow => typo_intervention(&proposal.buffer, s),
        _ => ("allow", "shadow.observed", 0u8),
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
/// (first-seen), input caps, then typo findings. The golden corpus pins this
/// exact assembly — static strings only, never raw text.
fn build_evidence(
    lexer_uncertainty: Vec<&'static str>,
    capped: bool,
    names_capped: bool,
    suggestion: Option<&layers::typo::Suggestion>,
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
    match config_path() {
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
            println!(
                "  mode:       {}",
                match resolve_mode() {
                    Mode::Shadow => "shadow",
                    Mode::Suggest => "suggest (L1 typo prompts)",
                }
            );
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

        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/eval/golden/typo.json");
        let text = std::fs::read_to_string(path).expect("read golden corpus");
        let cases: Vec<Case> = serde_json::from_str(&text).expect("parse golden corpus");
        assert!(!cases.is_empty());

        let paired = cases.iter().filter(|c| c.pair.is_some()).count();
        assert!(
            paired * 100 >= cases.len() * 30,
            "SPEC §11: ≥30% of golden cases must be counterfactual pairs \
             ({paired}/{} are)",
            cases.len()
        );

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
            let evidence = build_evidence(lexed.uncertainty, false, false, suggestion.as_ref());
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

    #[test]
    fn mode_vocabulary_is_closed_and_fails_to_shadow() {
        assert_eq!(parse_mode("shadow"), Mode::Shadow);
        assert_eq!(parse_mode("suggest"), Mode::Suggest);
        // warn/confirm include L1 prompts (SPEC §8) until their layers land
        assert_eq!(parse_mode("warn"), Mode::Suggest);
        assert_eq!(parse_mode("confirm"), Mode::Suggest);
        // anything else — typos, injection attempts, empty — is shadow
        assert_eq!(parse_mode(""), Mode::Shadow);
        assert_eq!(parse_mode("SUGGEST"), Mode::Shadow);
        assert_eq!(parse_mode("$(rm -rf /)"), Mode::Shadow);
    }

    #[test]
    fn config_value_reads_the_spec_15_shape() {
        let text = "# comment\nmode = suggest   # trailing comment\nmodel = qwen3.5:4b\n";
        assert_eq!(config_value(text, "mode"), Some("suggest"));
        assert_eq!(config_value(text, "model"), Some("qwen3.5:4b"));
        assert_eq!(config_value(text, "missing"), None);
        assert_eq!(config_value("", "mode"), None);
        // first match wins; malformed lines are skipped
        assert_eq!(config_value("garbage\nmode=a\nmode=b", "mode"), Some("a"));
    }

    #[test]
    fn find_in_path_finds_sh() {
        // /bin/sh exists on any Linux we support.
        assert!(find_in_path("sh").is_some());
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
