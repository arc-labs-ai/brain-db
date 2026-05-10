//! # brain-index
//!
//! HNSW index for approximate nearest neighbour search. Wraps hnsw_rs with the parameters and lifecycle (build, search, snapshot, rebuild) defined in the spec.
//!
//! See `spec/06_ann_index/` for the authoritative design.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

/// Crate-level marker. Placeholder until implementation begins.
pub const SPEC_REFERENCE: &str = "spec/06_ann_index/";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_reference_is_set() {
        assert!(SPEC_REFERENCE.starts_with("spec/"));
    }
}
