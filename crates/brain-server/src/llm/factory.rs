//! `build_summarizer(&Config) -> Arc<dyn Summarizer>` (sub-task 9.15).
//!
//! Conditional on Cargo features:
//!
//! - `backend == "disabled"` → `Arc::new(DisabledSummarizer)`.
//! - `backend == "openai"`, feature on → `Arc::new(OpenAiSummarizer::new(...))`.
//! - `backend == "ollama"`, feature on → `Arc::new(OllamaSummarizer::new(...))`.
//! - `backend == "openai"`, feature *off* → startup error.
//! - `backend == "ollama"`, feature *off* → startup error.
//!
//! Startup errors are surfaced as `BuildSummarizerError` so
//! `linux_main::run` can log + return `ExitCode::FAILURE`.

#![cfg(target_os = "linux")]

use std::sync::Arc;
#[cfg(any(feature = "summarizer-openai", feature = "summarizer-ollama"))]
use std::time::Duration;

use brain_workers::{DisabledSummarizer, Summarizer};
use thiserror::Error;

use crate::config::{Config, SummarizerBackend};

#[derive(Debug, Error)]
#[allow(dead_code)] // some variants only constructed under feature gates
pub(crate) enum BuildSummarizerError {
    #[error(
        "summarizer.backend = \"openai\" but the `summarizer-openai` Cargo feature \
         is not enabled in this build"
    )]
    OpenAiFeatureMissing,
    #[error(
        "summarizer.backend = \"ollama\" but the `summarizer-ollama` Cargo feature \
         is not enabled in this build"
    )]
    OllamaFeatureMissing,
    #[error(
        "summarizer.backend = \"openai\" requires summarizer.openai_api_key_env to \
         name an environment variable holding the API key"
    )]
    OpenAiKeyEnvUnset,
    #[error(
        "summarizer.openai_api_key_env = {env_var:?}, but that environment variable \
         is not set or is empty"
    )]
    OpenAiKeyMissing { env_var: String },
    #[error("summarizer bridge runtime initialisation failed: {0}")]
    BridgeInit(#[from] std::io::Error),
}

/// Build the configured Summarizer. Returns `Arc<dyn Summarizer>` so
/// it slots directly into `ShardSpawnConfig` / `register_phase8_workers`.
pub(crate) fn build_summarizer(cfg: &Config) -> Result<Arc<dyn Summarizer>, BuildSummarizerError> {
    match cfg.summarizer.backend {
        SummarizerBackend::Disabled => Ok(Arc::new(DisabledSummarizer)),
        SummarizerBackend::Openai => build_openai(cfg),
        SummarizerBackend::Ollama => build_ollama(cfg),
    }
}

// --- OpenAI ---------------------------------------------------------

#[cfg(feature = "summarizer-openai")]
fn build_openai(cfg: &Config) -> Result<Arc<dyn Summarizer>, BuildSummarizerError> {
    let env_var = cfg
        .summarizer
        .openai_api_key_env
        .as_deref()
        .ok_or(BuildSummarizerError::OpenAiKeyEnvUnset)?;
    let api_key = std::env::var(env_var)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| BuildSummarizerError::OpenAiKeyMissing {
            env_var: env_var.to_owned(),
        })?;
    let bridge = crate::llm::bridge::SummarizerBridge::new(Duration::from_secs(u64::from(
        cfg.summarizer.request_timeout_sec,
    )))?;
    Ok(Arc::new(crate::llm::openai::OpenAiSummarizer::new(
        cfg.summarizer.openai_api_base.clone(),
        api_key,
        cfg.summarizer.openai_model.clone(),
        cfg.summarizer.openai_temperature,
        cfg.summarizer.max_summary_chars,
        bridge,
    )))
}

#[cfg(not(feature = "summarizer-openai"))]
fn build_openai(_cfg: &Config) -> Result<Arc<dyn Summarizer>, BuildSummarizerError> {
    Err(BuildSummarizerError::OpenAiFeatureMissing)
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
        cfg.summarizer.openai_temperature, // shared with OpenAI
        bridge,
    )))
}

#[cfg(not(feature = "summarizer-ollama"))]
fn build_ollama(_cfg: &Config) -> Result<Arc<dyn Summarizer>, BuildSummarizerError> {
    Err(BuildSummarizerError::OllamaFeatureMissing)
}
