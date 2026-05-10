//! # brain-ops
//!
//! The five cognitive primitives plus link/unlink and transactions. Wires together the planner, storage, metadata, embedder, and index. Idempotency lives at this layer.
//!
//! See `spec/09_cognitive_operations/` for the authoritative design.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

/// Crate-level marker. Placeholder until implementation begins.
pub const SPEC_REFERENCE: &str = "spec/09_cognitive_operations/";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_reference_is_set() {
        assert!(SPEC_REFERENCE.starts_with("spec/"));
    }
}
