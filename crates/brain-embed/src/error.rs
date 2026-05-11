//! Workspace-wide error type for `brain-embed`.
//!
//! Grown across sub-tasks. 5.1 adds the model-load variants; later
//! sub-tasks (tokenisation, forward pass, cache, batcher) extend.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("model path does not exist or is not a directory: {0}")]
    ModelPathInvalid(PathBuf),

    #[error("config.json missing or unreadable in {dir}: {source}")]
    ConfigRead {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("config.json failed to parse: {0}")]
    ConfigParse(String),

    #[error("tokenizer.json missing or unreadable in {dir}: {source}")]
    TokenizerRead {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("tokenizer.json failed to load: {0}")]
    TokenizerParse(String),

    /// Per spec `§04/03 §11` plus our SD-5.1-1: we refuse `pytorch_model.bin`
    /// outright (spec allows warn-and-load; we don't). `model.safetensors`
    /// is the only accepted weight format.
    #[error("model.safetensors missing in {0}; pickle (.bin) weights are refused")]
    WeightsMissing(PathBuf),

    #[error("weights load failed: {0}")]
    WeightsLoad(String),

    #[error("weights hash read failed: {0}")]
    WeightsHash(#[source] std::io::Error),

    #[error("unsupported device (v1 is CPU-only)")]
    UnsupportedDevice,

    #[error("warm-up inference failed: {0}")]
    WarmupFailed(String),
}
