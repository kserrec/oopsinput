//! Decision layers (SPEC §5). Run in order, cheapest first; each is
//! independently testable and disableable.

pub mod context;
pub mod danger;
pub mod infer;
pub mod typo;
