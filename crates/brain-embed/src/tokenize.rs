//! WordPiece tokenisation for BGE-small. Produces the three tensors
//! BERT's forward pass expects: `input_ids`, `token_type_ids` (always
//! zero), `attention_mask`.
//!
//! See:
//! - `spec/04_embedding_layer/02_tokenization.md` §1–§3 — pipeline +
//!   hard 512-token cap.
//! - `spec/04_embedding_layer/02_tokenization.md` §7–§8 — tokeniser is
//!   loaded once at startup and immutable; thread-safe at encode time.
//!
//! Design notes (see `.claude/plans/phase-05-task-02.md`):
//!
//! - We do **not** mutate the shared tokeniser (no `with_truncation` /
//!   `with_padding` at encode time). Truncation and padding happen
//!   here, by hand, after a no-limit `encode` from the crate.
//! - Truncation is detected by comparing the pre-truncation length
//!   against the cap; this is how operators see "content lost".
//! - All tensors are `DType::U32` for parity with `ModelHandle::warmup_once`.

use candle_core::{DType, Device, Tensor};
use tokenizers::Tokenizer;

use crate::error::EmbedError;

/// Hard cap from spec §04/02 §3. Inputs longer than this are truncated
/// (right-side) and `Tokenized::truncated_flags` records it.
pub const MAX_TOKEN_LENGTH: usize = 512;

/// BERT's `[PAD]` token literal. Resolved to its id via the tokeniser
/// (BERT-uncased it's `0`, but we never hard-code that).
const PAD_TOKEN: &str = "[PAD]";

/// Tokenised text ready to feed into `ModelHandle::forward`.
///
/// All three tensors share shape `(batch, seq_len)` and live on the
/// supplied `Device`. `actual_lengths[i]` and `truncated_flags[i]`
/// describe the input at batch position `i`.
#[derive(Debug)]
pub struct Tokenized {
    /// Token ids, `u32`. `[CLS] + body + [SEP] + [PAD]*` per row.
    pub input_ids: Tensor,
    /// Segment ids — always zero for our single-sequence inputs
    /// (spec §04/02 §2 step 7).
    pub token_type_ids: Tensor,
    /// `1` at real-token positions, `0` at `[PAD]` positions.
    pub attention_mask: Tensor,
    /// Number of non-pad tokens per row, batch order.
    pub actual_lengths: Vec<usize>,
    /// `true` iff the input was longer than [`MAX_TOKEN_LENGTH`] before
    /// truncation (spec §04/02 §3).
    pub truncated_flags: Vec<bool>,
}

/// Tokenise a single text. Convenience wrapper around
/// [`encode_batch`] with a one-element batch.
pub fn encode_single(
    tokenizer: &Tokenizer,
    text: &str,
    device: &Device,
) -> Result<Tokenized, EmbedError> {
    encode_batch(tokenizer, &[text], device)
}

/// Tokenise a batch of texts. The result is padded to the longest row
/// in the batch (or [`MAX_TOKEN_LENGTH`], whichever is smaller).
///
/// Errors:
/// - `TokenizationFailed` — empty batch, missing `[PAD]` token in the
///   vocab, or the crate's `encode_batch` failed.
/// - `TensorBuild` — `candle_core` rejected our token slab.
pub fn encode_batch(
    tokenizer: &Tokenizer,
    texts: &[&str],
    device: &Device,
) -> Result<Tokenized, EmbedError> {
    if texts.is_empty() {
        return Err(EmbedError::TokenizationFailed("empty batch".to_string()));
    }

    let pad_id = tokenizer
        .token_to_id(PAD_TOKEN)
        .ok_or_else(|| EmbedError::TokenizationFailed(format!("missing {PAD_TOKEN} in vocab")))?;

    // 1. Encode every text with special tokens, no truncation, no
    //    padding. We do truncation + padding ourselves so the shared
    //    tokeniser stays read-only (spec §02 §7-§8).
    let inputs: Vec<tokenizers::EncodeInput> = texts
        .iter()
        .map(|t| tokenizers::EncodeInput::Single((*t).into()))
        .collect();
    let encodings = tokenizer
        .encode_batch_char_offsets(inputs, true)
        .map_err(|e| EmbedError::TokenizationFailed(format!("encode_batch: {e}")))?;

    let batch = encodings.len();
    debug_assert_eq!(batch, texts.len());

    // 2. Per row: capture pre-truncation length, apply 512 cap, then
    //    compute the batch-wide pad width.
    let mut raw_ids: Vec<Vec<u32>> = Vec::with_capacity(batch);
    let mut actual_lengths: Vec<usize> = Vec::with_capacity(batch);
    let mut truncated_flags: Vec<bool> = Vec::with_capacity(batch);

    for (i, enc) in encodings.iter().enumerate() {
        let full = enc.get_ids();
        let pre_trunc_len = full.len();
        let truncated = pre_trunc_len > MAX_TOKEN_LENGTH;
        let kept = if truncated {
            // Right-side truncation per spec §04/02 §3: keep the first
            // (MAX_TOKEN_LENGTH - 1) tokens, then overwrite the last
            // slot with `[SEP]` so the model still sees the sentinel.
            //
            // `[SEP]` id is at position `pre_trunc_len - 1` of `full`
            // because `encode` was called with `add_special_tokens=true`.
            let sep_id = full[pre_trunc_len - 1];
            let mut ids = full[..MAX_TOKEN_LENGTH].to_vec();
            *ids.last_mut().expect("MAX_TOKEN_LENGTH > 0") = sep_id;
            ids
        } else {
            full.to_vec()
        };

        if truncated {
            tracing::warn!(
                target: "brain_embed::truncation",
                row = i,
                pre_trunc_len,
                cap = MAX_TOKEN_LENGTH,
                "input truncated; tail tokens dropped per spec §04/02 §3"
            );
        }

        actual_lengths.push(kept.len());
        truncated_flags.push(truncated);
        raw_ids.push(kept);
    }

    // 3. Pad to the longest row (capped at MAX_TOKEN_LENGTH).
    let seq_len = raw_ids.iter().map(Vec::len).max().expect("batch non-empty");
    let mut padded_ids: Vec<u32> = vec![pad_id; batch * seq_len];
    let mut mask: Vec<u32> = vec![0; batch * seq_len];
    for (row, ids) in raw_ids.iter().enumerate() {
        let start = row * seq_len;
        padded_ids[start..start + ids.len()].copy_from_slice(ids);
        for slot in &mut mask[start..start + ids.len()] {
            *slot = 1;
        }
    }
    let type_ids: Vec<u32> = vec![0; batch * seq_len];

    // 4. Build the three tensors.
    let shape = (batch, seq_len);
    let input_ids = Tensor::from_vec(padded_ids, shape, device)
        .map_err(|e| EmbedError::TensorBuild(format!("input_ids: {e}")))?;
    let attention_mask = Tensor::from_vec(mask, shape, device)
        .map_err(|e| EmbedError::TensorBuild(format!("attention_mask: {e}")))?;
    let token_type_ids = Tensor::from_vec(type_ids, shape, device)
        .map_err(|e| EmbedError::TensorBuild(format!("token_type_ids: {e}")))?;

    // Sanity: candle's `Tensor::from_vec` infers DType from the source
    // slice, but we want explicit U32 contract for downstream callers.
    debug_assert_eq!(input_ids.dtype(), DType::U32);
    debug_assert_eq!(attention_mask.dtype(), DType::U32);
    debug_assert_eq!(token_type_ids.dtype(), DType::U32);

    Ok(Tokenized {
        input_ids,
        token_type_ids,
        attention_mask,
        actual_lengths,
        truncated_flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_path() -> PathBuf {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("tests/fixtures/tokenizer-tiny.json");
        p
    }

    fn load_tiny() -> Tokenizer {
        Tokenizer::from_file(fixture_path()).expect("tiny tokenizer fixture loads")
    }

    fn col(tensor: &Tensor, row: usize) -> Vec<u32> {
        tensor.get(row).unwrap().to_vec1::<u32>().unwrap()
    }

    #[test]
    fn empty_batch_rejected() {
        let tk = load_tiny();
        let dev = Device::Cpu;
        match encode_batch(&tk, &[], &dev) {
            Err(EmbedError::TokenizationFailed(msg)) => assert!(msg.contains("empty")),
            other => panic!("expected TokenizationFailed, got {other:?}"),
        }
    }

    #[test]
    fn single_text_has_cls_and_sep() {
        let tk = load_tiny();
        let dev = Device::Cpu;
        let out = encode_single(&tk, "hello world", &dev).unwrap();
        assert_eq!(out.actual_lengths.len(), 1);
        assert!(!out.truncated_flags[0]);
        let ids = col(&out.input_ids, 0);
        // First and last non-pad token must be [CLS] / [SEP].
        let cls = tk.token_to_id("[CLS]").unwrap();
        let sep = tk.token_to_id("[SEP]").unwrap();
        assert_eq!(ids[0], cls, "first token must be [CLS]");
        let len = out.actual_lengths[0];
        assert_eq!(ids[len - 1], sep, "last real token must be [SEP]");
    }

    #[test]
    fn token_type_ids_are_all_zero() {
        let tk = load_tiny();
        let dev = Device::Cpu;
        let out = encode_single(&tk, "hello", &dev).unwrap();
        let row = col(&out.token_type_ids, 0);
        assert!(row.iter().all(|&v| v == 0), "spec §04/02 §2 step 7");
    }

    #[test]
    fn attention_mask_matches_real_tokens() {
        let tk = load_tiny();
        let dev = Device::Cpu;
        // Mixed-length batch forces padding on the shorter row.
        let out = encode_batch(&tk, &["hello world", "hi"], &dev).unwrap();
        let mask0 = col(&out.attention_mask, 0);
        let mask1 = col(&out.attention_mask, 1);
        // Row 0 should be all-1 up to its actual length, then 0.
        let len0 = out.actual_lengths[0];
        let len1 = out.actual_lengths[1];
        assert!(mask0[..len0].iter().all(|&v| v == 1));
        assert!(mask0[len0..].iter().all(|&v| v == 0));
        assert!(mask1[..len1].iter().all(|&v| v == 1));
        assert!(mask1[len1..].iter().all(|&v| v == 0));
        // Shorter row must be strictly shorter.
        assert!(len1 < len0);
    }

    #[test]
    fn batch_pads_to_longest_row() {
        let tk = load_tiny();
        let dev = Device::Cpu;
        let out = encode_batch(&tk, &["hello world hello", "hi"], &dev).unwrap();
        let (batch, seq) = out.input_ids.dims2().unwrap();
        assert_eq!(batch, 2);
        let max_actual = *out.actual_lengths.iter().max().unwrap();
        assert_eq!(seq, max_actual);
    }

    #[test]
    fn empty_string_tokenises_to_cls_sep() {
        let tk = load_tiny();
        let dev = Device::Cpu;
        let out = encode_single(&tk, "", &dev).unwrap();
        // [CLS] + [SEP] = 2 tokens.
        assert_eq!(out.actual_lengths[0], 2);
        assert!(!out.truncated_flags[0]);
    }

    #[test]
    fn truncation_detected_and_capped() {
        let tk = load_tiny();
        let dev = Device::Cpu;
        // Generate text long enough that the tokeniser produces more
        // than MAX_TOKEN_LENGTH tokens. The tiny vocab has a handful
        // of real words + [UNK]; one word per repetition is plenty.
        let long = "hello ".repeat(MAX_TOKEN_LENGTH * 2);
        let out = encode_single(&tk, &long, &dev).unwrap();
        assert!(
            out.truncated_flags[0],
            "long input must be flagged truncated"
        );
        assert_eq!(out.actual_lengths[0], MAX_TOKEN_LENGTH);

        let (_, seq) = out.input_ids.dims2().unwrap();
        assert_eq!(seq, MAX_TOKEN_LENGTH);

        // Last kept token must be [SEP] (we overwrite the truncation
        // boundary so the model still sees the sentinel).
        let ids = col(&out.input_ids, 0);
        let sep = tk.token_to_id("[SEP]").unwrap();
        assert_eq!(ids[MAX_TOKEN_LENGTH - 1], sep);
    }
}
