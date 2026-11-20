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
//! - [`admission`] — the one byte budget derivation runs inside, drawn on by
//!   both provider payloads and the preparation stage that feeds them.
//! - [`inflight`] — what that budget is currently spent on, metered apart by
//!   shape.
//! - [`prep`] — `PrepWorkload` + `prep_bytes`: what a batch's preparation
//!   stage will hold, described in counts and lengths so the estimate does not
//!   drag the pipeline's types into this crate.
//! - [`pricing`] — `ModelPricing`: the published per-model provider rates, and
//!   the one place a billed call becomes a dollar figure.
//! - [`router`] — `ModelRouter` + `Provider` (maps model id → provider).
//! - [`resolver`] — `LlmCredentialResolver` (maps namespace → credential),
//!   plus `CredentialScope` for cache-key isolation. The production
//!   implementation lives in `brain-server`, over Brain's own credential
//!   store; the two resolvers here serve one client to every namespace and
//!   exist for tests and for embedding a hand-built client.
//! - [`providers`] — concrete impls (`AnthropicClient`, `OpenAIClient`),
//!   plus [`providers::sizing`], which sizes the one runtime their HTTP runs
//!   on against the cores the shard tier left.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod admission;
pub mod client;
pub mod error;
pub mod inflight;
pub mod prep;
pub mod pricing;
pub mod providers;
pub mod resolver;
pub mod router;
pub mod types;

pub use client::LlmClient;
pub use error::{FailureClass, LlmError};
pub use pricing::ModelPricing;
pub use providers::sizing::reserve_for_shards;
pub use providers::{AnthropicClient, OpenAIClient};
pub use resolver::{
    CredentialFuture, CredentialOutcome, CredentialScope, FixedResolver, LlmCredentialResolver,
    ResolvedCredential, RouterResolver,
};
pub use router::{ModelRouter, Provider};
pub use types::{LlmMessage, LlmRequest, LlmResponse, LlmRole};
