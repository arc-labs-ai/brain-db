//! Inference pipeline: model loading, tokenization, and forward pass.
//!
//! - [`load`] — `ModelHandle::load` (config + tokenizer + safetensors
//!   weights + fingerprint + warm-up).
//! - [`tokenize`] — BERT WordPiece wrapper; encode single + batch.
//! - [`forward`] — forward pass + mean-pool + L2-normalise.

pub mod forward;
pub mod load;
pub mod tokenize;

pub use forward::{
    embed_batch, embed_text, forward_pooled, l2_normalize_in_place, matmul_split_count, VECTOR_DIM,
};
pub use load::ModelHandle;
pub use tokenize::{encode_batch, encode_single, Tokenized, MAX_TOKEN_LENGTH};
