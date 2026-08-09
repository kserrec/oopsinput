//! Policy — turns the layers' evidence into a decision (SPEC §4 vocabulary),
//! applies the mode ceiling (§8), the intervention budget and per-rule
//! cooldown (§7 habituation control), and owns the full config surface (§15).
//!
//! The split matters for evaluation: `warranted` is the pure, mode-blind
//! matrix the golden corpus pins (same command, different context, different
//! answer); `cap_for_mode` and `apply_gates` then bound what actually becomes
//! visible. In Shadow/Suggest, a warranted Warn/Confirm is recorded as
//! `observe` and its mode-blind reason is persisted explicitly for M5's
//! hypothetical-intervention report.

use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::layers::context::Context;
use crate::layers::danger::Analysis;

// ---- decision vocabulary --------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Context affirmatively clean — the counterfactual half of a pair.
    Allow,
    /// Candidate recorded; nothing visible warranted (or possible).
    Observe,
    Warn,
    Confirm,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Observe => "observe",
            Self::Warn => "warn",
            Self::Confirm => "confirm",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assessment {
    pub verdict: Verdict,
    pub reason: &'static str,
}

const fn assess(verdict: Verdict, reason: &'static str) -> Assessment {
    Assessment { verdict, reason }
}

/// The mode-blind decision matrix: what the evidence *warrants*. Curated and
/// deterministic; every arm exists to make a golden pair pass — a rule with
/// no context in which it stays silent doesn't belong here (SPEC §11).
pub fn warranted(danger: &Analysis, ctx: Option<&Context>) -> Assessment {
    if danger.codes.is_empty() {
        // Legacy string from M1 — the PTY suite and existing logs pin it.
        return assess(Verdict::Allow, "shadow.observed");
    }
    if danger.catastrophic {
        return assess(Verdict::Confirm, "policy.direct_catastrophic");
    }
    let git = ctx.and_then(|c| c.git.as_ref());
    let targets = ctx.map(|c| c.targets.as_slice()).unwrap_or(&[]);

    // Work-loss Git commands affect different file classes: reset --hard
    // discards tracked/staged work, while clean -f deletes untracked work.
    // A fact about the other class cannot justify an intervention.
    let reset_hard = danger.has("git.reset_hard");
    let clean_force = danger.has("git.clean_force");
    if reset_hard || clean_force {
        return match git {
            Some(g) => {
                let tracked_at_risk = reset_hard && g.dirty.is_some_and(|dirty| dirty > 0);
                let untracked_at_risk = clean_force && g.untracked == Some(true);
                if tracked_at_risk || untracked_at_risk {
                    assess(Verdict::Warn, "policy.dirty_work_at_risk")
                } else {
                    let reset_clear = !reset_hard || g.dirty == Some(0);
                    let clean_clear = !clean_force || g.untracked == Some(false);
                    if reset_clear && clean_clear {
                        assess(Verdict::Allow, "policy.context_clear")
                    } else {
                        // A relevant status fact is unavailable: no claim, no
                        // nag — fail toward silence.
                        assess(Verdict::Observe, "policy.evidence_unavailable")
                    }
                }
            }
            // not in a repo: the command will fail on its own
            None => assess(Verdict::Observe, "policy.evidence_unavailable"),
        };
    }
    if danger.has("git.push_force") {
        return match git {
            Some(g) if g.branch_main_like => assess(Verdict::Warn, "policy.main_branch_force"),
            Some(_) => assess(Verdict::Allow, "policy.context_clear"),
            None => assess(Verdict::Observe, "policy.evidence_unavailable"),
        };
    }
    // Writing to a block device by name shape is warn-worthy on its own —
    // there is no benign-context read of it that L3 can establish.
    if danger.has("fs.target_blockdev") {
        return assess(Verdict::Warn, "policy.blockdev_write");
    }
    if danger.has("fs.rm_recursive") {
        let target_flagged = danger.has("fs.target_cwd")
            || danger.has("fs.target_parent")
            || targets
                .iter()
                .any(|t| t.is_cwd || t.is_parent || t.near_miss);
        if target_flagged {
            return assess(Verdict::Warn, "policy.target_context");
        }
        // The all-exist check below can only vouch for rm when rm's own
        // operands were all knowable — the target list is shared across
        // rules, and a redirect's existing file must not clear an
        // `rm -rf $DIR` (bughunt 2026-08-06).
        if danger.has("fs.rm_target_unknown") {
            return assess(Verdict::Observe, "policy.evidence_unavailable");
        }
        if !targets.is_empty() && targets.iter().all(|t| t.exists) {
            return assess(Verdict::Allow, "policy.context_clear");
        }
        return assess(Verdict::Observe, "policy.evidence_unavailable");
    }
    // Recognized but not yet graduated past shadow (SPEC §8): the pilot's
    // event log decides which of these earn a warn tier.
    assess(Verdict::Observe, "policy.candidate_observed")
}

// ---- L4 candidate gate + advisory evidence (SPEC §5-L4, M4) ---------------

/// Should the inference layer be consulted? Only when the danger layer
/// marked a candidate AND the deterministic context left genuine ambiguity —
/// the two Observe reasons where L3 neither cleared the command nor decided
/// against it. Everything else is settled without a model: Allow means the
/// context vouched for it, Warn/Confirm mean the facts already speak, and
/// direct-catastrophic is excluded outright so no model output can ever
/// touch that path (SPEC §9-6 / M4 acceptance).
pub fn l4_gate(danger: &Analysis, warranted: Assessment) -> bool {
    !danger.codes.is_empty()
        && !danger.catastrophic
        && warranted.verdict == Verdict::Observe
        && matches!(
            warranted.reason,
            "policy.evidence_unavailable" | "policy.candidate_observed"
        )
}

/// Deterministic consumption of the model's advisory evidence (SPEC §2-7:
/// "the model is evidence, not authority"). Exactly two arms, both upgrades
/// capped at Warn: the model asserting a probable mismatch, or reporting
/// that the untrusted text tried to instruct it. Every other answer —
/// including a confident "no mismatch" — changes nothing: there is no
/// downgrade arm, so a lying or compromised model can never clear a command,
/// and Confirm remains reachable only through deterministic rules.
pub fn apply_model_evidence(
    warranted: Assessment,
    consult: Option<&crate::layers::infer::Consult>,
) -> Assessment {
    use crate::layers::infer::{Consult, ModelAssessment};
    match consult {
        Some(Consult::Evidence(e)) => match e.assessment {
            ModelAssessment::ProbableMismatch => assess(Verdict::Warn, "policy.model_mismatch"),
            ModelAssessment::AdversarialOrUntrustedInstruction => {
                assess(Verdict::Warn, "policy.model_adversarial")
            }
            _ => warranted,
        },
        _ => warranted,
    }
}

// ---- modes (SPEC §8) ------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Shadow,
    Suggest,
    Warn,
    Confirm,
}

/// Closed vocabulary; anything else is None (caller warns, defaults Shadow).
fn parse_mode(s: &str) -> Option<Mode> {
    match s {
        "shadow" => Some(Mode::Shadow),
        "suggest" => Some(Mode::Suggest),
        "warn" => Some(Mode::Warn),
        "confirm" => Some(Mode::Confirm),
        _ => None,
    }
}

/// The mode is a ceiling, never a floor. Downgrades preserve the policy
/// reason — that is what makes shadow data usable (§11): an `observe` with
/// reason `policy.dirty_work_at_risk` is a hypothetical intervention.
pub fn cap_for_mode(assessment: Assessment, mode: Mode) -> Assessment {
    match (mode, assessment.verdict) {
        (Mode::Shadow | Mode::Suggest, Verdict::Warn | Verdict::Confirm) => {
            assess(Verdict::Observe, assessment.reason)
        }
        (Mode::Warn, Verdict::Confirm) => assess(Verdict::Warn, assessment.reason),
        _ => assessment,
    }
}

// ---- habituation control: budget + cooldown (SPEC §7) ---------------------

/// One intervention that was actually shown, and what the user did about it.
/// Each new record is appended as one line. The only rewrite is the M5
/// retention sweep, which atomically removes expired records under the same
/// cross-process state lock every writer takes.
///
/// Appending without an uncoordinated read-modify-write is the point
/// (test-audit 2026-08-06, proven): the previous JSON blob let two shells each
/// overwrite the other's outcome, so the hourly cap under-counted and a
/// cooldown could vanish. The stable state lock now also makes retention
/// compaction safe without weakening that invariant.
#[derive(Serialize, Deserialize)]
struct Intervention {
    ts_ms: u64,
    rule: String,
    ran_unchanged: bool,
    /// A timeout may physically run unchanged in Warn mode, but it is not the
    /// deliberate repeated `r` that earns a rule cooldown. Default preserves
    /// compatibility with policy records written before this field existed.
    #[serde(default)]
    timed_out: bool,
    /// The admission this outcome completes. Old records have no ID; keeping
    /// the field optional makes the append-only format backward compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reservation_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct ReservationRecord {
    ts_ms: u64,
    rule: String,
    reservation_id: String,
    reservation_state: ReservationState,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReservationState {
    Reserved,
    Released,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PolicyRecord {
    Intervention(Intervention),
    Reservation(ReservationRecord),
}

/// Recent shown interventions and prompt admissions, oldest first. A prompt
/// admission temporarily spends one budget slot until it is completed,
/// released because no prompt was shown, or expires after the maximum prompt
/// lifetime. That makes the read/check/spend decision atomic across shells.
#[derive(Default)]
pub struct History {
    shown: Vec<Intervention>,
    reservations: Vec<ReservationRecord>,
}

const HOUR_MS: u64 = 3_600_000;
const COOLDOWN_MS: u64 = 24 * HOUR_MS;
/// The warning prompt's bounded retry loop lasts at most 320 seconds. Ten
/// minutes leaves ample room for a live prompt while ensuring a killed or
/// suspended process cannot consume budget indefinitely.
const RESERVATION_TTL_MS: u64 = 10 * 60 * 1_000;
/// Run-unchanged outcomes in a row before a rule goes quiet for a day.
const COOLDOWN_TRIGGER: usize = 3;
static RESERVATION_ID: AtomicU64 = AtomicU64::new(0);

impl History {
    fn shown_within_hour(&self, now_ms: u64) -> usize {
        let shown = self
            .shown
            .iter()
            .filter(|i| now_ms.saturating_sub(i.ts_ms) < HOUR_MS)
            .count();
        let reserved = self
            .reservations
            .iter()
            .filter(|reservation| {
                let age = now_ms.saturating_sub(reservation.ts_ms);
                age < RESERVATION_TTL_MS
            })
            .count();
        shown + reserved
    }

    /// A rule is asleep when its last `COOLDOWN_TRIGGER` outcomes were all
    /// "ran unchanged" — the user saying *I mean it, stop asking* — and that
    /// run is still inside the cooldown window. Any edit or cancel in the
    /// recent run breaks the streak, which is the reset the old counter did.
    fn rule_in_cooldown(&self, rule: &str, now_ms: u64) -> bool {
        let recent: Vec<&Intervention> = self
            .shown
            .iter()
            .rev()
            .filter(|i| i.rule == rule)
            .take(COOLDOWN_TRIGGER)
            .collect();
        recent.len() == COOLDOWN_TRIGGER
            && recent.iter().all(|i| i.ran_unchanged && !i.timed_out)
            && recent
                .first()
                .is_some_and(|newest| now_ms.saturating_sub(newest.ts_ms) < COOLDOWN_MS)
    }
}

/// Gate a visible intervention through the budget and the per-rule cooldown.
/// Direct-catastrophic is exempt from both (SPEC §7). Exhaustion degrades to
/// observe (shadow recording), never to nagging.
///
/// This is a pure decision over history. `admit_intervention` holds the shared
/// state lock around loading that history, applying this function, and
/// appending a short-lived reservation, so concurrent shells cannot all pass
/// the same last budget slot.
pub fn apply_gates(
    assessment: Assessment,
    primary_code: Option<&str>,
    catastrophic: bool,
    history: &History,
    now_ms: u64,
    budget_per_hour: u32,
) -> Assessment {
    if !matches!(assessment.verdict, Verdict::Warn | Verdict::Confirm) {
        return assessment;
    }
    if catastrophic {
        return assessment;
    }
    if let Some(code) = primary_code
        && history.rule_in_cooldown(code, now_ms)
    {
        return assess(Verdict::Observe, "policy.rule_cooldown");
    }
    if history.shown_within_hour(now_ms) as u32 >= budget_per_hour {
        return assess(Verdict::Observe, "policy.budget_exhausted");
    }
    assessment
}

/// The rule a cooldown keys on: the first danger code (rules note their own
/// code before any target classification, so this is the rule identity).
pub fn primary_code(danger: &Analysis) -> Option<&'static str> {
    danger.codes.first().copied()
}

/// Only the tail matters — the budget looks back an hour and a cooldown a
/// day — so a long-lived log costs one bounded read, not a growing one
/// (audit 2026-08-06: our own file is not a size guarantee).
const HISTORY_TAIL_BYTES: u64 = 512 * 1024;
/// The configured hourly budget is capped at 1,000. Reservations and their
/// outcomes are folded to one logical admission before this cap is applied,
/// so the bounded history still holds a full worst-case budget window.
const MAX_HISTORY_RECORDS: usize = 2_048;

/// The loader with the state directory explicit — the seam tests run through,
/// matching `events::append_to`. (A test that re-implements this logic in its
/// own body proves nothing about this function: test-audit 2026-08-06 deleted
/// the caps from the old loader and watched the old test pass anyway.)
#[cfg(test)]
pub(crate) fn load_history_from(dir: &std::path::Path) -> History {
    try_load_history_from(dir).unwrap_or_default()
}

/// Admission must distinguish an empty history from one it could not verify:
/// treating an unreadable/symlinked file as empty would reopen every budget
/// slot. The test-only wrapper above retains the old fail-silent inspection
/// seam; the live admission path consumes this result directly.
fn try_load_history_from(dir: &Path) -> std::io::Result<History> {
    let path = dir.join("policy.jsonl");
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => {
            return Err(std::io::Error::other(
                "policy history is not a regular file",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(History::default());
        }
        Err(error) => return Err(error),
    };
    let mut f = std::fs::File::open(&path)?;
    crate::state::opened_regular_file_metadata(&path, &f, "policy history")?;
    // Read only the tail; seek past everything older.
    if f.metadata()?.len() > HISTORY_TAIL_BYTES {
        f.seek(std::io::SeekFrom::End(-(HISTORY_TAIL_BYTES as i64)))?;
    }
    let mut text = String::new();
    f.read_to_string(&mut text)?;
    // The first line of a tail read is usually cut mid-record; it simply
    // fails to parse, along with any line a partial write left behind.
    let mut shown = Vec::new();
    let mut reservations = std::collections::HashMap::new();
    for record in text
        .lines()
        .filter_map(|line| serde_json::from_str::<PolicyRecord>(line).ok())
    {
        match record {
            PolicyRecord::Intervention(intervention) => {
                if let Some(id) = intervention.reservation_id.as_deref() {
                    reservations.remove(id);
                }
                shown.push(intervention);
            }
            PolicyRecord::Reservation(reservation) => match reservation.reservation_state {
                ReservationState::Reserved => {
                    reservations.insert(reservation.reservation_id.clone(), reservation);
                }
                ReservationState::Released => {
                    reservations.remove(&reservation.reservation_id);
                }
            },
        }
    }
    if shown.len() > MAX_HISTORY_RECORDS {
        let cut = shown.len() - MAX_HISTORY_RECORDS;
        shown.drain(..cut);
    }
    Ok(History {
        shown,
        reservations: reservations.into_values().collect(),
    })
}

/// The result of atomically checking and, when visible, reserving one warning
/// budget slot. The opaque token follows the prompt so its outcome or a
/// not-shown release can close the reservation.
pub(crate) struct Admission {
    pub(crate) assessment: Assessment,
    pub(crate) reservation: Option<ReservationToken>,
}

pub(crate) struct ReservationToken {
    dir: PathBuf,
    id: String,
    rule: String,
}

/// Atomically load policy history, apply budget/cooldown gates, and reserve a
/// visible non-catastrophic intervention. State failures suppress the prompt:
/// the original command runs unchanged instead of allowing an uncoordinated
/// shell to exceed the user's hourly cap.
pub(crate) fn admit_intervention(
    assessment: Assessment,
    primary_code: Option<&str>,
    catastrophic: bool,
    now_ms: u64,
    budget_per_hour: u32,
) -> Admission {
    let dir = crate::state::state_dir();
    admit_intervention_in(
        dir.as_deref(),
        assessment,
        primary_code,
        catastrophic,
        now_ms,
        budget_per_hour,
    )
}

fn admit_intervention_in(
    dir: Option<&Path>,
    assessment: Assessment,
    primary_code: Option<&str>,
    catastrophic: bool,
    now_ms: u64,
    budget_per_hour: u32,
) -> Admission {
    if !matches!(assessment.verdict, Verdict::Warn | Verdict::Confirm) || catastrophic {
        return Admission {
            assessment,
            reservation: None,
        };
    }
    let (Some(dir), Some(code)) = (dir, primary_code) else {
        return Admission {
            assessment: assess(Verdict::Observe, "policy.evidence_unavailable"),
            reservation: None,
        };
    };
    let transaction = match crate::state::StateTransaction::begin(dir) {
        Ok(transaction) => transaction,
        Err(_) => {
            return Admission {
                assessment: assess(Verdict::Observe, "policy.evidence_unavailable"),
                reservation: None,
            };
        }
    };
    if transaction.prepare_jsonl("policy.jsonl", now_ms).is_err() {
        return Admission {
            assessment: assess(Verdict::Observe, "policy.evidence_unavailable"),
            reservation: None,
        };
    }
    let history = match try_load_history_from(dir) {
        Ok(history) => history,
        Err(_) => {
            return Admission {
                assessment: assess(Verdict::Observe, "policy.evidence_unavailable"),
                reservation: None,
            };
        }
    };
    let gated = apply_gates(
        assessment,
        Some(code),
        false,
        &history,
        now_ms,
        budget_per_hour,
    );
    if !matches!(gated.verdict, Verdict::Warn | Verdict::Confirm) {
        return Admission {
            assessment: gated,
            reservation: None,
        };
    }

    let id = format!(
        "{}-{now_ms}-{}",
        std::process::id(),
        RESERVATION_ID.fetch_add(1, Ordering::Relaxed)
    );
    let Some(line) = reservation_line(code, &id, ReservationState::Reserved, now_ms) else {
        return Admission {
            assessment: assess(Verdict::Observe, "policy.evidence_unavailable"),
            reservation: None,
        };
    };
    if transaction
        .append_jsonl("policy.jsonl", line.as_bytes())
        .is_err()
    {
        return Admission {
            assessment: assess(Verdict::Observe, "policy.evidence_unavailable"),
            reservation: None,
        };
    }
    Admission {
        assessment: gated,
        reservation: Some(ReservationToken {
            dir: dir.to_path_buf(),
            id,
            rule: code.to_string(),
        }),
    }
}

/// Release a slot when terminal setup failed before a warning was visible.
/// A failed bounded append is safe: the reservation expires on its own.
pub(crate) fn release_admission(reservation: Option<ReservationToken>, now_ms: u64) {
    let Some(reservation) = reservation else {
        return;
    };
    let Some(line) = reservation_line(
        &reservation.rule,
        &reservation.id,
        ReservationState::Released,
        now_ms,
    ) else {
        return;
    };
    let _ =
        crate::state::append_jsonl_after_prompt(&reservation.dir, "policy.jsonl", line.as_bytes());
}

/// Complete a reserved admission with the outcome of the prompt. Before this
/// append the reservation spends the slot; after it, the shown intervention
/// spends the same slot, so no observer can see a gap between them.
pub(crate) fn record_admitted_outcome(
    reservation: Option<ReservationToken>,
    code: &str,
    ran_unchanged: bool,
    timed_out: bool,
    now_ms: u64,
) {
    let (dir, rule, reservation_id) = match reservation {
        Some(reservation) => (
            Some(reservation.dir),
            reservation.rule,
            Some(reservation.id),
        ),
        None => (crate::state::state_dir(), code.to_string(), None),
    };
    let (Some(dir), Some(line)) = (
        dir,
        outcome_line(
            &rule,
            ran_unchanged,
            timed_out,
            reservation_id.as_deref(),
            now_ms,
        ),
    ) else {
        return;
    };
    let _ = crate::state::append_jsonl_after_prompt(&dir, "policy.jsonl", line.as_bytes());
}

#[cfg(test)]
pub(crate) fn append_outcome_to(
    dir: &std::path::Path,
    code: &str,
    ran_unchanged: bool,
    now_ms: u64,
) {
    let Some(line) = outcome_line(code, ran_unchanged, false, None, now_ms) else {
        return;
    };
    let _ = crate::state::append_jsonl(dir, "policy.jsonl", line.as_bytes(), now_ms);
}

fn outcome_line(
    code: &str,
    ran_unchanged: bool,
    timed_out: bool,
    reservation_id: Option<&str>,
    now_ms: u64,
) -> Option<String> {
    let mut line = serde_json::to_string(&Intervention {
        ts_ms: now_ms,
        rule: code.to_string(),
        ran_unchanged,
        timed_out,
        reservation_id: reservation_id.map(str::to_string),
    })
    .ok()?;
    line.push('\n');
    Some(line)
}

fn reservation_line(
    code: &str,
    reservation_id: &str,
    reservation_state: ReservationState,
    now_ms: u64,
) -> Option<String> {
    let mut line = serde_json::to_string(&ReservationRecord {
        ts_ms: now_ms,
        rule: code.to_string(),
        reservation_id: reservation_id.to_string(),
        reservation_state,
    })
    .ok()?;
    line.push('\n');
    Some(line)
}

// ---- config (SPEC §15) ----------------------------------------------------

pub struct Config {
    pub mode: Mode,
    pub det_timeout_ms: u64,
    pub budget_per_hour: u32,
    /// Empty/absent = deterministic-only. Consumed by the model layer (M4).
    pub model: Option<String>,
    pub model_timeout_ms: u64,
    /// Opt-in research capture (M5). Parsed and validated now.
    #[allow(dead_code)]
    pub log_raw: bool,
    /// Validation complaints: unknown keys (by line number — raw key text is
    /// untrusted and never echoed), invalid values. Emitted once per config
    /// change via `emit_config_warnings_once`.
    pub warnings: Vec<String>,
}

/// Deterministic-path deadline default (SPEC §10/§15) — the single source
/// for the 150 ms figure: the config default here, the watchdog fallback in
/// main.rs.
pub(crate) const DET_TIMEOUT_DEFAULT_MS: u64 = 150;

impl Default for Config {
    fn default() -> Self {
        Config {
            mode: Mode::Shadow,
            det_timeout_ms: DET_TIMEOUT_DEFAULT_MS,
            budget_per_hour: 3,
            model: None,
            model_timeout_ms: 2_000,
            log_raw: false,
            warnings: Vec::new(),
        }
    }
}

/// Bytes read from the config file — it is a handful of `key = value` lines.
const CONFIG_READ_CAP: u64 = 64 * 1024;

/// $XDG_CONFIG_HOME/oopsinput/config else ~/.config/oopsinput/config.
pub fn config_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("oopsinput/config"));
    }
    let home = std::env::var("HOME").ok()?;
    (!home.is_empty()).then(|| PathBuf::from(home).join(".config/oopsinput/config"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigFileState {
    Regular,
    Missing,
    NonRegular,
    TooLarge,
    Unavailable,
}

enum ConfigReadError {
    TooLarge,
    Unavailable,
}

pub(crate) struct ConfigInspection {
    pub path: Option<PathBuf>,
    pub file_state: ConfigFileState,
    pub config: Config,
    pub mode_override_valid: bool,
}

/// Classify the config leaf exactly as the loader does. `doctor` consumes the
/// same answer so it cannot call a symlink "present" while `load_config`
/// correctly ignores it.
pub(crate) fn config_file_state(path: &std::path::Path) -> ConfigFileState {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => ConfigFileState::Regular,
        Ok(_) => ConfigFileState::NonRegular,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ConfigFileState::Missing,
        Err(_) => ConfigFileState::Unavailable,
    }
}

/// Load and validate the full SPEC §15 surface. $OOPSINPUT_MODE overrides
/// the file's mode (and, being an explicit act, never warns).
pub fn load_config() -> Config {
    inspect_config().config
}

/// The same fail-open config load as `load_config`, plus the exact reason the
/// file did or did not participate. Runtime analysis needs only the effective
/// config; `doctor` also needs to distinguish a valid absent file from a
/// regular file that could not be opened, verified, read, or decoded.
pub(crate) fn inspect_config() -> ConfigInspection {
    let path = config_path();
    let (mut cfg, file_state) = match path.as_deref() {
        Some(path) => match config_file_state(path) {
            ConfigFileState::Regular => match read_config_file(path) {
                Ok(text) => (parse_config(&text), ConfigFileState::Regular),
                Err(ConfigReadError::TooLarge) => (Config::default(), ConfigFileState::TooLarge),
                Err(ConfigReadError::Unavailable) => {
                    (Config::default(), ConfigFileState::Unavailable)
                }
            },
            state => (Config::default(), state),
        },
        None => (Config::default(), ConfigFileState::Unavailable),
    };
    let mode_override_valid = match std::env::var("OOPSINPUT_MODE") {
        Ok(v) => match parse_mode(&v) {
            Some(mode) => {
                cfg.mode = mode;
                true
            }
            None => {
                cfg.mode = Mode::Shadow;
                false
            }
        },
        Err(std::env::VarError::NotPresent) => true,
        Err(std::env::VarError::NotUnicode(_)) => false,
    };
    ConfigInspection {
        path,
        file_state,
        config: cfg,
        mode_override_valid,
    }
}

fn read_config_file(path: &std::path::Path) -> Result<String, ConfigReadError> {
    let mut buf = Vec::new();
    let file = std::fs::File::open(path).map_err(|_| ConfigReadError::Unavailable)?;
    crate::state::opened_regular_file_metadata(path, &file, "config file")
        .map_err(|_| ConfigReadError::Unavailable)?;
    file.take(CONFIG_READ_CAP + 1)
        .read_to_end(&mut buf)
        .map_err(|_| ConfigReadError::Unavailable)?;
    if buf.len() as u64 > CONFIG_READ_CAP {
        return Err(ConfigReadError::TooLarge);
    }
    String::from_utf8(buf).map_err(|_| ConfigReadError::Unavailable)
}

/// `key = value`, `#` comments, first occurrence of a key wins, unknown keys
/// warned by line number, invalid values fall back to the default with a
/// warning (SPEC §15).
fn parse_config(text: &str) -> Config {
    let mut cfg = Config::default();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.split('#').next().unwrap_or("");
        if line.trim().is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            cfg.warnings
                .push(format!("line {line_no}: not `key = value`, ignored"));
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if !KNOWN_KEYS.contains(&k) {
            // Unknown key: named by line number only — the key text is
            // untrusted input and never reaches a terminal (SPEC §9-5).
            cfg.warnings
                .push(format!("line {line_no}: unknown key, ignored"));
            continue;
        }
        if !seen.insert(k) {
            continue; // first occurrence won
        }
        match k {
            "mode" => match parse_mode(v) {
                Some(m) => cfg.mode = m,
                None => cfg
                    .warnings
                    .push(format!("line {line_no}: invalid mode, using shadow")),
            },
            "model" => cfg.model = (!v.is_empty()).then(|| v.to_string()),
            "model_timeout_ms" => parse_num(v, 100..=60_000, &mut cfg.model_timeout_ms, || {
                cfg.warnings.push(format!(
                    "line {line_no}: invalid model_timeout_ms, using 2000"
                ))
            }),
            "det_timeout_ms" => parse_num(v, 50..=5_000, &mut cfg.det_timeout_ms, || {
                cfg.warnings
                    .push(format!("line {line_no}: invalid det_timeout_ms, using 150"))
            }),
            "budget_per_hour" => {
                let mut val = cfg.budget_per_hour as u64;
                parse_num(v, 0..=1_000, &mut val, || {
                    cfg.warnings
                        .push(format!("line {line_no}: invalid budget_per_hour, using 3"))
                });
                cfg.budget_per_hour = val as u32;
            }
            "log_raw" => match v {
                "true" => cfg.log_raw = true,
                "false" => cfg.log_raw = false,
                _ => cfg
                    .warnings
                    .push(format!("line {line_no}: invalid log_raw, using false")),
            },
            _ => {} // KNOWN_KEYS and these arms list the same keys
        }
    }
    cfg
}

/// The SPEC §15 key set — kept beside `parse_config`, whose dispatch arms
/// must mirror it.
const KNOWN_KEYS: [&str; 6] = [
    "mode",
    "model",
    "model_timeout_ms",
    "det_timeout_ms",
    "budget_per_hour",
    "log_raw",
];

fn parse_num(
    v: &str,
    range: std::ops::RangeInclusive<u64>,
    out: &mut u64,
    on_invalid: impl FnOnce(),
) {
    match v.parse::<u64>() {
        Ok(n) if range.contains(&n) => *out = n,
        _ => on_invalid(),
    }
}

/// Emit config complaints to stderr at most once per distinct set of
/// complaints (SPEC §15 "say so once"): a marker in the state dir holds the
/// fingerprint of what was last shown; editing the config re-arms it.
pub fn emit_config_warnings_once(cfg: &Config) {
    if cfg.warnings.is_empty() {
        return;
    }
    let Some(dir) = crate::state::state_dir() else {
        return;
    };
    let fp = warnings_fingerprint(&cfg.warnings);
    let update = match crate::state::begin_small_file_update(&dir, "config_warned", fp.as_bytes()) {
        Ok(Some(update)) => Some(update),
        Ok(None) => return,
        // State is evidence, not permission to communicate a config problem.
        // If coordination is unavailable, show the warning and try again on
        // a later command rather than silently suppressing it.
        Err(_) => None,
    };
    if display_config_warnings(&cfg.warnings).is_err() {
        // A marker means "shown", not merely "attempted". If no diagnostic
        // channel is available, leave it absent and retry on a later command.
        return;
    }
    if let Some(update) = update {
        let _ = update.commit();
    }
}

/// The Zsh adapter discards the binary's ordinary streams, so it opts into a
/// direct /dev/tty diagnostic only when a warning actually exists. Other
/// callers keep conventional stderr behavior, which is also testable without
/// a controlling terminal. Warning text contains trusted framing plus line
/// numbers only; raw config keys and values never enter this string.
fn display_config_warnings(warnings: &[String]) -> std::io::Result<()> {
    let rendered = warnings
        .iter()
        .map(|warning| format!("oopsinput: config: {warning}\n"))
        .collect::<String>();
    if std::env::var_os("OOPSINPUT_DIAGNOSTICS_TTY").is_some() {
        #[cfg(debug_assertions)]
        if std::env::var_os("OOPSINPUT_TEST_NO_TTY").is_some() {
            return Err(std::io::Error::other("diagnostic terminal unavailable"));
        }
        let mut tty = std::fs::OpenOptions::new().write(true).open("/dev/tty")?;
        tty.write_all(rendered.as_bytes())
    } else {
        std::io::stderr().lock().write_all(rendered.as_bytes())
    }
}

fn warnings_fingerprint(warnings: &[String]) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    warnings.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::context::{GitFacts, TargetFact};
    use crate::layers::danger;
    use crate::lexer::lex;

    fn danger_for(buffer: &str) -> danger::Analysis {
        danger::analyze_with_home(&lex(buffer), Some("/home/u"))
    }

    fn git(dirty: Option<u32>, untracked: Option<bool>, main_like: bool) -> GitFacts {
        GitFacts {
            detached: false,
            branch_main_like: main_like,
            dirty,
            untracked,
        }
    }

    fn target(exists: bool, is_cwd: bool, is_parent: bool, near_miss: bool) -> TargetFact {
        TargetFact {
            exists,
            is_dir: exists,
            is_symlink: false,
            entries: None,
            is_cwd,
            is_parent,
            near_miss,
        }
    }

    fn ctx(git: Option<GitFacts>, targets: Vec<TargetFact>) -> Context {
        Context { git, targets }
    }

    // Golden-corpus fixture shapes (eval/golden/policy.json), shared by the
    // corpus test and the M4 model-comparison harness.
    #[derive(serde::Deserialize)]
    struct GitFix {
        #[serde(default)]
        detached: bool,
        #[serde(default)]
        main_like: bool,
        dirty: Option<u32>,
        untracked: Option<bool>,
    }
    #[derive(serde::Deserialize)]
    struct TargetFix {
        exists: bool,
        #[serde(default)]
        is_cwd: bool,
        #[serde(default)]
        is_parent: bool,
        #[serde(default)]
        near_miss: bool,
    }
    #[derive(serde::Deserialize)]
    struct Case {
        name: String,
        #[serde(default)]
        pair: Option<String>,
        buffer: String,
        git: Option<GitFix>,
        #[serde(default)]
        targets: Vec<TargetFix>,
        expect_verdict: String,
        expect_reason: String,
    }

    impl Case {
        fn context(&self) -> Context {
            ctx(
                self.git.as_ref().map(|g| GitFacts {
                    detached: g.detached,
                    branch_main_like: g.main_like,
                    dirty: g.dirty,
                    untracked: g.untracked,
                }),
                self.targets
                    .iter()
                    .map(|t| target(t.exists, t.is_cwd, t.is_parent, t.near_miss))
                    .collect(),
            )
        }
    }

    /// SPEC §11 golden corpus, policy slice: buffer + context fixture →
    /// expected verdict and reason. These are the context-flip counterfactual
    /// pairs the danger corpus could not express: same command, different
    /// context, different decision.
    #[test]
    fn golden_policy_corpus() {
        let cases: Vec<Case> = crate::golden_cases("policy.json", |c: &Case| c.pair.is_some());

        for c in &cases {
            let d = danger_for(&c.buffer);
            let context = c.context();
            let got = warranted(&d, Some(&context));
            assert_eq!(
                got.verdict.as_str(),
                c.expect_verdict,
                "case '{}': wrong verdict (reason {})",
                c.name,
                got.reason
            );
            assert_eq!(
                got.reason, c.expect_reason,
                "case '{}': wrong reason",
                c.name
            );
        }
    }

    #[test]
    fn another_rules_target_never_vouches_for_rm() {
        // Regression (bughunt 2026-08-06): `echo hi > exists.txt && rm -rf
        // $DIR` — the redirect's existing target satisfied the all-exist
        // check and the unknowable rm was labeled context-clear.
        let d = danger_for("echo hi > exists.txt && rm -rf $DIR");
        let context = ctx(None, vec![target(true, false, false, false)]);
        let got = warranted(&d, Some(&context));
        assert_eq!(got.verdict, Verdict::Observe);
        assert_eq!(got.reason, "policy.evidence_unavailable");
        // ...while a fully-literal rm with existing targets stays clear
        let d = danger_for("echo hi > exists.txt && rm -rf ./build");
        let context = ctx(
            None,
            vec![
                target(true, false, false, false),
                target(true, false, false, false),
            ],
        );
        assert_eq!(warranted(&d, Some(&context)).reason, "policy.context_clear");
    }

    #[test]
    fn no_evidence_keeps_the_legacy_shadow_strings() {
        let d = danger_for("ls -la");
        let got = warranted(&d, None);
        assert_eq!(got.verdict, Verdict::Allow);
        assert_eq!(got.reason, "shadow.observed");
    }

    #[test]
    fn catastrophic_confirms_regardless_of_context() {
        let d = danger_for("rm -rf /");
        // even a "clean" context cannot soften it
        let clean = ctx(Some(git(Some(0), Some(false), false)), vec![]);
        let got = warranted(&d, Some(&clean));
        assert_eq!(got.verdict, Verdict::Confirm);
        assert_eq!(got.reason, "policy.direct_catastrophic");
    }

    #[test]
    fn mode_is_a_ceiling_and_preserves_reasons() {
        let warn = assess(Verdict::Warn, "policy.dirty_work_at_risk");
        let confirm = assess(Verdict::Confirm, "policy.direct_catastrophic");
        // shadow/suggest: nothing visible, reason survives for the report
        for m in [Mode::Shadow, Mode::Suggest] {
            let capped = cap_for_mode(warn, m);
            assert_eq!(capped.verdict, Verdict::Observe);
            assert_eq!(capped.reason, "policy.dirty_work_at_risk");
            assert_eq!(cap_for_mode(confirm, m).verdict, Verdict::Observe);
        }
        // warn mode: pausing confirms degrade to nonblocking warns
        assert_eq!(cap_for_mode(confirm, Mode::Warn).verdict, Verdict::Warn);
        assert_eq!(cap_for_mode(warn, Mode::Warn).verdict, Verdict::Warn);
        // confirm mode: full strength
        assert_eq!(
            cap_for_mode(confirm, Mode::Confirm).verdict,
            Verdict::Confirm
        );
        // allow/observe are never upgraded by any mode
        let allow = assess(Verdict::Allow, "policy.context_clear");
        assert_eq!(cap_for_mode(allow, Mode::Confirm), allow);
    }

    /// Build a history directly, as if these interventions had been shown.
    fn history_of(entries: &[(u64, &str, bool)]) -> History {
        History {
            shown: entries
                .iter()
                .map(|(ts, rule, ran)| Intervention {
                    ts_ms: *ts,
                    rule: rule.to_string(),
                    ran_unchanged: *ran,
                    timed_out: false,
                    reservation_id: None,
                })
                .collect(),
            reservations: Vec::new(),
        }
    }

    #[test]
    fn budget_exhaustion_degrades_to_observe() {
        let warn = assess(Verdict::Warn, "policy.dirty_work_at_risk");
        let now = 10 * HOUR_MS;
        // Two shown this hour, budget 3: the next one still passes.
        let h = history_of(&[
            (now - 60_000, "git.reset_hard", false),
            (now - 30_000, "x", false),
        ]);
        assert_eq!(
            apply_gates(warn, Some("git.reset_hard"), false, &h, now, 3).verdict,
            Verdict::Warn
        );
        // Three shown this hour: the next degrades rather than nags.
        let h = history_of(&[
            (now - 60_000, "git.reset_hard", false),
            (now - 30_000, "x", false),
            (now - 10_000, "y", false),
        ]);
        let got = apply_gates(warn, Some("git.reset_hard"), false, &h, now, 3);
        assert_eq!(got.verdict, Verdict::Observe);
        assert_eq!(got.reason, "policy.budget_exhausted");
        // ...and those same three no longer count an hour later.
        let later = now + HOUR_MS;
        assert_eq!(
            apply_gates(warn, Some("git.reset_hard"), false, &h, later, 3).verdict,
            Verdict::Warn
        );
    }

    #[test]
    fn catastrophic_is_exempt_from_budget_and_cooldown() {
        let confirm = assess(Verdict::Confirm, "policy.direct_catastrophic");
        let now = 100 * HOUR_MS;
        // Budget blown AND the rule asleep.
        let h = history_of(&[
            (now - 1000, "other", false),
            (now - 900, "other", false),
            (now - 800, "other", false),
            (now - 700, "fs.rm_recursive", true),
            (now - 600, "fs.rm_recursive", true),
            (now - 500, "fs.rm_recursive", true),
        ]);
        assert_eq!(
            apply_gates(confirm, Some("fs.rm_recursive"), true, &h, now, 3).verdict,
            Verdict::Confirm,
            "a catastrophic finding is never gated"
        );
    }

    #[test]
    fn repeated_run_unchanged_puts_a_rule_to_sleep_and_any_other_outcome_wakes_it() {
        let warn = assess(Verdict::Warn, "policy.target_context");
        let now = 100 * HOUR_MS;
        let three_runs = [
            (now - 3000, "fs.rm_recursive", true),
            (now - 2000, "fs.rm_recursive", true),
            (now - 1000, "fs.rm_recursive", true),
        ];

        let h = history_of(&three_runs);
        let got = apply_gates(warn, Some("fs.rm_recursive"), false, &h, now, 99);
        assert_eq!(got.verdict, Verdict::Observe);
        assert_eq!(got.reason, "policy.rule_cooldown");

        // A different rule is unaffected.
        assert_eq!(
            apply_gates(warn, Some("git.reset_hard"), false, &h, now, 99).verdict,
            Verdict::Warn
        );

        // One edit/cancel anywhere in the recent run breaks the streak.
        let mut woken = three_runs.to_vec();
        woken.push((now - 500, "fs.rm_recursive", false));
        assert_eq!(
            apply_gates(
                warn,
                Some("fs.rm_recursive"),
                false,
                &history_of(&woken),
                now,
                99
            )
            .verdict,
            Verdict::Warn,
            "an edit or cancel must wake the rule back up"
        );

        // Two run-unchanged is not yet three.
        assert_eq!(
            apply_gates(
                warn,
                Some("fs.rm_recursive"),
                false,
                &history_of(&three_runs[1..]),
                now,
                99
            )
            .verdict,
            Verdict::Warn
        );

        // And the sleep expires on its own.
        let after = now + COOLDOWN_MS + 1;
        assert_eq!(
            apply_gates(warn, Some("fs.rm_recursive"), false, &h, after, 99).verdict,
            Verdict::Warn
        );
    }

    #[test]
    fn timeouts_spend_budget_but_never_manufacture_a_cooldown() {
        // Reproduced by tracing the real prompt path on 2026-08-08: advisory
        // timeouts were stored as deliberate run-unchanged outcomes, so three
        // unattended prompts silenced the rule for 24 hours.
        let warn = assess(Verdict::Warn, "policy.target_context");
        let now = 100 * HOUR_MS;
        let h = History {
            shown: (1..=3)
                .map(|n| Intervention {
                    ts_ms: now - n * 1_000,
                    rule: "fs.rm_recursive".to_string(),
                    ran_unchanged: true,
                    timed_out: true,
                    reservation_id: None,
                })
                .collect(),
            reservations: Vec::new(),
        };
        assert_eq!(
            apply_gates(warn, Some("fs.rm_recursive"), false, &h, now, 99).verdict,
            Verdict::Warn,
            "a timeout is not the user saying 'I mean it'"
        );
        assert_eq!(
            apply_gates(warn, Some("other"), false, &h, now, 3).reason,
            "policy.budget_exhausted",
            "the visibly displayed prompts still spend the hourly budget"
        );
    }

    #[test]
    fn checking_the_gates_never_records_anything() {
        // Structural replacement for the old `commit: false` flag: gating is
        // a pure read, and only a prompt the user actually saw is recorded.
        let dir = std::env::temp_dir().join(format!("oopsinput-pure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let warn = assess(Verdict::Warn, "policy.dirty_work_at_risk");
        for _ in 0..10 {
            let h = load_history_from(&dir);
            assert_eq!(
                apply_gates(warn, Some("git.reset_hard"), false, &h, HOUR_MS, 3).verdict,
                Verdict::Warn
            );
        }
        assert!(
            !dir.join("policy.jsonl").exists(),
            "gate checks must not write anything"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_admission_never_exceeds_the_warning_budget() {
        // Reproduced in the pre-fix flow on 2026-08-08: eight callers could
        // all load the same two-record history, pass a budget of three, and
        // show eight prompts before any outcome append happened. Admission
        // now reserves the slot while the shared state lock is still held.
        let dir =
            std::env::temp_dir().join(format!("oopsinput-admission-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let warn = assess(Verdict::Warn, "policy.dirty_work_at_risk");
        let now = 20 * HOUR_MS;

        let admissions = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..8 {
                let dir = dir.clone();
                let barrier = barrier.clone();
                handles.push(scope.spawn(move || {
                    barrier.wait();
                    admit_intervention_in(Some(&dir), warn, Some("git.reset_hard"), false, now, 3)
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        let mut granted = Vec::new();
        for admission in admissions {
            if admission.assessment.verdict == Verdict::Warn {
                granted.push(
                    admission
                        .reservation
                        .expect("every visible admission must reserve its slot"),
                );
            } else {
                assert_eq!(admission.assessment.verdict, Verdict::Observe);
                assert_eq!(admission.assessment.reason, "policy.budget_exhausted");
                assert!(admission.reservation.is_none());
            }
        }
        assert_eq!(granted.len(), 3, "simultaneous callers exceeded the cap");

        for reservation in granted {
            record_admitted_outcome(Some(reservation), "git.reset_hard", false, false, now + 1);
        }
        let after =
            admit_intervention_in(Some(&dir), warn, Some("git.reset_hard"), false, now + 2, 3);
        assert_eq!(after.assessment.reason, "policy.budget_exhausted");
        assert!(after.reservation.is_none());

        let loaded = load_history_from(&dir);
        assert_eq!(loaded.shown.len(), 3);
        assert!(loaded.reservations.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unseen_and_abandoned_admissions_do_not_spend_budget_forever() {
        let dir = std::env::temp_dir().join(format!(
            "oopsinput-admission-release-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let warn = assess(Verdict::Warn, "policy.target_context");
        let now = 30 * HOUR_MS;

        let unseen =
            admit_intervention_in(Some(&dir), warn, Some("fs.rm_recursive"), false, now, 1);
        assert_eq!(unseen.assessment.verdict, Verdict::Warn);
        release_admission(unseen.reservation, now + 1);
        let abandoned =
            admit_intervention_in(Some(&dir), warn, Some("fs.rm_recursive"), false, now + 2, 1);
        assert_eq!(
            abandoned.assessment.verdict,
            Verdict::Warn,
            "a terminal setup failure must release its unused slot"
        );

        let blocked =
            admit_intervention_in(Some(&dir), warn, Some("fs.rm_recursive"), false, now + 3, 1);
        assert_eq!(blocked.assessment.reason, "policy.budget_exhausted");
        assert!(blocked.reservation.is_none());

        // Simulate the admitted process dying before it can release or
        // complete. Its token is intentionally dropped.
        drop(abandoned.reservation);
        let after_expiry = admit_intervention_in(
            Some(&dir),
            warn,
            Some("fs.rm_recursive"),
            false,
            now + 2 + RESERVATION_TTL_MS + 1,
            1,
        );
        assert_eq!(
            after_expiry.assessment.verdict,
            Verdict::Warn,
            "an abandoned reservation became a permanent denial"
        );
        release_admission(after_expiry.reservation, now + RESERVATION_TTL_MS + 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_shells_never_lose_a_recorded_intervention() {
        // Regression (test-audit 2026-08-06, proven): the old state was a
        // JSON blob loaded, modified and written back, so two shells that
        // finished warnings in the same instant each recorded a spend and the
        // second write dropped the first — the hourly budget under-counted and
        // a cooldown could vanish. Appends must interleave, never overwrite.
        let dir = std::env::temp_dir().join(format!("oopsinput-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::thread::scope(|s| {
            for t in 0..8 {
                let dir = dir.clone();
                s.spawn(move || {
                    for i in 0..25 {
                        // Every writer loads first, exactly as the real flow
                        // does — the old design lost writes precisely here.
                        let _ = load_history_from(&dir);
                        append_outcome_to(&dir, "git.reset_hard", false, 1_000 + t * 100 + i);
                    }
                });
            }
        });

        let loaded = load_history_from(&dir);
        assert_eq!(
            loaded.shown.len(),
            8 * 25,
            "interventions were lost to concurrent writers"
        );
    }

    #[test]
    fn history_survives_a_partial_line_and_keeps_the_newest_tail() {
        let dir = std::env::temp_dir().join(format!("oopsinput-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..5u64 {
            append_outcome_to(&dir, "git.reset_hard", false, 1_000 + i);
        }
        // A torn write leaves a partial line; it must be skipped, not fatal.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("policy.jsonl"))
            .unwrap();
        f.write_all(b"{\"ts_ms\":99,\"rule\":\"tru").unwrap();
        drop(f);
        assert_eq!(
            load_history_from(&dir).shown.len(),
            5,
            "a partial trailing line must be skipped, not lose the file"
        );

        // A file larger than the tail window still loads, bounded.
        for i in 0..4_000u64 {
            append_outcome_to(&dir, "git.reset_hard", false, 10_000 + i);
        }
        let loaded = load_history_from(&dir);
        assert!(!loaded.shown.is_empty(), "tail read produced nothing");
        assert!(
            loaded.shown.len() <= MAX_HISTORY_RECORDS,
            "tail read is unbounded: {}",
            loaded.shown.len()
        );
        assert!(
            loaded.shown.iter().all(|entry| entry.ts_ms >= 10_000),
            "records older than the bounded tail survived"
        );
        assert_eq!(
            loaded.shown.last().map(|entry| entry.ts_ms),
            Some(13_999),
            "the newest intervention was lost"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlinked_history_path_is_refused_not_written_through() {
        // Regression (audit 2026-08-06): our state writes must never land on
        // a file someone else pointed our path at.
        let dir = std::env::temp_dir().join(format!("oopsinput-pollink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "PRECIOUS\n").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("policy.jsonl")).unwrap();

        append_outcome_to(&dir, "git.reset_hard", true, 1);
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "PRECIOUS\n",
            "an intervention was appended through a symlink"
        );

        // The config-warning marker takes the same locked, atomic state path.
        let link = dir.join("marker");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(
            crate::state::replace_small_file(&dir, "marker", b"x").is_err(),
            "write not refused"
        );
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "PRECIOUS\n");

        // A normal path still writes, user-only.
        let plain = dir.join("plain");
        assert!(crate::state::replace_small_file(&dir, "plain", b"{}").is_ok());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&plain).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_parses_the_full_spec_15_surface() {
        let cfg = parse_config(
            "# comment\n\
             mode = warn   # trailing\n\
             model = qwen3.5:4b\n\
             model_timeout_ms = 1500\n\
             det_timeout_ms = 200\n\
             budget_per_hour = 5\n\
             log_raw = false\n",
        );
        assert_eq!(cfg.mode, Mode::Warn);
        assert_eq!(cfg.model.as_deref(), Some("qwen3.5:4b"));
        assert_eq!(cfg.model_timeout_ms, 1_500);
        assert_eq!(cfg.det_timeout_ms, 200);
        assert_eq!(cfg.budget_per_hour, 5);
        assert!(!cfg.log_raw);
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
    }

    #[test]
    fn config_reader_refuses_a_symlink_and_accepts_the_same_regular_file() {
        // M5 audit hardening (2026-08-08): config is read before the main
        // analysis pipeline, so reject a non-regular path before open and
        // verify the opened inode before consuming any bytes.
        let dir =
            std::env::temp_dir().join(format!("oopsinput-config-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let regular = dir.join("regular");
        let link = dir.join("config");
        std::fs::write(&regular, "mode = confirm\n").unwrap();
        std::os::unix::fs::symlink(&regular, &link).unwrap();

        assert_eq!(config_file_state(&link), ConfigFileState::NonRegular);
        assert_eq!(config_file_state(&regular), ConfigFileState::Regular);
        assert!(matches!(
            read_config_file(&link),
            Err(ConfigReadError::Unavailable)
        ));
        assert_eq!(
            read_config_file(&regular).ok().as_deref(),
            Some("mode = confirm\n")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_values_fall_back_and_warn_unknown_keys_warn_by_line_only() {
        let cfg = parse_config(
            "mode = aggressive\n\
             det_timeout_ms = never\n\
             budget_per_hour = -2\n\
             log_raw = yes\n\
             $(injection attempt) = x\n\
             garbage line\n",
        );
        assert_eq!(cfg.mode, Mode::Shadow);
        assert_eq!(cfg.det_timeout_ms, 150);
        assert_eq!(cfg.budget_per_hour, 3);
        assert!(!cfg.log_raw);
        assert_eq!(cfg.warnings.len(), 6);
        // untrusted key text never appears in a warning (SPEC §9-5)
        assert!(
            cfg.warnings.iter().all(|w| !w.contains("injection")),
            "{:?}",
            cfg.warnings
        );
    }

    #[test]
    fn first_key_occurrence_wins_and_out_of_range_rejected() {
        let cfg = parse_config("mode = warn\nmode = confirm\n");
        assert_eq!(cfg.mode, Mode::Warn);
        let cfg = parse_config("det_timeout_ms = 10\n"); // below floor
        assert_eq!(cfg.det_timeout_ms, 150);
        assert_eq!(cfg.warnings.len(), 1);
    }

    #[test]
    fn empty_mode_value_and_empty_model_are_defaults() {
        let cfg = parse_config("model = \n");
        assert!(cfg.model.is_none());
        assert!(cfg.warnings.is_empty());
    }

    #[test]
    fn warnings_fingerprint_is_stable_and_distinguishes() {
        let a1 = warnings_fingerprint(&["x".into(), "y".into()]);
        let a2 = warnings_fingerprint(&["x".into(), "y".into()]);
        let b = warnings_fingerprint(&["x".into()]);
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
    }

    /// M4 item 5 evaluation harness — not a CI test (needs live Ollama on
    /// 127.0.0.1:11434). Run by hand:
    ///
    ///   cargo test model_paired_comparison -- --ignored --nocapture
    ///
    /// Model name from OOPSINPUT_EVAL_MODEL (default qwen3:1.7b), 60 s per
    /// case. Replays every gate-eligible golden case (the ambiguous observe
    /// cases — the only ones a live run would consult) against the real
    /// model and reports what apply_model_evidence would have done. Results
    /// and the SPEC §11 default-config decision are recorded in
    /// eval/model-comparison-2026-08-06.md.
    #[test]
    #[ignore = "live-model evaluation harness, run by hand (see eval/)"]
    fn model_paired_comparison() {
        use crate::proposal::{Proposal, ResolutionKind};

        let model =
            std::env::var("OOPSINPUT_EVAL_MODEL").unwrap_or_else(|_| "qwen3:1.7b".to_string());
        let cases: Vec<Case> = crate::golden_cases("policy.json", |c: &Case| c.pair.is_some());
        println!("== paired-corpus comparison, model = {model} ==");
        let mut eligible = 0u32;
        let mut changed = 0u32;
        for c in &cases {
            let d = danger_for(&c.buffer);
            let context = c.context();
            let w = warranted(&d, Some(&context));
            assert_eq!(w.verdict.as_str(), c.expect_verdict, "case {}", c.name);
            if !l4_gate(&d, w) {
                continue;
            }
            eligible += 1;
            let p = Proposal {
                buffer: c.buffer.clone(),
                res_kind: ResolutionKind::Command,
                capped: false,
                names: vec![],
                names_capped: false,
                recency: vec![],
            };
            let got = crate::layers::infer::consult(&model, 60_000, &p, &d, Some(&context));
            let after = apply_model_evidence(w, Some(&got));
            let delta = if after == w {
                "unchanged".to_string()
            } else {
                changed += 1;
                format!("CHANGED -> {}/{}", after.verdict.as_str(), after.reason)
            };
            let code = primary_code(&d).unwrap_or("-");
            match &got {
                Consult::Evidence(e) => println!(
                    "  {:42} [{code}] expected {}, model {} ({:?}) — {delta}\n      reason: {}",
                    c.name,
                    c.expect_reason,
                    e.assessment.evidence_code(),
                    e.kind,
                    e.reason.replace(['\n', '\r'], " ")
                ),
                Consult::Unavailable(u) => {
                    println!("  {:42} [{code}] model UNAVAILABLE ({u})", c.name)
                }
            }
        }
        println!(
            "== {eligible} gate-eligible cases, {changed} verdicts would change \
             (every change on this corpus is a raised intervention) =="
        );
    }

    // ---- L4 gate + advisory evidence (M4) ----------------------------------

    use crate::layers::infer::{Consult, MismatchKind, ModelAssessment, ModelEvidence};

    fn model_says(assessment: ModelAssessment) -> Consult {
        Consult::Evidence(ModelEvidence {
            assessment,
            kind: MismatchKind::Target,
            reason: "because".into(),
        })
    }

    #[test]
    fn gate_opens_only_on_ambiguous_candidates() {
        // git reset --hard outside a repo: candidate, context unavailable.
        let d = danger_for("git reset --hard");
        assert!(l4_gate(&d, warranted(&d, None)));
        // Same command, context decided (dirty work): the facts speak — no model.
        let dirty = ctx(Some(git(Some(3), Some(false), false)), vec![]);
        assert!(!l4_gate(&d, warranted(&d, Some(&dirty))));
        // Same command, context affirmatively clean: cleared — no model.
        let clean = ctx(Some(git(Some(0), Some(false), false)), vec![]);
        assert!(!l4_gate(&d, warranted(&d, Some(&clean))));
        // Ungraduated candidate (shape recognized, no policy arm): ambiguous.
        let force = danger_for("mv -f a b");
        assert!(l4_gate(&force, warranted(&force, None)));
        // No danger candidate at all: the common path never consults.
        let benign = danger_for("ls -la");
        assert!(!l4_gate(&benign, warranted(&benign, None)));
    }

    #[test]
    fn gate_never_opens_for_direct_catastrophic() {
        // M4 acceptance seed: the model can never touch the catastrophic
        // path, because consultation never happens on it.
        let d = danger_for("rm -rf /");
        assert!(d.catastrophic);
        assert!(!l4_gate(&d, warranted(&d, None)));
    }

    #[test]
    fn model_probable_mismatch_upgrades_to_warn_never_confirm() {
        let base = assess(Verdict::Observe, "policy.evidence_unavailable");
        let up = apply_model_evidence(base, Some(&model_says(ModelAssessment::ProbableMismatch)));
        assert_eq!(up.verdict, Verdict::Warn);
        assert_eq!(up.reason, "policy.model_mismatch");
        let adv = apply_model_evidence(
            base,
            Some(&model_says(
                ModelAssessment::AdversarialOrUntrustedInstruction,
            )),
        );
        assert_eq!(adv.verdict, Verdict::Warn);
        assert_eq!(adv.reason, "policy.model_adversarial");
    }

    #[test]
    fn model_can_never_clear_or_soften_a_command() {
        // A lying model saying "no mismatch" must change nothing — there is
        // no downgrade arm, pinned here against every non-upgrading answer.
        let base = assess(Verdict::Observe, "policy.candidate_observed");
        for a in [
            ModelAssessment::NoMismatchEvidence,
            ModelAssessment::PossibleMismatch,
            ModelAssessment::InsufficientEvidence,
            ModelAssessment::Unsupported,
        ] {
            assert_eq!(apply_model_evidence(base, Some(&model_says(a))), base);
        }
        assert_eq!(
            apply_model_evidence(base, Some(&Consult::Unavailable("model.timeout"))),
            base
        );
        assert_eq!(apply_model_evidence(base, None), base);
    }
}
