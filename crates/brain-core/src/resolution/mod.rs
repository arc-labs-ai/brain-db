//! Entity resolution: turning a surface name into a canonical `EntityId`.
//!
//! The live write path resolves surface names in
//! `brain-extractors::resolver`; the pieces below are the shared,
//! storage-agnostic building blocks it (and the workers) reuse:
//!
//! - [`trigrams`] — n-gram extraction + Jaccard similarity.
//! - [`confidence`] — noisy-OR aggregation with kind-specific decay.
//! - [`referential`] — shared non-referential-surface backstop.

pub mod confidence;
pub mod referential;
pub mod trigrams;

pub use confidence::{aggregate_confidence, ConfidenceConfig};
pub use referential::is_non_referential_surface;
pub use trigrams::{extract_trigrams, jaccard};
