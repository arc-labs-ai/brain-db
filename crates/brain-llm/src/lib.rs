//! # brain-llm
//!
//! LLM transport clients for Brain's LLM extractor tier and supersession
//! judge.
//!
//! ## Module map
//!
//! - [`types`] — provider-agnostic `LlmMessage` / `LlmRequest` /
//!   `LlmResponse` / `LlmRole` / `SystemBlock`.
//! - [`client`] — `LlmClient` trait + `LlmFuture`.
//! - [`error`] — `LlmError` taxonomy.
//! - [`router`] — `ModelRouter` + `Provider` (maps model id → provider).
//! - [`providers`] — concrete impls (`AnthropicClient`, `OpenAIClient`).

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod client;
pub mod error;
pub mod providers;
pub mod router;
pub mod types;

pub use client::LlmClient;
pub use error::{FailureClass, LlmError};
pub use providers::{AnthropicClient, OpenAIClient};
pub use router::{ModelRouter, Provider};
pub use types::{LlmMessage, LlmRequest, LlmResponse, LlmRole};
