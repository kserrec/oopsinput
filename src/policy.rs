//! Policy — turns the layers' evidence into a decision (SPEC §4 vocabulary),
//! applies the mode ceiling (§8), the intervention budget and per-rule
//! cooldown (§7 habituation control), and owns the full config surface (§15).
//!
//! The split matters for evaluation: `warranted` is the pure, mode-blind
//! matrix the golden corpus pins (same command, different context, different
//! answer); `cap_for_mode` and `apply_gates` then bound what actually becomes
//! visible. In shadow, a warranted warn is recorded as `observe` with its
//! policy reason preserved — that preserved reason IS the shadow conversion:
//! `oopsinput report` (M5) counts hypothetical interventions from it.

use std::io::{Read, Seek};
use std::path::PathBuf;

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
    let has = |code: &str| danger.codes.contains(&code);
    let git = ctx.and_then(|c| c.git.as_ref());
    let targets = ctx.map(|c| c.targets.as_slice()).unwrap_or(&[]);

    // Work-loss git commands: the question is whether there is work to lose.
    if has("git.reset_hard") || has("git.clean_force") {
        return match git {
            Some(g) => match (g.dirty, g.untracked) {
                (Some(d), u) if d > 0 || u == Some(true) => {
                    assess(Verdict::Warn, "policy.dirty_work_at_risk")
                }
                (Some(_), Some(_)) => assess(Verdict::Allow, "policy.context_clear"),
                // status unavailable: no claim, no nag — fail toward silence
                _ => assess(Verdict::Observe, "policy.evidence_unavailable"),
            },
            // not in a repo: the command will fail on its own
            None => assess(Verdict::Observe, "policy.evidence_unavailable"),
        };
    }
    if has("git.push_force") {
        return match git {
            Some(g) if g.branch_main_like => assess(Verdict::Warn, "policy.main_branch_force"),
            Some(_) => assess(Verdict::Allow, "policy.context_clear"),
            None => assess(Verdict::Observe, "policy.evidence_unavailable"),
        };
    }
    // Writing to a block device by name shape is warn-worthy on its own —
    // there is no benign-context read of it that L3 can establish.
    if has("fs.target_blockdev") {
        return assess(Verdict::Warn, "policy.blockdev_write");
    }
    if has("fs.rm_recursive") {
        let target_flagged = has("fs.target_cwd")
            || has("fs.target_parent")
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
        if has("fs.rm_target_unknown") {
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
/// Appended as a single line and never rewritten.
///
/// Append-only is the entire point (test-audit 2026-08-06, proven): the
/// previous design loaded a JSON blob, modified it, and wrote it back, so two
/// shells finishing warnings in the same instant each recorded a spend and
/// the second write dropped the first — the hourly cap silently under-counted
/// and a cooldown could vanish. The event log already solved this exact class
/// with one atomic append per line; this is the same fix, and it removes the
/// race by construction rather than by locking.
#[derive(Serialize, Deserialize)]
struct Intervention {
    ts_ms: u64,
    rule: String,
    ran_unchanged: bool,
}

/// Recent shown interventions, oldest first. Read once per candidate command.
#[derive(Default)]
pub struct History {
    shown: Vec<Intervention>,
}

const HOUR_MS: u64 = 3_600_000;
const COOLDOWN_MS: u64 = 24 * HOUR_MS;
/// Run-unchanged outcomes in a row before a rule goes quiet for a day.
const COOLDOWN_TRIGGER: usize = 3;

impl History {
    fn shown_within_hour(&self, now_ms: u64) -> usize {
        self.shown
            .iter()
            .filter(|i| now_ms.saturating_sub(i.ts_ms) < HOUR_MS)
            .count()
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
            && recent.iter().all(|i| i.ran_unchanged)
            && recent
                .first()
                .is_some_and(|newest| now_ms.saturating_sub(newest.ts_ms) < COOLDOWN_MS)
    }
}

/// Gate a visible intervention through the budget and the per-rule cooldown.
/// Direct-catastrophic is exempt from both (SPEC §7). Exhaustion degrades to
/// observe (shadow recording), never to nagging.
///
/// This is a pure decision over history — showing is what spends budget, and
/// only `record_outcome` (called after the prompt) writes. That ordering is
/// why an intervention nobody saw can no longer consume anything.
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
const HISTORY_TAIL_BYTES: u64 = 64 * 1024;
/// Records parsed from that tail. Far more than an hour or a day can hold.
const MAX_HISTORY_RECORDS: usize = 2_048;

pub fn load_history() -> History {
    match crate::events::state_dir() {
        Some(dir) => load_history_from(&dir),
        None => History::default(),
    }
}

/// The loader with the state directory explicit — the seam tests run through,
/// matching `events::append_to`. (A test that re-implements this logic in its
/// own body proves nothing about this function: test-audit 2026-08-06 deleted
/// the caps from the old loader and watched the old test pass anyway.)
pub(crate) fn load_history_from(dir: &std::path::Path) -> History {
    let path = dir.join("policy.jsonl");
    let Ok(mut f) = std::fs::File::open(&path) else {
        return History::default();
    };
    // Read only the tail; seek past everything older.
    if let Ok(meta) = f.metadata()
        && meta.len() > HISTORY_TAIL_BYTES
    {
        let _ = f.seek(std::io::SeekFrom::End(-(HISTORY_TAIL_BYTES as i64)));
    }
    let mut text = String::new();
    if f.read_to_string(&mut text).is_err() {
        return History::default();
    }
    // The first line of a tail read is usually cut mid-record; it simply
    // fails to parse, along with any line a partial write left behind.
    let mut shown: Vec<Intervention> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Intervention>(l).ok())
        .collect();
    if shown.len() > MAX_HISTORY_RECORDS {
        let cut = shown.len() - MAX_HISTORY_RECORDS;
        shown.drain(..cut);
    }
    History { shown }
}

/// Record what the user did at a warning we actually showed. One line, one
/// write syscall, `O_APPEND` — so concurrent shells interleave safely instead
/// of overwriting each other (test-audit 2026-08-06).
pub fn record_outcome(code: &str, ran_unchanged: bool, now_ms: u64) {
    let Some(dir) = crate::events::state_dir() else {
        return;
    };
    append_outcome_to(&dir, code, ran_unchanged, now_ms);
}

pub(crate) fn append_outcome_to(
    dir: &std::path::Path,
    code: &str,
    ran_unchanged: bool,
    now_ms: u64,
) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(mut line) = serde_json::to_string(&Intervention {
        ts_ms: now_ms,
        rule: code.to_string(),
        ran_unchanged,
    }) else {
        return;
    };
    line.push('\n');

    // Never append through a symlink someone else placed in our state dir
    // (audit 2026-08-06; the same rule the installer and event log follow).
    let path = dir.join("policy.jsonl");
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return;
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&path)
    else {
        return;
    };
    let _ = f.write_all(line.as_bytes());
}

/// Write a small state file, refusing to write *through* a symlink — the same
/// rule the installer follows (audit 2026-08-06): a path in our state dir that
/// someone else turned into a link to their file is not a path we truncate.
fn write_user_only(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::other("state path is a symlink"));
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)
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

/// Load and validate the full SPEC §15 surface. $OOPSINPUT_MODE overrides
/// the file's mode (and, being an explicit act, never warns).
pub fn load_config() -> Config {
    let mut cfg = match config_path() {
        Some(path) => match read_config_file(&path) {
            Some(text) => parse_config(&text),
            None => Config::default(),
        },
        None => Config::default(),
    };
    if let Ok(v) = std::env::var("OOPSINPUT_MODE") {
        cfg.mode = parse_mode(&v).unwrap_or(Mode::Shadow);
    }
    cfg
}

fn read_config_file(path: &std::path::Path) -> Option<String> {
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(CONFIG_READ_CAP)
        .read_to_end(&mut buf)
        .ok()?;
    String::from_utf8(buf).ok()
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
    let Some(dir) = crate::events::state_dir() else {
        return;
    };
    let fp = warnings_fingerprint(&cfg.warnings);
    let marker = dir.join("config_warned");
    if std::fs::read_to_string(&marker).is_ok_and(|prev| prev == fp) {
        return;
    }
    for w in &cfg.warnings {
        eprintln!("oopsinput: config: {w}");
    }
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = write_user_only(&marker, fp.as_bytes());
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

    /// SPEC §11 golden corpus, policy slice: buffer + context fixture →
    /// expected verdict and reason. These are the context-flip counterfactual
    /// pairs the danger corpus could not express: same command, different
    /// context, different decision.
    #[test]
    fn golden_policy_corpus() {
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

        let cases: Vec<Case> = crate::golden_cases("policy.json", |c: &Case| c.pair.is_some());

        for c in &cases {
            let d = danger_for(&c.buffer);
            let context = ctx(
                c.git.as_ref().map(|g| GitFacts {
                    detached: g.detached,
                    branch_main_like: g.main_like,
                    dirty: g.dirty,
                    untracked: g.untracked,
                }),
                c.targets
                    .iter()
                    .map(|t| target(t.exists, t.is_cwd, t.is_parent, t.near_miss))
                    .collect(),
            );
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
                })
                .collect(),
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
    fn history_survives_a_partial_line_and_reads_only_the_tail() {
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

        // The config-warning marker takes the same path through write_user_only.
        let link = dir.join("marker");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(write_user_only(&link, b"x").is_err(), "write not refused");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "PRECIOUS\n");

        // A normal path still writes, user-only.
        let plain = dir.join("plain");
        assert!(write_user_only(&plain, b"{}").is_ok());
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

        let model =
            std::env::var("OOPSINPUT_EVAL_MODEL").unwrap_or_else(|_| "qwen3:1.7b".to_string());
        let cases: Vec<Case> = crate::golden_cases("policy.json", |c: &Case| c.pair.is_some());
        println!("== paired-corpus comparison, model = {model} ==");
        let mut eligible = 0u32;
        let mut changed = 0u32;
        for c in &cases {
            let lexed = crate::lexer::lex(&c.buffer);
            let d = danger::analyze_with_home(&lexed, Some("/home/u"));
            let context = Context {
                git: c.git.as_ref().map(|g| GitFacts {
                    detached: g.detached,
                    branch_main_like: g.main_like,
                    dirty: g.dirty,
                    untracked: g.untracked,
                }),
                targets: c
                    .targets
                    .iter()
                    .map(|t| target(t.exists, t.is_cwd, t.is_parent, t.near_miss))
                    .collect(),
            };
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
