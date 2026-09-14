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

use std::borrow::Cow;
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

/// Special tokens the tokenizer injects around a `(query, passage)`
/// pair (`<s> query </s></s> passage </s>`): four added tokens. The
/// tokenizer subtracts this from `max_length` before truncating, so
/// the real content budget for a pair is `max_len - PAIR_SPECIAL_TOKENS`.
const PAIR_SPECIAL_TOKENS: usize = 4;

/// Passage token budget reserved when clamping an over-long query, so
/// the cross-encoder always scores the query against a non-empty
/// passage and `OnlySecond` always has room to trim rather than
/// erroring with `SequenceTooShort`. Also absorbs any small
/// decode/re-encode drift from the clamp round-trip.
const MIN_PASSAGE_BUDGET: usize = 8;

/// Truncation parameters for a `(query, passage)` cross-encoder.
///
/// Pairs are built as `(query, candidate)` in [`CrossEncoder::score_pairs`],
/// so the passage is the SECOND sequence. [`TruncationStrategy::OnlySecond`]
/// keeps the query intact and trims the passage tail to fit `max_length`.
///
/// [`TruncationStrategy::OnlyFirst`] would trim the query instead, which is
/// wrong in two ways: (a) a long passage paired with a short query overshoots
/// the query length, so tokenizers raises `SequenceTooShort` and fails the
/// entire `encode_batch` — one long candidate silently collapses rerank to
/// RRF-only for the whole query; (b) even when it does not error, the query
/// gets mutilated and the cross-encoder scores garbage logits.
pub(crate) fn truncation_params(max_len: usize) -> TruncationParams {
    TruncationParams {
        max_length: max_len,
        strategy: tokenizers::TruncationStrategy::OnlySecond,
        stride: 0,
        direction: tokenizers::TruncationDirection::Right,
    }
}

/// Resolve the pad `(id, token)` used to fill batch rows.
///
/// Prefer the tokenizer's own padding config when present; otherwise
/// fall back to the model's `pad_token_id` (id 1 = `<pad>` for
/// XLM-RoBERTa) rather than 0 (`<s>`). A stock bge-reranker-base
/// `tokenizer.json` configures no padding, so the old `0` fallback
/// filled batches with the sentence-start token — benign only because
/// the attention mask zeroes pad positions, but wrong on its face.
pub(crate) fn resolve_pad_params(
    tokenizer_padding: Option<(u32, String)>,
    config_pad_token_id: u32,
) -> (u32, String) {
    tokenizer_padding.unwrap_or_else(|| (config_pad_token_id, "<pad>".to_string()))
}

/// Clamp an over-long query so it can never overflow `max_len` and
/// poison the whole rerank batch.
///
/// The batch tokenizer trims the *passage* (`OnlySecond`), so a query
/// that alone tokenizes beyond the pair's content budget would leave
/// nothing to trim and fail `encode_batch` atomically — collapsing
/// rerank to RRF-only for the entire request. Here we measure the
/// query with a truncation-free `probe` tokenizer, and if it exceeds
/// the budget we truncate its token ids and decode them back to a
/// bounded query string. The budget reserves room for the pair's
/// special tokens plus a minimum passage, so `OnlySecond` always has
/// something to trim.
///
/// Normal-length queries are returned untouched (`Cow::Borrowed`), so
/// their tokenisation is byte-identical to before this guard existed.
pub(crate) fn clamp_query_tokens<'q>(
    probe: &Tokenizer,
    query: &'q str,
    max_len: usize,
) -> Cow<'q, str> {
    let budget = max_len.saturating_sub(PAIR_SPECIAL_TOKENS + MIN_PASSAGE_BUDGET);
    if budget == 0 {
        return Cow::Borrowed(query);
    }
    // `false` = no special tokens: measure the raw query tokens only.
    let ids = match probe.encode(query, false) {
        Ok(enc) => enc.get_ids().to_vec(),
        // If measurement fails, leave the query as-is; the batch path
        // surfaces any genuine tokenisation error.
        Err(_) => return Cow::Borrowed(query),
    };
    if ids.len() <= budget {
        return Cow::Borrowed(query);
    }
    match probe.decode(&ids[..budget], true) {
        Ok(s) => Cow::Owned(s),
        Err(_) => Cow::Borrowed(query),
    }
}

/// Reject any non-finite logit, symmetric with the embed path's
/// NaN/Inf guard. A single non-finite score would corrupt the
/// caller's re-sort (NaN compares false in every direction) and
/// propagate through the normalized-fused / `alpha * logit` blend.
pub(crate) fn check_finite_scores(scores: &[f32]) -> Result<(), RerankError> {
    if let Some(i) = scores.iter().position(|s| !s.is_finite()) {
        return Err(RerankError::NumericFailure(format!(
            "logit {i} is non-finite ({})",
            scores[i]
        )));
    }
    Ok(())
}

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

    #[error("rerank score was non-finite (NaN or Inf): {0}")]
    NumericFailure(String),

    #[error("rerank service thread is unavailable (shut down or panicked)")]
    ServiceUnavailable,
}

/// Loaded cross-encoder. Owns the XLM-RoBERTa sequence-classifier
/// (backbone + relevance head), the tokenizer, and the target
/// device.
pub struct CrossEncoder {
    model: XLMRobertaForSequenceClassification,
    tokenizer: Tokenizer,
    /// Truncation/padding-free clone of `tokenizer`, used only to
    /// measure and clamp an over-long query (see [`clamp_query_tokens`]).
    /// The batch tokenizer trims the passage (`OnlySecond`), which
    /// cannot measure the query in isolation without erroring.
    query_probe: Tokenizer,
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
        let (pad_id, pad_token) = resolve_pad_params(
            tokenizer
                .get_padding()
                .map(|p| (p.pad_id, p.pad_token.clone())),
            model_config.pad_token_id,
        );
        tokenizer.with_padding(Some(PaddingParams {
            strategy: tokenizers::PaddingStrategy::BatchLongest,
            direction: tokenizers::PaddingDirection::Right,
            pad_to_multiple_of: None,
            pad_id,
            pad_type_id: 0,
            pad_token,
        }));
        tokenizer
            .with_truncation(Some(truncation_params(max_len)))
            .map_err(|e| RerankError::TokenizerParse(e.to_string()))?;

        // A truncation/padding-free clone used solely to measure a
        // query's true token length when clamping (the batch tokenizer's
        // `OnlySecond` errors on a lone over-long sequence).
        let mut query_probe = tokenizer.clone();
        query_probe
            .with_truncation(None)
            .map_err(|e| RerankError::TokenizerParse(e.to_string()))?;
        query_probe.with_padding(None);

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
            query_probe,
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

        // Clamp an over-long query so a single pathological cue cannot
        // fail the atomic `encode_batch` and silently drop rerank to
        // RRF-only for every candidate. Normal queries pass through
        // untouched.
        let query = clamp_query_tokens(&self.query_probe, query, self.max_len);

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
        check_finite_scores(&scores)?;
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

    use tokenizers::models::wordlevel::WordLevel;
    use tokenizers::pre_tokenizers::whitespace::Whitespace;
    use tokenizers::processors::template::TemplateProcessing;

    /// Build a minimal real `Tokenizer` that emits two sequences for a
    /// `(query, passage)` pair — `<s> query </s></s> passage </s>` — using
    /// the exact production [`truncation_params`]. This lets us exercise the
    /// truncation strategy against the pair-building order without loading
    /// the multi-hundred-MB bge-reranker-base checkout.
    ///
    /// `strategy` is a parameter so a single builder can construct both the
    /// production (`OnlySecond`) tokenizer and a contrasting `OnlyFirst` one
    /// that reproduces the bug.
    fn build_pair_tokenizer(max_len: usize, strategy: tokenizers::TruncationStrategy) -> Tokenizer {
        let words = [
            "<s>", "</s>", "[UNK]", "where", "does", "alice", "work", "cat", "stripe",
        ];
        let vocab_json = {
            let entries: Vec<String> = words
                .iter()
                .enumerate()
                .map(|(i, w)| format!("{:?}:{i}", *w))
                .collect();
            format!("{{{}}}", entries.join(","))
        };
        let vocab_dir = tempfile::tempdir().expect("tempdir");
        let vocab_path = vocab_dir.path().join("vocab.json");
        std::fs::write(&vocab_path, vocab_json).expect("write vocab.json");
        let wl = WordLevel::from_file(vocab_path.to_str().expect("utf8 path"), "[UNK]".to_string())
            .expect("build wordlevel model");

        let mut tok = Tokenizer::new(wl);
        tok.with_pre_tokenizer(Some(Whitespace {}));
        let post = TemplateProcessing::builder()
            .try_single("<s> $A </s>")
            .expect("single template")
            .try_pair("<s> $A </s> </s> $B </s>")
            .expect("pair template")
            .special_tokens(vec![("<s>", 0u32), ("</s>", 1u32)])
            .build()
            .expect("build post-processor");
        tok.with_post_processor(Some(post));

        let mut params = truncation_params(max_len);
        params.strategy = strategy;
        tok.with_truncation(Some(params)).expect("set truncation");
        tok
    }

    /// Count how many tokens in an encoding belong to sequence `seq`
    /// (0 = query, 1 = passage; `None` = an added special token).
    fn seq_count(enc: &tokenizers::Encoding, seq: usize) -> usize {
        enc.get_sequence_ids()
            .iter()
            .filter(|s| **s == Some(seq))
            .count()
    }

    #[test]
    fn truncation_params_targets_the_passage() {
        // The load-time choice is `OnlySecond`; pairs are (query, passage),
        // so the passage (second) is what gets trimmed.
        let p = truncation_params(DEFAULT_MAX_TOKEN_LEN);
        assert_eq!(p.strategy, tokenizers::TruncationStrategy::OnlySecond);
        assert_eq!(p.max_length, DEFAULT_MAX_TOKEN_LEN);
        assert_eq!(p.stride, 0);
    }

    #[test]
    fn long_passage_short_query_keeps_query_and_trims_passage() {
        let max_len = 32;
        let tok = build_pair_tokenizer(max_len, tokenizers::TruncationStrategy::OnlySecond);

        let query = "where does alice work"; // 4 tokens
        let passage_words = 200;
        let passage = vec!["cat"; passage_words].join(" ");

        // (a) A very-long passage must NOT raise SequenceTooShort.
        let enc = tok
            .encode((query, passage.as_str()), true)
            .expect("encode long-passage pair must succeed");

        // (b) The whole thing fits under the cap.
        assert!(
            enc.get_ids().len() <= max_len,
            "encoding {} exceeds cap {max_len}",
            enc.get_ids().len()
        );

        // (c) Every query token survives; the passage was the thing cut.
        assert_eq!(seq_count(&enc, 0), 4, "all query tokens must be preserved");
        assert!(
            seq_count(&enc, 1) < passage_words,
            "passage should have been truncated"
        );
        assert!(seq_count(&enc, 1) > 0, "some passage should remain");
    }

    #[test]
    fn batch_with_long_passage_does_not_poison_the_batch() {
        let max_len = 32;
        let tok = build_pair_tokenizer(max_len, tokenizers::TruncationStrategy::OnlySecond);

        let query = "where does alice work";
        let short = "stripe";
        let long = vec!["cat"; 300].join(" ");

        // Mirrors `score_pairs`: same query, per-candidate passages.
        let pairs = vec![
            (query.to_string(), short.to_string()),
            (query.to_string(), long.clone()),
        ];
        let batch = tok
            .encode_batch(pairs, true)
            .expect("mixed batch must not fail because one passage is long");

        assert_eq!(batch.len(), 2);
        for enc in &batch {
            assert!(
                enc.get_ids().len() <= max_len,
                "each encoding must respect the cap"
            );
            assert_eq!(seq_count(enc, 0), 4, "query preserved in every pair");
        }
    }

    #[test]
    fn only_first_strategy_reproduces_the_bug() {
        // Regression guard: this is the OLD behavior. With `OnlyFirst`, a
        // long passage paired with a short query forces truncation of the
        // query, and since the amount to remove exceeds the query length,
        // tokenizers errors — which is exactly what poisoned the batch.
        let tok = build_pair_tokenizer(32, tokenizers::TruncationStrategy::OnlyFirst);
        let query = "where does alice work";
        let passage = vec!["cat"; 200].join(" ");
        let result = tok.encode((query, passage.as_str()), true);
        assert!(
            result.is_err(),
            "OnlyFirst must fail on a short-query/long-passage pair (SequenceTooShort)"
        );
    }

    /// Build a truncation/padding-free probe tokenizer (mirrors the
    /// `query_probe` built in [`CrossEncoder::load`]).
    fn build_probe_tokenizer(max_len: usize) -> Tokenizer {
        let mut probe = build_pair_tokenizer(max_len, tokenizers::TruncationStrategy::OnlySecond);
        probe.with_truncation(None).expect("disable truncation");
        probe.with_padding(None);
        probe
    }

    #[test]
    fn overlong_query_is_clamped_and_does_not_poison_batch() {
        let max_len = 32;
        let batch_tok = build_pair_tokenizer(max_len, tokenizers::TruncationStrategy::OnlySecond);
        let probe = build_probe_tokenizer(max_len);

        // A query that alone tokenizes far past the cap.
        let long_query = vec!["cat"; 200].join(" ");

        // (a) Baseline: WITHOUT clamping, the raw over-long query poisons
        //     the atomic batch — this is the RR1 bug.
        let raw_pairs = vec![(long_query.clone(), "stripe".to_string())];
        assert!(
            batch_tok.encode_batch(raw_pairs, true).is_err(),
            "raw over-long query must fail encode_batch (reproduces the bug)"
        );

        // (b) After clamping, the query shrinks...
        let clamped = clamp_query_tokens(&probe, &long_query, max_len);
        assert!(
            matches!(clamped, Cow::Owned(_)),
            "an over-long query must be clamped (owned)"
        );
        assert!(
            probe
                .encode(clamped.as_ref(), false)
                .expect("encode clamped")
                .get_ids()
                .len()
                <= max_len.saturating_sub(PAIR_SPECIAL_TOKENS + MIN_PASSAGE_BUDGET),
            "clamped query must fit the reserved budget"
        );

        // (c) ...and the clamped query paired with candidates encodes
        //     cleanly — one encoding per candidate, no batch failure.
        let cands = ["stripe", "where does alice work"];
        let pairs: Vec<(String, String)> = cands
            .iter()
            .map(|c| (clamped.to_string(), (*c).to_string()))
            .collect();
        let batch = batch_tok
            .encode_batch(pairs, true)
            .expect("clamped query must not poison the batch");
        assert_eq!(batch.len(), cands.len(), "one encoding per candidate");
        for enc in &batch {
            assert!(
                enc.get_ids().len() <= max_len,
                "each encoding must respect the cap"
            );
        }
    }

    #[test]
    fn normal_query_is_returned_byte_identical() {
        let max_len = 32;
        let probe = build_probe_tokenizer(max_len);
        let q = "where does alice work";
        let clamped = clamp_query_tokens(&probe, q, max_len);
        assert!(
            matches!(clamped, Cow::Borrowed(_)),
            "a normal query must not be reallocated"
        );
        assert_eq!(clamped.as_ref(), q, "normal query must be byte-identical");
    }

    #[test]
    fn check_finite_scores_accepts_finite_and_rejects_non_finite() {
        assert!(check_finite_scores(&[]).is_ok());
        assert!(check_finite_scores(&[-3.5, 0.0, 12.25]).is_ok());

        match check_finite_scores(&[1.0, f32::NAN, 2.0]) {
            Err(RerankError::NumericFailure(msg)) => assert!(msg.contains("logit 1")),
            other => panic!("expected NumericFailure on NaN, got {other:?}"),
        }
        match check_finite_scores(&[f32::INFINITY]) {
            Err(RerankError::NumericFailure(msg)) => assert!(msg.contains("logit 0")),
            other => panic!("expected NumericFailure on Inf, got {other:?}"),
        }
        assert!(matches!(
            check_finite_scores(&[f32::NEG_INFINITY]),
            Err(RerankError::NumericFailure(_))
        ));
    }

    #[test]
    fn pad_falls_back_to_config_pad_token_id_not_zero() {
        // Stock bge-reranker-base tokenizer has no padding configured →
        // None. We must fall back to config.pad_token_id (1 = <pad>),
        // never the old 0 (<s>).
        let (id, tok) = resolve_pad_params(None, 1);
        assert_eq!(id, 1, "pad id must come from config.pad_token_id, not 0");
        assert_eq!(tok, "<pad>");

        // An explicit tokenizer padding config wins over the fallback.
        let (id2, tok2) = resolve_pad_params(Some((7, "[PAD]".to_string())), 1);
        assert_eq!(id2, 7);
        assert_eq!(tok2, "[PAD]");
    }

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
