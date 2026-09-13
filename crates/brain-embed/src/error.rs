//! Workspace-wide error type for `brain-embed`.
//!
//! Covers the model-load, tokenisation, forward-pass, cache, and
//! batcher failure modes.

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

    /// We refuse `pytorch_model.bin` outright. `model.safetensors`
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

    /// Tokeniser failed to encode (vocab missing, internal error, etc.).
    #[error("tokenisation failed: {0}")]
    TokenizationFailed(String),

    /// Building one of the BERT input tensors from token ids failed.
    /// Distinct from `WarmupFailed` because the failure is at tensor
    /// construction time, not during the forward pass.
    #[error("tensor build failed: {0}")]
    TensorBuild(String),

    /// The forward pass returned a pathological vector: NaN, Inf, or
    /// a near-zero norm mandate rejection.
    #[error("numeric failure in embedding output: {0}")]
    NumericFailure(String),

    /// Model output dimension did not match `VECTOR_DIM` (384 for v1).
    /// Almost always means the operator pointed `model_path` at a
    /// non-BGE-small model.
    #[error("model output dim mismatch: expected {expected}, got {got}")]
    OutputDimMismatch { expected: usize, got: usize },
}
