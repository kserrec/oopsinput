//! Append-only JSONL event log. SPEC §9/§14: structural features only — no raw
//! command text, paths, or secrets; user-only permissions; write failures never
//! block the user's command (fail open, stay silent).

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[derive(Serialize)]
pub struct Event {
    /// unix milliseconds
    pub ts_ms: u64,
    pub decision: &'static str,
    pub reason_code: &'static str,
    /// stable evidence codes (static strings by type — can't carry raw text)
    pub evidence: Vec<&'static str>,
    /// closed-vocabulary resolution kind of the command word
    pub res_kind: &'static str,
    /// first command word contains an expansion/substitution/glob
    pub cmd_expands: bool,
    /// structural size features — never content
    pub buffer_bytes: usize,
    pub word_count: usize,
    pub duration_us: u128,
    /// Context-layer counts (present only when L3 ran): dirty tracked files,
    /// and the largest entry count among directory targets. Counts only —
    /// never names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctx_git_dirty: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctx_target_entries: Option<u32>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// State dir: $OOPSINPUT_STATE_DIR override (tests, custom setups) else
/// $XDG_STATE_HOME/oopsinput else ~/.local/state/oopsinput.
fn state_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("OOPSINPUT_STATE_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("oopsinput"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".local/state/oopsinput"))
}

/// Append one event. Any failure is swallowed: logging must never cost the
/// user their command or their prompt.
pub fn append(event: &Event) {
    let Some(dir) = state_dir() else { return };
    append_to(&dir, event);
}

fn append_to(dir: &std::path::Path, event: &Event) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // Best-effort tighten to user-only; failure is not fatal.
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));

    let Ok(mut line) = serde_json::to_string(event) else {
        return;
    };
    // One buffer, one write syscall: concurrent shells append to this file,
    // and O_APPEND atomicity holds per write call — a separate newline write
    // would let two processes interleave and corrupt the JSONL stream.
    line.push('\n');
    let Ok(mut f) = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(dir.join("events.jsonl"))
    else {
        return;
    };
    let _ = f.write_all(line.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_appends_never_interleave() {
        // Regression (bughunt #1): the line and its newline were two separate
        // write syscalls, so simultaneous shells could corrupt the JSONL
        // stream. Hammer from many threads and require every line to parse.
        let dir = std::env::temp_dir().join(format!("oopsinput-ev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        std::thread::scope(|s| {
            for t in 0..16 {
                let dir = dir.clone();
                s.spawn(move || {
                    for i in 0..50 {
                        append_to(
                            &dir,
                            &Event {
                                ts_ms: t * 1000 + i,
                                decision: "allow",
                                reason_code: "shadow.observed",
                                evidence: vec!["syntax.heredoc"],
                                res_kind: "command",
                                cmd_expands: false,
                                buffer_bytes: 10,
                                word_count: 2,
                                duration_us: 1,
                                ctx_git_dirty: None,
                                ctx_target_entries: None,
                            },
                        );
                    }
                });
            }
        });

        let log = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 16 * 50, "lost or glued lines");
        for line in lines {
            assert!(
                serde_json::from_str::<serde_json::Value>(line).is_ok(),
                "corrupt JSONL line: {line}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn event_serializes_structural_fields_only() {
        let e = Event {
            ts_ms: 1,
            decision: "allow",
            reason_code: "shadow.observed",
            evidence: vec!["syntax.opaque_substitution"],
            res_kind: "command",
            cmd_expands: true,
            buffer_bytes: 9,
            word_count: 2,
            duration_us: 42,
            ctx_git_dirty: Some(14),
            ctx_target_entries: None,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"decision\":\"allow\""));
        assert!(json.contains("\"buffer_bytes\":9"));
        assert!(json.contains("\"evidence\":[\"syntax.opaque_substitution\"]"));
        assert!(json.contains("\"ctx_git_dirty\":14"));
        // absent context counts stay out of the line entirely
        assert!(!json.contains("ctx_target_entries"));
        // The event type has no field that could carry buffer content; this
        // test documents that invariant for future editors.
        assert!(!json.contains("buffer_text"));
    }
}
