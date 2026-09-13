//! # brain-rerank
//!
//! Cross-encoder reranker for Brain's retrieval. Loads
//! `BAAI/bge-reranker-base` (a `BertForSequenceClassification`
//! with `num_labels=1`) via candle and scores `(query, candidate)`
//! pairs.
//!
//! The substrate calls [`CrossEncoder::score_pairs`] after RRF
//! fusion picks the top-N candidates: each pair's score is the
//! raw logit from the classification head — higher is more
//! relevant. The caller re-sorts the fused list by these scores
//! and truncates to `final_top_k`.
//!
//! The reranker is **optional**. When the model is unavailable
//! (no on-disk weights, unsupported device, etc.) the retrieval
//! pipeline gracefully falls back to RRF-only ranking and logs
//! the skip at `info` level.

#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]
#![forbid(unsafe_code)]

mod model;
mod service;

pub use model::{CrossEncoder, RerankError, DEFAULT_MAX_TOKEN_LEN};
pub use service::RerankService;

use std::env;
use std::path::PathBuf;

/// Default discovery name for the bge-reranker-base directory
/// under `$XDG_DATA_HOME/brain/models/`.
const MODEL_DIR_NAME: &str = "bge-reranker-base";

/// Environment override; when set, used verbatim.
const ENV_VAR: &str = "BRAIN_RERANK_MODEL_DIR";

/// Resolve the cross-encoder model directory using the same
/// strategy `brain-embed` uses for BGE-small and `brain-extractors`
/// uses for GLiNER: explicit env var → `$XDG_DATA_HOME` →
/// `$HOME/.local/share/brain/models/`.
///
/// Returns `None` only when neither env var nor `$HOME` is set
/// (vanishingly rare; CI / containers always set one).
#[must_use]
pub fn auto_discover_model_dir() -> Option<PathBuf> {
    if let Ok(explicit) = env::var(ENV_VAR) {
        let p = PathBuf::from(explicit);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    if let Ok(xdg) = env::var("XDG_DATA_HOME") {
        let mut p = PathBuf::from(xdg);
        p.push("brain");
        p.push("models");
        p.push(MODEL_DIR_NAME);
        return Some(p);
    }
    if let Ok(home) = env::var("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".local");
        p.push("share");
        p.push("brain");
        p.push("models");
        p.push(MODEL_DIR_NAME);
        return Some(p);
    }
    None
}

/// Best-effort load: returns `Ok(None)` (rather than an error) when
/// auto-discovery succeeds but the directory is empty / missing.
/// The retrieval pipeline treats `Ok(None)` as "reranker not
/// configured; fall back to RRF" and logs at `info`.
///
/// Hard errors (corrupt weights, unsupported device on an
/// existing directory) still propagate so operators see the cause.
pub fn try_load() -> Result<Option<CrossEncoder>, RerankError> {
    let Some(dir) = auto_discover_model_dir() else {
        return Ok(None);
    };
    if !dir.is_dir() {
        tracing::info!(
            target: "brain_rerank",
            model_dir = %dir.display(),
            "cross-encoder model directory not present; reranker disabled",
        );
        return Ok(None);
    }
    Ok(Some(CrossEncoder::load(&dir)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate the process-global `BRAIN_RERANK_MODEL_DIR`
    // env var, so they must not run concurrently — otherwise one
    // test's `remove_var` lets another fall through to XDG discovery
    // (which finds a real bootstrapped model in a dev container).
    // Serialise them through a module-local lock; recover from
    // poisoning so one panicking test doesn't cascade.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn auto_discover_returns_a_path_in_test_env() {
        let _env = env_guard();
        // CI / dev containers always set HOME at minimum.
        let path = auto_discover_model_dir();
        if env::var("HOME").is_ok() || env::var("XDG_DATA_HOME").is_ok() {
            assert!(path.is_some());
        }
    }

    #[test]
    fn auto_discover_honours_env_override() {
        let _env = env_guard();
        // Use a unique sentinel value so we don't clash with the
        // ambient env on whatever workstation this runs on.
        env::set_var(ENV_VAR, "/tmp/brain-rerank-test-override");
        let path = auto_discover_model_dir();
        env::remove_var(ENV_VAR);
        assert_eq!(path, Some(PathBuf::from("/tmp/brain-rerank-test-override")),);
    }

    #[test]
    fn try_load_returns_none_when_directory_missing() {
        let _env = env_guard();
        // Point at a path that definitely doesn't exist; expect
        // graceful disable (Ok(None)) not a hard error.
        env::set_var(ENV_VAR, "/tmp/nonexistent-brain-rerank-dir-xyzzy");
        let result = try_load();
        env::remove_var(ENV_VAR);
        assert!(matches!(result, Ok(None)));
    }
}
