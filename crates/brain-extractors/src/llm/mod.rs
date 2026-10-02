//! LLM extractor — Tier 3.
//!
//! Wraps an [`brain_llm::LlmClient`] (Anthropic / OpenAI) behind the
//! [`crate::framework::extractor::Extractor`] trait with a per-shard
//! response cache, per-call cost budget, JSON-schema validation +
//! retry, and projection to `ExtractedItem`s.
//!
//! ## Submodules
//!
//! - [`pricing`] — `CostBudget`, `Pricing`, `estimate_cost`.
//! - [`extractor`] — `LlmExtractor`, `LlmExtractorInner`,
//!   `BuildRequestStats`, and the `Extractor` trait impl.
//! - [`cache`] — idempotency cache helpers over `LlmCacheDb`.
//! - [`validation`] — JSON-schema validation helper.

pub mod cache;
pub mod extractor;
pub mod pricing;
pub mod validation;

#[cfg(test)]
mod tests;

pub use extractor::{
    BuildRequestStats, LlmExtractor, LlmExtractorInner, DEFAULT_LLM_MAX_TOKENS,
};
pub use pricing::{estimate_cost, CostBudget, Pricing};
