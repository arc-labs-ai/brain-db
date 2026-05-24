//! `HnswIndex<const M: usize>` — HNSW with PQ-compressed payload.
//!
//! Mirrors the [`crate::hnsw::HnswIndex`] surface (insert, search,
//! tombstone, len) but stores `[u8; M]` PQ codes in the graph instead
//! of full-precision `[f32; D]` vectors. The query side stays `f32`
//! so the search wrapper can build an ADC LUT.
//!
//! Re-rank against the full-precision arena is intentionally NOT done
//! here — task 25.5 layers it on top via [`crate::hnsw::rerank`]
//! once the arena reader trait threads through. The PQ search result
//! ordering is therefore ADC-approximate; callers that need exact
//! ranking call the re-rank helper.
//!
//! Spec: `spec/09_indexing/07_hnsw_pq.md` §7.

use std::sync::Arc;

use brain_core::MemoryId;
use hnsw_rs::prelude::{Hnsw, Neighbour};
use thiserror::Error;

use crate::idmap::{IdMap, IdMapError};
use crate::params::{IndexParams, IndexParamsError, DEFAULT_CAPACITY_HINT, MAX_LAYER, VECTOR_DIM};
use crate::pq::{encode, install_search_lut, Codebook, CodebookError, EncodeError, Lut, PqDist};
use crate::tombstones::TombstoneBitmap;

/// Default over-fetch multiplier for the per-search bailout loop.
/// Mirrors `HnswIndex`'s constant; the bailout escalation rules apply
/// identically.
const OVER_FACTOR: usize = 2;

/// HNSW with PQ-compressed payload, parameterised by subquantiser
/// count `M`. The full vector dimension is fixed at [`VECTOR_DIM`] —
/// PQ's training assumes the BGE-small embedding shape and the
/// codebook bakes it in.
///
/// **Single-writer:** `insert` takes `&mut self`. See
/// [`HnswIndex`]'s preamble for the discipline.
pub struct HnswIndex<const M: usize> {
    inner: Hnsw<'static, u8, PqDist<M>>,
    params: IndexParams,
    codebook: Arc<Codebook<M>>,
    id_map: IdMap,
    tombstones: TombstoneBitmap,
}

#[derive(Debug, Error)]
pub enum HnswError {
    #[error("invalid params: {0}")]
    InvalidParams(#[from] IndexParamsError),

    #[error("codebook invalid: {0}")]
    CodebookMismatch(#[from] CodebookError),

    #[error("duplicate memory_id: {memory_id_bytes:?}")]
    DuplicateMemoryId { memory_id_bytes: [u8; 16] },

    #[error("id_map exhausted: u32::MAX internal ids allocated")]
    IdMapExhausted,

    #[error("memory_id not found in id_map: {memory_id_bytes:?}")]
    MemoryIdNotFound { memory_id_bytes: [u8; 16] },

    #[error("encoding failed: {0}")]
    Encode(#[from] EncodeError),
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

impl<const M: usize> HnswIndex<M> {
    /// Build a fresh empty index over PQ codes. The codebook is moved
    /// into an `Arc` and shared with the inner [`PqDist`] — cloning
    /// the index handle would re-share the same codebook by reference.
    pub fn new(params: IndexParams, codebook: Codebook<M>) -> Result<Self, HnswError> {
        params.validate()?;
        let dist = PqDist::<M>::new(&codebook);
        let inner = Hnsw::<u8, PqDist<M>>::new(
            params.m,
            DEFAULT_CAPACITY_HINT,
            MAX_LAYER,
            params.ef_construction,
            dist,
        );
        Ok(Self {
            inner,
            params,
            codebook: Arc::new(codebook),
            id_map: IdMap::new(),
            tombstones: TombstoneBitmap::new(),
        })
    }

    /// Insert a full-precision vector. The vector is PQ-encoded
    /// internally; only the `[u8; M]` code lands in the HNSW graph.
    /// Callers retain the full-precision vector in the arena for the
    /// later re-rank pass.
    pub fn insert(
        &mut self,
        memory_id: MemoryId,
        vector: &[f32; VECTOR_DIM],
    ) -> Result<(), HnswError> {
        let code = encode::<M>(vector, &self.codebook)?;
        let internal_id = self.id_map.insert(memory_id)?;
        // `Hnsw::insert_slice` accepts `(&[T], usize)`.
        self.inner.insert_slice((&code[..], internal_id as usize));
        Ok(())
    }

    /// Search for the `k` ADC-nearest candidates. The result ordering
    /// is ADC-approximate — exact ranking against full-precision
    /// vectors is the re-rank pass's job (task 25.5).
    ///
    /// `ef` works the same as in [`HnswIndex::search`]: `None` uses
    /// `params.ef_search`; `Some` clamps to `[k, params.ef_search_max]`.
    ///
    /// Tombstoned memories are always excluded.
    #[must_use]
    pub fn search<F>(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
        ef: Option<usize>,
        filter: F,
    ) -> Vec<(MemoryId, f32)>
    where
        F: Fn(MemoryId) -> bool,
    {
        if k == 0 || self.is_empty() {
            return Vec::new();
        }

        // Build the LUT once per search and install it for the inner
        // traversal. The guard clears the thread-local on drop, so an
        // early return or panic doesn't leak the LUT to the next call.
        let lut = Arc::new(Lut::<M>::build(query, &self.codebook));
        let _guard = install_search_lut::<M>(Arc::clone(&lut));

        // The query "code" is only consumed for hnsw_rs's API surface
        // — `PqDist::eval` reads the LUT, ignores the query bytes.
        // Encoding the query also gives the construction path a real
        // reference if any internal call sneaks in unexpectedly.
        let query_code = encode::<M>(query, &self.codebook).unwrap_or([0u8; M]);

        let total_nodes = self.len();
        let mut ef = self.resolve_ef(k, ef);
        let mut fetch_multiplier = OVER_FACTOR;
        let mut results: Vec<(MemoryId, f32)> = Vec::with_capacity(k);

        loop {
            results.clear();
            let fetch_k = k.saturating_mul(fetch_multiplier).min(total_nodes);
            let neighbours: Vec<Neighbour> = self.inner.search(&query_code[..], fetch_k, ef);

            for n in neighbours {
                if results.len() >= k {
                    break;
                }
                let Ok(internal_id) = u32::try_from(n.d_id) else {
                    continue;
                };
                if self.tombstones.is_set(internal_id) {
                    continue;
                }
                let Some(memory_id) = self.id_map.lookup_reverse(internal_id) else {
                    tracing::warn!(
                        internal_id,
                        "hnsw_rs returned an internal id with no MemoryId mapping; dropping",
                    );
                    continue;
                };
                if !filter(memory_id) {
                    continue;
                }
                // n.distance is ADC squared L2 between query and the
                // candidate's centroid composition. Re-rank converts
                // this back to cosine similarity against the full-
                // precision vector; without re-rank, we hand callers
                // the raw ADC distance for ordering only.
                results.push((memory_id, n.distance));
            }

            if results.len() >= k {
                break;
            }
            let fetch_saturated = fetch_k >= total_nodes;
            let ef_saturated = ef >= self.params.ef_search_max;
            if fetch_saturated && ef_saturated {
                tracing::debug!(
                    requested_k = k,
                    returned = results.len(),
                    "pq search bailout exhausted; returning partial results",
                );
                break;
            }
            if !fetch_saturated {
                fetch_multiplier = fetch_multiplier.saturating_mul(2);
            }
            if !ef_saturated {
                ef = ef.saturating_mul(2).min(self.params.ef_search_max);
            }
        }

        results
    }

    /// Convenience: search with no extra filter.
    #[must_use]
    pub fn search_all(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
        ef: Option<usize>,
    ) -> Vec<(MemoryId, f32)> {
        self.search(query, k, ef, |_| true)
    }

    /// Mark a memory as tombstoned. Search skips tombstoned candidates
    /// implicitly. Returns [`HnswError::MemoryIdNotFound`] if the id
    /// was never inserted.
    pub fn mark_tombstoned(&mut self, memory_id: MemoryId) -> Result<(), HnswError> {
        let internal_id = self
            .id_map
            .lookup_forward(memory_id)
            .ok_or(HnswError::MemoryIdNotFound {
                memory_id_bytes: memory_id.to_be_bytes(),
            })?;
        self.tombstones.set(internal_id);
        Ok(())
    }

    /// `true` if `memory_id` is currently tombstoned. Returns `false`
    /// for unknown ids (read-only paths are fail-soft).
    #[must_use]
    pub fn is_tombstoned(&self, memory_id: MemoryId) -> bool {
        self.id_map
            .lookup_forward(memory_id)
            .is_some_and(|internal| self.tombstones.is_set(internal))
    }

    /// True iff `memory_id` was inserted (regardless of tombstone state).
    #[must_use]
    pub fn contains(&self, memory_id: MemoryId) -> bool {
        self.id_map.lookup_forward(memory_id).is_some()
    }

    /// Total nodes in the HNSW (tombstoned or not).
    #[must_use]
    pub fn len(&self) -> usize {
        self.id_map.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.id_map.len() == 0
    }

    /// Number of tombstoned entries — bits set in the local
    /// tombstone bitmap.
    #[must_use]
    pub fn tombstone_count(&self) -> usize {
        self.tombstones.count()
    }

    /// Borrow the trained codebook the index was built from. The
    /// re-rank pass needs read access to the same codebook the index
    /// was encoded against.
    #[must_use]
    pub fn codebook(&self) -> &Arc<Codebook<M>> {
        &self.codebook
    }

    /// The configured params, including the active [`PqParams`].
    #[must_use]
    pub fn params(&self) -> IndexParams {
        self.params
    }

    /// Resolve the effective `ef_search` for a single query. Mirrors
    /// the clamp rules in [`HnswIndex::search`].
    fn resolve_ef(&self, k: usize, ef: Option<usize>) -> usize {
        let requested = ef.unwrap_or(self.params.ef_search);
        requested.max(k).min(self.params.ef_search_max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pq::PQ_CENTROIDS_PER_SUBSPACE;

    /// Same hand-built codebook fixture used by the distance tests:
    /// subspace `s` centroid `k` is `[k, 0, 0, ...]`. Distances are
    /// trivially hand-checkable.
    fn arithmetic_codebook<const M: usize>() -> Codebook<M> {
        let sub_dim = VECTOR_DIM / M;
        let mut centroids = vec![0.0_f32; M * PQ_CENTROIDS_PER_SUBSPACE * sub_dim];
        for s in 0..M {
            for k in 0..PQ_CENTROIDS_PER_SUBSPACE {
                let offset = (s * PQ_CENTROIDS_PER_SUBSPACE + k) * sub_dim;
                centroids[offset] = k as f32;
            }
        }
        Codebook::<M>::from_trained(centroids, sub_dim).unwrap()
    }

    fn pq_params_default() -> IndexParams {
        IndexParams::default_v1()
    }

    fn mid(slot: u8) -> MemoryId {
        MemoryId::pack(1, slot as u64, 1)
    }

    /// Build a 384-dim vector whose first component in each subspace
    /// equals the per-subspace target index. Encoding such a vector
    /// against [`arithmetic_codebook`] yields code `[t0, t1, ..., t7]`.
    fn vec_with_targets(targets: [u8; 8]) -> [f32; VECTOR_DIM] {
        let sub_dim = VECTOR_DIM / 8;
        let mut v = [0.0_f32; VECTOR_DIM];
        for (s, &t) in targets.iter().enumerate() {
            v[s * sub_dim] = t as f32;
        }
        v
    }

    #[test]
    fn insert_then_search_returns_inserted_id() {
        let mut idx = HnswIndex::<8>::new(pq_params_default(), arithmetic_codebook::<8>()).unwrap();
        let v = vec_with_targets([10, 10, 10, 10, 10, 10, 10, 10]);
        idx.insert(mid(1), &v).unwrap();
        let results = idx.search_all(&v, 1, None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, mid(1));
    }

    #[test]
    fn duplicate_insert_rejected() {
        let mut idx = HnswIndex::<8>::new(pq_params_default(), arithmetic_codebook::<8>()).unwrap();
        let v = vec_with_targets([5, 5, 5, 5, 5, 5, 5, 5]);
        idx.insert(mid(2), &v).unwrap();
        let result = idx.insert(mid(2), &v);
        assert!(matches!(result, Err(HnswError::DuplicateMemoryId { .. })));
    }

    #[test]
    fn nan_vector_rejected_at_insert() {
        let mut idx = HnswIndex::<8>::new(pq_params_default(), arithmetic_codebook::<8>()).unwrap();
        let mut v = vec_with_targets([1, 1, 1, 1, 1, 1, 1, 1]);
        v[100] = f32::NAN;
        let result = idx.insert(mid(3), &v);
        assert!(matches!(
            result,
            Err(HnswError::Encode(EncodeError::NotANumber))
        ));
    }

    #[test]
    fn search_excludes_tombstoned() {
        let mut idx = HnswIndex::<8>::new(pq_params_default(), arithmetic_codebook::<8>()).unwrap();
        for i in 0..5 {
            idx.insert(mid(i), &vec_with_targets([i, i, i, i, i, i, i, i])).unwrap();
        }
        idx.mark_tombstoned(mid(2)).unwrap();

        let results = idx.search_all(&vec_with_targets([2, 2, 2, 2, 2, 2, 2, 2]), 5, None);
        assert!(
            results.iter().all(|(id, _)| *id != mid(2)),
            "tombstoned id leaked: {results:?}"
        );
    }

    #[test]
    fn search_ranks_closer_first() {
        let mut idx = HnswIndex::<8>::new(pq_params_default(), arithmetic_codebook::<8>()).unwrap();
        // Insert vectors that encode to per-subspace targets 0..16.
        for t in 0..16 {
            idx.insert(mid(t as u8), &vec_with_targets([t; 8])).unwrap();
        }
        // Query that encodes near target=10.
        let results = idx.search_all(&vec_with_targets([10; 8]), 3, None);
        assert_eq!(results.len(), 3);
        // Closest must be id=10 (encodes to [10;8]; ADC distance 0).
        assert_eq!(results[0].0, mid(10));
        // ADC distance is non-decreasing across the top-K.
        for w in results.windows(2) {
            assert!(
                w[0].1 <= w[1].1,
                "results not sorted ascending by ADC distance: {results:?}"
            );
        }
    }

    #[test]
    fn search_empty_index_returns_empty() {
        let idx = HnswIndex::<8>::new(pq_params_default(), arithmetic_codebook::<8>()).unwrap();
        let results = idx.search_all(&vec_with_targets([0; 8]), 5, None);
        assert!(results.is_empty());
    }

    #[test]
    fn search_zero_k_returns_empty() {
        let mut idx = HnswIndex::<8>::new(pq_params_default(), arithmetic_codebook::<8>()).unwrap();
        idx.insert(mid(1), &vec_with_targets([1; 8])).unwrap();
        let results = idx.search_all(&vec_with_targets([1; 8]), 0, None);
        assert!(results.is_empty());
    }

    #[test]
    fn filter_excludes_unwanted_ids() {
        let mut idx = HnswIndex::<8>::new(pq_params_default(), arithmetic_codebook::<8>()).unwrap();
        for t in 0..5u8 {
            idx.insert(mid(t), &vec_with_targets([t; 8])).unwrap();
        }
        let blocked = mid(3);
        let results = idx.search(
            &vec_with_targets([3; 8]),
            5,
            None,
            |id| id != blocked,
        );
        assert!(
            results.iter().all(|(id, _)| *id != blocked),
            "filtered id leaked: {results:?}"
        );
    }

    #[test]
    fn contains_and_len_track_inserts() {
        let mut idx = HnswIndex::<8>::new(pq_params_default(), arithmetic_codebook::<8>()).unwrap();
        assert_eq!(idx.len(), 0);
        assert!(idx.is_empty());
        assert!(!idx.contains(mid(1)));

        idx.insert(mid(1), &vec_with_targets([1; 8])).unwrap();
        assert_eq!(idx.len(), 1);
        assert!(!idx.is_empty());
        assert!(idx.contains(mid(1)));
        assert!(!idx.contains(mid(2)));
    }

    #[test]
    fn codebook_accessor_returns_arc_to_same_data() {
        let cb_bytes_before: Vec<f32> = arithmetic_codebook::<8>().as_flat().to_vec();
        let idx = HnswIndex::<8>::new(pq_params_default(), arithmetic_codebook::<8>()).unwrap();
        let cb_arc = idx.codebook();
        assert_eq!(cb_arc.as_flat(), cb_bytes_before.as_slice());
    }
}
