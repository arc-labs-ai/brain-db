//! `HnswIndex` — full-precision HNSW over memory embeddings.
//!
//! Stores the 384-dim BGE-small vectors directly in the graph under
//! cosine distance. Mirrors [`crate::entity_hnsw::EntityHnswIndex`] and
//! [`crate::statement_hnsw::StatementHnswIndex`].
//!
//! Pure HNSW is exact and fast up to ~10M vectors per shard, which is
//! the operating range; search returns cosine similarity directly, so
//! no re-rank against the arena is needed. Product quantization stays
//! available in [`crate::pq`] as a future opt-in for corpora that
//! outgrow full-precision RAM, but it is not the default path.
//!
//! **Single-writer:** `insert` takes `&mut self`.

use std::path::Path;

use brain_core::MemoryId;
use hnsw_rs::prelude::{DistCosine, Hnsw, Neighbour};
use thiserror::Error;

use crate::idmap::{IdMap, IdMapError};
use crate::params::{IndexParams, IndexParamsError, DEFAULT_CAPACITY_HINT, MAX_LAYER, VECTOR_DIM};
use crate::tombstones::TombstoneBitmap;

/// Over-fetch multiplier for the per-search bailout loop. The graph
/// returns at most `fetch_k` raw neighbours; tombstone + caller filters
/// can drop some, so we over-fetch and escalate `ef` until we have `k`.
const OVER_FACTOR: usize = 2;

/// Per-shard HNSW over memory embeddings (full-precision, cosine).
///
/// **Single-writer** by `&mut self` discipline.
pub struct HnswIndex {
    inner: Hnsw<'static, f32, DistCosine>,
    params: IndexParams,
    id_map: IdMap,
    tombstones: TombstoneBitmap,
}

#[derive(Debug, Error)]
pub enum HnswError {
    #[error("invalid params: {0}")]
    InvalidParams(#[from] IndexParamsError),

    #[error("duplicate memory_id: {memory_id_bytes:?}")]
    DuplicateMemoryId { memory_id_bytes: [u8; 16] },

    #[error("id_map exhausted: u32::MAX internal ids allocated")]
    IdMapExhausted,

    #[error("memory_id not found in id_map: {memory_id_bytes:?}")]
    MemoryIdNotFound { memory_id_bytes: [u8; 16] },

    #[error("snapshot I/O: {0}")]
    SnapshotIo(#[from] std::io::Error),

    #[error("snapshot corrupt or incompatible: {0}")]
    SnapshotCorrupt(String),
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

impl HnswIndex {
    /// Build a fresh empty index with the given parameters.
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

    /// Insert a full-precision vector under `memory_id`.
    pub fn insert(
        &mut self,
        memory_id: MemoryId,
        vector: &[f32; VECTOR_DIM],
    ) -> Result<(), HnswError> {
        let internal_id = self.id_map.insert(memory_id)?;
        self.inner
            .insert_slice((vector.as_slice(), internal_id as usize));
        Ok(())
    }

    /// Search the `k` nearest memories to `query`, returning
    /// `(MemoryId, cosine_similarity)` sorted descending by similarity.
    /// Similarity = `1.0 - distance`; for L2-normalised input it lies in
    /// `[-1, 1]` with `1.0` being identical. The score is **exact** (no
    /// re-rank needed). Tombstoned and filtered-out ids are excluded.
    ///
    /// `ef`: `None` uses `params.ef_search`; `Some` is clamped to
    /// `[k, params.ef_search_max]`.
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

        let total_nodes = self.len();
        let mut ef = self.resolve_ef(k, ef);
        let mut fetch_multiplier = OVER_FACTOR;
        let mut results: Vec<(MemoryId, f32)> = Vec::with_capacity(k);

        loop {
            results.clear();
            let fetch_k = k.saturating_mul(fetch_multiplier).min(total_nodes);
            let neighbours: Vec<Neighbour> = self.inner.search(query.as_slice(), fetch_k, ef);

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
                results.push((memory_id, 1.0 - n.distance));
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
                    "hnsw search bailout exhausted; returning partial results",
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
        let internal_id =
            self.id_map
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

    /// The configured params.
    #[must_use]
    pub fn params(&self) -> IndexParams {
        self.params
    }

    /// Borrow the internal id-map. Snapshot writers need this to
    /// serialise the forward direction + `next_id`.
    #[must_use]
    pub fn id_map(&self) -> &IdMap {
        &self.id_map
    }

    /// Borrow the internal tombstone bitmap. Snapshot writers need
    /// this to serialise the bitmap and its set-count.
    #[must_use]
    pub fn tombstones(&self) -> &TombstoneBitmap {
        &self.tombstones
    }

    /// Resolve the effective `ef_search` for a single query.
    fn resolve_ef(&self, k: usize, ef: Option<usize>) -> usize {
        let requested = ef.unwrap_or(self.params.ef_search);
        requested.max(k).min(self.params.ef_search_max)
    }

    /// Dump the inner `hnsw_rs` graph to `<dir>/<basename>.hnsw.{graph,data}`.
    /// Wraps [`hnsw_rs::api::AnnT::file_dump`]. Caller ensures `dir` exists.
    pub fn file_dump(&self, dir: &Path, basename: &str) -> Result<String, HnswError> {
        use hnsw_rs::api::AnnT;
        self.inner
            .file_dump(dir, basename)
            .map_err(|e| HnswError::SnapshotIo(std::io::Error::other(format!("file_dump: {e}"))))
    }

    /// Rehydrate from persisted parts. Used by snapshot-load: the
    /// `inner` Hnsw graph is reloaded via `hnsw_rs::HnswIo`, and the
    /// id_map/tombstones are reconstructed from the wrapper body.
    #[must_use]
    pub fn from_persisted_parts(
        params: IndexParams,
        inner: Hnsw<'static, f32, DistCosine>,
        id_map: IdMap,
        tombstones: TombstoneBitmap,
    ) -> Self {
        Self {
            inner,
            params,
            id_map,
            tombstones,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a normalized 384-dim vector with a single "1.0" peak at
    /// index `seed % VECTOR_DIM`, matching the entity/statement HNSW
    /// fixtures (BGE outputs are L2-normalised; one-hot approximates it).
    fn one_hot(seed: usize) -> [f32; VECTOR_DIM] {
        let mut v = [0.0; VECTOR_DIM];
        v[seed % VECTOR_DIM] = 1.0;
        v
    }

    fn mid(n: u64) -> MemoryId {
        MemoryId::pack(1, n, 1)
    }

    /// End-to-end scenario over the live `HnswIndex`: insert →
    /// contains → search returns it → tombstone excludes → tombstone
    /// of an unknown id errors. Mirrors the entity/statement HNSW
    /// matrix against `MemoryId`. (`HnswIndex` has no in-place
    /// `rebuild`; tombstone compaction happens via snapshot reload.)
    #[test]
    fn insert_search_tombstone_rebuild_scenario() {
        let mut idx = HnswIndex::new(IndexParams::default()).unwrap();

        let a = mid(1);
        let b = mid(2);
        let c = mid(3);
        idx.insert(a, &one_hot(0)).unwrap();
        idx.insert(b, &one_hot(1)).unwrap();
        idx.insert(c, &one_hot(2)).unwrap();

        // contains
        assert!(idx.contains(a));
        assert!(!idx.contains(mid(99)));
        assert_eq!(idx.len(), 3);

        // search returns the self-match with ~1.0 similarity
        let r = idx.search_all(&one_hot(1), 1, None);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, b);
        assert!(
            r[0].1 > 0.99,
            "self-search similarity should be ~1.0; got {}",
            r[0].1
        );

        // tombstone excludes from search
        idx.mark_tombstoned(b).unwrap();
        assert!(idx.is_tombstoned(b));
        assert_eq!(idx.tombstone_count(), 1);
        let r = idx.search_all(&one_hot(1), 3, None);
        let ids: Vec<MemoryId> = r.iter().map(|(id, _)| *id).collect();
        assert!(!ids.contains(&b), "tombstoned id surfaced in search");

        // tombstone on an unknown id errors
        let err = idx.mark_tombstoned(mid(99)).expect_err("unknown id");
        assert!(matches!(err, HnswError::MemoryIdNotFound { .. }));
    }
}
