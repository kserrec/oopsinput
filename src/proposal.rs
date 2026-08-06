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
    /// Recency relation (SPEC §5-L3), newest first. Computed *in the plugin*
    /// so no raw history text ever crosses the boundary.
    pub recency: Vec<RecencyEntry>,
}

/// One structural summary of a recent command. The plugin sanitizes the
/// word fields to `[A-Za-z0-9_-]{1,32}` (anything else becomes `_`), and the
/// parser re-enforces that here — trust nothing about the transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecencyEntry {
    /// How many commands back (1 = the previous command).
    pub age: u8,
    /// The entry shares a non-command word with the current buffer — e.g.
    /// "two commands ago referenced ./build".
    pub shares_word: bool,
    /// Sanitized command word, or "_" when it wasn't identifier-like.
    pub cmd: String,
    /// Sanitized second word (subcommand position), or "_".
    pub sub: String,
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

/// Recency entries kept; the plugin sends 5, the cap allows a little drift.
const MAX_RECENCY: usize = 8;

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
        let payload = parse_payload(bytes)?;

        Ok(Proposal {
            buffer: payload.buffer,
            res_kind,
            capped: payload.capped,
            names: payload.names,
            names_capped: payload.names_capped,
            recency: payload.recency,
        })
    }
}

struct Payload {
    buffer: String,
    capped: bool,
    names: Vec<String>,
    names_capped: bool,
    recency: Vec<RecencyEntry>,
}

/// Split the stdin payload into its NUL-separated sections:
/// `buffer [\0 names [\0 recency]]` — names newline-separated, recency one
/// entry per line. NUL is a collision-free separator: zsh strings (command
/// buffers, alias/function names) can never contain it.
fn parse_payload(mut bytes: Vec<u8>) -> Result<Payload, ReadError> {
    let cap = MAX_INPUT_BYTES as usize;
    // Search the full read (up to cap + 1 bytes), not just the capped prefix:
    // a NUL landing exactly on the cap boundary is still a complete buffer
    // with a (lost) tail section, not a capped buffer (bughunt 2026-08-06).
    let Some(pos) = bytes.iter().position(|&b| b == 0) else {
        // Buffer only: the whole payload is the command.
        let (buffer, capped) = buffer_from_bytes(bytes)?;
        return Ok(Payload {
            buffer,
            capped,
            names: Vec::new(),
            names_capped: false,
            recency: Vec::new(),
        });
    };

    let over_cap = bytes.len() > cap;
    let mut rest = bytes.split_off(pos + 1);
    bytes.pop(); // the NUL separator

    // The buffer section ended at the separator, so it is complete: any UTF-8
    // error here is the caller's malformed input (fail open), never our cap.
    let buffer = String::from_utf8(bytes).map_err(|_| ReadError::Stdin)?;

    // Second separator: names | recency. Absent = names-only payload.
    let (names_bytes, recency_bytes, names_end_seen) = match rest.iter().position(|&b| b == 0) {
        Some(p) => {
            let after = rest.split_off(p + 1);
            rest.pop();
            (rest, Some(after), true)
        }
        None => (rest, None, false),
    };

    let mut names: Vec<String> = names_bytes
        .split(|&b| b == b'\n')
        .filter(|n| !n.is_empty())
        .filter_map(|n| std::str::from_utf8(n).ok().map(str::to_string))
        .take(MAX_NAMES)
        .collect();
    // Cap fell inside the names section (no closing separator was read): the
    // final name may be cut mid-name — drop it rather than risk suggesting a
    // corruption.
    let names_capped = over_cap && !names_end_seen;
    if names_capped {
        names.pop();
    }

    let mut raw_recency: Vec<&[u8]> = recency_bytes
        .as_deref()
        .unwrap_or(&[])
        .split(|&b| b == b'\n')
        .collect();
    if over_cap && names_end_seen {
        // The cap fell inside the recency tail: the last RAW line may be cut.
        // It must be dropped before parsing — a cut line usually fails the
        // parse and vanishes on its own, and popping after parsing would
        // then remove a complete entry instead (probed: the unit test below
        // caught exactly that).
        raw_recency.pop();
    }
    let recency: Vec<RecencyEntry> = raw_recency
        .into_iter()
        .filter_map(|l| std::str::from_utf8(l).ok())
        .filter_map(parse_recency_line)
        .take(MAX_RECENCY)
        .collect();

    Ok(Payload {
        buffer,
        capped: false,
        names,
        names_capped,
        recency,
    })
}

/// `<age> <shares> <cmd> <sub>` — strict on the numeric fields (a malformed
/// line is dropped, never guessed at), re-sanitizing on the word fields.
fn parse_recency_line(line: &str) -> Option<RecencyEntry> {
    let mut f = line.split(' ');
    let age: u8 = f.next()?.parse().ok().filter(|a| (1..=9).contains(a))?;
    let shares_word = match f.next()? {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    let cmd = sanitize_word(f.next().unwrap_or("_"));
    let sub = sanitize_word(f.next().unwrap_or("_"));
    Some(RecencyEntry {
        age,
        shares_word,
        cmd,
        sub,
    })
}

/// The only word shape allowed through the recency channel. The plugin
/// enforces this too; enforcing it again here means a compromised or skewed
/// adapter still cannot push arbitrary text into prompts or logs.
fn sanitize_word(w: &str) -> String {
    let ok = !w.is_empty()
        && w.len() <= 32
        && w.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok { w.to_string() } else { "_".to_string() }
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
        let Ok(p) = parse_payload(b"gti status\0git\nls\nmyalias\n".to_vec()) else {
            panic!("names payload must parse");
        };
        assert_eq!(p.buffer, "gti status");
        assert!(!p.capped);
        assert_eq!(p.names, vec!["git", "ls", "myalias"]);
        assert!(!p.names_capped);
        assert!(p.recency.is_empty());
    }

    #[test]
    fn payload_without_names_has_empty_pool() {
        let Ok(p) = parse_payload(b"git status".to_vec()) else {
            panic!("plain payload must parse");
        };
        assert_eq!(p.buffer, "git status");
        assert!(p.names.is_empty());
        assert!(!p.names_capped);
    }

    #[test]
    fn empty_names_section_is_fine() {
        let Ok(p) = parse_payload(b"cmd\0".to_vec()) else {
            panic!("empty names section must parse");
        };
        assert_eq!(p.buffer, "cmd");
        assert!(p.names.is_empty());
    }

    #[test]
    fn invalid_utf8_name_is_skipped_not_fatal() {
        let Ok(p) = parse_payload(b"ok\0good\nb\xffad\nalso\n".to_vec()) else {
            panic!("one bad name must not kill the check");
        };
        assert_eq!(p.names, vec!["good", "also"]);
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
        let Ok(p) = parse_payload(bytes) else {
            panic!("capped names payload must parse");
        };
        assert_eq!(p.buffer, "gti");
        assert!(!p.capped, "buffer is complete — must not be marked capped");
        assert!(p.names_capped);
        assert_eq!(
            p.names,
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
        let Ok(p) = parse_payload(bytes) else {
            panic!("boundary payload must parse");
        };
        assert_eq!(p.buffer.len(), MAX);
        assert!(!p.capped, "buffer ended at the separator — not capped");
        assert!(p.names.is_empty());
        assert!(
            p.names_capped,
            "the names section itself was beyond the read"
        );
    }

    // ---- recency section (SPEC §5-L3 transport) ----

    #[test]
    fn full_three_section_payload_parses() {
        let Ok(p) = parse_payload(b"rm -rf ./build\x00\x001 0 make _\n2 1 cp _\n".to_vec()) else {
            panic!("three-section payload must parse");
        };
        assert_eq!(p.buffer, "rm -rf ./build");
        assert!(p.names.is_empty());
        assert!(!p.names_capped, "names section ended at its separator");
        assert_eq!(p.recency.len(), 2);
        assert_eq!(p.recency[0].age, 1);
        assert!(!p.recency[0].shares_word);
        assert_eq!(p.recency[0].cmd, "make");
        assert_eq!(p.recency[1].age, 2);
        assert!(p.recency[1].shares_word);
        assert_eq!(p.recency[1].cmd, "cp");
        assert_eq!(p.recency[1].sub, "_");
    }

    #[test]
    fn names_and_recency_coexist() {
        let Ok(p) = parse_payload(b"gti\0git\nls\n\x001 1 git diff\n".to_vec()) else {
            panic!("payload must parse");
        };
        assert_eq!(p.names, vec!["git", "ls"]);
        assert_eq!(p.recency.len(), 1);
        assert_eq!(p.recency[0].cmd, "git");
        assert_eq!(p.recency[0].sub, "diff");
    }

    #[test]
    fn malformed_recency_lines_are_dropped_never_guessed() {
        let Ok(p) = parse_payload(
            b"x\x00\x00bad line here\n0 0 under _\n12 0 overage _\n1 2 badshare _\n3 1 ok _\n"
                .to_vec(),
        ) else {
            panic!("payload must parse");
        };
        assert_eq!(p.recency.len(), 1, "only the well-formed line survives");
        assert_eq!(p.recency[0].age, 3);
    }

    #[test]
    fn recency_words_are_resanitized_against_a_hostile_adapter() {
        // The plugin sanitizes, but the parser must not trust it: raw text
        // must never ride the recency channel into prompts or logs.
        let Ok(p) = parse_payload(b"x\x00\x001 0 $(evil) --flag=secret\n2 0 git push\n".to_vec())
        else {
            panic!("payload must parse");
        };
        assert_eq!(p.recency[0].cmd, "_", "shell metachars must collapse");
        assert_eq!(p.recency[0].sub, "_", "an = carries values — collapse");
        assert_eq!(p.recency[1].cmd, "git");
        assert_eq!(p.recency[1].sub, "push");
        // over-long words collapse too
        let long = format!("x\0\x001 0 {} _\n", "a".repeat(33));
        let p = parse_payload(long.into_bytes()).ok().unwrap_or_else(|| {
            panic!("payload must parse");
        });
        assert_eq!(p.recency[0].cmd, "_");
    }

    #[test]
    fn cap_inside_recency_drops_last_line_keeps_the_rest() {
        let mut bytes = b"gti\0git\n\x001 0 make _\n2 0 git diff\n".to_vec();
        let pad = MAX + 1 - bytes.len();
        bytes.extend(std::iter::repeat_n(b'y', pad)); // giant cut last line
        let Ok(p) = parse_payload(bytes) else {
            panic!("capped recency payload must parse");
        };
        assert_eq!(p.buffer, "gti");
        assert_eq!(p.names, vec!["git"], "names ended at their separator");
        assert!(!p.names_capped, "names section is complete");
        assert_eq!(p.recency.len(), 2, "cut recency line must be dropped");
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
