//! JSONL event log: new records append; retention atomically compacts expired
//! records. SPEC §9/§14: structural features only — no raw command text,
//! paths, or secrets; user-only permissions; write failures never block the
//! user's command (fail open, stay silent).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::BufRead;
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
    /// Ollama model state immediately before a consultation: warm | cold |
    /// unknown. Absent when L4 did not run. Structural metadata only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_state: Option<&'static str>,
    /// Context-layer counts (present only when L3 ran): dirty tracked files,
    /// and the largest entry count among directory targets. Counts only —
    /// never names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctx_git_dirty: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctx_target_entries: Option<u32>,
    /// What the user did at a visible L2+ intervention (SPEC §4 — central to
    /// evaluation, distinct from the decision): edited | cancelled |
    /// ran_unchanged. Absent when nothing was shown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<&'static str>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Append one event. Any failure is swallowed: logging must never cost the
/// user their command or their prompt.
pub fn append(event: &Event) {
    let Some(dir) = crate::state::state_dir() else {
        return;
    };
    append_to(&dir, event);
}

fn append_to(dir: &std::path::Path, event: &Event) {
    let Ok(mut line) = serde_json::to_string(event) else {
        return;
    };
    // One buffer under the shared state lock: concurrent shells cannot
    // interleave a line or race the retention compactor.
    line.push('\n');
    let result = if crate::prompt_is_active() {
        crate::state::append_jsonl_after_prompt(dir, "events.jsonl", line.as_bytes(), event.ts_ms)
    } else {
        crate::state::append_jsonl(dir, "events.jsonl", line.as_bytes(), event.ts_ms)
    };
    let _ = result;
}

#[derive(Debug)]
pub enum ReportError {
    StateDirUnavailable,
    Read(std::io::Error),
}

impl std::fmt::Display for ReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StateDirUnavailable => {
                write!(f, "HOME is unset, so the state directory cannot be located")
            }
            Self::Read(error) => write!(f, "could not read the event log: {error}"),
        }
    }
}

#[derive(serde::Deserialize)]
struct StoredEvent {
    decision: String,
    reason_code: String,
    #[serde(default)]
    evidence: Vec<String>,
    duration_us: u128,
    #[serde(default)]
    model_state: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
}

#[derive(Default)]
struct Report {
    events: usize,
    malformed: usize,
    model_consulted: usize,
    visible_interventions: usize,
    hypothetical_interventions: usize,
    decisions: BTreeMap<String, usize>,
    evidence: BTreeMap<String, usize>,
    hypothetical_reasons: BTreeMap<String, usize>,
    outcomes: BTreeMap<String, usize>,
    deterministic_us: Vec<u128>,
    model_warm_us: Vec<u128>,
    model_cold_us: Vec<u128>,
    model_unknown_us: Vec<u128>,
}

impl Report {
    fn record(&mut self, event: StoredEvent) {
        self.events += 1;
        *self.decisions.entry(event.decision.clone()).or_default() += 1;
        for code in &event.evidence {
            *self.evidence.entry(code.clone()).or_default() += 1;
        }
        if let Some(outcome) = event.outcome {
            self.visible_interventions += 1;
            *self.outcomes.entry(outcome).or_default() += 1;
        }
        if event.decision == "observe" && event.reason_code.starts_with("policy.") {
            self.hypothetical_interventions += 1;
            *self
                .hypothetical_reasons
                .entry(event.reason_code)
                .or_default() += 1;
        }

        let consulted = event.evidence.iter().any(|code| code.starts_with("model."));
        if !consulted {
            self.deterministic_us.push(event.duration_us);
            return;
        }
        self.model_consulted += 1;
        match event.model_state.as_deref() {
            Some("warm") => self.model_warm_us.push(event.duration_us),
            Some("cold") => self.model_cold_us.push(event.duration_us),
            _ => self.model_unknown_us.push(event.duration_us),
        }
    }
}

/// Build the human-facing M5 report from the append-only event log. Missing
/// logs are an empty data set, not an error. A malformed/torn individual line
/// is skipped and counted; valid neighboring events remain usable.
pub fn report_text() -> Result<String, ReportError> {
    let dir = crate::state::state_dir().ok_or(ReportError::StateDirUnavailable)?;
    report_text_from_path(&dir.join("events.jsonl"))
}

fn report_text_from_path(path: &std::path::Path) -> Result<String, ReportError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return Err(ReportError::Read(std::io::Error::other(
                "event log is not a regular file",
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(render_report(&Report::default()));
        }
        Err(error) => return Err(ReportError::Read(error)),
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(render_report(&Report::default()));
        }
        Err(error) => return Err(ReportError::Read(error)),
    };
    let mut report = Report::default();
    for line in std::io::BufReader::new(file).split(b'\n') {
        let line = line.map_err(ReportError::Read)?;
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<StoredEvent>(&line) {
            Ok(event) => report.record(event),
            Err(_) => report.malformed += 1,
        }
    }
    Ok(render_report(&report))
}

fn render_report(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "oopsinput report");
    let _ = writeln!(out, "  events: {}", report.events);
    if report.malformed > 0 {
        let _ = writeln!(out, "  malformed lines skipped: {}", report.malformed);
    }
    if report.events == 0 {
        let _ = writeln!(out, "  no events recorded");
        return out;
    }

    let _ = writeln!(out, "\nrates");
    write_rate(
        &mut out,
        "model consulted",
        report.model_consulted,
        report.events,
        false,
    );
    write_rate(
        &mut out,
        "visible L2+ interventions",
        report.visible_interventions,
        report.events,
        true,
    );
    write_rate(
        &mut out,
        "hypothetical interventions",
        report.hypothetical_interventions,
        report.events,
        true,
    );

    let _ = writeln!(out, "\ndecisions");
    write_ranked(&mut out, &report.decisions, report.events, true);

    let _ = writeln!(out, "\nanalysis latency");
    write_latency(&mut out, "deterministic", &report.deterministic_us);
    write_latency(&mut out, "model warm", &report.model_warm_us);
    write_latency(&mut out, "model cold", &report.model_cold_us);
    write_latency(&mut out, "model unknown", &report.model_unknown_us);

    let _ = writeln!(out, "\nevidence codes");
    if report.evidence.is_empty() {
        let _ = writeln!(out, "  none");
    } else {
        write_ranked(&mut out, &report.evidence, report.events, false);
    }

    let _ = writeln!(out, "\nhypothetical intervention reasons");
    if report.hypothetical_reasons.is_empty() {
        let _ = writeln!(out, "  none");
    } else {
        write_ranked(&mut out, &report.hypothetical_reasons, report.events, false);
    }

    let _ = writeln!(out, "\nvisible intervention outcomes");
    if report.outcomes.is_empty() {
        let _ = writeln!(out, "  none");
    } else {
        write_ranked(
            &mut out,
            &report.outcomes,
            report.visible_interventions,
            true,
        );
    }
    out
}

fn write_rate(out: &mut String, label: &str, count: usize, total: usize, per_thousand: bool) {
    let percent = count as f64 * 100.0 / total as f64;
    if per_thousand {
        let rate = count as f64 * 1_000.0 / total as f64;
        let _ = writeln!(
            out,
            "  {label}: {count}/{total} ({percent:.2}%; {rate:.2} per 1,000)"
        );
    } else {
        let _ = writeln!(out, "  {label}: {count}/{total} ({percent:.2}%)");
    }
}

fn write_ranked(out: &mut String, counts: &BTreeMap<String, usize>, total: usize, rates: bool) {
    let mut ranked: Vec<_> = counts.iter().collect();
    ranked.sort_by(|(left_name, left_count), (right_name, right_count)| {
        right_count
            .cmp(left_count)
            .then_with(|| left_name.cmp(right_name))
    });
    for (name, count) in ranked {
        let name = crate::ui::escape_for_display(name);
        if rates {
            let percent = *count as f64 * 100.0 / total as f64;
            let _ = writeln!(out, "  {name}: {count} ({percent:.2}%)");
        } else {
            let _ = writeln!(out, "  {name}: {count}");
        }
    }
}

fn write_latency(out: &mut String, label: &str, values: &[u128]) {
    if values.is_empty() {
        let _ = writeln!(out, "  {label}: no events");
        return;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let p50 = percentile(&sorted, 50);
    let p95 = percentile(&sorted, 95);
    let p99 = percentile(&sorted, 99);
    let _ = writeln!(
        out,
        "  {label} (n={}): p50 {}, p95 {}, p99 {}",
        sorted.len(),
        format_duration_us(p50),
        format_duration_us(p95),
        format_duration_us(p99),
    );
}

/// Nearest-rank percentile over a non-empty sorted sample.
fn percentile(sorted: &[u128], percent: usize) -> u128 {
    let rank = sorted.len().saturating_mul(percent).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn format_duration_us(us: u128) -> String {
    if us < 1_000 {
        format!("{us} us")
    } else if us < 1_000_000 {
        format!("{:.3} ms", us as f64 / 1_000.0)
    } else {
        format!("{:.3} s", us as f64 / 1_000_000.0)
    }
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
                                model_state: None,
                                ctx_git_dirty: None,
                                ctx_target_entries: None,
                                outcome: None,
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
    fn a_symlinked_log_path_is_refused_not_written_through() {
        // Regression (audit 2026-08-06): appending through a symlink placed
        // in our state dir would grow the log onto someone else's file.
        let dir = std::env::temp_dir().join(format!("oopsinput-evlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "PRECIOUS\n").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("events.jsonl")).unwrap();

        append_to(
            &dir,
            &Event {
                ts_ms: 1,
                decision: "allow",
                reason_code: "shadow.observed",
                evidence: vec![],
                res_kind: "command",
                cmd_expands: false,
                buffer_bytes: 1,
                word_count: 1,
                duration_us: 1,
                model_state: None,
                ctx_git_dirty: None,
                ctx_target_entries: None,
                outcome: None,
            },
        );

        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "PRECIOUS\n",
            "the log was written through a symlink"
        );
        assert!(
            report_text_from_path(&dir.join("events.jsonl")).is_err(),
            "report followed a symlinked event log"
        );
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
            model_state: Some("warm"),
            ctx_git_dirty: Some(14),
            ctx_target_entries: None,
            outcome: Some("cancelled"),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"decision\":\"allow\""));
        assert!(json.contains("\"buffer_bytes\":9"));
        assert!(json.contains("\"evidence\":[\"syntax.opaque_substitution\"]"));
        assert!(json.contains("\"model_state\":\"warm\""));
        assert!(json.contains("\"ctx_git_dirty\":14"));
        // absent context counts stay out of the line entirely
        assert!(!json.contains("ctx_target_entries"));
        // The redaction is structural: `Event` has no field that could hold
        // command text, so this test pins the *serialized shape* rather than
        // asserting the absence of a field that never existed (test-audit
        // 2026-08-06 found that assertion could not fail). The real guard
        // against leakage is the PTY test that greps a live log for a secret.
    }

    #[test]
    fn report_splits_model_latency_and_keeps_good_lines_around_a_torn_one() {
        // Probed 2026-08-07 through the real `report` command: a mixed log
        // containing deterministic, warm, cold, and legacy (state absent)
        // model events plus a torn final line. These are the two concrete M5
        // failure modes: treating legacy model latency as deterministic would
        // corrupt the SPEC §10 percentiles, and one interrupted append must
        // not erase every valid event around it. The hostile evidence code
        // also pins SPEC §9-4 at this new display surface.
        let dir = std::env::temp_dir().join(format!("oopsinput-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hostile = serde_json::json!({
            "decision": "allow",
            "reason_code": "shadow.observed",
            "evidence": ["evil\u{1b}[31m"],
            "duration_us": 300,
        });
        let log = format!(
            "{{\"decision\":\"allow\",\"reason_code\":\"shadow.observed\",\"evidence\":[],\"duration_us\":100}}\n\
             {{\"decision\":\"observe\",\"reason_code\":\"policy.dirty_work_at_risk\",\"evidence\":[\"git.reset_hard\"],\"duration_us\":200}}\n\
             {{\"decision\":\"observe\",\"reason_code\":\"policy.model_mismatch\",\"evidence\":[\"model.probable_mismatch\"],\"duration_us\":1500000,\"model_state\":\"warm\"}}\n\
             {{\"decision\":\"observe\",\"reason_code\":\"policy.evidence_unavailable\",\"evidence\":[\"model.timeout\"],\"duration_us\":2500000,\"model_state\":\"cold\"}}\n\
             {{\"decision\":\"observe\",\"reason_code\":\"policy.evidence_unavailable\",\"evidence\":[\"model.invalid\"],\"duration_us\":3500000}}\n\
             {{\"decision\":\"warn\",\"reason_code\":\"policy.target_context\",\"evidence\":[\"fs.rm_recursive\"],\"duration_us\":400,\"outcome\":\"cancelled\"}}\n\
             {hostile}\n\
             this line is torn\n"
        );
        std::fs::write(dir.join("events.jsonl"), log).unwrap();

        let out = report_text_from_path(&dir.join("events.jsonl")).unwrap();
        assert!(out.contains("events: 7"), "{out}");
        assert!(out.contains("malformed lines skipped: 1"), "{out}");
        assert!(out.contains("hypothetical interventions: 4/7"), "{out}");
        assert!(
            out.contains("deterministic (n=4): p50 200 us, p95 400 us"),
            "{out}"
        );
        assert!(out.contains("model warm (n=1): p50 1.500 s"), "{out}");
        assert!(out.contains("model cold (n=1): p50 2.500 s"), "{out}");
        assert!(out.contains("model unknown (n=1): p50 3.500 s"), "{out}");
        assert!(out.contains("evil^[[31m: 1"), "{out}");
        assert!(
            !out.contains('\u{1b}'),
            "active escape reached report: {out:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_of_an_absent_log_is_an_empty_success() {
        // Probed 2026-08-07 with a fresh state directory: a new install has
        // no events file yet, and `report` must explain that rather than turn
        // a normal first run into an error.
        let dir =
            std::env::temp_dir().join(format!("oopsinput-report-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = report_text_from_path(&dir.join("events.jsonl")).unwrap();
        assert_eq!(out, "oopsinput report\n  events: 0\n  no events recorded\n");
    }
}
