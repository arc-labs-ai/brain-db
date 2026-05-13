//! LLM-backed Summarizer adapters (sub-task 9.15).
//!
//! Spec §11/03 §6 — the consolidation worker needs an LLM to turn
//! a cluster of episodic memories into a single consolidated summary.
//! Brain doesn't bundle an LLM; this module hosts two feature-gated
//! HTTP adapters:
//!
//! - [`openai::OpenAiSummarizer`] (feature `summarizer-openai`)
//! - [`ollama::OllamaSummarizer`] (feature `summarizer-ollama`)
//!
//! Both call out to an external HTTPS endpoint. To avoid running
//! reqwest's async futures inside the per-shard Glommio executor
//! (where Tokio's reactor isn't registered), the adapters share a
//! dedicated bridge Tokio runtime ([`bridge::SummarizerBridge`]).
//! Glommio-side callers post requests through a `flume` channel and
//! await the response off the bridge's worker.
//!
//! See [`factory::build_summarizer`] for the config-driven
//! construction entry point.

#![cfg(target_os = "linux")]

pub(crate) mod prompt;

#[cfg(any(feature = "summarizer-openai", feature = "summarizer-ollama"))]
pub(crate) mod bridge;

#[cfg(feature = "summarizer-openai")]
pub(crate) mod openai;

#[cfg(feature = "summarizer-ollama")]
pub(crate) mod ollama;

pub(crate) mod factory;
