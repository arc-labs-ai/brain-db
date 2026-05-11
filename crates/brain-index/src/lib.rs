//! # brain-index
//!
//! Approximate-nearest-neighbour index for Brain, wrapping `hnsw_rs::Hnsw`
//! with the parameters and lifecycle (build, search, snapshot, rebuild)
//! defined in `spec/06_ann_index/`.
//!
//! This crate is a **closed leaf**: vectors in, candidates out. It has
//! no dependency on `brain-storage` or `brain-metadata`; the cross-crate
//! composition (rebuilding the HNSW from arena slots + active-memory
//! scans) lives in a higher-layer crate from Phase 7 onward.
//!
//! ## Current surface (sub-task 4.1)
//!
//! - [`IndexParams`] — HNSW knobs with spec defaults
//!   (`M=16, ef_construction=200, ef_search=64, ef_search_max=500`).
//! - [`HnswIndex<D>`] — const-generic over vector dim. Production use
//!   pins `D = `[`VECTOR_DIM`] (= 384 for BGE-small).
//!
//! Later sub-tasks add `MemoryId` mapping (4.2), tombstone filtering
//! (4.3/4.4), persistence (4.5), rebuild (4.6), the recall benchmark
//! (4.7), and the `ArcSwap` concurrency wrapper (4.8).

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod hnsw;
pub mod idmap;
pub mod params;
pub mod persistence;
pub mod rebuild;
pub mod shared;
pub mod tombstones;

pub use hnsw::{HnswError, HnswIndex};
pub use idmap::{IdMap, IdMapError};
pub use params::{IndexParams, IndexParamsError, MAX_LAYER, VECTOR_DIM};
pub use rebuild::RebuildReport;
pub use shared::{SharedHnsw, Writer};
pub use tombstones::TombstoneBitmap;
