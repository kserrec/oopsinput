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
}

pub enum ReadError {
    Stdin,
}

/// Hard cap on proposal input size; larger input is truncated and the analysis
/// degrades to `observe` per SPEC §10 (in shadow, that distinction is recorded
/// only).
const MAX_INPUT_BYTES: u64 = 1 << 20;

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

        let mut buffer = String::new();
        std::io::stdin()
            .take(MAX_INPUT_BYTES)
            .read_to_string(&mut buffer)
            .map_err(|_| ReadError::Stdin)?;

        Ok(Proposal { buffer, res_kind })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_vocabulary_is_closed() {
        assert_eq!(ResolutionKind::parse("alias"), ResolutionKind::Alias);
        assert_eq!(ResolutionKind::parse("none"), ResolutionKind::None);
        // raw text collapses to Other — never stored verbatim
        assert_eq!(ResolutionKind::parse("$(rm -rf /)"), ResolutionKind::Other);
        assert_eq!(ResolutionKind::parse(""), ResolutionKind::Other);
    }
}
