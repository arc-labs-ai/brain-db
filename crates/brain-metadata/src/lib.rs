//! # brain-metadata
//!
//! redb-backed metadata: agents, contexts, memory metadata, edges, idempotency table, and the durable LSN checkpoint.
//!
//! See `spec/07_metadata_graph/` for the authoritative design.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

/// Crate-level marker. Placeholder until implementation begins.
pub const SPEC_REFERENCE: &str = "spec/07_metadata_graph/";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_reference_is_set() {
        assert!(SPEC_REFERENCE.starts_with("spec/"));
    }
}
