//! `CrossEncoder` — load `BAAI/bge-reranker-base` and score
//! `(query, candidate)` pairs.
//!
//! Architecture: `XLMRobertaForSequenceClassification` with
//! `num_labels = 1`. bge-reranker-base is XLM-RoBERTa, not BERT:
//! the weights are prefixed `roberta.` and the relevance head is a
//! two-layer `RobertaClassificationHead` (`classifier.dense` →
//! activation → `classifier.out_proj`) applied to the `<s>` token,
//! with no BERT-style pooler. The reranker concatenates a query and
//! a candidate as `<s> query </s></s> candidate </s>`, encodes, then
//! the head projects the `<s>` hidden state to a single logit. That
//! raw logit *is* the relevance score — higher means more relevant.
//!
//! We delegate the whole forward to candle's
//! [`XLMRobertaForSequenceClassification`], which owns the backbone
//! and the classification head; loading and scoring stay thin.

use std::path::Path;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::xlm_roberta::{
    Config as XlmRobertaConfig, XLMRobertaForSequenceClassification,
};
use thiserror::Error;
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};

/// File names inside the model directory. Same convention as
/// `brain-embed` for BGE-small.
const CONFIG_FILE: &str = "config.json";
const TOKENIZER_FILE: &str = "tokenizer.json";
const WEIGHTS_FILE: &str = "model.safetensors";

/// Default per-pair token cap. bge-reranker-base was trained
/// with a 512-token cap; we mirror that.
pub const DEFAULT_MAX_TOKEN_LEN: usize = 512;

/// Errors raised by the cross-encoder loader / scorer. Hot-path
/// callers (the retrieval executor) downgrade `Skipped` returns to
/// "RRF-only result" with a single `info` log.
#[derive(Debug, Error)]
pub enum RerankError {
    #[error("model path does not exist or is not a directory: {0}")]
    ModelPathInvalid(std::path::PathBuf),

    #[error("config.json missing or unreadable in {dir}: {source}")]
    ConfigRead {
        dir: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("config.json failed to parse: {0}")]
    ConfigParse(String),

    #[error("tokenizer.json failed to load: {0}")]
    TokenizerParse(String),

    #[error("model.safetensors missing in {0}; pickle (.bin) weights are refused")]
    WeightsMissing(std::path::PathBuf),

    #[error("weights load failed: {0}")]
    WeightsLoad(String),

    #[error("tokenisation failed: {0}")]
    TokenizationFailed(String),

    #[error("forward pass failed: {0}")]
    ForwardFailed(String),

    #[error("rerank score had unexpected shape: {0}")]
    BadShape(String),

    #[error("rerank service thread is unavailable (shut down or panicked)")]
    ServiceUnavailable,
}

/// Loaded cross-encoder. Owns the XLM-RoBERTa sequence-classifier
/// (backbone + relevance head), the tokenizer, and the target
/// device.
pub struct CrossEncoder {
    model: XLMRobertaForSequenceClassification,
    tokenizer: Tokenizer,
    device: Device,
    max_len: usize,
}

impl CrossEncoder {
    /// Load the model directory. Mirrors the six-step sequence
    /// used by `brain-embed` for BGE-small.
    pub fn load(dir: &Path) -> Result<Self, RerankError> {
        if !dir.is_dir() {
            return Err(RerankError::ModelPathInvalid(dir.to_path_buf()));
        }

        let config_path = dir.join(CONFIG_FILE);
        let config_bytes =
            std::fs::read(&config_path).map_err(|source| RerankError::ConfigRead {
                dir: dir.to_path_buf(),
                source,
            })?;
        let model_config: XlmRobertaConfig = serde_json::from_slice(&config_bytes)
            .map_err(|e| RerankError::ConfigParse(e.to_string()))?;

        let tokenizer_path = dir.join(TOKENIZER_FILE);
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| RerankError::TokenizerParse(e.to_string()))?;

        // Pad to the longest item in a batch; truncate to model max.
        // XLM-RoBERTa reserves two position slots for the padding
        // offset (`max_position_embeddings=514`), so cap token length
        // at the smaller of that and [`DEFAULT_MAX_TOKEN_LEN`] (512) —
        // the offset is applied inside the embeddings, so 512 real
        // tokens stay in-bounds.
        let max_len = std::cmp::min(model_config.max_position_embeddings, DEFAULT_MAX_TOKEN_LEN);
        let (pad_id, pad_token) = tokenizer
            .get_padding()
            .map(|p| (p.pad_id, p.pad_token.clone()))
            .unwrap_or((0, "[PAD]".to_string()));
        tokenizer.with_padding(Some(PaddingParams {
            strategy: tokenizers::PaddingStrategy::BatchLongest,
            direction: tokenizers::PaddingDirection::Right,
            pad_to_multiple_of: None,
            pad_id,
            pad_type_id: 0,
            pad_token,
        }));
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: max_len,
                strategy: tokenizers::TruncationStrategy::OnlyFirst,
                stride: 0,
                direction: tokenizers::TruncationDirection::Right,
            }))
            .map_err(|e| RerankError::TokenizerParse(e.to_string()))?;

        let weights_path = dir.join(WEIGHTS_FILE);
        if !weights_path.is_file() {
            return Err(RerankError::WeightsMissing(dir.to_path_buf()));
        }

        let device = Device::Cpu;
        let dtype = DType::F32;

        let tensors = candle_core::safetensors::load(&weights_path, &device).map_err(|e| {
            RerankError::WeightsLoad(format!("safetensors::load({weights_path:?}): {e}"))
        })?;
        let vb = VarBuilder::from_tensors(tensors, dtype, &device);

        // The classifier wraps the backbone (`roberta.*`) and the
        // relevance head (`classifier.dense` / `classifier.out_proj`)
        // from the root `vb`; `num_labels = 1` for binary relevance.
        let model =
            XLMRobertaForSequenceClassification::new(1, &model_config, vb).map_err(|e| {
                RerankError::WeightsLoad(format!("XLMRobertaForSequenceClassification::new: {e}"))
            })?;

        tracing::info!(
            target: "brain_rerank",
            model_dir = %dir.display(),
            "loaded cross-encoder",
        );
        Ok(Self {
            model,
            tokenizer,
            device,
            max_len,
        })
    }

    /// Score `(query, candidate)` pairs. Returns one logit per
    /// candidate, in the same order as input. Higher = more
    /// relevant.
    ///
    /// Empty `candidates` returns an empty `Vec` with zero
    /// allocations on the hot path; callers should still check
    /// before calling to avoid a needless tokenizer trip.
    pub fn score_pairs(&self, query: &str, candidates: &[&str]) -> Result<Vec<f32>, RerankError> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        // Build `(query, candidate)` pairs. bge-reranker's tokenizer
        // injects `<s> query </s></s> candidate </s>`; XLM-RoBERTa
        // uses a single token-type (`type_vocab_size = 1`), so the
        // type ids are all zero.
        let pairs: Vec<(String, String)> = candidates
            .iter()
            .map(|c| (query.to_string(), (*c).to_string()))
            .collect();

        let encoded = self
            .tokenizer
            .encode_batch(pairs, true)
            .map_err(|e| RerankError::TokenizationFailed(e.to_string()))?;

        let batch_size = encoded.len();
        let seq_len = encoded
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .ok_or_else(|| RerankError::TokenizationFailed("empty batch".into()))?;

        let mut input_ids = Vec::with_capacity(batch_size * seq_len);
        let mut type_ids = Vec::with_capacity(batch_size * seq_len);
        let mut attn_mask = Vec::with_capacity(batch_size * seq_len);
        for e in &encoded {
            input_ids.extend_from_slice(e.get_ids());
            type_ids.extend_from_slice(e.get_type_ids());
            attn_mask.extend_from_slice(e.get_attention_mask());
        }

        let input_ids = Tensor::from_vec(input_ids, (batch_size, seq_len), &self.device)
            .map_err(|e| RerankError::ForwardFailed(format!("input_ids tensor: {e}")))?;
        let type_ids = Tensor::from_vec(type_ids, (batch_size, seq_len), &self.device)
            .map_err(|e| RerankError::ForwardFailed(format!("type_ids tensor: {e}")))?;
        let attn_mask = Tensor::from_vec(attn_mask, (batch_size, seq_len), &self.device)
            .map_err(|e| RerankError::ForwardFailed(format!("attn_mask tensor: {e}")))?;

        // candle's classifier takes (input_ids, attention_mask,
        // token_type_ids) — note the arg order differs from BERT — and
        // returns the head logits directly: it pools the `<s>` token,
        // runs `dense → activation → out_proj`, so no manual pooling
        // happens here.
        let logits = self
            .model
            .forward(&input_ids, &attn_mask, &type_ids)
            .map_err(|e| {
                RerankError::ForwardFailed(format!(
                    "XLMRobertaForSequenceClassification::forward: {e}"
                ))
            })?;

        // logits shape: (batch, 1). Squeeze the last dim and pull to host.
        let scores: Vec<f32> = logits
            .squeeze(1)
            .and_then(|t| t.to_vec1::<f32>())
            .map_err(|e| RerankError::BadShape(format!("logits to_vec1: {e}")))?;

        if scores.len() != batch_size {
            return Err(RerankError::BadShape(format!(
                "expected {batch_size} scores, got {}",
                scores.len()
            )));
        }
        Ok(scores)
    }

    /// Per-pair token cap used by this loader. Useful for tests
    /// and diagnostics; the cap is also enforced by the tokenizer's
    /// truncation params set at load time.
    #[must_use]
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    /// Device the model runs on. Always `Cpu` in v1.
    #[must_use]
    pub fn device(&self) -> &Device {
        &self.device
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_rejects_missing_dir() {
        let bogus = std::path::PathBuf::from("/nonexistent/brain-rerank/test/path");
        match CrossEncoder::load(&bogus) {
            Err(RerankError::ModelPathInvalid(p)) => assert_eq!(p, bogus),
            Err(e) => panic!("wrong error: {e}"),
            Ok(_) => panic!("expected ModelPathInvalid"),
        }
    }

    #[test]
    fn load_rejects_empty_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        match CrossEncoder::load(dir.path()) {
            // No config.json → ConfigRead.
            Err(RerankError::ConfigRead { .. }) => {}
            Err(e) => panic!("wrong error: {e}"),
            Ok(_) => panic!("expected ConfigRead"),
        }
    }

    /// Smoke test that the API shape is correct. Gated behind a
    /// real model on disk via `BRAIN_RERANK_MODEL_DIR`; ignored by
    /// default so unit tests don't depend on the model bootstrap.
    #[test]
    #[ignore = "requires BRAIN_RERANK_MODEL_DIR to point at a real bge-reranker-base checkout"]
    fn score_pairs_returns_score_per_candidate() {
        let dir = std::env::var("BRAIN_RERANK_MODEL_DIR")
            .expect("BRAIN_RERANK_MODEL_DIR must point at the model directory");
        let enc = CrossEncoder::load(std::path::Path::new(&dir)).expect("load cross-encoder");
        let q = "where does Alice work?";
        let cands = ["Alice works at Stripe.", "the weather is nice today."];
        let scores = enc.score_pairs(q, &cands).expect("score pairs");
        assert_eq!(scores.len(), cands.len(), "one score per candidate");
    }

    /// End-to-end relevance check: the relevant candidate scores
    /// higher than the irrelevant one. Real model required.
    #[test]
    #[ignore = "requires a real bge-reranker-base checkout"]
    fn real_rerank_orders_relevant_higher() {
        let dir = std::env::var("BRAIN_RERANK_MODEL_DIR")
            .expect("BRAIN_RERANK_MODEL_DIR must point at the model directory");
        let enc = CrossEncoder::load(std::path::Path::new(&dir)).expect("load cross-encoder");
        let q = "where does Alice work?";
        let cands = [
            "Alice currently works at Stripe.",
            "the weather is nice today.",
        ];
        let scores = enc.score_pairs(q, &cands).expect("score pairs");
        assert!(
            scores[0] > scores[1],
            "relevant candidate must outscore irrelevant: {scores:?}",
        );
    }
}
