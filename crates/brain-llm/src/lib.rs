//! # brain-llm
//!
//! LLM transport clients (Anthropic, OpenAI) for Brain's LLM
//! extractor tier. Spec §22/09.
//!
//! Phase 21.1 + 21.2 land:
//! - `LlmClient` trait (object-safe, returns boxed future).
//! - `LlmRequest` / `LlmResponse` / `LlmError`.
//! - `AnthropicClient` (api.anthropic.com Messages API).
//! - `OpenAIClient` (api.openai.com Chat Completions, with
//!   structured-output mode for schema-validated requests).
//! - `ModelRouter` with both providers wired.
//!
//! Phase 21.3 wires the LLM extractor in brain-extractors.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod anthropic;
pub mod client;
pub mod error;
pub mod openai;
pub mod router;
pub mod types;

pub use anthropic::AnthropicClient;
pub use client::LlmClient;
pub use error::LlmError;
pub use openai::OpenAIClient;
pub use router::{ModelRouter, Provider};
pub use types::{LlmMessage, LlmRequest, LlmResponse, LlmRole};
