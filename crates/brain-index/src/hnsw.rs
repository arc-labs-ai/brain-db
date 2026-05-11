//! `HnswIndex<const D: usize>` — const-generic wrapper around `hnsw_rs::Hnsw<f32, DistCosine>`.
//!
//! Spec references:
//! - `spec/06_ann_index/02_parameters.md` — defaults and ranges.
//! - `spec/06_ann_index/01_hnsw_primer.md` §7 — distance metric: cosine on
//!   L2-normalised vectors (BGE-small output, so cosine = dot product).
//! - `spec/06_ann_index/03_insertion.md` §1–2, §10 — id_map pattern;
//!   duplicate-MemoryId is a bug we detect rather than letting hnsw_rs
//!   silently overwrite.
//! - `spec/06_ann_index/04_search.md` §1 — search returns sorted ascending
//!   by distance.
//!
//! ## Current surface (through sub-task 4.2)
//!
//! - [`HnswIndex::new`] — construct with [`crate::params::IndexParams`].
//! - [`HnswIndex::insert`] — `&mut self` + [`MemoryId`] + `&[f32; D]`.
//!   Returns [`HnswError::DuplicateMemoryId`] on re-insert.
//! - [`HnswIndex::search`] — `&self` + `&[f32; D]` + `k` + optional ef
//!   override (clamped to `[k, params.ef_search_max]`).
//!   Returns `Vec<(MemoryId, f32)>` sorted ascending by distance.
//! - [`HnswIndex::contains`], [`HnswIndex::len`], [`HnswIndex::is_empty`].
//!
//! ## What's NOT here yet
//!
//! - **Tombstone bitmap** — sub-task 4.3.
//! - **Search post-filter / tombstone awareness** — sub-task 4.4.
//! - **Persistence** — sub-task 4.5 (writes both the hnsw_rs graph and
//!   the [`crate::idmap::IdMap`] contents).
//! - **Rebuild from external iterator** — sub-task 4.6.
//! - **Concurrency wrapper** (`ArcSwap` + pending buffer) — sub-task 4.8.

use brain_core::MemoryId;
use hnsw_rs::prelude::{DistCosine, Hnsw, Neighbour};
use thiserror::Error;

use crate::idmap::{IdMap, IdMapError};
use crate::params::{IndexParams, IndexParamsError, DEFAULT_CAPACITY_HINT, MAX_LAYER};
use crate::tombstones::TombstoneBitmap;

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
    id_map: IdMap,
    tombstones: TombstoneBitmap,
}

/// Errors from [`HnswIndex`] construction and operations.
///
/// Persistence (4.5) and rebuild (4.6) will extend this enum with I/O
/// variants.
#[derive(Debug, Error)]
pub enum HnswError {
    #[error("invalid params: {0}")]
    InvalidParams(#[from] IndexParamsError),

    /// `memory_id` was already inserted. Per spec §06/03 §10 re-inserting
    /// an existing MemoryId is a caller bug; we detect rather than let
    /// hnsw_rs silently overwrite.
    #[error("duplicate memory_id: {memory_id_bytes:?}")]
    DuplicateMemoryId { memory_id_bytes: [u8; 16] },

    /// The internal `u32` id_map allocator hit `u32::MAX`. Spec's
    /// per-shard ceiling is ~10M memories — this is unreachable in
    /// practice; the check is defensive.
    #[error("id_map exhausted: u32::MAX internal ids allocated")]
    IdMapExhausted,

    /// State-changing operation referenced a `MemoryId` not present in
    /// the id_map. Spec `§06/05` calls re-tombstoning known memories;
    /// an unknown id is a caller bug.
    ///
    /// Note: the read-only [`HnswIndex::is_tombstoned`] returns `false`
    /// rather than this error — query paths are fail-soft.
    #[error("memory_id not found in id_map: {memory_id_bytes:?}")]
    MemoryIdNotFound { memory_id_bytes: [u8; 16] },
}

impl From<IdMapError> for HnswError {
    fn from(e: IdMapError) -> Self {
        match e {
            IdMapError::AlreadyInserted { memory_id_bytes } => {
                HnswError::DuplicateMemoryId { memory_id_bytes }
            }
            IdMapError::Exhausted => HnswError::IdMapExhausted,
        }
    }
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
            id_map: IdMap::new(),
            tombstones: TombstoneBitmap::new(),
        })
    }

    /// Insert `vector` under `memory_id`. Single-writer per shard —
    /// encoded via `&mut self`.
    ///
    /// Returns [`HnswError::DuplicateMemoryId`] if `memory_id` was
    /// already inserted; the index is unchanged on the duplicate path
    /// (no internal id burned). Spec §06/03 §10.
    pub fn insert(&mut self, memory_id: MemoryId, vector: &[f32; D]) -> Result<(), HnswError> {
        let internal_id = self.id_map.insert(memory_id)?;
        // `Hnsw::insert_slice` takes a `(&[T], usize)` tuple.
        self.inner
            .insert_slice((vector.as_slice(), internal_id as usize));
        Ok(())
    }

    /// Search for the `k` nearest neighbours of `query`. Returns
    /// `(MemoryId, distance)` tuples sorted ascending by distance
    /// (best match first).
    ///
    /// `ef` overrides the per-query search width:
    /// - `None` → uses `params.ef_search`.
    /// - `Some(v)` → clamped to `[k, params.ef_search_max]` per
    ///   `spec/06_ann_index/02_parameters.md` §5 (`ef = max(K, default)`)
    ///   and §8 (the `ef_search_max` cap).
    ///
    /// If hnsw_rs returns an internal id not present in this index's
    /// id_map (defensive — should never happen in practice), the
    /// corresponding result is dropped with a `tracing::warn!`.
    #[must_use]
    pub fn search(&self, query: &[f32; D], k: usize, ef: Option<usize>) -> Vec<(MemoryId, f32)> {
        let ef = self.resolve_ef(k, ef);
        let neighbours: Vec<Neighbour> = self.inner.search(query.as_slice(), k, ef);
        neighbours
            .into_iter()
            .filter_map(|n| {
                let internal_id = u32::try_from(n.d_id).ok()?;
                match self.id_map.lookup_reverse(internal_id) {
                    Some(memory_id) => Some((memory_id, n.distance)),
                    None => {
                        tracing::warn!(
                            internal_id,
                            "hnsw_rs returned an internal id with no MemoryId mapping; \
                             dropping result"
                        );
                        None
                    }
                }
            })
            .collect()
    }

    /// Does this index hold a vector for `memory_id`?
    #[must_use]
    pub fn contains(&self, memory_id: MemoryId) -> bool {
        self.id_map.contains(memory_id)
    }

    /// Mark `memory_id` as tombstoned. The node stays in the graph
    /// (spec `§06/05 §2`); search filtering at sub-task 4.4 drops
    /// tombstoned candidates from results.
    ///
    /// Returns [`HnswError::MemoryIdNotFound`] if `memory_id` isn't in
    /// the id_map.
    pub fn mark_tombstoned(&mut self, memory_id: MemoryId) -> Result<(), HnswError> {
        let internal_id =
            self.id_map
                .lookup_forward(memory_id)
                .ok_or(HnswError::MemoryIdNotFound {
                    memory_id_bytes: memory_id.to_be_bytes(),
                })?;
        self.tombstones.set(internal_id);
        Ok(())
    }

    /// Is `memory_id` tombstoned? Returns `false` for unknown ids —
    /// query paths are fail-soft.
    #[must_use]
    pub fn is_tombstoned(&self, memory_id: MemoryId) -> bool {
        match self.id_map.lookup_forward(memory_id) {
            Some(id) => self.tombstones.is_set(id),
            None => false,
        }
    }

    /// Running count of tombstoned memories in this index. O(1) per
    /// spec `§06/05 §13`'s `tombstone_ratio` metric expectation.
    #[must_use]
    pub fn tombstone_count(&self) -> usize {
        self.tombstones.count()
    }

    /// Number of vectors inserted. Cheap.
    #[must_use]
    pub fn len(&self) -> usize {
        self.id_map.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.id_map.is_empty()
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

    fn mid(slot: u64) -> MemoryId {
        MemoryId::pack(1, slot, 1)
    }

    fn params_d4() -> IndexParams {
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
    fn insert_with_memory_id_increments_len() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(2), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(3), &vec4(0.0, 0.0, 1.0, 0.0)).unwrap();
        assert_eq!(idx.len(), 3);
        assert!(!idx.is_empty());
    }

    #[test]
    fn identical_vector_self_match_returns_memory_id() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        let v = vec4(0.5, 0.5, 0.5, 0.5);
        let id = mid(42);
        idx.insert(id, &v).unwrap();
        let results = idx.search(&v, 1, None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, id);
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
            idx.insert(mid(u64::from(i)), &vec4(f, f * 2.0, f * 3.0, f * 4.0))
                .unwrap();
        }
        let q = vec4(1.0, 2.0, 3.0, 4.0);
        let results = idx.search(&q, 3, None);
        assert!(results.len() <= 3, "got {} results", results.len());
    }

    #[test]
    fn search_results_are_sorted_ascending() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.1, 0.0, 0.0)).unwrap();
        idx.insert(mid(2), &vec4(0.9, 0.5, 0.0, 0.0)).unwrap();
        idx.insert(mid(3), &vec4(0.5, 0.9, 0.0, 0.0)).unwrap();
        idx.insert(mid(4), &vec4(0.1, 1.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(5), &vec4(0.0, 0.0, 1.0, 1.0)).unwrap();
        let q = vec4(1.0, 0.0, 0.0, 0.0);
        let results = idx.search(&q, 5, None);
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
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(2), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        let q = vec4(1.0, 0.0, 0.0, 0.0);
        // 9999 well above ef_search_max=500; clamps inside resolve_ef.
        let results = idx.search(&q, 2, Some(9999));
        assert!(results.len() <= 2);
        // Top hit is mid(1) (closer to the query).
        assert_eq!(results[0].0, mid(1));
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

    // ----- 4.2-specific tests --------------------------------------------

    #[test]
    fn duplicate_memory_id_returns_error() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        // Second insert of the same MemoryId rejects.
        match idx.insert(mid(1), &vec4(0.0, 1.0, 0.0, 0.0)) {
            Err(HnswError::DuplicateMemoryId { memory_id_bytes }) => {
                assert_eq!(memory_id_bytes, mid(1).to_be_bytes());
            }
            Err(e) => panic!("wrong error: {e}"),
            Ok(()) => panic!("expected DuplicateMemoryId"),
        }
        assert_eq!(idx.len(), 1, "duplicate insert must not advance len");
    }

    #[test]
    fn search_results_carry_memory_ids() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(100), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(200), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        let results = idx.search(&vec4(1.0, 0.1, 0.0, 0.0), 2, None);
        let ids: Vec<MemoryId> = results.iter().map(|(id, _)| *id).collect();
        assert!(
            ids.contains(&mid(100)),
            "expected mid(100) in {:?}",
            results
        );
        assert!(
            ids.contains(&mid(200)),
            "expected mid(200) in {:?}",
            results
        );
    }

    #[test]
    fn contains_after_insert() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        assert!(!idx.contains(mid(7)));
        idx.insert(mid(7), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        assert!(idx.contains(mid(7)));
        assert!(!idx.contains(mid(8)));
    }

    // ----- 4.3-specific tests --------------------------------------------

    #[test]
    fn mark_tombstoned_consults_idmap() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        assert!(!idx.is_tombstoned(mid(1)));
        idx.mark_tombstoned(mid(1)).unwrap();
        assert!(idx.is_tombstoned(mid(1)));
        assert_eq!(idx.tombstone_count(), 1);
    }

    #[test]
    fn mark_tombstoned_unknown_returns_error() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        match idx.mark_tombstoned(mid(999)) {
            Err(HnswError::MemoryIdNotFound { memory_id_bytes }) => {
                assert_eq!(memory_id_bytes, mid(999).to_be_bytes());
            }
            Err(e) => panic!("wrong error: {e}"),
            Ok(()) => panic!("expected MemoryIdNotFound"),
        }
        assert_eq!(idx.tombstone_count(), 0);
    }

    #[test]
    fn is_tombstoned_unknown_returns_false() {
        // Query path is fail-soft: unknown MemoryId is not an error.
        let idx = HnswIndex::<4>::new(params_d4()).unwrap();
        assert!(!idx.is_tombstoned(mid(999)));
    }

    #[test]
    fn tombstone_count_pin() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(2), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(3), &vec4(0.0, 0.0, 1.0, 0.0)).unwrap();
        assert_eq!(idx.tombstone_count(), 0);
        idx.mark_tombstoned(mid(1)).unwrap();
        idx.mark_tombstoned(mid(2)).unwrap();
        assert_eq!(idx.tombstone_count(), 2);
        // mid(3) untouched.
        assert!(idx.is_tombstoned(mid(1)));
        assert!(idx.is_tombstoned(mid(2)));
        assert!(!idx.is_tombstoned(mid(3)));
    }
}
