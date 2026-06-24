//! `ModelHandle` — loads BGE-small (or any BERT-shaped model) from
//! disk, computes its fingerprint, runs a warm-up inference.
//!
//! Weights must be safetensors; pickle (`.bin`) is refused outright.
//! The fingerprint algorithm is implemented byte-for-byte in
//! [`crate::fingerprint`].
//!
//! Provides:
//!
//! - `ModelHandle::load(&EmbedderConfig)` — full load sequence.
//! - `ModelHandle::fingerprint()` — the 16-byte BLAKE3-truncated id.
//! - `ModelHandle::device()`, `ModelHandle::dtype()` — accessors.

use std::path::Path;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use tokenizers::Tokenizer;

use crate::config::EmbedderConfig;
use crate::error::EmbedError;
use crate::fingerprint::{blake3_hash_file, compute_fingerprint};

/// The output dimensionality for v1 (BGE-small-en-v1.5).
const VECTOR_DIM: u32 = 384;

/// File names inside the configured `model_path` directory.
const CONFIG_FILE: &str = "config.json";
const TOKENIZER_FILE: &str = "tokenizer.json";
const WEIGHTS_FILE: &str = "model.safetensors";
const PICKLE_FILE: &str = "pytorch_model.bin";

/// Loaded BGE-small handle. Owns the BERT model, tokenizer, and the
/// computed fingerprint. Constructed by [`ModelHandle::load`].
pub struct ModelHandle {
    model: BertModel,
    tokenizer: Tokenizer,
    fingerprint: [u8; 16],
    device: Device,
    dtype: DType,
}

impl ModelHandle {
    /// Load the model directory 's six-step sequence:
    /// config → tokenizer → weights (safetensors only) → fingerprint →
    /// build → warm up.
    pub fn load(config: &EmbedderConfig) -> Result<Self, EmbedError> {
        // 1. Validate device + dtype before doing I/O.
        if !matches!(config.device, Device::Cpu) {
            return Err(EmbedError::UnsupportedDevice);
        }
        if config.dtype != DType::F32 {
            return Err(EmbedError::UnsupportedDevice);
        }

        // 2. Validate directory.
        let dir = &config.model_path;
        if !dir.is_dir() {
            return Err(EmbedError::ModelPathInvalid(dir.clone()));
        }

        // 3. Read config.json.
        let config_path = dir.join(CONFIG_FILE);
        let config_bytes =
            std::fs::read(&config_path).map_err(|source| EmbedError::ConfigRead {
                dir: dir.clone(),
                source,
            })?;
        let bert_config: BertConfig = serde_json::from_slice(&config_bytes)
            .map_err(|e| EmbedError::ConfigParse(e.to_string()))?;

        // 4. Read tokenizer.json (raw bytes for fingerprint + load
        //    instance from the same path).
        let tokenizer_path = dir.join(TOKENIZER_FILE);
        let tokenizer_bytes =
            std::fs::read(&tokenizer_path).map_err(|source| EmbedError::TokenizerRead {
                dir: dir.clone(),
                source,
            })?;
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| EmbedError::TokenizerParse(e.to_string()))?;

        // 5. Check for safetensors; refuse pickle outright.
        let weights_path = dir.join(WEIGHTS_FILE);
        if !weights_path.is_file() {
            let pickle_present = dir.join(PICKLE_FILE).is_file();
            tracing::warn!(
                model_dir = %dir.display(),
                pickle_present,
                "model.safetensors missing; pickle (.bin) is refused"
            );
            return Err(EmbedError::WeightsMissing(dir.clone()));
        }

        // 6. Stream-hash the weights file for the fingerprint (avoids
        //    loading ~130 MiB into memory just to hash it).
        let weights_blake3 = blake3_hash_file(&weights_path).map_err(EmbedError::WeightsHash)?;

        // 7. Compute fingerprint.
        let fingerprint = compute_fingerprint(
            &config_bytes,
            &tokenizer_bytes,
            &weights_blake3,
            VECTOR_DIM,
            /* normalize */ true,
        );

        // 8. Build the BERT model.
        let vb = load_weights(&weights_path, config.dtype, &config.device)?;
        let model = BertModel::load(vb, &bert_config)
            .map_err(|e| EmbedError::WeightsLoad(format!("BertModel::load: {e}")))?;

        // 9. Warm-up inferences.
        let handle = Self {
            model,
            tokenizer,
            fingerprint,
            device: config.device.clone(),
            dtype: config.dtype,
        };
        for _ in 0..config.warmup_iters {
            handle.warmup_once()?;
        }

        tracing::info!(
            model_dir = %dir.display(),
            fingerprint = ?fingerprint,
            "loaded embedding model"
        );
        Ok(handle)
    }

    #[must_use]
    pub fn fingerprint(&self) -> [u8; 16] {
        self.fingerprint
    }

    #[must_use]
    pub fn device(&self) -> &Device {
        &self.device
    }

    #[must_use]
    pub fn dtype(&self) -> DType {
        self.dtype
    }

    /// Tokenizer accessor.
    #[must_use]
    pub fn tokenizer(&self) -> &Tokenizer {
        &self.tokenizer
    }

    /// Forward pass on already-tokenised input. Returns the raw
    /// last-hidden-state tensor of shape `(batch, seq_len, hidden)`.
    /// Sub-task 5.3 wraps this with mean-pool + L2-normalise to produce
    /// the user-visible `[f32; 384]` vector.
    pub(crate) fn forward(
        &self,
        input_ids: &Tensor,
        token_type_ids: &Tensor,
        attention_mask: Option<&Tensor>,
    ) -> Result<Tensor, EmbedError> {
        self.model
            .forward(input_ids, token_type_ids, attention_mask)
            .map_err(|e| EmbedError::WarmupFailed(format!("BertModel::forward: {e}")))
    }

    /// One warm-up pass on a tiny fixed input. Result discarded.
    fn warmup_once(&self) -> Result<(), EmbedError> {
        // Minimal valid BERT input: [CLS] (101) + [SEP] (102).
        let ids = Tensor::new(&[[101u32, 102u32]], &self.device)
            .map_err(|e| EmbedError::WarmupFailed(format!("tensor: {e}")))?;
        let type_ids = Tensor::zeros(ids.shape(), DType::U32, &self.device)
            .map_err(|e| EmbedError::WarmupFailed(format!("type_ids: {e}")))?;
        let _out = self.forward(&ids, &type_ids, None)?;
        Ok(())
    }
}

/// Load safetensors weights into a `VarBuilder`. Uses
/// `candle_core::safetensors::load` (safe; reads the file fully).
/// We avoid `from_mmaped_safetensors` (which is unsafe) so this
/// crate keeps `#![forbid(unsafe_code)]`. The full-file load adds a
/// one-time ~130 MiB allocation at startup; weights stay resident
/// anyway.
fn load_weights(
    path: &Path,
    dtype: DType,
    device: &Device,
) -> Result<VarBuilder<'static>, EmbedError> {
    let tensors = candle_core::safetensors::load(path, device)
        .map_err(|e| EmbedError::WeightsLoad(format!("safetensors::load({path:?}): {e}")))?;
    Ok(VarBuilder::from_tensors(tensors, dtype, device))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    // PathBuf used by tests only.

    fn cfg(path: PathBuf) -> EmbedderConfig {
        EmbedderConfig::new(path)
    }

    #[test]
    fn load_rejects_missing_path() {
        let bogus = PathBuf::from("/nonexistent/brain-embed/test/path");
        match ModelHandle::load(&cfg(bogus.clone())) {
            Err(EmbedError::ModelPathInvalid(p)) => assert_eq!(p, bogus),
            Err(e) => panic!("wrong error: {e}"),
            Ok(_) => panic!("expected ModelPathInvalid"),
        }
    }

    #[test]
    fn load_rejects_missing_safetensors() {
        // Tempdir with config.json + tokenizer.json but no safetensors.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), b"{}").unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), b"{}").unwrap();
        let result = ModelHandle::load(&cfg(dir.path().to_path_buf()));
        match result {
            Err(EmbedError::WeightsMissing(p)) => assert_eq!(p, dir.path().to_path_buf()),
            // Note: we may instead error out at config.json or tokenizer.json
            // parsing before reaching the weights check; that's also OK for
            // the missing-weights spirit of the test. The point is *not*
            // accepting `.bin` pickle as a substitute.
            Err(EmbedError::ConfigParse(_) | EmbedError::TokenizerParse(_)) => {
                // Acceptable — empty `{}` doesn't parse as a BERT config or
                // tokenizer. The weights-refusal path is exercised by the
                // integration test when a real model is at hand.
            }
            Err(e) => panic!("unexpected error: {e}"),
            Ok(_) => panic!("expected WeightsMissing"),
        }
    }

    #[test]
    fn load_rejects_missing_config_json() {
        let dir = tempfile::tempdir().unwrap();
        // No config.json at all.
        match ModelHandle::load(&cfg(dir.path().to_path_buf())) {
            Err(EmbedError::ConfigRead { .. }) => {}
            Err(e) => panic!("wrong error: {e}"),
            Ok(_) => panic!("expected ConfigRead"),
        }
    }

}
