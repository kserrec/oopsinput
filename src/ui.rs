//! /dev/tty prompts and the display escaper (SPEC §7, §9-5).
//!
//! Everything shown to the user that originated outside this binary — typed
//! words, candidate names, later paths and model reasons — passes through
//! `escape_for_display` first, so no control character, ANSI/OSC sequence, or
//! bidi override can reach the terminal active. The fixed `oopsinput:` prefix
//! frames every message.
//!
//! Prompts talk to /dev/tty directly, never stdout/stderr (stdout carries the
//! decision JSON and the plugin discards both streams). Single-key reads use
//! `stty` on the tty for mode switching — termios is out of reach without a
//! dependency (allowlist: SPEC §12), and `stty` is coreutils, invoked with
//! fixed argv on our own terminal, inside the same-user trust boundary SPEC
//! §9 states. Timeout comes from the terminal itself (`min 0 time N`): no
//! threads, no signals.
//!
//! CALLER CONTRACT: only invoke a prompt after the analysis watchdog has been
//! neutralized — a prompt legitimately outlives the analysis deadline, and a
//! watchdog exit mid-prompt would skip terminal-mode restore.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};

/// Outcome of the L1 typo prompt (SPEC §7): `y` consents to the correction,
/// Ctrl-C cancels outright, and everything else — `n`, any other key, the
/// timeout, a missing tty, any error — runs the original unchanged (it was
/// unexecutable anyway, so that is the do-nothing outcome).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypoChoice {
    Correct,
    Original,
    Cancel,
}

/// Prompt read timeout in deciseconds (VTIME): 10 s, then `n` (SPEC §5-L1).
const PROMPT_TIMEOUT_DS: &str = "100";

/// Ask the L1 typo question on /dev/tty. Total failure of any step degrades
/// to `Original` — the user just sees their command fail naturally.
pub fn prompt_typo(typed: &str, candidate: &str) -> TypoChoice {
    let Ok(mut tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") else {
        return TypoChoice::Original;
    };
    let Some(saved) = stty(&tty, &["-g"]) else {
        return TypoChoice::Original;
    };
    // The guard owns its own handle to the tty so the main handle stays free
    // for the prompt's reads and writes.
    let Ok(guard_handle) = tty.try_clone() else {
        return TypoChoice::Original;
    };
    let _restore = SttyRestore {
        tty: guard_handle,
        saved,
    };
    // -icanon: byte-at-a-time reads. -echo: the pressed key is not printed.
    // -isig: Ctrl-C arrives as byte 0x03 instead of killing us mid-prompt
    // (it must mean "cancel", not "fail open and run the typo").
    // min 0 time N: the read itself times out; an empty read means "n".
    if stty(
        &tty,
        &[
            "-icanon",
            "-echo",
            "-isig",
            "min",
            "0",
            "time",
            PROMPT_TIMEOUT_DS,
        ],
    )
    .is_none()
    {
        return TypoChoice::Original; // restore guard still runs
    }
    run_typo_prompt(&mut tty, typed, candidate)
}

/// Restores the saved `stty -g` state on scope exit, whatever path leaves.
struct SttyRestore {
    tty: File,
    saved: String,
}

impl Drop for SttyRestore {
    fn drop(&mut self) {
        let _ = stty(&self.tty, &[&self.saved]);
    }
}

/// Absolute paths we accept for `stty`, in order. SECURITY (audit
/// 2026-08-06): resolving `stty` through $PATH executed an attacker's binary
/// whenever an untrusted directory preceded the system ones — `.` on PATH, or
/// the common dev setup where a tool like direnv puts a repo's `./bin` on
/// PATH. Any typo then ran that repo's `stty`. The typo layer fires on
/// *unresolvable* commands, so the trigger is any typo and the name is fixed
/// and predictable — that is strictly more than a poisoned PATH normally
/// buys. Never resolve this by name.
const STTY_PATHS: [&str; 2] = ["/bin/stty", "/usr/bin/stty"];

fn stty_path() -> Option<&'static str> {
    STTY_PATHS
        .into_iter()
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file()))
}

/// Hard timeout for an `stty` child (SPEC §9-1: external helpers run with
/// fixed argv, no shell, hard timeout). Generous — stty is a few
/// milliseconds of work; this only ever fires on a wedged system.
const STTY_TIMEOUT_MS: u64 = 2_000;

/// Run `stty` against our tty (as its stdin), bounded. Returns trimmed stdout
/// on success, None on any failure or timeout.
///
/// SECURITY (audit 2026-08-06): this call happens after the watchdog retires,
/// so an unbounded child hung the user's shell indefinitely with nothing to
/// recover it. Everything past the retirement point must be bounded by
/// construction; that is what makes retiring safe.
fn stty(tty: &File, args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new(stty_path()?);
    cmd.args(args)
        .stdin(tty.try_clone().ok()?)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    run_bounded(cmd, STTY_TIMEOUT_MS)
}

/// Spawn a child, wait at most `timeout_ms`, and return its trimmed stdout.
/// A child that overruns is killed and reaped, and the call reports failure —
/// no path through this function can block longer than the timeout.
fn run_bounded(mut cmd: std::process::Command, timeout_ms: u64) -> Option<String> {
    let mut child = cmd.spawn().ok()?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            // Wedged or unwaitable: kill it and degrade. Reaped immediately
            // so we never leave a zombie behind.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    // Callers pipe only tiny outputs (stty's mode string), far below the pipe
    // buffer, so reading after exit cannot deadlock.
    let mut out = String::new();
    child.stdout.as_mut()?.read_to_string(&mut out).ok()?;
    Some(out.trim().to_string())
}

/// The prompt proper, generic over the terminal handle so the key protocol is
/// testable without a tty. One key decides; the escaper guards every piece of
/// untrusted text in the message.
fn run_typo_prompt<T: Read + Write>(tty: &mut T, typed: &str, candidate: &str) -> TypoChoice {
    let msg = format!(
        "oopsinput: '{}' not found — did you mean '{}'? [y/n] ",
        escape_for_display(typed),
        escape_for_display(candidate),
    );
    if tty
        .write_all(msg.as_bytes())
        .and_then(|()| tty.flush())
        .is_err()
    {
        return TypoChoice::Original;
    }
    let mut key = [0u8; 1];
    let choice = match tty.read(&mut key) {
        Ok(1) => match key[0] {
            b'y' | b'Y' => TypoChoice::Correct,
            0x03 => TypoChoice::Cancel, // Ctrl-C
            _ => TypoChoice::Original,
        },
        // 0 bytes = terminal-level timeout; errors degrade the same way.
        _ => TypoChoice::Original,
    };
    let _ = tty.write_all(b"\r\n");
    choice
}

/// Neutralize text for display (SPEC §9-5): C0/C1 controls and DEL become
/// caret/escape notation, bidi controls and invisible formatting characters
/// become visible `\u{...}` escapes. Anything that survives this function is
/// inert on a terminal.
pub fn escape_for_display(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if !needs_escape(c) {
            out.push(c);
        } else if c == '\u{7F}' {
            out.push_str("^?");
        } else if (c as u32) < 0x20 {
            // C0 in caret notation: ESC → ^[, BEL → ^G, newline → ^J …
            out.push('^');
            out.push(((c as u8) ^ 0x40) as char);
        } else {
            // C1 controls, bidi overrides/isolates, invisible formatting.
            out.push_str(&format!("\\u{{{:04X}}}", c as u32));
        }
    }
    out
}

/// Characters that must never reach the terminal raw: all Unicode `Cc`
/// controls (C0, DEL, C1 — covers ESC/CSI/OSC introducers), bidi embedding/
/// override/isolate controls and marks, line/paragraph separators, and
/// zero-width/invisible formatting characters that could disguise what a
/// suggested command really is.
fn needs_escape(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}'                  // arabic letter mark
            | '\u{200B}'..='\u{200F}'   // zero-widths + LRM/RLM
            | '\u{2028}' | '\u{2029}'   // line / paragraph separator
            | '\u{202A}'..='\u{202E}'   // bidi embeddings + overrides
            | '\u{2060}'                // word joiner
            | '\u{2066}'..='\u{2069}'   // bidi isolates
            | '\u{FEFF}' // zero-width no-break space / BOM
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- escaper ----

    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(escape_for_display("git status"), "git status");
        assert_eq!(escape_for_display("dziękuję-嗯-€"), "dziękuję-嗯-€");
        assert_eq!(escape_for_display(""), "");
    }

    #[test]
    fn ansi_and_osc_sequences_are_neutralized() {
        // CSI color: ESC becomes ^[ and the rest is inert text.
        assert_eq!(escape_for_display("\u{1b}[31mred"), "^[[31mred");
        // OSC title set with BEL terminator.
        assert_eq!(escape_for_display("\u{1b}]0;EVIL\u{7}x"), "^[]0;EVIL^Gx");
        // C1 CSI (single byte 0x9B) — no ESC needed, must still die.
        assert_eq!(escape_for_display("\u{9b}31mred"), "\\u{009B}31mred");
    }

    #[test]
    fn c0_controls_use_caret_notation() {
        assert_eq!(escape_for_display("a\nb\tc\rd"), "a^Jb^Ic^Md");
        assert_eq!(escape_for_display("\u{0}"), "^@");
        assert_eq!(escape_for_display("\u{7f}"), "^?");
    }

    #[test]
    fn bidi_and_invisible_characters_become_visible() {
        // The classic RLO spoof: "gi<RLO>ffud.sh" renders as "gihs.duff".
        assert_eq!(
            escape_for_display("gi\u{202E}ffud.sh"),
            "gi\\u{202E}ffud.sh"
        );
        assert_eq!(escape_for_display("g\u{200B}it"), "g\\u{200B}it");
        assert_eq!(
            escape_for_display("\u{2066}x\u{2069}"),
            "\\u{2066}x\\u{2069}"
        );
        assert_eq!(escape_for_display("a\u{FEFF}b"), "a\\u{FEFF}b");
    }

    #[test]
    fn fuzz_smoke_no_active_character_survives() {
        // SPEC §9-5 fuzz target: byte soup heavy in escape machinery; every
        // output char must be inert, output stays bounded, nothing panics.
        let alphabet: Vec<char> =
            "ab0;:m[]()\u{1b}\u{7}\u{8}\u{9b}\u{9d}\r\n\t\u{0}\u{7f}\u{202E}\u{202A}\u{2066}\u{200B}\u{FEFF}\u{061C}注€y"
                .chars()
                .collect();
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let len = (next() % 48) as usize;
            let s: String = (0..len)
                .map(|_| alphabet[(next() as usize) % alphabet.len()])
                .collect();
            let escaped = escape_for_display(&s);
            assert!(
                escaped.chars().all(|c| !needs_escape(c)),
                "active character survived escaping {s:?} -> {escaped:?}"
            );
            assert!(escaped.chars().count() <= s.chars().count() * 10);
            // Idempotent: escaping escaped text changes nothing.
            assert_eq!(escape_for_display(&escaped), escaped);
        }
    }

    // ---- external helper invocation (audit 2026-08-06) ----

    #[test]
    fn stty_is_never_resolved_through_path() {
        // Regression: resolving by name executed an attacker's `stty` when an
        // untrusted directory preceded the system ones on PATH (`.` on PATH,
        // or a repo's ./bin added by direnv). Any typo triggered it.
        for p in STTY_PATHS {
            assert!(
                p.starts_with('/'),
                "stty must be an absolute path, got {p:?}"
            );
        }
        // And one of them must actually exist, or prompts silently degrade.
        assert!(
            stty_path().is_some(),
            "no stty found at any absolute path: {STTY_PATHS:?}"
        );
    }

    #[test]
    fn a_wedged_helper_is_killed_at_the_deadline() {
        // Regression: the prompt path runs after the watchdog retires, so an
        // unbounded external child hung the user's shell indefinitely with
        // nothing left to recover it (SPEC §9-1 requires a hard timeout).
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let started = std::time::Instant::now();
        let out = run_bounded(cmd, 300);
        let elapsed = started.elapsed();

        assert!(out.is_none(), "a wedged helper must report failure");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "run_bounded blocked for {elapsed:?} — the deadline did not fire"
        );
    }

    #[test]
    fn a_healthy_helper_still_returns_its_output() {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "printf 'mode-string\\n'"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        assert_eq!(run_bounded(cmd, 2_000).as_deref(), Some("mode-string"));
    }

    #[test]
    fn a_failing_helper_reports_failure() {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "exit 1"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        assert!(run_bounded(cmd, 2_000).is_none());
    }

    // ---- prompt key protocol ----

    /// Fake terminal: scripted keystrokes in, captured display out.
    struct FakeTty {
        input: std::io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl FakeTty {
        fn new(keys: &[u8]) -> Self {
            FakeTty {
                input: std::io::Cursor::new(keys.to_vec()),
                output: Vec::new(),
            }
        }
        fn shown(&self) -> String {
            String::from_utf8_lossy(&self.output).into_owned()
        }
    }

    impl Read for FakeTty {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }
    impl Write for FakeTty {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn y_consents_and_message_is_framed() {
        let mut tty = FakeTty::new(b"y");
        assert_eq!(run_typo_prompt(&mut tty, "gti", "git"), TypoChoice::Correct);
        let shown = tty.shown();
        assert!(
            shown.starts_with("oopsinput: "),
            "trusted prefix missing: {shown}"
        );
        assert!(
            shown.contains("'gti' not found"),
            "typed word missing: {shown}"
        );
        assert!(
            shown.contains("did you mean 'git'?"),
            "candidate missing: {shown}"
        );
        assert!(shown.contains("[y/n]"), "keys missing: {shown}");
    }

    #[test]
    fn uppercase_y_also_consents() {
        let mut tty = FakeTty::new(b"Y");
        assert_eq!(run_typo_prompt(&mut tty, "gti", "git"), TypoChoice::Correct);
    }

    #[test]
    fn n_and_any_other_key_run_the_original() {
        for key in [b'n', b'N', b' ', b'q', 0x1b, b'\r'] {
            let mut tty = FakeTty::new(&[key]);
            assert_eq!(
                run_typo_prompt(&mut tty, "gti", "git"),
                TypoChoice::Original,
                "key {key:#04x} must mean original"
            );
        }
    }

    #[test]
    fn ctrl_c_cancels() {
        let mut tty = FakeTty::new(&[0x03]);
        assert_eq!(run_typo_prompt(&mut tty, "gti", "git"), TypoChoice::Cancel);
    }

    #[test]
    fn timeout_empty_read_runs_the_original() {
        // `min 0 time N` expiry surfaces as a 0-byte read.
        let mut tty = FakeTty::new(b"");
        assert_eq!(
            run_typo_prompt(&mut tty, "gti", "git"),
            TypoChoice::Original
        );
    }

    #[test]
    fn hostile_names_are_escaped_in_the_prompt() {
        // A candidate carrying an OSC sequence and a bidi override must be
        // displayed inert (SPEC §9-5 — this is the anti-spoofing invariant).
        let mut tty = FakeTty::new(b"n");
        run_typo_prompt(&mut tty, "x\u{1b}]0;EVIL\u{7}", "gi\u{202E}t");
        let shown = tty.shown();
        assert!(
            !shown.contains('\u{1b}'),
            "raw ESC reached the tty: {shown:?}"
        );
        assert!(
            !shown.contains('\u{202E}'),
            "raw bidi override reached the tty: {shown:?}"
        );
        assert!(
            shown.contains("^[]0;EVIL^G"),
            "escaped form missing: {shown:?}"
        );
        assert!(
            shown.contains("gi\\u{202E}t"),
            "escaped bidi missing: {shown:?}"
        );
    }
}
