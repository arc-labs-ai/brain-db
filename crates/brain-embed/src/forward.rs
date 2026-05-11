//! Forward pass + `[CLS]` pooling + L2 normalise.
//!
//! Composes 5.1's [`ModelHandle`] with 5.2's [`Tokenized`] to produce
//! substrate-owned 384-dim L2-normalised `f32` vectors.
//!
//! See:
//! - `spec/04_embedding_layer/03_inference.md` §3 — forward pass shape
//!   (embeddings → encoder → `[CLS]` pool → 384-dim output). Step 5
//!   (normalisation) is the *substrate's* job, applied here.
//! - `spec/04_embedding_layer/02_tokenization.md` §10 — for
//!   `bge-small-en-v1.5`, the official approach is `[CLS]` pooling
//!   (phase-doc's "mean-pool" claim was wrong; spec wins).
//! - `spec/04_embedding_layer/04_normalization.md` §1 + §4 — the
//!   reference L2-normalise implementation we mirror.
//! - `spec/04_embedding_layer/04_normalization.md` §8 + §9 — NaN /
//!   Inf / zero-norm outputs are rejected, not propagated.
//!
//! BGE-small specifically uses CLS pooling with **no** pooler MLP:
//! `last_hidden_state[:, 0]` *is* the embedding. `candle_transformers`'
//! `BertModel` returns the hidden states directly without applying
//! `BertPooler`, so taking position 0 of the seq axis is correct.

use candle_core::Tensor;

use crate::error::EmbedError;
use crate::model::ModelHandle;
use crate::tokenize::{encode_batch, encode_single, Tokenized};

/// Output dimensionality. Pinned at 384 for v1 (BGE-small).
pub const VECTOR_DIM: usize = 384;

/// Norm guard from spec §04/04 §4: below this we treat the vector as
/// pathological (zero-norm) and refuse to divide.
const ZERO_NORM_EPS: f32 = 1e-8;

/// L2-normalise in place. Reference scalar implementation per spec
/// §04/04 §4. Returns the *original* norm (pre-normalisation) so the
/// caller can decide whether to reject (zero-vector defence).
///
/// On a near-zero vector (`norm < ZERO_NORM_EPS`), the contents are
/// left untouched — the caller must check the returned norm.
pub fn l2_normalize_in_place(v: &mut [f32; VECTOR_DIM]) -> f32 {
    let norm_sq: f32 = v.iter().map(|x| x * x).sum();
    let norm = norm_sq.sqrt();
    if norm < ZERO_NORM_EPS {
        return norm;
    }
    let inv = 1.0 / norm;
    for x in v.iter_mut() {
        *x *= inv;
    }
    norm
}

/// Run BERT forward on pre-tokenised input, extract `[CLS]` (seq pos 0),
/// L2-normalise per row, return one unit vector per batch row.
///
/// Rejects rows whose output contains NaN/Inf or whose pre-normalisation
/// norm is below [`ZERO_NORM_EPS`] (spec §04/04 §8–§9).
pub fn forward_pooled(
    handle: &ModelHandle,
    tokens: &Tokenized,
) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
    // 1. Forward pass with the attention mask. Without the mask the
    //    model attends to [PAD] positions and contaminates the [CLS]
    //    output for short rows in a mixed-length batch.
    let hidden = handle.forward(
        &tokens.input_ids,
        &tokens.token_type_ids,
        Some(&tokens.attention_mask),
    )?;

    // 2. Shape sanity. Expect (batch, seq_len, hidden_dim). Catch
    //    "operator pointed at the wrong model" as cheaply as possible.
    let dims = hidden.dims();
    if dims.len() != 3 {
        return Err(EmbedError::NumericFailure(format!(
            "BertModel output had {} dims, expected 3 (batch, seq, hidden)",
            dims.len()
        )));
    }
    let (batch, _seq, hidden_dim) = (dims[0], dims[1], dims[2]);
    if hidden_dim != VECTOR_DIM {
        return Err(EmbedError::OutputDimMismatch {
            expected: VECTOR_DIM,
            got: hidden_dim,
        });
    }

    // 3. Extract [CLS] at seq position 0 → shape (batch, hidden).
    let cls = extract_cls(&hidden)?;

    // 4. Pull into host memory as Vec<Vec<f32>>. candle does the dtype
    //    cast if needed (BertModel weights are F32 in our config).
    let rows: Vec<Vec<f32>> = cls
        .to_vec2::<f32>()
        .map_err(|e| EmbedError::NumericFailure(format!("to_vec2 cls: {e}")))?;
    if rows.len() != batch {
        return Err(EmbedError::NumericFailure(format!(
            "cls had {} rows, expected batch={batch}",
            rows.len()
        )));
    }

    // 5. Validate + normalise each row.
    let mut out: Vec<[f32; VECTOR_DIM]> = Vec::with_capacity(batch);
    for (row_idx, row) in rows.into_iter().enumerate() {
        if row.len() != VECTOR_DIM {
            return Err(EmbedError::OutputDimMismatch {
                expected: VECTOR_DIM,
                got: row.len(),
            });
        }
        let mut arr: [f32; VECTOR_DIM] = [0.0; VECTOR_DIM];
        arr.copy_from_slice(&row);

        // NaN / Inf check (spec §04/04 §8).
        if let Some(bad) = arr.iter().position(|x| !x.is_finite()) {
            return Err(EmbedError::NumericFailure(format!(
                "row {row_idx} element {bad} is NaN or Inf"
            )));
        }

        // L2-normalise + zero-norm check (spec §04/04 §4 + §9).
        let pre_norm = l2_normalize_in_place(&mut arr);
        if pre_norm < ZERO_NORM_EPS {
            return Err(EmbedError::NumericFailure(format!(
                "row {row_idx} has near-zero norm: {pre_norm}"
            )));
        }
        out.push(arr);
    }

    Ok(out)
}

/// Convenience: tokenise + forward + pool + normalise for a single text.
pub fn embed_text(handle: &ModelHandle, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
    let tokens = encode_single(handle.tokenizer(), text, handle.device())?;
    let mut out = forward_pooled(handle, &tokens)?;
    debug_assert_eq!(out.len(), 1);
    out.pop()
        .ok_or_else(|| EmbedError::NumericFailure("empty forward_pooled output".into()))
}

/// Convenience: tokenise + forward + pool + normalise for a batch.
pub fn embed_batch(
    handle: &ModelHandle,
    texts: &[&str],
) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
    let tokens = encode_batch(handle.tokenizer(), texts, handle.device())?;
    forward_pooled(handle, &tokens)
}

/// Extract `[CLS]` from BertModel's `(batch, seq_len, hidden_dim)`
/// output. The result is `(batch, hidden_dim)`.
fn extract_cls(hidden: &Tensor) -> Result<Tensor, EmbedError> {
    hidden
        .narrow(1, 0, 1)
        .and_then(|t| t.squeeze(1))
        .map_err(|e| EmbedError::NumericFailure(format!("extract_cls: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_unit_vector_unchanged() {
        let mut v = [0.0f32; VECTOR_DIM];
        v[0] = 1.0;
        let n = l2_normalize_in_place(&mut v);
        assert!((n - 1.0).abs() < 1e-6);
        assert!((v[0] - 1.0).abs() < 1e-6);
        for x in &v[1..] {
            assert_eq!(*x, 0.0);
        }
    }

    #[test]
    fn normalise_norm_two_to_unit() {
        let mut v = [0.0f32; VECTOR_DIM];
        v[0] = 2.0;
        let n = l2_normalize_in_place(&mut v);
        assert!((n - 2.0).abs() < 1e-6);
        // After normalisation: v[0] == 1.0.
        assert!((v[0] - 1.0).abs() < 1e-6);

        let post_norm_sq: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((post_norm_sq - 1.0).abs() < 1e-6);
    }

    #[test]
    fn normalise_arbitrary_vector_becomes_unit() {
        // Pseudo-random but deterministic.
        let mut v = [0.0f32; VECTOR_DIM];
        for (i, slot) in v.iter_mut().enumerate() {
            *slot = ((i as f32 * 0.731).sin() + 0.1) * ((i as f32).cos() + 1.5);
        }
        let n = l2_normalize_in_place(&mut v);
        assert!(n > 0.0);
        let post: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((post - 1.0).abs() < 1e-5, "post-norm = {post}");
    }

    #[test]
    fn normalise_zero_vector_left_alone() {
        let mut v = [0.0f32; VECTOR_DIM];
        let n = l2_normalize_in_place(&mut v);
        assert!(n < ZERO_NORM_EPS);
        assert!(v.iter().all(|x| *x == 0.0), "zero vector must stay zero");
    }

    #[test]
    fn normalise_near_zero_vector_left_alone() {
        let mut v = [0.0f32; VECTOR_DIM];
        v[0] = 1e-12; // below the eps
        let n = l2_normalize_in_place(&mut v);
        assert!(n < ZERO_NORM_EPS);
        assert_eq!(v[0], 1e-12, "near-zero contents must be untouched");
    }
}
