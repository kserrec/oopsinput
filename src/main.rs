//! oopsinput — catches commands that probably aren't what you meant, before they run.
//!
//! M0 skeleton: subcommand dispatch and the `check` seam are real; analysis is a
//! placeholder that always allows. See SPEC.md (canonical) and PLAN.md.
//!
//! Exit code contract (the zsh plugin treats anything unexpected as fail-open allow):
//!   0  = allow, run the original buffer unchanged
//!   10 = replace buffer with text from fd 3 and run it   (M2, typo `y`)
//!   11 = restore original buffer to ZLE for editing       (M3)
//!   12 = cancel, run nothing                              (M3)
//!   2  = usage error
//!   1  = internal error (plugin fails open)

use std::io::Read;
use std::process::ExitCode;
use std::time::Instant;

use serde::Serialize;

/// Hard cap on proposal input size; larger input degrades to `observe` per SPEC §10.
const MAX_INPUT_BYTES: u64 = 1 << 20;

#[derive(Serialize)]
struct Decision {
    decision: &'static str,
    reason_code: &'static str,
    evidence: Vec<String>,
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
        Some("check") => check(),
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

/// Read one proposal (raw buffer for now; JSON envelope lands in M1) from stdin,
/// analyze, print a Decision as JSON on stdout, and signal via exit code.
fn check() -> ExitCode {
    let started = Instant::now();

    let mut buffer = String::new();
    if std::io::stdin()
        .take(MAX_INPUT_BYTES)
        .read_to_string(&mut buffer)
        .is_err()
    {
        // Non-UTF8 or read failure: fail open, say nothing on stdout the
        // plugin would misread.
        return ExitCode::from(1);
    }

    // M0 placeholder: no analysis yet. The seam (stdin -> decision JSON ->
    // exit code) is the stable contract everything else plugs into.
    let decision = Decision {
        decision: "allow",
        reason_code: "m0.placeholder",
        evidence: Vec::new(),
        timings_us: Timings {
            total: started.elapsed().as_micros(),
        },
    };

    match serde_json::to_string(&decision) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(_) => ExitCode::from(1),
    }
}

/// Environment sanity checks. Grows per milestone (plugin install state,
/// config validity, model reachability).
fn doctor() -> ExitCode {
    println!("oopsinput doctor");
    println!("  version:    {}", env!("CARGO_PKG_VERSION"));

    let zsh = find_in_path("zsh");
    println!(
        "  zsh:        {}",
        zsh.as_deref().unwrap_or("NOT FOUND in PATH")
    );

    let home = std::env::var("HOME").unwrap_or_default();
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
    println!("  plugin:     not installed (arrives in M1)");

    if zsh.is_some() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// PATH lookup via direct metadata checks — never through a shell (SPEC §9).
fn find_in_path(name: &str) -> Option<String> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let candidate = format!("{dir}/{name}");
        if let Ok(meta) = std::fs::metadata(&candidate)
            && meta.is_file()
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
}
