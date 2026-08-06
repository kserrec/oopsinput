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
mod lexer;
mod proposal;

use std::process::ExitCode;
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

/// If analysis wedges for any reason, exit with the fail-open code before the
/// shell-side wait becomes perceptible. The process is per-command, so a blunt
/// exit is safe: there is nothing to clean up that matters more than the
/// user's prompt.
fn arm_watchdog() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(deadline_ms()));
        std::process::exit(1);
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
    let mut evidence = lexed.uncertainty;
    if proposal.capped {
        evidence.push("input.capped");
    }

    let duration_us = started.elapsed().as_micros();
    let decision = Decision {
        decision: "allow",
        reason_code: "shadow.observed",
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

    match serde_json::to_string(&decision) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(_) => ExitCode::from(1),
    }
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

    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        println!("  config:     HOME is unset — cannot locate config or plugin");
    } else {
        let config = format!("{home}/.config/oopsinput/config");
        let config_exists = std::fs::metadata(&config).is_ok();
        println!(
            "  config:     {config} {}",
            if config_exists {
                "(present)"
            } else {
                "(absent — defaults in effect)"
            }
        );
        println!("  mode:       shadow (default)");

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
