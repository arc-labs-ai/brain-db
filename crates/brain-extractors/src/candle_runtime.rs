//! Candle-backed BERT token-classification runtime. Phase 20.7b.
//!
//! Implements the `BertRuntime` trait declared in
//! [`crate::classifier`] using `candle_transformers::BertModel`
//! plus a linear classifier head loaded from the same
//! `model.safetensors` blob the operator supplies.
//!
//! ## Load path
//!
//! 1. Read `config.json` → `BertConfig`.
//! 2. Read `tokenizer.json` → `Tokenizer`.
//! 3. Load `model.safetensors` via `candle_core::safetensors::load`
//!    (no mmap — `#![forbid(unsafe_code)]` keeps us out of
//!    `from_mmaped_safetensors`).
//! 4. Build a `VarBuilder`; try the HuggingFace
//!    `BertForTokenClassification` layout first
//!    (`bert.*` prefix), fall back to a root-level layout.
//! 5. Extract the classifier head as a `candle_nn::Linear` from
//!    `classifier.{weight,bias}`.
//! 6. Optional warm-up forwards.
//!
//! ## Forward pass
//!
//! 1. Tokenise with the operator-supplied tokenizer; capture
//!    sub-word byte offsets via `tokenizers::Encoding::offsets`.
//! 2. Truncate to `max_seq_len`.
//! 3. Build `input_ids` / `attention_mask` / `token_type_ids`
//!    tensors of shape `(1, seq_len)`.
//! 4. Forward through BertModel → hidden states of shape
//!    `(1, seq_len, hidden)`.
//! 5. Linear head → logits `(1, seq_len, num_labels)`.
//! 6. Softmax over the last axis → per-token probabilities.
//! 7. Argmax → label index + confidence per token.
//! 8. Skip special-token positions ([CLS], [SEP], [PAD]).
//! 9. BIO decoder (`crate::labels::decode_bio`) collapses adjacent
//!    `B-X I-X I-X...` runs into one [`TokenClassification`].
//! 10. Map sub-token spans back to byte ranges in the original
//!     text using the tokenizer offsets captured in step 1.

use std::path::Path;

use candle_core::{DType, Device, Tensor};
use candle_nn::{ops, Linear, Module, VarBuilder};
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use tokenizers::Tokenizer;

use crate::classifier::{BertRuntime, TokenClassification};
use crate::extractor::ExtractorError;
use crate::labels::decode_bio;

/// The candle-backed BertRuntime.
pub(crate) struct CandleBertRuntime {
    model: BertModel,
    classifier: Linear,
    tokenizer: Tokenizer,
    device: Device,
    /// Captured at load for diagnostics; the actual `predict()`
    /// path uses `labels.len()` supplied by the caller so
    /// per-load mismatches don't propagate as silent index
    /// errors.
    #[allow(dead_code)]
    num_labels: usize,
}

impl CandleBertRuntime {
    /// Construct from a validated operator-provided model
    /// directory. Callers (i.e., `BertTokenClassifier::load`)
    /// have already verified the four required files exist and
    /// pickle has been refused.
    pub(crate) fn load(
        dir: &Path,
        device: Device,
        dtype: DType,
        warmup_iters: usize,
        num_labels: usize,
    ) -> Result<Self, ExtractorError> {
        // 1. Config.
        let config_bytes = std::fs::read(dir.join("config.json")).map_err(|e| {
            ExtractorError::ModelNotFound {
                id: format!("config.json read failed: {e}"),
            }
        })?;
        let bert_config: BertConfig =
            serde_json::from_slice(&config_bytes).map_err(|e| ExtractorError::OutputDecodeFailed {
                reason: format!("config.json parse failed: {e}"),
            })?;

        // Cross-check: config.json's id2label / num_labels (when
        // present) MUST match the labels.txt the loader already
        // read. We don't fail on mismatch here — labels.txt is
        // authoritative — but the operator-setup doc explains the
        // contract.
        let hidden_size = bert_config.hidden_size;

        // 2. Tokenizer.
        let tokenizer_path = dir.join("tokenizer.json");
        let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            ExtractorError::OutputDecodeFailed {
                reason: format!("tokenizer.json parse failed: {e}"),
            }
        })?;

        // 3. Safetensors.
        let weights_path = dir.join("model.safetensors");
        let tensors = candle_core::safetensors::load(&weights_path, &device).map_err(|e| {
            ExtractorError::InferenceFailed {
                reason: format!("safetensors::load({}): {e}", weights_path.display()),
            }
        })?;
        let vb_root = VarBuilder::from_tensors(tensors, dtype, &device);

        // 4. BertModel layout discovery.
        //
        // HuggingFace `BertForTokenClassification` nests `bert.*`.
        // Some checkpoints (older models, custom training scripts)
        // place BertModel tensors at the root. Try the nested
        // layout first; fall back to root only if that fails with
        // a clear "tensor not found" signal.
        let model = match BertModel::load(vb_root.pp("bert"), &bert_config) {
            Ok(m) => m,
            Err(_nested_err) => BertModel::load(vb_root.clone(), &bert_config).map_err(|e| {
                ExtractorError::InferenceFailed {
                    reason: format!(
                        "BertModel load failed under both `bert.*` and root layouts: {e}"
                    ),
                }
            })?,
        };

        // 5. Classifier head — `classifier.weight` /
        //    `classifier.bias`. Linear: hidden_size → num_labels.
        let classifier_vb = vb_root.pp("classifier");
        let classifier = candle_nn::linear(hidden_size, num_labels, classifier_vb).map_err(|e| {
            ExtractorError::InferenceFailed {
                reason: format!("classifier head load failed: {e}"),
            }
        })?;

        let runtime = Self {
            model,
            classifier,
            tokenizer,
            device,
            num_labels,
        };

        // 6. Warm-up.
        for _ in 0..warmup_iters {
            let _ = runtime.warmup_once();
        }

        Ok(runtime)
    }

    fn warmup_once(&self) -> Result<(), ExtractorError> {
        // Minimal valid BERT input: [CLS] (101) [SEP] (102). Some
        // tokenizers use 0 / 2 / 3 — we don't rely on the warm-up
        // succeeding for any particular model.
        let ids = Tensor::new(&[[101u32, 102u32]], &self.device).map_err(|e| {
            ExtractorError::InferenceFailed {
                reason: format!("warmup tensor: {e}"),
            }
        })?;
        let type_ids = Tensor::zeros(ids.shape(), DType::U32, &self.device).map_err(|e| {
            ExtractorError::InferenceFailed {
                reason: format!("warmup type_ids: {e}"),
            }
        })?;
        let _ = self
            .model
            .forward(&ids, &type_ids, None)
            .map_err(|e| ExtractorError::InferenceFailed {
                reason: format!("warmup forward: {e}"),
            })?;
        Ok(())
    }
}

impl BertRuntime for CandleBertRuntime {
    fn predict(
        &self,
        text: &str,
        max_seq_len: usize,
        labels: &[String],
    ) -> Result<Vec<TokenClassification>, ExtractorError> {
        // 1. Tokenise. `encode(text, true)` adds special tokens
        //    ([CLS], [SEP]) — the standard NER training format.
        let mut encoding =
            self.tokenizer
                .encode(text, true)
                .map_err(|e| ExtractorError::FeatureExtractionFailed {
                    reason: format!("tokenizer.encode: {e}"),
                })?;

        // 2. Truncate to max_seq_len. The Encoding::truncate API
        //    keeps the bookkeeping (offsets, special tokens mask)
        //    in sync.
        if encoding.get_ids().len() > max_seq_len {
            encoding.truncate(max_seq_len, 0, tokenizers::TruncationDirection::Right);
        }

        let ids: Vec<u32> = encoding.get_ids().to_vec();
        let attention_mask_vec: Vec<u32> = encoding.get_attention_mask().to_vec();
        let type_ids_vec: Vec<u32> = encoding.get_type_ids().to_vec();
        let offsets: Vec<(usize, usize)> = encoding.get_offsets().to_vec();
        let special_mask: Vec<u32> = encoding.get_special_tokens_mask().to_vec();

        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let seq_len = ids.len();
        let device = &self.device;

        // 3. Build tensors of shape (1, seq_len).
        let input_ids = Tensor::new(ids.as_slice(), device)
            .and_then(|t| t.reshape((1, seq_len)))
            .map_err(|e| ExtractorError::InferenceFailed {
                reason: format!("input_ids tensor: {e}"),
            })?;
        let attention_mask = Tensor::new(attention_mask_vec.as_slice(), device)
            .and_then(|t| t.reshape((1, seq_len)))
            .map_err(|e| ExtractorError::InferenceFailed {
                reason: format!("attention_mask tensor: {e}"),
            })?;
        let token_type_ids = Tensor::new(type_ids_vec.as_slice(), device)
            .and_then(|t| t.reshape((1, seq_len)))
            .map_err(|e| ExtractorError::InferenceFailed {
                reason: format!("token_type_ids tensor: {e}"),
            })?;

        // 4. Forward → (1, seq_len, hidden).
        let hidden = self
            .model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask))
            .map_err(|e| ExtractorError::InferenceFailed {
                reason: format!("BertModel::forward: {e}"),
            })?;

        // 5. Linear head → (1, seq_len, num_labels).
        let logits = self
            .classifier
            .forward(&hidden)
            .map_err(|e| ExtractorError::InferenceFailed {
                reason: format!("classifier head forward: {e}"),
            })?;

        // 6. Softmax over the last axis.
        let probs =
            ops::softmax_last_dim(&logits).map_err(|e| ExtractorError::InferenceFailed {
                reason: format!("softmax: {e}"),
            })?;

        // 7. Pull onto the host: (seq_len, num_labels) since
        //    batch=1.
        let probs_2d = probs
            .squeeze(0)
            .and_then(|t| t.to_dtype(DType::F32))
            .and_then(|t| t.to_vec2::<f32>())
            .map_err(|e| ExtractorError::OutputDecodeFailed {
                reason: format!("probs to_vec2: {e}"),
            })?;
        if probs_2d.len() != seq_len {
            return Err(ExtractorError::OutputDecodeFailed {
                reason: format!(
                    "probs row count {} != seq_len {seq_len}",
                    probs_2d.len()
                ),
            });
        }

        // 8. Argmax per token; skip special-token positions; build
        //    parallel label / confidence vectors that align with
        //    the `offsets` slice.
        let mut tok_labels: Vec<&str> = Vec::with_capacity(seq_len);
        let mut tok_confs: Vec<f32> = Vec::with_capacity(seq_len);
        let mut tok_offsets: Vec<(usize, usize)> = Vec::with_capacity(seq_len);
        for (i, row) in probs_2d.iter().enumerate() {
            if special_mask.get(i) == Some(&1) {
                continue;
            }
            let (best_idx, best_p) = argmax(row);
            let label = labels
                .get(best_idx)
                .map(|s| s.as_str())
                .unwrap_or("O");
            tok_labels.push(label);
            tok_confs.push(best_p);
            tok_offsets.push(offsets[i]);
        }
        if tok_labels.is_empty() {
            return Ok(Vec::new());
        }

        // 9. BIO decode → token-level spans.
        let bio_spans = decode_bio(&tok_labels, &tok_confs);

        // 10. Map token-spans → byte ranges in the original text.
        let mut out = Vec::with_capacity(bio_spans.len());
        for span in bio_spans {
            // `decode_bio` returns `end_token` as exclusive.
            let last_token = span.end_token.saturating_sub(1);
            let start_byte = tok_offsets[span.start_token].0;
            let end_byte = tok_offsets[last_token].1;
            // Skip spans whose offsets collapsed to nothing (e.g.,
            // tokenizer emitted (0,0) for an unknown char).
            if end_byte <= start_byte {
                continue;
            }
            let text_slice = text
                .get(start_byte..end_byte)
                .unwrap_or("")
                .to_string();
            out.push(TokenClassification {
                label: span.label,
                text: text_slice,
                start: start_byte,
                end: end_byte,
                confidence: span.confidence,
            });
        }
        Ok(out)
    }
}

/// Argmax that returns `(index, value)` of the maximum probability
/// in a row. NaN-safe: NaN values are treated as -Inf so they
/// never win.
fn argmax(row: &[f32]) -> (usize, f32) {
    let mut best_idx = 0usize;
    let mut best = f32::NEG_INFINITY;
    for (i, &v) in row.iter().enumerate() {
        let safe = if v.is_finite() { v } else { f32::NEG_INFINITY };
        if safe > best {
            best = safe;
            best_idx = i;
        }
    }
    (best_idx, if best.is_finite() { best } else { 0.0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argmax_picks_highest() {
        let (i, v) = argmax(&[0.1, 0.5, 0.3]);
        assert_eq!(i, 1);
        assert!((v - 0.5).abs() < 1e-6);
    }

    #[test]
    fn argmax_handles_nan() {
        let (i, v) = argmax(&[f32::NAN, 0.5, f32::NAN]);
        assert_eq!(i, 1);
        assert!((v - 0.5).abs() < 1e-6);
    }

    #[test]
    fn argmax_handles_all_nan() {
        let (_, v) = argmax(&[f32::NAN, f32::NAN]);
        assert_eq!(v, 0.0);
    }
}
