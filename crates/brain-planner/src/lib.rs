//! # brain-planner
//!
//! Logical plan tree, cost model, physical plan selection, and pull-based executor. Filter pushdown into the index where possible.
//!
//! See `spec/08_query_planner/` for the authoritative design.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

/// Crate-level marker. Placeholder until implementation begins.
pub const SPEC_REFERENCE: &str = "spec/08_query_planner/";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_reference_is_set() {
        assert!(SPEC_REFERENCE.starts_with("spec/"));
    }
}
