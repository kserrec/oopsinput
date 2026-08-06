//! Self-written bounded edit distance (dependency policy, SPEC §12): shared
//! by the typo layer (nearest command-word candidates) and the context layer
//! (near-miss target names).

/// Distance budget by typed-word length: short words get one edit, longer
/// words two. (An adjacent transposition counts as one edit.)
pub(crate) fn max_distance(len: usize) -> usize {
    if len <= 4 { 1 } else { 2 }
}

/// Optimal-string-alignment edit distance (insert, delete, substitute, plus
/// adjacent transposition at cost 1), bounded: None once the distance provably
/// exceeds `max`. O(|a|·|b|) time, three rows of memory.
pub(crate) fn bounded_osa(a: &[char], b: &[char], max: usize) -> Option<usize> {
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
}
