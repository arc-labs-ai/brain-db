//! `build_summarizer(&Config) -> Arc<dyn Summarizer>`.
//!
//! Conditional on Cargo features:
//!
//! - `backend == "disabled"` → `Arc::new(DisabledSummarizer)`.
//! - `backend == "llm"`, feature on → `Arc::new(ResolvedSummarizer::new(...))`,
//!   which bills each consolidation to the namespace it consolidates.
//! - `backend == "ollama"`, feature on → `Arc::new(OllamaSummarizer::new(...))`.
//! - either backend with its feature *off* → startup error.
//!
//! Startup errors are surfaced as `BuildSummarizerError` so
//! `linux_main::run` can log + return `ExitCode::FAILURE`.

#![cfg(target_os = "linux")]

use std::sync::Arc;
#[cfg(any(feature = "summarizer-llm", feature = "summarizer-ollama"))]
use std::time::Duration;

use brain_workers::{DisabledSummarizer, Summarizer};
use thiserror::Error;

use crate::config::{Config, SummarizerBackend};

#[derive(Debug, Error)]
#[allow(dead_code)] // some variants only constructed under feature gates
pub(crate) enum BuildSummarizerError {
    #[error(
        "summarizer.backend = \"llm\" but the `summarizer-llm` Cargo feature \
         is not enabled in this build"
    )]
    LlmFeatureMissing,
    #[error(
        "summarizer.backend = \"ollama\" but the `summarizer-ollama` Cargo feature \
         is not enabled in this build"
    )]
    OllamaFeatureMissing,
    #[error(
        "summarizer.backend = \"llm\" needs a credential store to resolve each \
         namespace's key from, and this deployment has none"
    )]
    NoCredentialStore,
    #[error("summarizer bridge runtime initialisation failed: {0}")]
    BridgeInit(#[from] std::io::Error),
}

/// Build the configured Summarizer. Returns `Arc<dyn Summarizer>` so
/// it slots directly into `ShardSpawnConfig` / `register_phase8_workers`.
/// `resolver` is the same per-namespace credential resolver every other
/// model-backed stage uses. `None` leaves only the credential-free backends
/// selectable, which is what a deployment with no credential store has.
pub(crate) fn build_summarizer(
    cfg: &Config,
    resolver: Option<Arc<dyn brain_llm::LlmCredentialResolver>>,
) -> Result<Arc<dyn Summarizer>, BuildSummarizerError> {
    match cfg.summarizer.backend {
        SummarizerBackend::Disabled => Ok(Arc::new(DisabledSummarizer)),
        SummarizerBackend::Llm => build_llm(cfg, resolver),
        SummarizerBackend::Ollama => build_ollama(cfg),
    }
}

// --- the namespace's own credential ---------------------------------

#[cfg(feature = "summarizer-llm")]
fn build_llm(
    cfg: &Config,
    resolver: Option<Arc<dyn brain_llm::LlmCredentialResolver>>,
) -> Result<Arc<dyn Summarizer>, BuildSummarizerError> {
    let resolver = resolver.ok_or(BuildSummarizerError::NoCredentialStore)?;
    Ok(Arc::new(crate::llm::resolved::ResolvedSummarizer::new(
        resolver,
        cfg.summarizer.temperature,
        cfg.summarizer.max_summary_chars,
        Duration::from_secs(u64::from(cfg.summarizer.request_timeout_sec)),
    )))
}

#[cfg(not(feature = "summarizer-llm"))]
fn build_llm(
    _cfg: &Config,
    _resolver: Option<Arc<dyn brain_llm::LlmCredentialResolver>>,
) -> Result<Arc<dyn Summarizer>, BuildSummarizerError> {
    Err(BuildSummarizerError::LlmFeatureMissing)
}

// --- Ollama ---------------------------------------------------------

#[cfg(feature = "summarizer-ollama")]
fn build_ollama(cfg: &Config) -> Result<Arc<dyn Summarizer>, BuildSummarizerError> {
    let bridge = crate::llm::bridge::SummarizerBridge::new(Duration::from_secs(u64::from(
        cfg.summarizer.request_timeout_sec,
    )))?;
    Ok(Arc::new(crate::llm::ollama::OllamaSummarizer::new(
        cfg.summarizer.ollama_base.clone(),
        cfg.summarizer.ollama_model.clone(),
        cfg.summarizer.temperature,
        bridge,
    )))
}

#[cfg(not(feature = "summarizer-ollama"))]
fn build_ollama(_cfg: &Config) -> Result<Arc<dyn Summarizer>, BuildSummarizerError> {
    Err(BuildSummarizerError::OllamaFeatureMissing)
}
