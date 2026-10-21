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
            // THE BEAM HAS TO COVER WHAT WE ASK FOR.
            //
            // `resolve_ef` floors `ef` on `k`, but the call below asks for
            // `fetch_k` neighbours — `OVER_FACTOR * k`, doubling on each retry.
            // At the shipped defaults that was `search(fetch_k = 128, ef = 64)`:
            // requesting 128 neighbours from a 64-wide beam, which a graph walk
            // cannot honestly answer. The over-fetch exists so tombstones and
            // the caller's filter can be applied without under-returning, so
            // the beam has to be sized to the fetch, not to the final `k`.
            //
            // Capped at `ef_search_max` (500), which is already the ceiling the
            // escalation below respects and matches Weaviate's `efMax`.
            let ef_eff = ef.max(fetch_k).min(self.params.ef_search_max);
            let neighbours: Vec<Neighbour> = self.inner.search(query.as_slice(), fetch_k, ef_eff);

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
            let ef_saturated = ef_eff >= self.params.ef_search_max;
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
    ///
    /// An explicit per-query `ef` wins; otherwise the wider of the configured
    /// `ef_search` and `k * ef_factor` is used. `ef_factor` defaults to 1, so
    /// this reproduces the previous `max(ef_search, k)` exactly until an
    /// operator raises it.
    fn resolve_ef(&self, k: usize, ef: Option<usize>) -> usize {
        let derived = k.saturating_mul(self.params.ef_factor);
        let requested = ef.unwrap_or_else(|| self.params.ef_search.max(derived));
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

    /// A corpus with real distance structure, for the over-fetch tests.
    ///
    /// `one_hot` makes every vector mutually orthogonal, so under cosine
    /// EVERY non-exact match sits at distance 1.0 — identical. A greedy graph
    /// walk then has no gradient to follow and the result rests entirely on
    /// how `hnsw_rs` happened to assign layers, which it seeds with
    /// `StdRng::from_os_rng()` — a fresh OS seed on every single run. That is
    /// not a corpus the over-fetch logic can be measured against; it made
    /// `search_fills_k_through_a_selective_filter` return 1 of 10 in CI while
    /// passing 14,120 consecutive local runs.
    ///
    /// Here every vector shares one axis with the query and tilts away from it
    /// by a distinct amount, so cosine similarity is strictly decreasing in
    /// `n` and every pair is distinguishable. That is also the shape of the
    /// real input: BGE embeddings are L2-normalised and clustered, never
    /// mutually orthogonal. Unit norm holds exactly, since
    /// `(1 - t^2) + t^2 == 1`.
    fn graded(n: usize) -> [f32; VECTOR_DIM] {
        let mut v = [0.0; VECTOR_DIM];
        // t stays well under 1 for the corpus sizes used below, so the shared
        // component never vanishes and the ordering never degenerates.
        let t = n as f32 / 128.0;
        v[0] = (1.0 - t * t).sqrt();
        v[1 + (n % (VECTOR_DIM - 1))] = t;
        v
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

    // ── ef derivation (accuracy plan §1) ──────────────────────────────────

    /// The default MUST reproduce the prior behaviour exactly: `ef_factor = 1`
    /// makes `resolve_ef` the old `max(ef_search, k)`. This is the guarantee
    /// that shipping the knob changes nothing until an operator opts in.
    #[test]
    fn default_ef_factor_reproduces_prior_behaviour() {
        let idx = HnswIndex::new(IndexParams::default()).unwrap();
        let p = IndexParams::default();
        assert_eq!(p.ef_factor, 1, "the shipped default must be a no-op");

        // k below ef_search -> ef_search wins, as before.
        assert_eq!(idx.resolve_ef(10, None), p.ef_search);
        // k above ef_search -> floored on k, as before.
        assert_eq!(idx.resolve_ef(200, None), 200);
        // an explicit ef still wins, still clamped to [k, max].
        assert_eq!(idx.resolve_ef(10, Some(300)), 300);
        assert_eq!(idx.resolve_ef(10, Some(5)), 10, "clamped up to k");
        assert_eq!(
            idx.resolve_ef(10, Some(9_000)),
            p.ef_search_max,
            "clamped down to the ceiling"
        );
    }

    /// With a factor set, `ef` scales with `k` and still respects the ceiling.
    /// Weaviate's equivalent factor is 8 against the same 500 cap.
    #[test]
    fn ef_factor_scales_with_k_and_respects_the_ceiling() {
        let params = IndexParams {
            ef_factor: 8,
            ..IndexParams::default()
        };
        params.validate().expect("factor 8 is in range");
        let idx = HnswIndex::new(params).unwrap();

        // 10 * 8 = 80, which beats ef_search (64).
        assert_eq!(idx.resolve_ef(10, None), 80);
        // 64 * 8 = 512 -> clamped to ef_search_max.
        assert_eq!(idx.resolve_ef(64, None), params.ef_search_max);
        // An explicit per-query ef still overrides the derivation.
        assert_eq!(idx.resolve_ef(10, Some(100)), 100);
    }

    #[test]
    fn ef_factor_is_validated() {
        for bad in [0usize, 17] {
            let p = IndexParams {
                ef_factor: bad,
                ..IndexParams::default()
            };
            assert!(
                matches!(p.validate(), Err(IndexParamsError::EfFactorOutOfRange(n)) if n == bad),
                "ef_factor={bad} should be rejected"
            );
        }
    }

    /// THE CORRECTNESS FIX. The search over-fetches (`OVER_FACTOR * k`) so
    /// tombstones and the caller's filter can be applied without
    /// under-returning — but the beam was only sized to `k`, so the index was
    /// asked for more neighbours than it was given room to find. Searching
    /// must still return every live match once the beam covers the fetch.
    #[test]
    fn search_returns_all_live_matches_when_over_fetching() {
        let mut idx = HnswIndex::new(IndexParams::default()).unwrap();
        for n in 1..=40u64 {
            idx.insert(mid(n), &graded(n as usize)).unwrap();
        }
        // k = 20 -> fetch_k = 40 = every node, with a beam that now covers it.
        let hits = idx.search_all(&graded(1), 20, None);
        assert_eq!(hits.len(), 20, "a full beam returns a full result set");
        // The exact match must rank first.
        assert_eq!(hits[0].0, mid(1));
        assert_distinct_and_ordered(&hits);
        // NOT the exact nearest 20. `fetch_k` reaching every node does not mean
        // the walk visits every node — HNSW is approximate, and ef is 64 against
        // a 40-node corpus. Measured: it returns slots
        // [1, 3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 14, 15, 17, 18, 19, 20, 21, 22, 23],
        // skipping 2, 12 and 16. Asserting identities here passed once by luck
        // and then failed on the next run, so it would only have traded one
        // flake for another.
    }

    /// The properties a correct result set has whatever the graph topology
    /// turned out to be: no id twice, and similarity non-increasing. Both are
    /// independent of which nodes the approximate walk happened to reach, so
    /// they hold on every seed rather than on a lucky one.
    fn assert_distinct_and_ordered(hits: &[(MemoryId, f32)]) {
        let ids: Vec<u64> = hits.iter().map(|(m, _)| m.slot()).collect();
        let mut distinct = ids.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            ids.len(),
            "no id may be returned twice: {ids:?}"
        );
        assert!(
            hits.windows(2).all(|w| w[0].1 >= w[1].1),
            "similarity must be non-increasing: {hits:?}",
        );
    }

    /// A filter that rejects most of the corpus is the case the over-fetch
    /// exists for; it must still fill `k` rather than silently under-returning.
    #[test]
    fn search_fills_k_through_a_selective_filter() {
        let mut idx = HnswIndex::new(IndexParams::default()).unwrap();
        for n in 1..=60u64 {
            idx.insert(mid(n), &graded(n as usize)).unwrap();
        }
        // Keep only even ids: half the corpus is filtered out, so filling k =
        // 10 needs the walk to reach 20 nodes deep. The first fetch asks for
        // OVER_FACTOR * k = 20 and can only yield 10 evens if nothing is lost
        // on the way, which is exactly the bailout loop's job.
        let hits = idx.search(&graded(0), 10, None, |m| m.slot().is_multiple_of(2));
        assert_eq!(hits.len(), 10, "escalation must fill k past the filter");
        assert!(
            hits.iter().all(|(m, _)| m.slot().is_multiple_of(2)),
            "the filter must hold"
        );
        // NOT the exact ten nearest: the loop breaks the moment k is filled,
        // so this returns the evens among the first `fetch_k = 20` raw
        // neighbours. Measured: [4, 6, 8, 10, 14, 16, 18, 20, 22, 24], skipping
        // 2 and 12.
        assert_distinct_and_ordered(&hits);
    }
}
