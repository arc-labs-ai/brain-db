//! Entity resolution: turning a surface name into a canonical `EntityId`.
//!
//! The resolver runs a tiered gauntlet — exact qname, alias, trigram
//! fuzzy match, then an HNSW embedding tie-break — and aggregates the
//! per-tier evidence into a confidence via noisy-OR. The supporting
//! pieces live alongside:
//!
//! - [`resolver`] — the gauntlet itself, `ResolutionOutcome`, config.
//! - [`trigrams`] — n-gram extraction + Jaccard similarity.
//! - [`confidence`] — noisy-OR aggregation with kind-specific decay.
//! - [`referential`] — shared non-referential-surface backstop.

pub mod confidence;
pub mod referential;
pub mod resolver;
pub mod trigrams;

pub use confidence::{aggregate_confidence, ConfidenceConfig};
pub use referential::is_non_referential_surface;
pub use resolver::{
    resolve_entity, ResolutionOutcome, ResolverConfig, ResolverEmbedder, ResolverError,
    ResolverIndex, ResolverLlm, ResolverLlmDecision, ResolverStorage, ResolverTier, TypeConstraint,
    VECTOR_DIM,
};
pub use trigrams::{extract_trigrams, jaccard};
