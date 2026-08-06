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

/// Outcome of an L2+ warning/confirmation prompt (SPEC §7): `e` restores the
/// exact buffer to ZLE for editing, `c` cancels (nothing runs), `r` runs the
/// original unchanged once. Timeout defaults are tier-specific: a warning is
/// advisory (timeout runs), a pausing confirmation is a gate (timeout
/// cancels — `r` is a distinct deliberate key, never the default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarnChoice {
    Edit,
    Cancel,
    RunOnce,
}

/// Prompt read timeout in deciseconds (VTIME): 10 s, then the default.
const PROMPT_TIMEOUT_DS: &str = "100";
/// Drain timeout while consuming an escape sequence's remaining bytes: the
/// terminal delivers the whole sequence in one burst, so 0.1 s is plenty.
const DRAIN_TIMEOUT_DS: &str = "1";

/// A terminal a prompt can read decision keys from. `set_drain` flips the
/// read timeout between "wait for a human" and "collect the rest of an
/// escape sequence"; test doubles ignore it (their bytes are all buffered).
pub(crate) trait PromptTty: Read + Write {
    fn set_drain(&mut self, _drain: bool) {}
}

/// The real /dev/tty in raw-ish mode. Constructed only via
/// `open_prompt_tty`, which owns the mode save/restore.
struct RealTty {
    file: File,
}

impl Read for RealTty {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buf)
    }
}
impl Write for RealTty {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
impl PromptTty for RealTty {
    fn set_drain(&mut self, drain: bool) {
        let t = if drain {
            DRAIN_TIMEOUT_DS
        } else {
            PROMPT_TIMEOUT_DS
        };
        // Failure tolerated: reads then use the previous timeout, which is
        // safe in both directions (just slower or snappier than ideal).
        let _ = stty(&self.file, &["min", "0", "time", t]);
    }
}

/// Open /dev/tty in single-key mode; the returned guard restores the saved
/// terminal state on drop, whatever path leaves.
fn open_prompt_tty() -> Option<(RealTty, SttyRestore)> {
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let saved = stty(&tty, &["-g"])?;
    // The guard owns its own handle to the tty so the main handle stays free
    // for the prompt's reads and writes.
    let guard_handle = tty.try_clone().ok()?;
    let restore = SttyRestore {
        tty: guard_handle,
        saved,
    };
    // -icanon: byte-at-a-time reads. -echo: the pressed key is not printed.
    // -isig: Ctrl-C arrives as byte 0x03 instead of killing us mid-prompt
    // (it must mean "cancel", not "fail open and run the command").
    // min 0 time N: the read itself times out; an empty read is the default.
    stty(
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
    )?; // on failure the restore guard still runs
    Some((RealTty { file: tty }, restore))
}

/// Ask the L1 typo question on /dev/tty. Total failure of any step degrades
/// to `Original` — the user just sees their command fail naturally.
pub fn prompt_typo(typed: &str, candidate: &str) -> TypoChoice {
    match open_prompt_tty() {
        Some((mut tty, _restore)) => run_typo_prompt(&mut tty, typed, candidate),
        None => TypoChoice::Original,
    }
}

/// Show an L2+ warning on /dev/tty. `lines` are pre-assembled (untrusted
/// pieces already escaped by the builder); this adds the trusted framing
/// prefix and the keys line. Total failure fails open to `RunOnce` — the
/// original command runs unchanged (SPEC §9-6/8).
pub fn prompt_warning(lines: &[String], pausing: bool) -> WarnChoice {
    match open_prompt_tty() {
        Some((mut tty, _restore)) => run_warning_prompt(&mut tty, lines, pausing),
        None => WarnChoice::RunOnce,
    }
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
    if !crate::proc::wait_or_kill(&mut child, deadline) {
        return None;
    }
    // Callers pipe only tiny outputs (stty's mode string), far below the pipe
    // buffer, so reading after exit cannot deadlock.
    let mut out = String::new();
    child.stdout.as_mut()?.read_to_string(&mut out).ok()?;
    Some(out.trim().to_string())
}

/// One decision keypress, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Char(u8),
    /// Lone ESC keypress (no sequence followed).
    Esc,
    /// A complete multi-byte escape sequence (arrow key, F-key, alt-chord) —
    /// consumed in full so its tail bytes can never leak into the next ZLE
    /// buffer as stray characters (deferred bughunt finding 2026-08-06).
    Seq,
    /// Terminal-level read timeout (`min 0 time N` expired).
    Timeout,
    Err,
}

/// Keys examined before a prompt gives up and takes its timeout default —
/// a human answering is a handful of keys; only hostile or wedged input
/// streams reach this bound.
const MAX_PROMPT_KEYS: usize = 32;

fn read_key<T: PromptTty>(tty: &mut T) -> Key {
    let mut b = [0u8; 1];
    match tty.read(&mut b) {
        Ok(0) => Key::Timeout,
        Err(_) => Key::Err,
        Ok(_) if b[0] != 0x1b => Key::Char(b[0]),
        Ok(_) => {
            tty.set_drain(true);
            let k = consume_escape_sequence(tty);
            tty.set_drain(false);
            k
        }
    }
}

/// Cursor on the byte after ESC. Consumes exactly one sequence: CSI
/// (`ESC [ params final`), SS3 (`ESC O x`), or an alt-modified character.
/// A read timeout mid-sequence means the ESC was a lone keypress (or the
/// stream is malformed — either way there is nothing more to consume).
fn consume_escape_sequence<T: Read>(tty: &mut T) -> Key {
    let mut b = [0u8; 1];
    match tty.read(&mut b) {
        Ok(0) | Err(_) => Key::Esc,
        Ok(_) => match b[0] {
            // CSI: parameter/intermediate bytes 0x20–0x3F, one final byte
            // 0x40–0x7E. Bounded — hostile input cannot hold the prompt.
            b'[' => {
                for _ in 0..16 {
                    match tty.read(&mut b) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if (0x40..=0x7e).contains(&b[0]) => break,
                        Ok(_) => {}
                    }
                }
                Key::Seq
            }
            b'O' => {
                let _ = tty.read(&mut b); // SS3 carries one final byte
                Key::Seq
            }
            _ => Key::Seq, // alt-modified character: consumed, ignored
        },
    }
}

/// The L1 prompt proper, generic over the terminal handle so the key
/// protocol is testable without a tty. One key decides; the escaper guards
/// every piece of untrusted text in the message.
fn run_typo_prompt<T: PromptTty>(tty: &mut T, typed: &str, candidate: &str) -> TypoChoice {
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
    let mut choice = TypoChoice::Original;
    for _ in 0..MAX_PROMPT_KEYS {
        choice = match read_key(tty) {
            Key::Char(b'y' | b'Y') => TypoChoice::Correct,
            Key::Char(0x03) => TypoChoice::Cancel, // Ctrl-C
            // an escape sequence is not an answer — keep waiting
            Key::Seq => continue,
            // any other key, lone ESC, timeout, error: the do-nothing outcome
            _ => TypoChoice::Original,
        };
        break;
    }
    let _ = tty.write_all(b"\r\n");
    choice
}

/// The L2+ warning prompt (SPEC §7 anatomy: the caller's lines say what the
/// command does, what it hits, and why context is unusual; this adds the
/// keys). Only deliberate keys decide — unrecognized keys are ignored, and
/// the timeout default depends on the tier: advisory warnings run the
/// command, pausing confirmations cancel it.
fn run_warning_prompt<T: PromptTty>(tty: &mut T, lines: &[String], pausing: bool) -> WarnChoice {
    let timeout_default = if pausing {
        WarnChoice::Cancel
    } else {
        WarnChoice::RunOnce
    };
    let mut msg = String::new();
    for line in lines {
        msg.push_str("oopsinput: ");
        msg.push_str(line);
        msg.push_str("\r\n");
    }
    msg.push_str("oopsinput: [e]dit  [c]ancel  [r]un unchanged ");
    if tty
        .write_all(msg.as_bytes())
        .and_then(|()| tty.flush())
        .is_err()
    {
        return WarnChoice::RunOnce; // fail open (SPEC §9-8)
    }
    let mut choice = timeout_default;
    for _ in 0..MAX_PROMPT_KEYS {
        choice = match read_key(tty) {
            Key::Char(b'e' | b'E') => WarnChoice::Edit,
            Key::Char(b'c' | b'C') | Key::Char(0x03) | Key::Esc => WarnChoice::Cancel,
            Key::Char(b'r' | b'R') => WarnChoice::RunOnce,
            Key::Timeout => timeout_default,
            Key::Err => WarnChoice::RunOnce, // fail open
            // not an answer: escape sequences and unrecognized keys
            Key::Seq | Key::Char(_) => continue,
        };
        break;
    }
    let _ = tty.write_all(b"\r\n");
    choice
}

/// Assemble the warning's message lines (SPEC §7 anatomy: what the command
/// does, what it hits, why the current context is unusual — the UI adds the
/// keys). Trusted template text plus escaped untrusted fragments only.
pub fn warning_lines(
    reason: &str,
    danger: &crate::layers::danger::Analysis,
    context: Option<&crate::layers::context::Context>,
    recency: &[crate::proposal::RecencyEntry],
) -> Vec<String> {
    let has = |code: &str| danger.codes.contains(&code);
    let git = context.and_then(|c| c.git.as_ref());
    let targets = || -> String {
        let joined = danger
            .targets
            .iter()
            .map(|t| format!("'{}'", escape_for_display(t)))
            .collect::<Vec<_>>()
            .join(", ");
        if joined.is_empty() {
            "its target".to_string()
        } else {
            joined
        }
    };
    match reason {
        "policy.dirty_work_at_risk" => {
            let action = if has("git.reset_hard") {
                "git reset --hard will discard uncommitted changes in tracked files"
            } else {
                "git clean -f will delete untracked files"
            };
            let mut facts = Vec::new();
            if let Some(d) = git.and_then(|g| g.dirty)
                && d > 0
            {
                facts.push(format!(
                    "{d} modified tracked file{}",
                    if d == 1 { "" } else { "s" }
                ));
            }
            if git.and_then(|g| g.untracked) == Some(true) {
                facts.push("untracked files present".to_string());
            }
            let mut lines = vec![
                action.to_string(),
                format!("right now: {}", facts.join(", ")),
            ];
            // "right after git diff" (SPEC §5-L3): name the previous command.
            // These words are charset-restricted twice (plugin, then parser),
            // and escaped here anyway — SPEC §9-4 says *all* displayed
            // untrusted text goes through the escaper, with no exemption for
            // text believed to be safe already (audit 2026-08-06: a rule that
            // holds only while a distant charset check stays correct is a
            // rule that breaks silently when that check is edited).
            if let Some(prev) = recency.first()
                && prev.age == 1
                && prev.cmd != "_"
            {
                let mut line = format!("previous command: {}", escape_for_display(&prev.cmd));
                if prev.sub != "_" {
                    line.push(' ');
                    line.push_str(&escape_for_display(&prev.sub));
                }
                lines.push(line);
            }
            lines
        }
        "policy.main_branch_force" => vec![
            "force-push will rewrite the remote branch's history".to_string(),
            "the current branch is a primary branch (main/master/trunk)".to_string(),
        ],
        "policy.target_context" => {
            let t = context.map(|c| c.targets.as_slice()).unwrap_or(&[]);
            let why = if has("fs.target_cwd") || t.iter().any(|t| t.is_cwd) {
                "the target is the current directory itself"
            } else if has("fs.target_parent") || t.iter().any(|t| t.is_parent) {
                "the target is the parent of the current directory"
            } else {
                "the target does not exist, but a similarly-named neighbor does — typo?"
            };
            vec![
                format!("recursive delete of {}", targets()),
                why.to_string(),
            ]
        }
        "policy.blockdev_write" => {
            let dev = danger
                .targets
                .iter()
                .find(|t| t.starts_with("/dev/"))
                .map(|t| escape_for_display(t))
                .unwrap_or_else(|| "a raw disk device".to_string());
            vec![
                format!("this writes directly to {dev}"),
                "everything currently stored there becomes unrecoverable".to_string(),
            ]
        }
        "policy.direct_catastrophic" => {
            let what = if has("fs.target_home") {
                "your entire home directory"
            } else {
                "the filesystem root"
            };
            vec![
                format!("this recursively deletes {what}"),
                "there is no undo".to_string(),
            ]
        }
        _ => vec![format!(
            "high-consequence command flagged by policy ({reason})"
        )],
    }
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
    impl PromptTty for FakeTty {} // all bytes pre-buffered: no drain switch

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
    fn escape_sequences_are_consumed_whole_and_are_not_answers() {
        // Regression (bughunt 2026-08-06, deferred to this rebuild): the old
        // single-byte read took an arrow key's ESC as the answer and left
        // "[A" behind, which leaked into the next ZLE buffer as stray
        // characters. The reader must swallow the complete sequence and keep
        // waiting for a real key.
        let mut tty = FakeTty::new(b"\x1b[Ay"); // Up arrow, then y
        assert_eq!(run_typo_prompt(&mut tty, "gti", "git"), TypoChoice::Correct);
        assert_eq!(
            tty.input.position() as usize,
            tty.input.get_ref().len(),
            "sequence bytes left unconsumed"
        );
        // SS3 arrow (application mode), then n
        let mut tty = FakeTty::new(b"\x1bOAn");
        assert_eq!(
            run_typo_prompt(&mut tty, "gti", "git"),
            TypoChoice::Original
        );
        assert_eq!(tty.input.position() as usize, tty.input.get_ref().len());
        // CSI with parameters (e.g. shift-arrow: ESC [ 1 ; 2 A), then y
        let mut tty = FakeTty::new(b"\x1b[1;2Ay");
        assert_eq!(run_typo_prompt(&mut tty, "gti", "git"), TypoChoice::Correct);
        assert_eq!(tty.input.position() as usize, tty.input.get_ref().len());
    }

    #[test]
    fn lone_esc_runs_the_original() {
        let mut tty = FakeTty::new(b"\x1b");
        assert_eq!(
            run_typo_prompt(&mut tty, "gti", "git"),
            TypoChoice::Original
        );
    }

    #[test]
    fn hostile_key_soup_cannot_hold_the_prompt_open() {
        // MAX_PROMPT_KEYS bounds the loop: endless sequences resolve to the
        // default rather than waiting forever.
        let soup: Vec<u8> = b"\x1b[A".repeat(100).to_vec();
        let mut tty = FakeTty::new(&soup);
        assert_eq!(
            run_typo_prompt(&mut tty, "gti", "git"),
            TypoChoice::Original
        );
        let mut tty = FakeTty::new(&soup);
        assert_eq!(
            run_warning_prompt(&mut tty, &["x".into()], false),
            WarnChoice::RunOnce
        );
    }

    // ---- warning prompt key protocol (SPEC §7) ----

    fn lines() -> Vec<String> {
        vec![
            "git reset --hard discards uncommitted changes".to_string(),
            "right now: 17 modified tracked files".to_string(),
        ]
    }

    #[test]
    fn warning_shows_anatomy_and_keys_with_trusted_framing() {
        let mut tty = FakeTty::new(b"c");
        assert_eq!(
            run_warning_prompt(&mut tty, &lines(), false),
            WarnChoice::Cancel
        );
        let shown = tty.shown();
        for line in shown.lines().filter(|l| !l.trim().is_empty()) {
            assert!(
                line.starts_with("oopsinput: "),
                "unframed line reached the tty: {line:?}"
            );
        }
        assert!(shown.contains("discards uncommitted changes"));
        assert!(shown.contains("17 modified tracked files"));
        assert!(shown.contains("[e]dit"));
        assert!(shown.contains("[c]ancel"));
        assert!(shown.contains("[r]un unchanged"));
    }

    #[test]
    fn warning_keys_decide() {
        for (keys, want) in [
            (&b"e"[..], WarnChoice::Edit),
            (b"E", WarnChoice::Edit),
            (b"c", WarnChoice::Cancel),
            (b"C", WarnChoice::Cancel),
            (b"r", WarnChoice::RunOnce),
            (b"R", WarnChoice::RunOnce),
            (b"\x03", WarnChoice::Cancel),    // Ctrl-C
            (b"\x1b", WarnChoice::Cancel),    // lone ESC = dismiss
            (b"xq!e", WarnChoice::Edit),      // unrecognized keys are ignored
            (b"\x1b[Bc", WarnChoice::Cancel), // arrow consumed, then c
        ] {
            let mut tty = FakeTty::new(keys);
            assert_eq!(
                run_warning_prompt(&mut tty, &lines(), false),
                want,
                "keys {keys:?}"
            );
        }
    }

    #[test]
    fn warning_timeout_default_depends_on_tier() {
        // advisory warning: timeout runs the command (nonblocking notice)
        let mut tty = FakeTty::new(b"");
        assert_eq!(
            run_warning_prompt(&mut tty, &lines(), false),
            WarnChoice::RunOnce
        );
        // pausing confirmation: timeout cancels — running is never the
        // default for predicted-irreversible commands (SPEC §7)
        let mut tty = FakeTty::new(b"");
        assert_eq!(
            run_warning_prompt(&mut tty, &lines(), true),
            WarnChoice::Cancel
        );
    }

    #[test]
    fn warning_lines_name_the_facts() {
        use crate::layers::context::{Context, GitFacts};
        // flagship: dirty reset — counts named, no raw command text needed
        let danger =
            crate::layers::danger::analyze_with_home(&crate::lexer::lex("git reset --hard"), None);
        let ctx = Context {
            git: Some(GitFacts {
                detached: false,
                branch_main_like: false,
                dirty: Some(17),
                untracked: Some(true),
            }),
            targets: vec![],
        };
        let lines = warning_lines("policy.dirty_work_at_risk", &danger, Some(&ctx), &[]);
        let joined = lines.join("\n");
        assert!(joined.contains("git reset --hard"), "{joined}");
        assert!(joined.contains("17 modified tracked files"), "{joined}");
        assert!(joined.contains("untracked files present"), "{joined}");

        // catastrophic home delete names what dies
        let danger = crate::layers::danger::analyze_with_home(&crate::lexer::lex("rm -rf ~"), None);
        let joined = warning_lines("policy.direct_catastrophic", &danger, None, &[]).join("\n");
        assert!(joined.contains("home directory"), "{joined}");

        // hostile target text is escaped before display (SPEC §9-5)
        let danger = crate::layers::danger::analyze_with_home(
            &crate::lexer::lex("rm -rf ./x\u{1b}EVIL\u{7}y"),
            None,
        );
        let joined = warning_lines("policy.target_context", &danger, None, &[]).join("\n");
        assert!(!joined.contains('\u{1b}'), "raw ESC in warning: {joined:?}");
        assert!(joined.contains("./x^[EVIL^Gy"), "{joined}");
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
