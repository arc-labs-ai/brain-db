//! `HnswIndex<const D: usize>` — const-generic wrapper around `hnsw_rs::Hnsw<f32, DistCosine>`.
//!
//! Spec references:
//! - `spec/06_ann_index/02_parameters.md` — defaults and ranges.
//! - `spec/06_ann_index/01_hnsw_primer.md` §7 — distance metric: cosine on
//!   L2-normalised vectors (BGE-small output, so cosine = dot product).
//! - `spec/06_ann_index/03_insertion.md` §29 — confirmed type alias
//!   `Hnsw<f32, DistCosine>`.
//! - `spec/06_ann_index/04_search.md` §1 — search returns sorted ascending
//!   by distance.
//!
//! ## Surface (sub-task 4.1 scope)
//!
//! - [`HnswIndex::new`] — construct with [`crate::params::IndexParams`].
//! - [`HnswIndex::insert`] — `&mut self` + raw `usize` id + `&[f32; D]`.
//! - [`HnswIndex::search`] — `&self` + `&[f32; D]` + `k` + optional ef
//!   override (clamped to `[k, params.ef_search_max]`).
//! - [`HnswIndex::len`] / [`HnswIndex::is_empty`].
//!
//! ## What's NOT here yet
//!
//! - **MemoryId mapping** — sub-task 4.2 adds the adapter that lets
//!   callers use `brain_core::MemoryId` instead of `usize`.
//! - **Tombstone filtering** — sub-task 4.3 + 4.4.
//! - **Persistence** — sub-task 4.5.
//! - **Rebuild** — sub-task 4.6.
//! - **Concurrency wrapper** (`ArcSwap` + pending buffer) — sub-task 4.8.
//!   Today, `&mut self` on insert encodes the single-writer discipline
//!   directly at the type level.

use hnsw_rs::prelude::{DistCosine, Hnsw, Neighbour};
use thiserror::Error;

use crate::params::{IndexParams, IndexParamsError, DEFAULT_CAPACITY_HINT, MAX_LAYER};

/// HNSW index parameterised by vector dimension `D`. Wraps
/// `hnsw_rs::Hnsw<f32, DistCosine>` with Brain's parameter discipline.
///
/// **Single-writer:** `insert` takes `&mut self`. hnsw_rs itself only
/// requires `&self` (it uses internal locking for its unused
/// multi-writer mode, spec `§06/08 §8`), but Brain's discipline
/// (CLAUDE.md §5 invariant 2) tightens this at the type level.
pub struct HnswIndex<const D: usize> {
    inner: Hnsw<'static, f32, DistCosine>,
    params: IndexParams,
    /// Local count, since `hnsw_rs::Hnsw::get_nb_point()` exists but we
    /// keep our own counter for `len()` to avoid touching hnsw_rs's
    /// internal locks on a hot path.
    len: usize,
}

/// Errors from [`HnswIndex`] construction and operations.
///
/// Sub-task 4.1 only surfaces parameter-validation failures. Persistence
/// (4.5) and rebuild (4.6) will extend this enum with I/O variants.
#[derive(Debug, Error)]
pub enum HnswError {
    #[error("invalid params: {0}")]
    InvalidParams(#[from] IndexParamsError),
}

impl<const D: usize> HnswIndex<D> {
    /// Build a fresh empty index using the given parameters.
    ///
    /// Validates `params` against `spec/06_ann_index/02_parameters.md`'s
    /// ranges. Pre-allocates internal tables sized to
    /// [`crate::params::DEFAULT_CAPACITY_HINT`]; this is a hint, not a cap.
    pub fn new(params: IndexParams) -> Result<Self, HnswError> {
        params.validate()?;
        let inner = Hnsw::<f32, DistCosine>::new(
            params.m,
            DEFAULT_CAPACITY_HINT,
            MAX_LAYER,
            params.ef_construction,
            DistCosine,
        );
        Ok(Self {
            inner,
            params,
            len: 0,
        })
    }

    /// Insert a vector under the supplied internal id. Single-writer per
    /// shard — encoded via `&mut self`.
    ///
    /// `id` is the raw `usize` hnsw_rs uses internally. Sub-task 4.2
    /// adds the `MemoryId` adapter layer.
    pub fn insert(&mut self, id: usize, vector: &[f32; D]) {
        // `Hnsw::insert_slice` takes a `(&[T], usize)` tuple.
        self.inner.insert_slice((vector.as_slice(), id));
        self.len += 1;
    }

    /// Search for the `k` nearest neighbours of `query`.
    ///
    /// `ef` overrides the per-query search width:
    /// - `None` → uses `params.ef_search`.
    /// - `Some(v)` → clamped to `[k, params.ef_search_max]` per
    ///   `spec/06_ann_index/02_parameters.md` §5 (the `ef = max(K, default)`
    ///   rule) and §8 (the `ef_search_max` cap).
    ///
    /// Results are sorted ascending by distance (best match first), matching
    /// `hnsw_rs`'s contract.
    #[must_use]
    pub fn search(&self, query: &[f32; D], k: usize, ef: Option<usize>) -> Vec<(usize, f32)> {
        let ef = self.resolve_ef(k, ef);
        let neighbours: Vec<Neighbour> = self.inner.search(query.as_slice(), k, ef);
        neighbours
            .into_iter()
            .map(|n| (n.d_id, n.distance))
            .collect()
    }

    /// Number of vectors inserted. Cheap; does not touch hnsw_rs's locks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The parameters this index was built with. Useful when sub-task 4.5
    /// (persistence) writes the snapshot header.
    #[must_use]
    pub fn params(&self) -> IndexParams {
        self.params
    }

    /// Compute the effective `ef` for a search per spec §02 §5 + §8:
    ///
    /// - Floor at `k` (hnsw_rs requires `ef >= k` for k results).
    /// - Ceiling at `params.ef_search_max`.
    fn resolve_ef(&self, k: usize, override_ef: Option<usize>) -> usize {
        let base = override_ef.unwrap_or(self.params.ef_search);
        base.max(k).min(self.params.ef_search_max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec4(a: f32, b: f32, c: f32, d: f32) -> [f32; 4] {
        // Normalise so cosine distance behaves cleanly.
        let n = (a * a + b * b + c * c + d * d).sqrt();
        [a / n, b / n, c / n, d / n]
    }

    fn params_d4() -> IndexParams {
        // Use spec defaults but with ef_search at the spec's minimum so
        // we exercise the per-query override path comfortably.
        IndexParams::default_v1()
    }

    #[test]
    fn new_with_defaults() {
        let idx = HnswIndex::<4>::new(params_d4()).unwrap();
        assert_eq!(idx.len(), 0);
        assert!(idx.is_empty());
        assert_eq!(idx.params(), IndexParams::default_v1());
    }

    #[test]
    fn new_rejects_invalid_params() {
        let mut bad = IndexParams::default_v1();
        bad.m = 0;
        // `HnswIndex` doesn't impl `Debug` (hnsw_rs's `Hnsw` doesn't either),
        // so we match the `Err` manually rather than `.unwrap_err()`.
        match HnswIndex::<4>::new(bad) {
            Err(HnswError::InvalidParams(IndexParamsError::MOutOfRange(0))) => {}
            Err(e) => panic!("wrong error: {e}"),
            Ok(_) => panic!("expected validation failure"),
        }
    }

    #[test]
    fn insert_increments_len() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(1, &vec4(1.0, 0.0, 0.0, 0.0));
        idx.insert(2, &vec4(0.0, 1.0, 0.0, 0.0));
        idx.insert(3, &vec4(0.0, 0.0, 1.0, 0.0));
        assert_eq!(idx.len(), 3);
        assert!(!idx.is_empty());
    }

    #[test]
    fn identical_vector_self_match_returns_distance_near_zero() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        let v = vec4(0.5, 0.5, 0.5, 0.5);
        idx.insert(42, &v);
        let results = idx.search(&v, 1, None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 42);
        assert!(
            results[0].1.abs() < 1e-5,
            "expected ~0 distance, got {}",
            results[0].1
        );
    }

    #[test]
    fn search_returns_at_most_k() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        for i in 1..=5u8 {
            let f = f32::from(i);
            idx.insert(i as usize, &vec4(f, f * 2.0, f * 3.0, f * 4.0));
        }
        let q = vec4(1.0, 2.0, 3.0, 4.0);
        let results = idx.search(&q, 3, None);
        assert!(results.len() <= 3, "got {} results", results.len());
    }

    #[test]
    fn search_results_are_sorted_ascending() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        // Five distinct unit vectors pointing in different directions.
        idx.insert(1, &vec4(1.0, 0.1, 0.0, 0.0));
        idx.insert(2, &vec4(0.9, 0.5, 0.0, 0.0));
        idx.insert(3, &vec4(0.5, 0.9, 0.0, 0.0));
        idx.insert(4, &vec4(0.1, 1.0, 0.0, 0.0));
        idx.insert(5, &vec4(0.0, 0.0, 1.0, 1.0));
        let q = vec4(1.0, 0.0, 0.0, 0.0);
        let results = idx.search(&q, 5, None);
        // Distances should be non-decreasing.
        for w in results.windows(2) {
            assert!(
                w[0].1 <= w[1].1 + 1e-6,
                "distances out of order: {} > {}",
                w[0].1,
                w[1].1
            );
        }
    }

    #[test]
    fn ef_search_max_caps_per_query_override() {
        // Spec §02 §8: ef_search_max=500 by default; per-query overrides
        // exceeding it must be clamped. We don't have a way to peek at
        // the ef hnsw_rs actually used, so we exercise the boundary via
        // the public surface: an enormous override must not panic, must
        // honour k.
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(1, &vec4(1.0, 0.0, 0.0, 0.0));
        idx.insert(2, &vec4(0.0, 1.0, 0.0, 0.0));
        let q = vec4(1.0, 0.0, 0.0, 0.0);
        // 9999 well above ef_search_max=500; clamps inside resolve_ef.
        let results = idx.search(&q, 2, Some(9999));
        assert!(results.len() <= 2);
        // Top hit is id=1 (closer).
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn empty_index_search_returns_empty() {
        let idx = HnswIndex::<4>::new(params_d4()).unwrap();
        let q = vec4(1.0, 0.0, 0.0, 0.0);
        let results = idx.search(&q, 5, None);
        assert!(results.is_empty());
    }

    #[test]
    fn resolve_ef_clamps_to_k_and_ef_search_max() {
        // Direct unit test of the clamp helper via the public path.
        let idx = HnswIndex::<4>::new(IndexParams::default_v1()).unwrap();
        // None → ef_search (64), bumped to k=128 → still ≤ ef_search_max (500).
        assert_eq!(idx.resolve_ef(128, None), 128);
        // None with k below ef_search → uses ef_search.
        assert_eq!(idx.resolve_ef(10, None), 64);
        // Override above ef_search_max → clamped.
        assert_eq!(idx.resolve_ef(10, Some(9999)), 500);
        // Override below k → bumped to k.
        assert_eq!(idx.resolve_ef(100, Some(50)), 100);
    }
}
