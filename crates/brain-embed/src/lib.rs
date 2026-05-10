//! # brain-embed
//!
//! Substrate-owned embedding: clients send text, the server embeds. BGE-small via candle. Includes batching window, LRU cache, and determinism guarantees.
//!
//! See `spec/04_embedding_layer/` for the authoritative design.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

/// Crate-level marker. Placeholder until implementation begins.
pub const SPEC_REFERENCE: &str = "spec/04_embedding_layer/";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_reference_is_set() {
        assert!(SPEC_REFERENCE.starts_with("spec/"));
    }
}
