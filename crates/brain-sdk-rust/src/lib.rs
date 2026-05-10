//! # brain-sdk-rust
//!
//! Idiomatic async Rust SDK. Connection pool, retry with exponential backoff plus jitter, auto-generated UUIDv7 RequestIds, streaming via async iterators with backpressure.
//!
//! See `spec/13_sdk_design/` for the authoritative design.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

/// Crate-level marker. Placeholder until implementation begins.
pub const SPEC_REFERENCE: &str = "spec/13_sdk_design/";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_reference_is_set() {
        assert!(SPEC_REFERENCE.starts_with("spec/"));
    }
}
