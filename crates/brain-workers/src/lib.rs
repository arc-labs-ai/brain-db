//! # brain-workers
//!
//! The 12 background workers: decay, access boost, consolidation, HNSW maintenance, idempotency cleanup, slot reclamation, WAL retention, edge scrub, counter reconciliation, statistics, embedder cache eviction, and snapshots.
//!
//! See `spec/11_background_workers/` for the authoritative design.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

/// Crate-level marker. Placeholder until implementation begins.
pub const SPEC_REFERENCE: &str = "spec/11_background_workers/";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_reference_is_set() {
        assert!(SPEC_REFERENCE.starts_with("spec/"));
    }
}
