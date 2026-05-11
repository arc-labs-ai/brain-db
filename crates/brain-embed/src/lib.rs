//! # brain-embed
//!
//! Substrate-owned embedding layer. Clients send text; the substrate
//! runs the embedding model (BGE-small-en-v1.5 by default) and returns
//! a 384-dim L2-normalised `f32` vector.
//!
//! See `spec/04_embedding_layer/` for the authoritative design.
//!
//! ## Current surface (sub-task 5.1)
//!
//! - [`EmbedderConfig`] — model path + device + dtype + warm-up
//!   iterations.
//! - [`ModelHandle::load`] — full load sequence (config + tokenizer +
//!   safetensors weights + fingerprint + warm-up).
//! - [`compute_fingerprint`] — pure function implementing
//!   `spec/04_embedding_layer/07_fingerprinting.md` §3 byte-for-byte.
//! - [`EmbedError`] — typed errors for the load path.
//!
//! Later sub-tasks add the user-facing `Embedder` facade (5.x),
//! tokeniser wrapper (5.2), forward + pool + normalise (5.3), batcher
//! (5.4), LRU cache (5.5), determinism test (5.6), throughput bench
//! (5.7).

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod cache;
pub mod config;
pub mod dispatcher;
pub mod error;
pub mod fingerprint;
pub mod forward;
pub mod model;
pub mod tokenize;

pub use cache::{CacheStats, CachingDispatcher, DEFAULT_CACHE_SIZE};
pub use config::EmbedderConfig;
pub use dispatcher::{CpuDispatcher, Dispatcher};
pub use error::EmbedError;
pub use fingerprint::{blake3_hash_file, blake3_hash_text, compute_fingerprint};
pub use forward::{embed_batch, embed_text, forward_pooled, l2_normalize_in_place, VECTOR_DIM};
pub use model::ModelHandle;
pub use tokenize::{encode_batch, encode_single, Tokenized, MAX_TOKEN_LENGTH};
