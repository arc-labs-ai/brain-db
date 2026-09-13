//! # brain-embed
//!
//! Substrate-owned embedding layer. Clients send text; the substrate
//! runs the embedding model (BGE-small-en-v1.5 by default) and returns
//! a 384-dim L2-normalised `f32` vector.
//!
//! ## Module map
//!
//! - [`model`] — the inference pipeline (model load, tokenize, forward).
//! - [`dispatcher`] — caller-facing surface (`Dispatcher` trait,
//!   `CpuDispatcher`, `CachingDispatcher`).
//! - [`config`] — `EmbedderConfig` (model path, device, dtype, warm-up).
//! - [`fingerprint`] — `compute_fingerprint` over weights + tokenizer +
//!   config bytes; used by storage to detect cross-model bytes.
//! - [`error`] — `EmbedError` taxonomy.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod config;
pub mod dispatcher;
pub mod error;
pub mod fingerprint;
pub mod model;

pub use config::EmbedderConfig;
pub use dispatcher::{
    CacheStats, CachingDispatcher, CpuDispatcher, Dispatcher, BGE_QUERY_PREFIX, DEFAULT_CACHE_SIZE,
};
pub use error::EmbedError;
pub use fingerprint::{blake3_hash_file, blake3_hash_text, compute_fingerprint};
pub use model::{
    embed_batch, embed_text, encode_batch, encode_single, forward_pooled, l2_normalize_in_place,
    ModelHandle, Tokenized, MAX_TOKEN_LENGTH, VECTOR_DIM,
};
