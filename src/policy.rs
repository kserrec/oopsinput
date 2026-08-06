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

use std::collections::HashMap;
use std::io::Read;
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
        if !targets.is_empty() && targets.iter().all(|t| t.exists) {
            return assess(Verdict::Allow, "policy.context_clear");
        }
        return assess(Verdict::Observe, "policy.evidence_unavailable");
    }
    // Recognized but not yet graduated past shadow (SPEC §8): the pilot's
    // event log decides which of these earn a warn tier.
    assess(Verdict::Observe, "policy.candidate_observed")
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

#[derive(Serialize, Deserialize, Default)]
pub struct PolicyState {
    /// Timestamps (ms) of visible interventions, pruned to the last hour.
    #[serde(default)]
    interventions_ts_ms: Vec<u64>,
    /// Per-rule cooldown state, keyed by the primary evidence code.
    #[serde(default)]
    cooldowns: HashMap<String, Cooldown>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Cooldown {
    until_ms: u64,
    consecutive_run_unchanged: u32,
}

const HOUR_MS: u64 = 3_600_000;
const COOLDOWN_MS: u64 = 24 * HOUR_MS;
/// Run-unchanged outcomes in a row before a rule goes quiet for a day.
const COOLDOWN_TRIGGER: u32 = 3;

/// Gate a visible intervention through the budget and the per-rule cooldown.
/// Direct-catastrophic is exempt from both (SPEC §7). `commit` records the
/// spend — pass false when the intervention cannot actually be shown yet.
/// Exhaustion degrades to observe (shadow recording), never to nagging.
pub fn apply_gates(
    assessment: Assessment,
    primary_code: Option<&str>,
    catastrophic: bool,
    state: &mut PolicyState,
    now_ms: u64,
    budget_per_hour: u32,
    commit: bool,
) -> Assessment {
    if !matches!(assessment.verdict, Verdict::Warn | Verdict::Confirm) {
        return assessment;
    }
    if catastrophic {
        return assessment;
    }
    if let Some(code) = primary_code
        && state
            .cooldowns
            .get(code)
            .is_some_and(|c| c.until_ms > now_ms)
    {
        return assess(Verdict::Observe, "policy.rule_cooldown");
    }
    state
        .interventions_ts_ms
        .retain(|t| now_ms.saturating_sub(*t) < HOUR_MS);
    if state.interventions_ts_ms.len() as u32 >= budget_per_hour {
        return assess(Verdict::Observe, "policy.budget_exhausted");
    }
    if commit {
        state.interventions_ts_ms.push(now_ms);
    }
    assessment
}

/// Called by the warning UI (next M3 item) with what the user did. Repeated
/// run-unchanged on the same rule reads as "I mean it, stop asking" and
/// triggers the cooldown; any edit/cancel resets it.
pub fn record_outcome(state: &mut PolicyState, code: &str, ran_unchanged: bool, now_ms: u64) {
    let c = state.cooldowns.entry(code.to_string()).or_default();
    if ran_unchanged {
        c.consecutive_run_unchanged += 1;
        if c.consecutive_run_unchanged >= COOLDOWN_TRIGGER {
            c.until_ms = now_ms + COOLDOWN_MS;
        }
    } else {
        c.consecutive_run_unchanged = 0;
        c.until_ms = 0;
    }
}

/// The rule a cooldown keys on: the first danger code (rules note their own
/// code before any target classification, so this is the rule identity).
pub fn primary_code(danger: &Analysis) -> Option<&'static str> {
    danger.codes.first().copied()
}

pub fn load_state() -> PolicyState {
    let Some(dir) = crate::events::state_dir() else {
        return PolicyState::default();
    };
    let Ok(text) = std::fs::read_to_string(dir.join("policy.json")) else {
        return PolicyState::default();
    };
    // Corrupt state self-heals to defaults — never blocks a command.
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save_state(state: &PolicyState) {
    let Some(dir) = crate::events::state_dir() else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(json) = serde_json::to_string(state) else {
        return;
    };
    let _ = write_user_only(&dir.join("policy.json"), json.as_bytes());
}

fn write_user_only(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
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
    #[allow(dead_code)]
    pub model: Option<String>,
    #[allow(dead_code)]
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

    #[test]
    fn budget_exhaustion_degrades_to_observe() {
        let mut state = PolicyState::default();
        let warn = assess(Verdict::Warn, "policy.dirty_work_at_risk");
        let now = 10 * HOUR_MS;
        // budget 3: three commits pass, the fourth degrades
        for _ in 0..3 {
            let got = apply_gates(
                warn,
                Some("git.reset_hard"),
                false,
                &mut state,
                now,
                3,
                true,
            );
            assert_eq!(got.verdict, Verdict::Warn);
        }
        let got = apply_gates(
            warn,
            Some("git.reset_hard"),
            false,
            &mut state,
            now,
            3,
            true,
        );
        assert_eq!(got.verdict, Verdict::Observe);
        assert_eq!(got.reason, "policy.budget_exhausted");
        // an hour later the window has rolled over
        let later = now + HOUR_MS;
        let got = apply_gates(
            warn,
            Some("git.reset_hard"),
            false,
            &mut state,
            later,
            3,
            true,
        );
        assert_eq!(got.verdict, Verdict::Warn);
    }

    #[test]
    fn catastrophic_is_exempt_from_budget_and_cooldown() {
        let mut state = PolicyState::default();
        let confirm = assess(Verdict::Confirm, "policy.direct_catastrophic");
        let now = HOUR_MS;
        // exhaust the budget
        for _ in 0..5 {
            apply_gates(
                assess(Verdict::Warn, "x"),
                Some("git.reset_hard"),
                false,
                &mut state,
                now,
                3,
                true,
            );
        }
        // and put the rule in cooldown
        for _ in 0..COOLDOWN_TRIGGER {
            record_outcome(&mut state, "fs.rm_recursive", true, now);
        }
        let got = apply_gates(
            confirm,
            Some("fs.rm_recursive"),
            true,
            &mut state,
            now,
            3,
            true,
        );
        assert_eq!(got.verdict, Verdict::Confirm, "catastrophic never gated");
    }

    #[test]
    fn repeated_run_unchanged_triggers_cooldown_and_any_other_outcome_resets() {
        let mut state = PolicyState::default();
        let warn = assess(Verdict::Warn, "policy.target_context");
        let now = HOUR_MS;
        for _ in 0..COOLDOWN_TRIGGER {
            record_outcome(&mut state, "fs.rm_recursive", true, now);
        }
        let got = apply_gates(
            warn,
            Some("fs.rm_recursive"),
            false,
            &mut state,
            now,
            3,
            true,
        );
        assert_eq!(got.verdict, Verdict::Observe);
        assert_eq!(got.reason, "policy.rule_cooldown");
        // a different rule is unaffected
        let got = apply_gates(
            warn,
            Some("git.reset_hard"),
            false,
            &mut state,
            now,
            3,
            true,
        );
        assert_eq!(got.verdict, Verdict::Warn);
        // an edit outcome resets the first rule
        record_outcome(&mut state, "fs.rm_recursive", false, now);
        let got = apply_gates(
            warn,
            Some("fs.rm_recursive"),
            false,
            &mut state,
            now,
            3,
            true,
        );
        assert_eq!(got.verdict, Verdict::Warn);
        // cooldown expires on its own
        let mut state2 = PolicyState::default();
        for _ in 0..COOLDOWN_TRIGGER {
            record_outcome(&mut state2, "fs.rm_recursive", true, now);
        }
        let after = now + COOLDOWN_MS + 1;
        let got = apply_gates(
            warn,
            Some("fs.rm_recursive"),
            false,
            &mut state2,
            after,
            3,
            true,
        );
        assert_eq!(got.verdict, Verdict::Warn);
    }

    #[test]
    fn uncommitted_gate_checks_spend_nothing() {
        // Until the warning UI exists, gating runs with commit=false: the
        // budget must not be consumed by interventions nobody saw.
        let mut state = PolicyState::default();
        let warn = assess(Verdict::Warn, "policy.dirty_work_at_risk");
        for _ in 0..10 {
            let got = apply_gates(
                warn,
                Some("git.reset_hard"),
                false,
                &mut state,
                HOUR_MS,
                3,
                false,
            );
            assert_eq!(got.verdict, Verdict::Warn);
        }
        assert!(state.interventions_ts_ms.is_empty());
    }

    #[test]
    fn state_roundtrips_and_corrupt_state_self_heals() {
        let mut state = PolicyState::default();
        record_outcome(&mut state, "git.reset_hard", true, 5);
        let json = serde_json::to_string(&state).unwrap();
        let back: PolicyState = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.cooldowns["git.reset_hard"].consecutive_run_unchanged,
            1
        );
        let corrupt: PolicyState = serde_json::from_str("{not json").unwrap_or_default();
        assert!(corrupt.cooldowns.is_empty());
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
}
