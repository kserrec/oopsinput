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
    /// closed-vocabulary resolution kind of the command word
    pub res_kind: &'static str,
    /// structural size features — never content
    pub buffer_bytes: usize,
    pub word_count: usize,
    pub duration_us: u128,
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
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    // Best-effort tighten to user-only; failure is not fatal.
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));

    let Ok(line) = serde_json::to_string(event) else {
        return;
    };
    let Ok(mut f) = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(dir.join("events.jsonl"))
    else {
        return;
    };
    let _ = writeln!(f, "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serializes_structural_fields_only() {
        let e = Event {
            ts_ms: 1,
            decision: "allow",
            reason_code: "shadow.observed",
            res_kind: "command",
            buffer_bytes: 9,
            word_count: 2,
            duration_us: 42,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"decision\":\"allow\""));
        assert!(json.contains("\"buffer_bytes\":9"));
        // The event type has no field that could carry buffer content; this
        // test documents that invariant for future editors.
        assert!(!json.contains("buffer_text"));
    }
}
