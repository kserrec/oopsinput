//! Proposal input for `check`: the raw buffer on stdin plus adapter-supplied
//! metadata as fixed-vocabulary flags. The buffer never travels through argv
//! (argv is world-readable in /proc); free-text metadata never travels through
//! argv either — only closed-vocabulary tokens do.

use std::io::Read;

/// How the shell resolved the command word. Closed vocabulary — anything the
/// adapter sends outside it collapses to `Other`, so raw user text can never
/// ride in on this flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionKind {
    Alias,
    Function,
    Builtin,
    Command,
    Hashed,
    Reserved,
    None,
    Unknown,
    Other,
}

impl ResolutionKind {
    pub fn parse(s: &str) -> Self {
        match s {
            "alias" => Self::Alias,
            "function" => Self::Function,
            "builtin" => Self::Builtin,
            "command" => Self::Command,
            "hashed" => Self::Hashed,
            "reserved" => Self::Reserved,
            "none" => Self::None,
            "unknown" => Self::Unknown,
            _ => Self::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alias => "alias",
            Self::Function => "function",
            Self::Builtin => "builtin",
            Self::Command => "command",
            Self::Hashed => "hashed",
            Self::Reserved => "reserved",
            Self::None => "none",
            Self::Unknown => "unknown",
            Self::Other => "other",
        }
    }
}

pub struct Proposal {
    pub buffer: String,
    pub res_kind: ResolutionKind,
    /// Input exceeded the size cap and was truncated at a char boundary.
    /// Oversized input is capped-and-analyzed, never dropped (fail open only
    /// on genuinely unreadable input).
    pub capped: bool,
    /// Adapter-supplied names of resolvable commands (aliases, functions,
    /// builtins, reserved words) — the typo layer's candidate pool beyond
    /// PATH. Arrives on stdin after a NUL separator, newline-separated.
    pub names: Vec<String>,
    /// The size cap fell inside the names section (candidates lost — harmless,
    /// the buffer itself is complete).
    pub names_capped: bool,
}

pub enum ReadError {
    Stdin,
}

/// Hard cap on proposal input size (SPEC §10). We read one byte past it so
/// "exactly at the cap" and "over the cap" are distinguishable.
const MAX_INPUT_BYTES: u64 = 1 << 20;

/// Hard cap on adapter-supplied names (SPEC §10: pathological input degrades,
/// never hangs). Per-name length is capped in the typo layer.
const MAX_NAMES: usize = 20_000;

impl Proposal {
    /// Parse `check` arguments plus stdin. Unknown flags are ignored (forward
    /// compatibility for the adapter seam).
    pub fn from_check_invocation(args: &[String]) -> Result<Self, ReadError> {
        let mut res_kind = ResolutionKind::Unknown;
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            if arg == "--res"
                && let Some(v) = it.next()
            {
                res_kind = ResolutionKind::parse(v);
            }
        }

        let mut bytes = Vec::new();
        std::io::stdin()
            .take(MAX_INPUT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ReadError::Stdin)?;
        let (buffer, capped, names, names_capped) = parse_payload(bytes)?;

        Ok(Proposal {
            buffer,
            res_kind,
            capped,
            names,
            names_capped,
        })
    }
}

/// Split the stdin payload: buffer bytes, optionally followed by a NUL and a
/// newline-separated list of resolvable names. NUL is a collision-free
/// separator — zsh strings (command buffers, alias/function names) can never
/// contain it. Returns (buffer, buffer_capped, names, names_capped).
fn parse_payload(mut bytes: Vec<u8>) -> Result<(String, bool, Vec<String>, bool), ReadError> {
    let cap = MAX_INPUT_BYTES as usize;
    // Search the full read (up to cap + 1 bytes), not just the capped prefix:
    // a NUL landing exactly on the cap boundary is still a complete buffer
    // with a (lost) names section, not a capped buffer (bughunt 2026-08-06).
    let Some(pos) = bytes.iter().position(|&b| b == 0) else {
        // No names section: the whole payload is the buffer.
        let (buffer, capped) = buffer_from_bytes(bytes)?;
        return Ok((buffer, capped, Vec::new(), false));
    };

    let over_cap = bytes.len() > cap;
    let names_bytes = bytes.split_off(pos + 1);
    bytes.pop(); // the NUL separator

    // The buffer section ended at the separator, so it is complete: any UTF-8
    // error here is the caller's malformed input (fail open), never our cap.
    let buffer = String::from_utf8(bytes).map_err(|_| ReadError::Stdin)?;

    let mut names: Vec<String> = names_bytes
        .split(|&b| b == b'\n')
        .filter(|n| !n.is_empty())
        .filter_map(|n| std::str::from_utf8(n).ok().map(str::to_string))
        .take(MAX_NAMES)
        .collect();
    if over_cap {
        // The cap fell inside the names section; the final name may be cut
        // mid-name — drop it rather than risk suggesting a corruption.
        names.pop();
    }
    Ok((buffer, false, names, over_cap))
}

/// Turn raw stdin bytes into the buffer string, enforcing the size cap at a
/// UTF-8 character boundary. Regression (bughunt, deferred from M1): a cap
/// landing mid-character used to error out, losing the event entirely —
/// oversized input must be capped-and-analyzed instead.
fn buffer_from_bytes(mut bytes: Vec<u8>) -> Result<(String, bool), ReadError> {
    let capped = bytes.len() as u64 > MAX_INPUT_BYTES;
    if capped {
        bytes.truncate(MAX_INPUT_BYTES as usize);
    }
    match String::from_utf8(bytes) {
        Ok(s) => Ok((s, capped)),
        // The only recoverable failure: our own truncation left an incomplete
        // multi-byte sequence at the very end (error_len() == None means the
        // input ended mid-character rather than containing an invalid byte).
        Err(e) if capped && e.utf8_error().error_len().is_none() => {
            let valid = e.utf8_error().valid_up_to();
            let mut b = e.into_bytes();
            b.truncate(valid);
            // b is valid UTF-8 by construction (valid_up_to is a boundary);
            // the lossy pass changes nothing, it just avoids unsafe.
            Ok((String::from_utf8_lossy(&b).into_owned(), true))
        }
        Err(_) => Err(ReadError::Stdin),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = MAX_INPUT_BYTES as usize;

    #[test]
    fn resolution_vocabulary_is_closed() {
        assert_eq!(ResolutionKind::parse("alias"), ResolutionKind::Alias);
        assert_eq!(ResolutionKind::parse("none"), ResolutionKind::None);
        // raw text collapses to Other — never stored verbatim
        assert_eq!(ResolutionKind::parse("$(rm -rf /)"), ResolutionKind::Other);
        assert_eq!(ResolutionKind::parse(""), ResolutionKind::Other);
    }

    #[test]
    fn small_valid_input_is_not_capped() {
        let (s, capped) = buffer_from_bytes(b"git status".to_vec()).unwrap_or(("".into(), true));
        assert_eq!(s, "git status");
        assert!(!capped);
    }

    #[test]
    fn input_exactly_at_cap_is_not_capped() {
        let bytes = vec![b'a'; MAX];
        let Ok((s, capped)) = buffer_from_bytes(bytes) else {
            panic!("exact-cap input must parse");
        };
        assert_eq!(s.len(), MAX);
        assert!(!capped);
    }

    #[test]
    fn oversized_input_is_truncated_not_dropped() {
        let bytes = vec![b'a'; MAX + 1];
        let Ok((s, capped)) = buffer_from_bytes(bytes) else {
            panic!("oversized input must be capped, not rejected");
        };
        assert_eq!(s.len(), MAX);
        assert!(capped);
    }

    #[test]
    fn cap_landing_mid_character_truncates_at_boundary() {
        // '€' is 3 bytes; place it so the cap cuts it in half. After take()
        // reads MAX+1 bytes, the euro's first two bytes are inside the cap.
        let mut bytes = vec![b'a'; MAX - 1];
        bytes.extend_from_slice("€".as_bytes()); // total MAX + 2
        bytes.truncate(MAX + 1); // what take(MAX+1) would deliver
        let Ok((s, capped)) = buffer_from_bytes(bytes) else {
            panic!("mid-character cap must truncate at a boundary, not error");
        };
        assert!(capped);
        assert_eq!(s.len(), MAX - 1, "incomplete tail character not dropped");
        assert!(s.bytes().all(|b| b == b'a'));
    }

    #[test]
    fn payload_with_names_parses() {
        let Ok((buffer, capped, names, names_capped)) =
            parse_payload(b"gti status\0git\nls\nmyalias\n".to_vec())
        else {
            panic!("names payload must parse");
        };
        assert_eq!(buffer, "gti status");
        assert!(!capped);
        assert_eq!(names, vec!["git", "ls", "myalias"]);
        assert!(!names_capped);
    }

    #[test]
    fn payload_without_names_has_empty_pool() {
        let Ok((buffer, _, names, names_capped)) = parse_payload(b"git status".to_vec()) else {
            panic!("plain payload must parse");
        };
        assert_eq!(buffer, "git status");
        assert!(names.is_empty());
        assert!(!names_capped);
    }

    #[test]
    fn empty_names_section_is_fine() {
        let Ok((buffer, _, names, _)) = parse_payload(b"cmd\0".to_vec()) else {
            panic!("empty names section must parse");
        };
        assert_eq!(buffer, "cmd");
        assert!(names.is_empty());
    }

    #[test]
    fn invalid_utf8_name_is_skipped_not_fatal() {
        let Ok((_, _, names, _)) = parse_payload(b"ok\0good\nb\xffad\nalso\n".to_vec()) else {
            panic!("one bad name must not kill the check");
        };
        assert_eq!(names, vec!["good", "also"]);
    }

    #[test]
    fn invalid_utf8_in_buffer_section_still_fails_open() {
        assert!(parse_payload(b"g\xffti\0git\n".to_vec()).is_err());
    }

    #[test]
    fn cap_inside_names_drops_last_name_keeps_buffer() {
        // Total exceeds the cap, but the NUL sits early: the buffer must be
        // intact and uncapped; the possibly-cut final name must be dropped.
        let mut bytes = b"gti\0alpha\nbeta\n".to_vec();
        let pad = MAX + 1 - bytes.len();
        bytes.extend(std::iter::repeat_n(b'x', pad)); // one giant final name
        let Ok((buffer, capped, names, names_capped)) = parse_payload(bytes) else {
            panic!("capped names payload must parse");
        };
        assert_eq!(buffer, "gti");
        assert!(!capped, "buffer is complete — must not be marked capped");
        assert!(names_capped);
        assert_eq!(
            names,
            vec!["alpha", "beta"],
            "cut final name must be dropped"
        );
    }

    #[test]
    fn nul_exactly_at_cap_is_a_complete_buffer() {
        // Regression (bughunt 2026-08-06): the NUL search once stopped at the
        // cap, so a buffer of exactly MAX bytes followed by the separator was
        // mislabeled as capped.
        let mut bytes = vec![b'a'; MAX];
        bytes.push(0);
        let Ok((buffer, capped, names, names_capped)) = parse_payload(bytes) else {
            panic!("boundary payload must parse");
        };
        assert_eq!(buffer.len(), MAX);
        assert!(!capped, "buffer ended at the separator — not capped");
        assert!(names.is_empty());
        assert!(names_capped, "the names section itself was beyond the read");
    }

    #[test]
    fn genuinely_invalid_utf8_still_fails_open() {
        // An invalid byte mid-stream is not our truncation — reject as before.
        assert!(buffer_from_bytes(b"git \xff status".to_vec()).is_err());
        // Incomplete sequence at the end of a small (uncapped) input: the
        // stream itself was malformed, not our cap.
        let mut bytes = b"ok ".to_vec();
        bytes.extend_from_slice(&"€".as_bytes()[..2]);
        assert!(buffer_from_bytes(bytes).is_err());
    }
}
