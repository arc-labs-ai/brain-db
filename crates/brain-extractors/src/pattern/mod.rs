//! Pattern extractor — Tier 1.
//!
//! Regex-driven extraction. Cheapest and most deterministic of the
//! three tiers. Runs synchronously during ENCODE.

pub mod extractor;
pub mod temporal;

pub use extractor::{CompiledRegex, PatternExtractor};
pub use temporal::TemporalExtractor;
