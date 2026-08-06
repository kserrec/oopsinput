//! L1 — typo layer (SPEC §5-L1). Fires only when the shell reported that the
//! command word resolves to nothing: the command was going to fail anyway, so
//! a suggestion costs the user nothing. Zero tolerance for false positives —
//! any doubt about what the user typed, or any exact match anywhere, means no
//! suggestion.
//!
//! Candidates come from PATH executables (scanned here, by readdir — never a
//! shell) plus plugin-supplied names (aliases, functions, builtins, reserved
//! words — only the live shell can see those). Matching is a self-written
//! bounded edit distance per the dependency policy (SPEC §12).

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::lexer::{Lexed, Token, command_words};
use crate::proposal::ResolutionKind;

pub struct Suggestion {
    /// The misspelled command word exactly as typed.
    pub typed: String,
    /// Name of the nearest resolvable command.
    pub candidate: String,
    /// Edit distance from the typed word (1 or 2).
    pub distance: usize,
}

/// A command word longer than this is not a typo we can usefully correct.
const MAX_WORD_CHARS: usize = 64;
/// Candidate names longer than this are skipped outright.
const MAX_NAME_BYTES: usize = 128;
/// Hard cap on entries examined per PATH directory (SPEC §10: pathological
/// environments degrade, never hang).
const MAX_DIR_ENTRIES: usize = 10_000;
/// Hard cap on executability stat() calls across the whole scan.
const MAX_STATS: usize = 200;

pub fn analyze(
    res_kind: ResolutionKind,
    lexed: &Lexed,
    extra_names: &[String],
) -> Option<Suggestion> {
    // Only `none` — an exact "does not resolve" report from the live shell.
    // Unknown/Other mean we don't know, and not-knowing never prompts.
    if res_kind != ResolutionKind::None {
        return None;
    }
    let word = literal_command_word(lexed)?;
    let path = std::env::var("PATH").unwrap_or_default();
    best_candidate(word, &path, extra_names)
}

/// The typed command word, only when we are certain it is the word the plugin
/// resolved (the buffer's first token) and its text is literal: unquoted, no
/// expansion, no path separator, sane length. A single character is never
/// corrected — nearly everything is within distance 1 of it.
fn literal_command_word(lexed: &Lexed) -> Option<&str> {
    let first_cmd = command_words(lexed).into_iter().next()?;
    match lexed.tokens.first() {
        Some(Token::Word(w)) if std::ptr::eq(w, first_cmd) => {}
        _ => return None,
    }
    let len = first_cmd.text.chars().count();
    if first_cmd.quoted
        || first_cmd.expands
        || first_cmd.text.contains('/')
        || !(2..=MAX_WORD_CHARS).contains(&len)
    {
        return None;
    }
    Some(&first_cmd.text)
}

/// Nearest candidate within the distance budget, or None. Deterministic
/// tie-break: smallest distance, then lexicographically smallest name. An
/// exact match anywhere (a name identical to the typed word) means the
/// resolution report and our scan disagree — suppress everything.
fn best_candidate(word: &str, path: &str, extra_names: &[String]) -> Option<Suggestion> {
    let mut search = Search {
        target: word.chars().collect(),
        max: max_distance(word.chars().count()),
        best: None,
        exact: false,
    };

    for name in extra_names {
        if let Some(d) = search.distance_if_useful(name) {
            search.accept(d, name);
        }
    }

    let mut seen_dirs = HashSet::new();
    let mut stats = 0usize;
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        if !seen_dirs.insert(dir) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.take(MAX_DIR_ENTRIES) {
            let Ok(entry) = entry else { continue };
            let name_os = entry.file_name();
            let Some(name) = name_os.to_str() else {
                continue;
            };
            let Some(d) = search.distance_if_useful(name) else {
                continue;
            };
            // A PATH entry only counts if it would actually run — a plain
            // file merely named like a command resolves to nothing.
            if stats >= MAX_STATS {
                continue;
            }
            stats += 1;
            if is_executable_file(&entry.path()) {
                search.accept(d, name);
            }
        }
    }

    if search.exact {
        return None;
    }
    search.best.map(|(distance, candidate)| Suggestion {
        typed: word.to_string(),
        candidate,
        distance,
    })
}

/// Build the corrected buffer: the typed command word replaced by the
/// candidate, **every other byte preserved exactly** (SPEC §9 — this string
/// becomes an executed command; it is the security-critical output). Returns
/// None unless the buffer's first non-whitespace run is byte-identical to
/// `typed` and ends at a word boundary — any disagreement between this check
/// and the lexer's view of the buffer means no replacement is offered.
pub fn replacement_buffer(buffer: &str, typed: &str, candidate: &str) -> Option<String> {
    if typed.is_empty() || candidate.is_empty() {
        return None;
    }
    let start = buffer.find(|c: char| !c.is_whitespace())?;
    let rest = &buffer[start..];
    if !rest.starts_with(typed) {
        return None;
    }
    let after = &rest[typed.len()..];
    // The word must end here too: whitespace, an operator, or end-of-buffer.
    // starts_with alone would let "gti" match into "gtifoo" if this check and
    // the lexer ever disagreed on word boundaries.
    if !after.is_empty() && !after.starts_with(|c: char| c.is_whitespace() || "|&;<>()".contains(c))
    {
        return None;
    }
    let mut out = String::with_capacity(buffer.len() - typed.len() + candidate.len());
    out.push_str(&buffer[..start]);
    out.push_str(candidate);
    out.push_str(after);
    Some(out)
}

/// Distance budget by typed-word length: short words get one edit, longer
/// words two. (An adjacent transposition counts as one edit.)
fn max_distance(len: usize) -> usize {
    if len <= 4 { 1 } else { 2 }
}

struct Search {
    target: Vec<char>,
    max: usize,
    best: Option<(usize, String)>,
    exact: bool,
}

impl Search {
    /// Distance to `name` if within budget AND it would improve the current
    /// best (so callers can skip verification work for losers).
    fn distance_if_useful(&self, name: &str) -> Option<usize> {
        if name.len() > MAX_NAME_BYTES {
            return None;
        }
        let cand: Vec<char> = name.chars().collect();
        let d = bounded_osa(&self.target, &cand, self.max)?;
        (d == 0 || self.improves(d, name)).then_some(d)
    }

    fn improves(&self, d: usize, name: &str) -> bool {
        match &self.best {
            None => true,
            Some((bd, bn)) => d < *bd || (d == *bd && name < bn.as_str()),
        }
    }

    fn accept(&mut self, d: usize, name: &str) {
        if d == 0 {
            self.exact = true;
        } else if self.improves(d, name) {
            self.best = Some((d, name.to_string()));
        }
    }
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Optimal-string-alignment edit distance (insert, delete, substitute, plus
/// adjacent transposition at cost 1), bounded: None once the distance provably
/// exceeds `max`. O(|a|·|b|) time, three rows of memory.
fn bounded_osa(a: &[char], b: &[char], max: usize) -> Option<usize> {
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let lb = b.len();
    let mut prev2: Vec<usize> = vec![0; lb + 1]; // row i-2
    let mut prev: Vec<usize> = (0..=lb).collect(); // row i-1
    let mut curr: Vec<usize> = vec![0; lb + 1]; // row i
    for i in 1..=a.len() {
        curr[0] = i;
        let mut row_min = i;
        for j in 1..=lb {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut d = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(prev2[j - 2] + 1);
            }
            curr[j] = d;
            row_min = row_min.min(d);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut prev2, &mut prev); // prev2 <- row i-1
        std::mem::swap(&mut prev, &mut curr); // prev  <- row i
    }
    (prev[lb] <= max).then_some(prev[lb])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn osa(a: &str, b: &str, max: usize) -> Option<usize> {
        let av: Vec<char> = a.chars().collect();
        let bv: Vec<char> = b.chars().collect();
        bounded_osa(&av, &bv, max)
    }

    #[test]
    fn osa_basics() {
        assert_eq!(osa("git", "git", 2), Some(0));
        assert_eq!(osa("gti", "git", 2), Some(1)); // transposition = 1 edit
        assert_eq!(osa("sl", "ls", 1), Some(1));
        assert_eq!(osa("grpe", "grep", 1), Some(1));
        assert_eq!(osa("pyhton", "python", 2), Some(1));
        assert_eq!(osa("cat", "car", 1), Some(1)); // substitution
        assert_eq!(osa("cta", "cat", 1), Some(1));
        assert_eq!(osa("ct", "cat", 1), Some(1)); // deletion
        assert_eq!(osa("caat", "cat", 1), Some(1)); // insertion
        assert_eq!(osa("", "", 0), Some(0));
        assert_eq!(osa("", "ab", 2), Some(2));
    }

    #[test]
    fn osa_bound_cuts_off() {
        assert_eq!(osa("abcdef", "uvwxyz", 2), None);
        assert_eq!(osa("git", "gitkraken", 2), None); // length prefilter
        assert_eq!(osa("kubectl", "kubect", 2), Some(1));
    }

    #[test]
    fn osa_is_unicode_aware() {
        assert_eq!(osa("dziękuję", "dziekuję", 2), Some(1));
        assert_eq!(osa("嗯", "嗯呢", 1), Some(1));
    }

    #[test]
    fn literal_word_gates() {
        let word = |src: &str| {
            let l = lex(src);
            literal_command_word(&l).map(str::to_string)
        };
        assert_eq!(word("gti status"), Some("gti".into()));
        assert_eq!(word("gti"), Some("gti".into()));
        // The plugin resolved the assignment, not the command word — never fire.
        assert_eq!(word("FOO=1 gti status"), None);
        assert_eq!(word("'gti' status"), None); // quoted
        assert_eq!(word("$X status"), None); // expands
        assert_eq!(word("./scrpt.sh"), None); // path-like: L3's job, not L1's
        assert_eq!(word("g"), None); // single char: too noisy
        assert_eq!(word(""), None);
        assert_eq!(word(&"x".repeat(65)), None); // over length cap
    }

    #[test]
    fn resolving_kinds_never_fire() {
        let lexed = lex("gti status");
        let names = vec!["git".to_string()];
        for kind in [
            ResolutionKind::Alias,
            ResolutionKind::Function,
            ResolutionKind::Builtin,
            ResolutionKind::Command,
            ResolutionKind::Hashed,
            ResolutionKind::Reserved,
            ResolutionKind::Unknown,
            ResolutionKind::Other,
        ] {
            assert!(
                analyze(kind, &lexed, &names).is_none(),
                "kind {kind:?} must never produce a suggestion"
            );
        }
    }

    #[test]
    fn extra_names_match_without_touching_disk() {
        let names = vec!["mylongalias".to_string(), "unrelated".to_string()];
        let s = best_candidate("mylongalais", "", &names).expect("suggestion");
        assert_eq!(s.candidate, "mylongalias");
        assert_eq!(s.distance, 1);
    }

    #[test]
    fn exact_match_anywhere_suppresses() {
        // Scan disagrees with the shell's resolution report → say nothing.
        let names = vec!["gti".to_string(), "git".to_string()];
        assert!(best_candidate("gti", "", &names).is_none());
    }

    #[test]
    fn tie_breaks_are_deterministic() {
        let names = vec!["gitb".to_string(), "gita".to_string()];
        let s = best_candidate("gitx", "", &names).expect("suggestion");
        assert_eq!(s.candidate, "gita"); // same distance → lexicographic
        // smaller distance always beats a lexicographically smaller name
        let names = vec!["aatxx".to_string(), "gitx".to_string()];
        let s = best_candidate("gitxx", "", &names).expect("suggestion");
        assert_eq!(s.candidate, "gitx"); // d=1 beats d=2 "aatxx"
    }

    #[test]
    fn short_words_get_distance_one_only() {
        let names = vec!["cat".to_string()];
        assert!(best_candidate("cxy", "", &names).is_none()); // d=2, len 3
        assert!(best_candidate("cay", "", &names).is_some()); // d=1
        let names = vec!["python".to_string()];
        assert!(best_candidate("pytohm", "", &names).is_some()); // d=2, len 6
    }

    #[test]
    fn path_scan_finds_executables_only() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("oopsinput-typo-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mk = |name: &str, mode: u32| {
            let p = dir.join(name);
            fs::write(&p, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
        };
        mk("frobnicate", 0o755);
        mk("frobnicatz", 0o644); // closer name, but not executable

        let path = dir.to_str().unwrap();
        let s = best_candidate("frobnicat", path, &[]).expect("suggestion");
        assert_eq!(s.candidate, "frobnicate");
        assert_eq!(s.distance, 1);

        // An exact-named plain file is not a resolution conflict.
        mk("mistyped", 0o644);
        let names = vec!["mistypes".to_string()];
        let s = best_candidate("mistyped", path, &names).expect("suggestion");
        assert_eq!(s.candidate, "mistypes");

        let _ = fs::remove_dir_all(&dir);
    }

    // ---- replacement_buffer: pinned (SPEC §9 — this output gets executed) ----

    #[test]
    fn replacement_swaps_only_the_command_word() {
        assert_eq!(
            replacement_buffer("gti status", "gti", "git").as_deref(),
            Some("git status")
        );
        assert_eq!(
            replacement_buffer("gti", "gti", "git").as_deref(),
            Some("git")
        );
        // every byte after the word preserved exactly: args, operators,
        // quoting, unicode, doubled spaces, trailing whitespace
        assert_eq!(
            replacement_buffer("gti  commit -m 'żółć 嗯' && ls |grep x ", "gti", "git").as_deref(),
            Some("git  commit -m 'żółć 嗯' && ls |grep x ")
        );
        // leading whitespace preserved exactly
        assert_eq!(
            replacement_buffer("  \tgti status", "gti", "git").as_deref(),
            Some("  \tgit status")
        );
        // a later occurrence of the typed word is never touched
        assert_eq!(
            replacement_buffer("gti log gti", "gti", "git").as_deref(),
            Some("git log gti")
        );
        // operator directly after the word
        assert_eq!(
            replacement_buffer("gti&&echo hi", "gti", "git").as_deref(),
            Some("git&&echo hi")
        );
    }

    #[test]
    fn replacement_refuses_on_any_disagreement() {
        // buffer does not start with the typed word
        assert_eq!(replacement_buffer("FOO=1 gti status", "gti", "git"), None);
        // typed word is a prefix of the actual first word — boundary check
        assert_eq!(replacement_buffer("gtifoo status", "gti", "git"), None);
        // empty pieces never construct anything
        assert_eq!(replacement_buffer("", "gti", "git"), None);
        assert_eq!(replacement_buffer("gti x", "", "git"), None);
        assert_eq!(replacement_buffer("gti x", "gti", ""), None);
        assert_eq!(replacement_buffer("   ", "gti", "git"), None);
    }

    #[test]
    fn empty_and_hostile_paths_are_harmless() {
        assert!(best_candidate("gti", "", &[]).is_none());
        assert!(best_candidate("gti", ":::", &[]).is_none());
        assert!(best_candidate("gti", "/nonexistent-dir-xyz:/dev/null", &[]).is_none());
    }
}
