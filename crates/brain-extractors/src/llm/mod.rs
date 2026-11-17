//! LLM extractor — Tier 3.
//!
//! Wraps an [`brain_llm::LlmClient`] (Anthropic / OpenAI) behind the
//! [`crate::framework::extractor::Extractor`] trait with a per-shard
//! response cache, JSON-schema validation + retry, and projection to
//! `ExtractedItem`s.
//!
//! Provider spend for a call lands on the returned
//! [`crate::framework::extractor::ExtractionResult::cost_micro_usd`], which the
//! worker writes to the extraction audit row. That is one of Brain's two
//! durable spend stores; the other is the `llm_spend` ledger, which covers the
//! model-backed stages that produce no audit row. See
//! [`brain_llm::pricing`] for the contract between them.
//!
//! ## Submodules
//!
//! - [`extractor`] — `LlmExtractor`, `LlmExtractorInner`,
//!   `BuildRequestStats`, and the `Extractor` trait impl.
//! - [`cache`] — idempotency cache helpers over `LlmCacheDb`.
//! - [`validation`] — JSON-schema validation helper.

pub mod cache;
pub mod extractor;
pub mod validation;

#[cfg(test)]
mod tests;

pub use extractor::{BuildRequestStats, LlmExtractor, LlmExtractorInner, DEFAULT_LLM_MAX_TOKENS};
