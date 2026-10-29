//! Concrete LLM provider implementations.
//!
//! Each provider implements [`crate::client::LlmClient`] over its
//! native HTTP API. The provider-agnostic request / response shapes
//! live in [`crate::types`]; conversion to and from each provider's
//! wire format lives in this module.

pub mod anthropic;
pub mod openai;
pub(crate) mod runtime;

pub use anthropic::AnthropicClient;
pub use openai::OpenAIClient;
