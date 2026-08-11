//! /dev/tty prompts and the display escaper (SPEC §7, §9-5).
//!
//! Everything shown to the user that originated outside this binary — typed
//! words, candidate names, later paths and model reasons — passes through
//! `escape_for_display` first, so no control character, ANSI/OSC sequence, or
//! bidi override can reach the terminal active. The fixed `*** oops? ***`
//! banner frames a typo block; the fixed `oopsinput:` prefix frames warning
//! and diagnostic lines. Trusted reverse video marks typo-choice focus.
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
/// `n` keeps the original, Tab + Enter activates a visibly focused choice,
/// and Ctrl-C cancels outright. Timeout stays distinct from a deliberate `n`
/// so evaluation never credits the user with a choice they did not make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypoChoice {
    Correct,
    Original,
    Cancel,
    Timeout,
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
    Timeout,
}

/// Whether a complete L2+ prompt reached the terminal. `NotShown` still
/// means fail open and run unchanged, but it must not be recorded as a
/// visible intervention or spend the user's warning budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningPrompt {
    NotShown,
    Shown(WarnChoice),
}

/// Prompt read timeout in deciseconds (VTIME): 10 s, then the default.
const PROMPT_TIMEOUT_DS: &str = "100";
/// Drain timeout while consuming an escape sequence's remaining bytes: the
/// terminal delivers the whole sequence in one burst, so 0.1 s is plenty.
const DRAIN_TIMEOUT_DS: &str = "1";
/// Bytes normally sufficient for an emitted CSI sequence before we switch to
/// a larger bounded drain. A final byte beyond this boundary must still be
/// consumed as part of the sequence, never reinterpreted as a consent key.
const CSI_FAST_BYTES: usize = 16;
const CSI_DRAIN_BYTES: usize = 256;
/// Keep each displayed command bounded even though the analyzed buffer cap is
/// much larger. The typo is at the beginning, so the useful contrast survives
/// truncation; the executed buffers themselves remain byte-exact and uncapped
/// by this display-only limit.
const TYPO_COMMAND_DISPLAY_LIMIT_BYTES: usize = 512;
const FOCUS_ON: &str = "\x1b[7m";
const FOCUS_OFF: &str = "\x1b[0m";

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

/// Ask the L1 typo question on /dev/tty. `original` and `replacement` are the
/// complete buffers; only their escaped, display-bounded representations are
/// shown. Total failure of any step degrades to `Original` — the user just
/// sees their command fail naturally.
pub fn prompt_typo(original: &str, replacement: &str) -> TypoChoice {
    match open_prompt_tty() {
        Some((mut tty, _restore)) => run_typo_prompt(&mut tty, original, replacement),
        None => TypoChoice::Original,
    }
}

/// Show an L2+ warning on /dev/tty. `lines` are pre-assembled (untrusted
/// pieces already escaped by the builder); this adds the trusted framing
/// prefix and the keys line. Total failure reports `NotShown`; the caller
/// still fails open and runs the original unchanged, without recording a
/// visible intervention or spending budget (SPEC §9-6/8).
pub fn prompt_warning(lines: &[String]) -> WarningPrompt {
    #[cfg(debug_assertions)]
    if std::env::var_os("OOPSINPUT_TEST_NO_TTY").is_some() {
        return WarningPrompt::NotShown;
    }
    match open_prompt_tty() {
        Some((mut tty, _restore)) => run_warning_prompt(&mut tty, lines),
        None => WarningPrompt::NotShown,
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
    /// Input exceeded the bounded escape-sequence drain without a final byte.
    /// The prompt takes its non-consent default and stops reading.
    InvalidSequence,
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
            // 0x40–0x7E. The second bounded drain is essential: returning
            // after exactly 16 parameter bytes once left the final `y`/`r`
            // for the outer prompt loop, where it became false consent.
            b'[' => {
                for _ in 0..CSI_FAST_BYTES {
                    match tty.read(&mut b) {
                        Ok(0) | Err(_) => return Key::InvalidSequence,
                        Ok(_) if (0x40..=0x7e).contains(&b[0]) => return Key::Seq,
                        Ok(_) if (0x20..=0x3f).contains(&b[0]) => {}
                        Ok(_) => return Key::InvalidSequence,
                    }
                }
                for _ in 0..CSI_DRAIN_BYTES {
                    match tty.read(&mut b) {
                        Ok(0) | Err(_) => return Key::InvalidSequence,
                        Ok(_) if (0x40..=0x7e).contains(&b[0]) => return Key::Seq,
                        Ok(_) if (0x20..=0x3f).contains(&b[0]) => {}
                        Ok(_) => return Key::InvalidSequence,
                    }
                }
                Key::InvalidSequence
            }
            b'O' => {
                // SS3 carries one final byte. If it does not arrive in the
                // drain window, stop this prompt: treating a delayed final
                // byte as a fresh key could turn it into consent.
                match tty.read(&mut b) {
                    Ok(0) | Err(_) => Key::InvalidSequence,
                    Ok(_) => Key::Seq,
                }
            }
            _ => Key::Seq, // alt-modified character: consumed, ignored
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypoFocus {
    Correction,
    Original,
}

fn toggle_typo_focus(focus: TypoFocus) -> TypoFocus {
    match focus {
        TypoFocus::Correction => TypoFocus::Original,
        TypoFocus::Original => TypoFocus::Correction,
    }
}

fn focused_choice(label: &str, focused: bool) -> String {
    if focused {
        format!("{FOCUS_ON}{label}{FOCUS_OFF}")
    } else {
        label.to_string()
    }
}

fn typo_choices_line(focus: TypoFocus) -> String {
    format!(
        "{}  {}",
        focused_choice("[y] run correction", focus == TypoFocus::Correction),
        focused_choice("[n] run original", focus == TypoFocus::Original),
    )
}

fn redraw_typo_choices<T: PromptTty>(tty: &mut T, focus: TypoFocus) -> std::io::Result<()> {
    tty.write_all(b"\r")?;
    tty.write_all(typo_choices_line(focus).as_bytes())?;
    tty.flush()
}

fn typo_focus_choice(focus: TypoFocus) -> TypoChoice {
    match focus {
        TypoFocus::Correction => TypoChoice::Correct,
        TypoFocus::Original => TypoChoice::Original,
    }
}

/// The L1 prompt proper, generic over the terminal handle so the key protocol
/// is testable without a tty. The escaper guards every piece of untrusted text
/// in the message; only trusted reverse-video sequences mark keyboard focus.
fn run_typo_prompt<T: PromptTty>(tty: &mut T, original: &str, replacement: &str) -> TypoChoice {
    let mut focus = TypoFocus::Original;
    let msg = format!(
        "\r\n\r\n*** oops? ***\r\nYou typed '{}'.\r\nDid you mean '{}'?\r\n{}",
        escape_for_typo_prompt(original),
        escape_for_typo_prompt(replacement),
        typo_choices_line(focus),
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
            Key::Char(b'n' | b'N') => TypoChoice::Original,
            Key::Char(b'\t') => {
                focus = toggle_typo_focus(focus);
                if redraw_typo_choices(tty, focus).is_err() {
                    let _ = tty.write_all(FOCUS_OFF.as_bytes());
                    break;
                }
                continue;
            }
            Key::Char(b'\r' | b'\n') => typo_focus_choice(focus),
            Key::Char(0x03) => TypoChoice::Cancel, // Ctrl-C
            // an escape sequence is not an answer — keep waiting
            Key::Seq => continue,
            Key::Timeout | Key::InvalidSequence => TypoChoice::Timeout,
            // any other key, lone ESC, or read error: the do-nothing outcome
            Key::Char(_) | Key::Esc => TypoChoice::Original,
            Key::Err => TypoChoice::Timeout,
        };
        break;
    }
    let ending: &[u8] = if choice == TypoChoice::Timeout {
        "\x1b[0m\r\noopsinput: timed out — running original unchanged\r\n".as_bytes()
    } else {
        b"\x1b[0m\r\n"
    };
    let _ = tty.write_all(ending);
    choice
}

/// The L2+ warning prompt (SPEC §7 anatomy: the caller's lines say what the
/// command does, what it hits, and why context is unusual; this adds the
/// keys). Only deliberate keys decide and unrecognized keys are ignored.
/// Timeout is returned distinctly; the caller applies the tier-specific
/// physical default without misrecording it as a deliberate choice.
fn run_warning_prompt<T: PromptTty>(tty: &mut T, lines: &[String]) -> WarningPrompt {
    let mut msg = String::from("\r\n");
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
        return WarningPrompt::NotShown; // fail open (SPEC §9-8)
    }
    let mut choice = WarnChoice::Timeout;
    for _ in 0..MAX_PROMPT_KEYS {
        choice = match read_key(tty) {
            Key::Char(b'e' | b'E') => WarnChoice::Edit,
            Key::Char(b'c' | b'C') | Key::Char(0x03) | Key::Esc => WarnChoice::Cancel,
            Key::Char(b'r' | b'R') => WarnChoice::RunOnce,
            Key::Timeout | Key::InvalidSequence => WarnChoice::Timeout,
            Key::Err => WarnChoice::Timeout,
            // not an answer: escape sequences and unrecognized keys
            Key::Seq | Key::Char(_) => continue,
        };
        break;
    }
    let _ = tty.write_all(b"\r\n");
    WarningPrompt::Shown(choice)
}

/// Assemble the warning's message lines (SPEC §7 anatomy: what the command
/// does, what it hits, why the current context is unusual — the UI adds the
/// keys). Trusted template text plus escaped untrusted fragments only.
pub fn warning_lines(
    reason: &str,
    danger: &crate::layers::danger::Analysis,
    context: Option<&crate::layers::context::Context>,
    recency: &[crate::proposal::RecencyEntry],
    model: Option<&crate::layers::infer::ModelEvidence>,
) -> Vec<String> {
    match reason {
        "policy.dirty_work_at_risk" => dirty_work_lines(danger, context, recency),
        "policy.main_branch_force" => vec![
            "force-push will rewrite the remote branch's history".to_string(),
            "the current branch is a primary branch (main/master/trunk)".to_string(),
        ],
        "policy.target_context" => {
            let t = context.map(|c| c.targets.as_slice()).unwrap_or(&[]);
            let why = if danger.has("fs.target_cwd") || t.iter().any(|t| t.is_cwd) {
                "the target is the current directory itself"
            } else if danger.has("fs.target_parent") || t.iter().any(|t| t.is_parent) {
                "the target is the parent of the current directory"
            } else {
                "the target does not exist, but a similarly-named neighbor does — typo?"
            };
            vec![
                format!("recursive delete of {}", display_targets(danger)),
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
            let what = if danger.has("fs.target_home") {
                "your entire home directory"
            } else {
                "the filesystem root"
            };
            vec![
                format!("this recursively deletes {what}"),
                "there is no undo".to_string(),
            ]
        }
        // The two model-driven warns (SPEC §5-L4). The reason sentence is
        // model output — untrusted, escaped, and labeled as the model's so
        // the user can weigh it accordingly (honest claims, SPEC §2-10).
        "policy.model_mismatch" => {
            // The gate's flagship class (git work-loss commands) has no
            // filesystem targets — the target clause only appears when
            // there is a real target to name (bughunt 2026-08-06).
            let what = if danger.targets.is_empty() {
                "high-consequence command, and the local model sees a \
                 probable mismatch with what you meant"
                    .to_string()
            } else {
                format!(
                    "high-consequence command targeting {}, and the local \
                     model sees a probable mismatch with what you meant",
                    display_targets(danger)
                )
            };
            model_warning_lines(what, model)
        }
        "policy.model_adversarial" => model_warning_lines(
            "this command's text appears designed to manipulate the guard itself".to_string(),
            model,
        ),
        _ => vec![format!(
            "high-consequence command flagged by policy ({reason})"
        )],
    }
}

fn dirty_work_lines(
    danger: &crate::layers::danger::Analysis,
    context: Option<&crate::layers::context::Context>,
    recency: &[crate::proposal::RecencyEntry],
) -> Vec<String> {
    let action = if danger.has("git.reset_hard") {
        "git reset --hard will discard uncommitted changes in tracked files"
    } else {
        "git clean -f will delete untracked files"
    };
    let git = context.and_then(|c| c.git.as_ref());
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
    // These words are charset-restricted twice (plugin, then parser), and
    // escaped here anyway — SPEC §9-4 says *all* displayed untrusted text
    // goes through the escaper, with no exemption for text believed to be
    // safe already (audit 2026-08-06: a rule that holds only while a distant
    // charset check stays correct is a rule that breaks silently when that
    // check is edited).
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

fn display_targets(danger: &crate::layers::danger::Analysis) -> String {
    let joined = danger
        .targets
        .iter()
        .map(|target| format!("'{}'", escape_for_display(target)))
        .collect::<Vec<_>>()
        .join(", ");
    if joined.is_empty() {
        "its target".to_string()
    } else {
        joined
    }
}

fn model_warning_lines(
    summary: String,
    model: Option<&crate::layers::infer::ModelEvidence>,
) -> Vec<String> {
    let mut lines = vec![summary];
    if let Some(model) = model {
        lines.push(format!("model: {}", escape_for_display(&model.reason)));
    }
    lines
}

/// Neutralize text for display (SPEC §9-5): C0/C1 controls and DEL become
/// caret/escape notation, bidi controls and invisible formatting characters
/// become visible `\u{...}` escapes. Anything that survives this function is
/// inert on a terminal.
pub fn escape_for_display(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        push_escaped_char(&mut out, c);
    }
    out
}

/// The same terminal-neutral representation as `escape_for_display`, bounded
/// for the two complete command buffers repeated in the typo prompt. The
/// ellipsis is trusted literal text; no raw control can survive before it.
fn escape_for_typo_prompt(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(TYPO_COMMAND_DISPLAY_LIMIT_BYTES + 3));
    for c in s.chars() {
        let before = out.len();
        push_escaped_char(&mut out, c);
        if out.len() > TYPO_COMMAND_DISPLAY_LIMIT_BYTES {
            out.truncate(before);
            out.push('…');
            break;
        }
    }
    out
}

fn push_escaped_char(out: &mut String, c: char) {
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
        //
        // Know its reach before trusting it (test-audit 2026-08-06): "inert"
        // here means `needs_escape`, so this test proves `escape_for_display`
        // agrees with that predicate — not that the predicate is right.
        // Deleting the bidi-override range from `needs_escape` leaves this
        // test green; the hand-written cases below are what catch that, and
        // a new dangerous character class needs a case there, not more fuzz.
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

    /// A terminal stream with an explicit read timeout between bytes. Real
    /// terminals can split an escape sequence across the 0.1-second drain
    /// window; a later final byte must not become a fresh prompt answer.
    struct GappedTty {
        input: std::collections::VecDeque<Option<u8>>,
        output: Vec<u8>,
    }

    impl GappedTty {
        fn new(input: impl IntoIterator<Item = Option<u8>>) -> Self {
            Self {
                input: input.into_iter().collect(),
                output: Vec::new(),
            }
        }
    }

    impl Read for GappedTty {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.input.pop_front().flatten() {
                Some(byte) => {
                    buf[0] = byte;
                    Ok(1)
                }
                None => Ok(0),
            }
        }
    }

    impl Write for GappedTty {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl PromptTty for GappedTty {}

    struct UnwritableTty;

    impl Read for UnwritableTty {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }

    impl Write for UnwritableTty {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("display unavailable"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl PromptTty for UnwritableTty {}

    #[test]
    fn y_consents_and_full_comparison_is_framed() {
        let mut tty = FakeTty::new(b"y");
        assert_eq!(
            run_typo_prompt(&mut tty, "gti pull", "git pull"),
            TypoChoice::Correct
        );
        let shown = tty.shown();
        assert!(
            shown.starts_with("\r\n\r\n*** oops? ***\r\n"),
            "prompt did not begin after a clean blank line with its trusted banner: {shown:?}"
        );
        assert!(
            shown.contains("You typed 'gti pull'."),
            "complete original buffer missing: {shown}"
        );
        assert!(
            shown.contains("Did you mean 'git pull'?"),
            "complete replacement buffer missing: {shown}"
        );
        assert!(
            shown.contains("[y] run correction") && shown.contains("[n] run original"),
            "compact choices missing: {shown}"
        );
        assert!(
            shown.contains("\x1b[7m[n] run original\x1b[0m"),
            "original choice did not start focused: {shown:?}"
        );
        assert!(!shown.contains("cancel"), "Ctrl-C was advertised: {shown}");
    }

    #[test]
    fn uppercase_y_also_consents() {
        let mut tty = FakeTty::new(b"Y");
        assert_eq!(run_typo_prompt(&mut tty, "gti", "git"), TypoChoice::Correct);
    }

    #[test]
    fn n_enter_and_unrecognized_keys_run_the_original() {
        for key in [b'n', b'N', b' ', b'q', 0x1b, b'\r', b'\n'] {
            let mut tty = FakeTty::new(&[key]);
            assert_eq!(
                run_typo_prompt(&mut tty, "gti", "git"),
                TypoChoice::Original,
                "key {key:#04x} must mean original"
            );
        }
    }

    #[test]
    fn tab_moves_focus_and_enter_activates_it() {
        let mut tty = FakeTty::new(b"\t\r");
        assert_eq!(
            run_typo_prompt(&mut tty, "gti pull", "git pull"),
            TypoChoice::Correct
        );
        assert!(
            tty.shown()
                .contains("\r\x1b[7m[y] run correction\x1b[0m  [n] run original"),
            "Tab did not redraw the same row with correction focused: {:?}",
            tty.shown()
        );

        let mut tty = FakeTty::new(b"\t\t\r");
        assert_eq!(
            run_typo_prompt(&mut tty, "gti pull", "git pull"),
            TypoChoice::Original,
            "a second Tab must cycle focus back to the original"
        );
    }

    #[test]
    fn full_command_text_is_escaped_and_display_bounded() {
        let hostile = format!("gti ^\n\x1b]0;EVIL\x07{}", "x".repeat(700));
        let corrected = hostile.replacen("gti", "git", 1);
        let mut tty = FakeTty::new(b"n");
        assert_eq!(
            run_typo_prompt(&mut tty, &hostile, &corrected),
            TypoChoice::Original
        );
        let shown = tty.shown();
        assert!(shown.contains("^J"), "newline was not escaped: {shown:?}");
        assert!(
            shown.contains("^[]0;EVIL^G"),
            "terminal controls were not escaped: {shown:?}"
        );
        assert!(shown.contains('…'), "long command was not display-bounded");
        let untrusted_section = shown
            .split_once("You typed '")
            .map(|(_, rest)| rest)
            .unwrap_or("");
        assert!(
            !untrusted_section.contains("\x1b]0;EVIL"),
            "untrusted raw ESC reached the prompt: {shown:?}"
        );
    }

    #[test]
    fn ctrl_c_cancels() {
        let mut tty = FakeTty::new(&[0x03]);
        assert_eq!(run_typo_prompt(&mut tty, "gti", "git"), TypoChoice::Cancel);
    }

    #[test]
    fn timeout_empty_read_runs_the_original() {
        // Owner-session reproduction (2026-08-09): the structural event was
        // `typo.timed_out`, but the same-row prompt ended without naming that
        // outcome, making fail-open look like unprompted acceptance. A direct
        // unchanged PTY probe measured the configured timeout at 10.07 s.
        // `min 0 time N` expiry surfaces as a 0-byte read.
        let mut tty = FakeTty::new(b"");
        assert_eq!(run_typo_prompt(&mut tty, "gti", "git"), TypoChoice::Timeout);
        assert!(
            tty.shown()
                .ends_with("\r\noopsinput: timed out — running original unchanged\r\n"),
            "timeout outcome was not made explicit: {:?}",
            tty.shown()
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
    fn long_csi_final_byte_never_becomes_a_prompt_answer() {
        // Real-PTY reproduction (2026-08-08): after sixteen CSI parameter
        // bytes, the old bounded reader returned early and the sequence's
        // final `y` became typo consent. A following ordinary `n` must be the
        // answer; without one, the prompt reports timeout.
        let mut tty = FakeTty::new(b"\x1b[1111111111111111yn");
        assert_eq!(
            run_typo_prompt(&mut tty, "gti", "git"),
            TypoChoice::Original
        );
        assert_eq!(tty.input.position() as usize, tty.input.get_ref().len());

        let mut tty = FakeTty::new(b"\x1b[1111111111111111y");
        assert_eq!(run_typo_prompt(&mut tty, "gti", "git"), TypoChoice::Timeout);

        let mut tty = FakeTty::new(b"\x1b[1111111111111111rc");
        assert_eq!(
            run_warning_prompt(&mut tty, &["x".into()]),
            WarningPrompt::Shown(WarnChoice::Cancel)
        );

        // A sequence that pauses longer than the drain window is incomplete,
        // not complete-plus-a-new-key. Before the fix, the delayed final `y`
        // or `r` was consumed by the outer prompt loop as consent.
        let mut tty = GappedTty::new([Some(0x1b), Some(b'['), Some(b'1'), None, Some(b'y')]);
        assert_eq!(run_typo_prompt(&mut tty, "gti", "git"), TypoChoice::Timeout);
        assert_eq!(
            tty.input.len(),
            1,
            "delayed CSI final was read as an answer"
        );

        let mut tty = GappedTty::new([Some(0x1b), Some(b'['), Some(b'1'), None, Some(b'r')]);
        assert_eq!(
            run_warning_prompt(&mut tty, &["x".into()]),
            WarningPrompt::Shown(WarnChoice::Timeout)
        );
        assert_eq!(tty.input.len(), 1, "delayed CSI final was read as consent");
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
            run_warning_prompt(&mut tty, &["x".into()]),
            WarningPrompt::Shown(WarnChoice::Timeout)
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
            run_warning_prompt(&mut tty, &lines()),
            WarningPrompt::Shown(WarnChoice::Cancel)
        );
        let shown = tty.shown();
        assert!(
            shown.starts_with("\r\n"),
            "warning did not begin on a clean line: {shown:?}"
        );
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
                run_warning_prompt(&mut tty, &lines()),
                WarningPrompt::Shown(want),
                "keys {keys:?}"
            );
        }
    }

    #[test]
    fn warning_timeout_stays_distinct_from_deliberate_keys() {
        let mut tty = FakeTty::new(b"");
        assert_eq!(
            run_warning_prompt(&mut tty, &lines()),
            WarningPrompt::Shown(WarnChoice::Timeout)
        );
    }

    #[test]
    fn warning_display_failure_is_not_a_visible_intervention() {
        // Regression (M5 bughunt 2026-08-08): setup/write failure used to
        // return RunOnce indistinguishably from a displayed prompt whose user
        // chose `r`, causing the caller to spend budget for a warning nobody
        // saw.
        assert_eq!(
            run_warning_prompt(&mut UnwritableTty, &lines()),
            WarningPrompt::NotShown
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
        let lines = warning_lines("policy.dirty_work_at_risk", &danger, Some(&ctx), &[], None);
        let joined = lines.join("\n");
        assert!(joined.contains("git reset --hard"), "{joined}");
        assert!(joined.contains("17 modified tracked files"), "{joined}");
        assert!(joined.contains("untracked files present"), "{joined}");

        // catastrophic home delete names what dies
        let danger = crate::layers::danger::analyze_with_home(&crate::lexer::lex("rm -rf ~"), None);
        let joined =
            warning_lines("policy.direct_catastrophic", &danger, None, &[], None).join("\n");
        assert!(joined.contains("home directory"), "{joined}");

        // hostile target text is escaped before display (SPEC §9-5)
        let danger = crate::layers::danger::analyze_with_home(
            &crate::lexer::lex("rm -rf ./x\u{1b}EVIL\u{7}y"),
            None,
        );
        let joined = warning_lines("policy.target_context", &danger, None, &[], None).join("\n");
        assert!(!joined.contains('\u{1b}'), "raw ESC in warning: {joined:?}");
        assert!(joined.contains("./x^[EVIL^Gy"), "{joined}");
    }

    #[test]
    fn model_warning_without_targets_reads_cleanly() {
        // Regression (bughunt 2026-08-06, probed: this test failed with
        // "high-consequence command targeting its target" before the fix):
        // the flagship gate-eligible class — git reset --hard, no
        // filesystem targets — hit the targets() placeholder and rendered
        // a degenerate first line. No targets ⇒ no target clause.
        use crate::layers::infer::{MismatchKind, ModelAssessment, ModelEvidence};
        let danger =
            crate::layers::danger::analyze_with_home(&crate::lexer::lex("git reset --hard"), None);
        assert!(danger.targets.is_empty(), "probe premise: no targets");
        let ev = ModelEvidence {
            assessment: ModelAssessment::ProbableMismatch,
            kind: MismatchKind::Target,
            reason: "x".into(),
        };
        let joined =
            warning_lines("policy.model_mismatch", &danger, None, &[], Some(&ev)).join("\n");
        assert!(
            !joined.contains("its target"),
            "degenerate target clause: {joined}"
        );
        assert!(joined.contains("high-consequence command"), "{joined}");

        // ...and with a real target the clause names it, as before.
        let danger = crate::layers::danger::analyze_with_home(
            &crate::lexer::lex("dd if=x of=backup.img"),
            None,
        );
        if !danger.targets.is_empty() {
            let joined =
                warning_lines("policy.model_mismatch", &danger, None, &[], Some(&ev)).join("\n");
            assert!(joined.contains("backup.img"), "{joined}");
        }
    }

    #[test]
    fn model_reason_is_labeled_and_escaped() {
        // The model's reason is untrusted output (SPEC §9-4/§9-5): a hostile
        // or confused model emitting ANSI must be displayed inert, and the
        // line must be labeled as the model's, not presented as our fact.
        use crate::layers::infer::{MismatchKind, ModelAssessment, ModelEvidence};
        let danger =
            crate::layers::danger::analyze_with_home(&crate::lexer::lex("git reset --hard"), None);
        let ev = ModelEvidence {
            assessment: ModelAssessment::ProbableMismatch,
            kind: MismatchKind::Target,
            reason: "wipes \u{1b}[31mEVIL\u{7} work".into(),
        };
        let joined =
            warning_lines("policy.model_mismatch", &danger, None, &[], Some(&ev)).join("\n");
        assert!(joined.contains("model: "), "{joined}");
        assert!(!joined.contains('\u{1b}'), "raw ESC in warning: {joined:?}");
        assert!(joined.contains("wipes ^[[31mEVIL^G work"), "{joined}");
    }

    #[test]
    fn hostile_names_are_escaped_in_the_prompt() {
        // A candidate carrying an OSC sequence and a bidi override must be
        // displayed inert (SPEC §9-5 — this is the anti-spoofing invariant).
        let mut tty = FakeTty::new(b"n");
        run_typo_prompt(&mut tty, "x\u{1b}]0;EVIL\u{7}", "gi\u{202E}t");
        let shown = tty.shown();
        // The prompt now owns exactly two trusted SGR sequences for focus.
        // Remove those fixed bytes before asserting that no untrusted escape
        // sequence survived the display escaper.
        let unstyled = shown.replace(FOCUS_ON, "").replace(FOCUS_OFF, "");
        assert!(
            !unstyled.contains('\u{1b}'),
            "raw ESC reached the tty: {shown:?}"
        );
        assert!(
            !unstyled.contains('\u{202E}'),
            "raw bidi override reached the tty: {shown:?}"
        );
        assert!(
            unstyled.contains("^[]0;EVIL^G"),
            "escaped form missing: {shown:?}"
        );
        assert!(
            unstyled.contains("gi\\u{202E}t"),
            "escaped bidi missing: {shown:?}"
        );
    }
}
