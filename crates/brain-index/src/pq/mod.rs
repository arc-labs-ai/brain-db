//! Product Quantization for the HNSW indexes.
//!
//! An opt-in compression layer that swaps the HNSW graph payload from
//! full-precision `[f32; D]` to `[u8; M]` PQ codes. The arena keeps
//! full-precision vectors so the search path can re-rank against
//! exact distances and recover recall.
//!
//! Layered intentionally:
//!
//! - [`params`]: `PqParams` knobs + validation (no runtime state).
//! - [`codebook`]: the trained quantiser (immutable artefact).
//! - [`kmeans`]: deterministic trainer that produces a [`Codebook`].

pub mod bootstrap;
pub mod codebook;
pub mod distance;
pub mod encode;
pub mod kmeans;
pub mod params;
pub mod rerank;

pub use bootstrap::{bootstrap_codebook, BOOTSTRAP_M};
pub use codebook::{Codebook, CodebookError};
pub use distance::{adc, install_search_lut, sdc, Lut, LutGuard, PqDist, SdcTable};
pub use encode::{encode, encode_batch, EncodeError};
pub use kmeans::{train, KmeansError};
pub use params::{
    PqParams, PqParamsError, MAX_TRAINING_SAMPLE, MIN_TRAINING_SAMPLE, PQ_BITS_V1,
    PQ_CENTROIDS_PER_SUBSPACE,
};
pub use rerank::rerank;
