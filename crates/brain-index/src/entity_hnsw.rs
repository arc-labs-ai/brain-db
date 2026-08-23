//! `EntityHnswIndex` — per-shard HNSW over entity embeddings.
//!
//! Distinct from the substrate's [`crate::HnswIndex`]
//! over memory embeddings:
//!
//! | Index | M | ef_construction | ef_search | capacity_hint |
//! |---|---|---|---|---|
//! | Memory (existing) | 16 | 200 | 64 | 1024 |
//! | Entity (this module) | 16 | **100** | 64 | **256** |
//!
//! Entity counts are typically
//! 10–100× smaller than memory counts per shard, so the index is
//! initialized smaller and its `ef_construction` is lower.
//!
//! - In-memory only; no `entity.hnsw` persistence.
//! - Single-owner; no concurrency wrapper.
//! - Inlined `EntityId ↔ u32` mapping (Vec + HashMap) — does NOT
//!   reuse the substrate's `MemoryId`-typed [`crate::IdMap`].

use std::collections::HashMap;

use brain_core::EntityId;
use hnsw_rs::prelude::{DistCosine, Hnsw, Neighbour};
use thiserror::Error;

use crate::params::{IndexParamsError, MAX_LAYER, VECTOR_DIM};
use crate::tombstones::TombstoneBitmap;

/// Over-fetch multiplier used to amortise post-hoc tombstone
/// filtering. Mirrors `crate::hnsw::OVER_FACTOR` but isolated here
/// so future tuning can diverge.
const OVER_FACTOR: usize = 2;

// ---------------------------------------------------------------------------
// EntityHnswParams.
// ---------------------------------------------------------------------------

/// HNSW knobs for the entity index. Defaults from
/// [`Self::default_v1`]: `M=16, ef_construction=100, ef_search=64`,
/// capacity hint 256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityHnswParams {
    /// Max edges per non-bottom-layer node range:
    /// 4..=64. Same ranges as memory HNSW.
    pub m: usize,
    /// Search width during insertion range: 50..=500.
    /// Entity HNSW uses 100 (lower than memory's 200) — entities are
    /// fewer per shard, the marginal gain of a wider construction
    /// search is small.
    pub ef_construction: usize,
    /// Default search width per query range: 10..=500.
    /// Per-query overrides are clamped to `[k, ef_search_max]`.
    pub ef_search: usize,
    /// Cap on per-query `ef_search` overrides.
    pub ef_search_max: usize,
    /// Initial `max_elements` hint to `hnsw_rs::Hnsw::new`. The crate
    /// uses this only to pre-size internal tables; it doesn't cap
    /// insert count says "typically 10K–100K entities
    /// per shard"; 256 is the small-test footprint.
    pub capacity_hint: usize,
}

impl EntityHnswParams {
    /// Per. Differences vs memory HNSW are commented
    /// inline.
    #[must_use]
    pub const fn default_v1() -> Self {
        Self {
            m: 16,
            ef_construction: 100, // ← vs memory's 200
            ef_search: 64,
            ef_search_max: 500,
            capacity_hint: 256, // ← vs memory's 1024
        }
    }

    /// Validate fields lie in the ranges (entity HNSW
    /// inherits the same range envelope as memory HNSW).
    pub fn validate(&self) -> Result<(), IndexParamsError> {
        if !(4..=64).contains(&self.m) {
            return Err(IndexParamsError::MOutOfRange(self.m));
        }
        if !(50..=500).contains(&self.ef_construction) {
            return Err(IndexParamsError::EfConstructionOutOfRange(
                self.ef_construction,
            ));
        }
        if !(10..=500).contains(&self.ef_search) {
            return Err(IndexParamsError::EfSearchOutOfRange(self.ef_search));
        }
        if self.ef_search_max < self.ef_search {
            return Err(IndexParamsError::EfSearchMaxBelowDefault {
                ef_search: self.ef_search,
                ef_search_max: self.ef_search_max,
            });
        }
        Ok(())
    }
}

impl Default for EntityHnswParams {
    fn default() -> Self {
        Self::default_v1()
    }
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum EntityHnswError {
    #[error("invalid params: {0}")]
    InvalidParams(#[from] IndexParamsError),

    /// `entity_id` was already inserted — duplicate
    /// inserts are caller bugs; we detect rather than letting
    /// hnsw_rs silently overwrite the embedding.
    #[error("duplicate entity_id {0:?}")]
    DuplicateEntity(EntityId),

    /// `entity_id` is not present in the index — returned by
    /// [`EntityHnswIndex::mark_tombstoned`] when called on a
    /// nonexistent id.
    #[error("entity {0:?} not present in the index")]
    UnknownEntity(EntityId),

    /// `ef_search` override exceeded `ef_search_max`.
    #[error("ef_search {ef} above ef_search_max {max}")]
    EfSearchTooLarge { ef: usize, max: usize },
}

// ---------------------------------------------------------------------------
// RebuildReport.
// ---------------------------------------------------------------------------

/// Outcome of [`EntityHnswIndex::rebuild`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildReport {
    /// Number of entities re-inserted from the input iterator.
    pub inserted: usize,
    /// Number of input entries skipped because the caller passed a
    /// duplicate EntityId in the rebuild iterator.
    pub duplicates_skipped: usize,
}

// ---------------------------------------------------------------------------
// EntityHnswIndex.
// ---------------------------------------------------------------------------

/// Per-shard HNSW over entity embeddings (384-dim, BGE-small).
///
/// **Single-writer** by `&mut self` discipline.
pub struct EntityHnswIndex {
    inner: Hnsw<'static, f32, DistCosine>,
    params: EntityHnswParams,
    /// Internal u32 id → EntityId. `None` only after rebuild drops a
    /// slot (or initial state).
    forward: Vec<Option<EntityId>>,
    /// EntityId → internal u32 id (the value used by `inner`).
    reverse: HashMap<EntityId, u32>,
    tombstones: TombstoneBitmap,
}

impl EntityHnswIndex {
    /// Construct an empty index with the given parameters.
    pub fn new(params: EntityHnswParams) -> Result<Self, EntityHnswError> {
        params.validate()?;
        let inner = Hnsw::<f32, DistCosine>::new(
            params.m,
            params.capacity_hint,
            MAX_LAYER,
            params.ef_construction,
            DistCosine,
        );
        Ok(Self {
            inner,
            params,
            forward: Vec::new(),
            reverse: HashMap::new(),
            tombstones: TombstoneBitmap::new(),
        })
    }

    /// Insert `vector` under `entity_id`.
    ///
    /// Returns [`EntityHnswError::DuplicateEntity`] if `entity_id` is
    /// already present (and the index is left unchanged — no
    /// internal id is burned).
    pub fn insert(
        &mut self,
        entity_id: EntityId,
        vector: &[f32; VECTOR_DIM],
    ) -> Result<(), EntityHnswError> {
        if self.reverse.contains_key(&entity_id) {
            return Err(EntityHnswError::DuplicateEntity(entity_id));
        }
        let internal_id = u32::try_from(self.forward.len())
            .expect("invariant: entity count per shard never reaches u32::MAX");
        self.forward.push(Some(entity_id));
        self.reverse.insert(entity_id, internal_id);
        self.inner
            .insert_slice((vector.as_slice(), internal_id as usize));
        Ok(())
    }

    /// Search the top-`k` nearest entities to `query`. Returns
    /// `(EntityId, similarity)` tuples sorted **descending by
    /// similarity** (best match first). Similarity = `1 - distance`;
    /// for L2-normalised input, similarity is in `[-1, 1]` with
    /// `1.0` being identical.
    ///
    /// Tombstoned entries are always excluded.
    pub fn search(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
    ) -> Result<Vec<(EntityId, f32)>, EntityHnswError> {
        self.search_with_ef(query, k, None)
    }

    /// Variant of [`Self::search`] with an explicit `ef_search`
    /// override. Passing `None` uses [`EntityHnswParams::ef_search`];
    /// `Some(v)` is clamped to `[k, ef_search_max]`.
    pub fn search_with_ef(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
        ef: Option<usize>,
    ) -> Result<Vec<(EntityId, f32)>, EntityHnswError> {
        if k == 0 || self.is_empty() {
            return Ok(Vec::new());
        }
        let ef = match ef {
            None => self.params.ef_search,
            Some(v) => {
                if v > self.params.ef_search_max {
                    return Err(EntityHnswError::EfSearchTooLarge {
                        ef: v,
                        max: self.params.ef_search_max,
                    });
                }
                v.max(k)
            }
        };

        // Escalate the fetch width (and `ef`) when tombstone attrition
        // starves the post-filter result set. A single fixed fetch can
        // return fewer than `k` live neighbours if more than half of the
        // top candidates were tombstoned by FORGET/consolidation before
        // the next rebuild; widen until we have `k` live results or the
        // graph is exhausted. Termination is guaranteed: `fetch_k` is
        // capped at the node count and `ef` at `ef_search_max`.
        let total_nodes = self.forward.len();
        let mut ef = ef;
        let mut fetch_multiplier = OVER_FACTOR;
        let mut out: Vec<(EntityId, f32)> = Vec::with_capacity(k);
        loop {
            out.clear();
            let fetch_k = k.saturating_mul(fetch_multiplier).min(total_nodes);
            let neighbours: Vec<Neighbour> = self.inner.search(query.as_slice(), fetch_k, ef);
            for n in neighbours {
                if out.len() >= k {
                    break;
                }
                let Ok(internal_id) = u32::try_from(n.d_id) else {
                    continue;
                };
                if self.tombstones.is_set(internal_id) {
                    continue;
                }
                let Some(Some(entity_id)) = self.forward.get(internal_id as usize) else {
                    tracing::warn!(
                        internal_id,
                        "entity HNSW returned an internal id with no EntityId mapping; dropping"
                    );
                    continue;
                };
                out.push((*entity_id, 1.0 - n.distance));
            }

            if out.len() >= k {
                break;
            }
            let fetch_saturated = fetch_k >= total_nodes;
            let ef_saturated = ef >= self.params.ef_search_max;
            if fetch_saturated && ef_saturated {
                tracing::debug!(
                    requested_k = k,
                    returned = out.len(),
                    "entity HNSW search bailout exhausted; returning partial results"
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
        // hnsw_rs returns ascending by distance → descending by
        // similarity once we convert. Already in the right order;
        // sort defensively in case the crate changes its contract.
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(out)
    }

    /// Mark `entity_id` as tombstoned. Search results filter
    /// tombstoned entries out; the underlying HNSW graph is
    /// unaffected until [`Self::rebuild`].
    pub fn mark_tombstoned(&mut self, entity_id: EntityId) -> Result<(), EntityHnswError> {
        let internal_id = self
            .reverse
            .get(&entity_id)
            .copied()
            .ok_or(EntityHnswError::UnknownEntity(entity_id))?;
        self.tombstones.set(internal_id);
        Ok(())
    }

    #[must_use]
    pub fn is_tombstoned(&self, entity_id: EntityId) -> bool {
        match self.reverse.get(&entity_id).copied() {
            Some(internal_id) => self.tombstones.is_set(internal_id),
            None => false,
        }
    }

    #[must_use]
    pub fn contains(&self, entity_id: EntityId) -> bool {
        self.reverse.contains_key(&entity_id)
    }

    /// Number of mapped entities (including tombstoned).
    #[must_use]
    pub fn len(&self) -> usize {
        self.reverse.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reverse.is_empty()
    }

    /// Count of currently tombstoned entries.
    #[must_use]
    pub fn tombstone_count(&self) -> usize {
        self.tombstones.count()
    }

    #[must_use]
    pub fn params(&self) -> EntityHnswParams {
        self.params
    }

    /// Discard the current index and re-insert every `(EntityId,
    /// vector)` from `entities`. Tombstones are cleared; the
    /// underlying `hnsw_rs::Hnsw` is replaced with a fresh instance.
    ///
    /// Duplicate EntityIds in the input are skipped (counted in the
    /// returned report). Callers should pre-filter tombstoned
    /// entities; this function does NOT honor any prior tombstone
    /// state.
    pub fn rebuild<I>(&mut self, entities: I) -> Result<RebuildReport, EntityHnswError>
    where
        I: IntoIterator<Item = (EntityId, [f32; VECTOR_DIM])>,
    {
        // Fresh HNSW + fresh mappings.
        self.inner = Hnsw::<f32, DistCosine>::new(
            self.params.m,
            self.params.capacity_hint,
            MAX_LAYER,
            self.params.ef_construction,
            DistCosine,
        );
        self.forward.clear();
        self.reverse.clear();
        self.tombstones.clear();

        let mut report = RebuildReport::default();
        for (id, vector) in entities {
            if self.reverse.contains_key(&id) {
                report.duplicates_skipped += 1;
                continue;
            }
            let internal_id = u32::try_from(self.forward.len())
                .expect("invariant: rebuilt entity count never reaches u32::MAX");
            self.forward.push(Some(id));
            self.reverse.insert(id, internal_id);
            self.inner
                .insert_slice((vector.as_slice(), internal_id as usize));
            report.inserted += 1;
        }
        Ok(report)
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn zeros() -> [f32; VECTOR_DIM] {
        [0.0; VECTOR_DIM]
    }

    /// Build a normalized 384-dim vector with a single "1.0" peak at
    /// index `seed % VECTOR_DIM` for deterministic, well-spaced
    /// fixtures. (BGE outputs are L2-normalised; one-hot is the
    /// simplest reasonable approximation for unit-tests.)
    fn one_hot(seed: usize) -> [f32; VECTOR_DIM] {
        let mut v = zeros();
        v[seed % VECTOR_DIM] = 1.0;
        v
    }

    /// The query / anchor vector the widening tests probe with.
    fn query() -> [f32; VECTOR_DIM] {
        one_hot(0)
    }

    /// A vector in a tight cluster around [`query`]: a dominant query
    /// component plus a unique perturbation of magnitude `mag` on dimension
    /// `1 + seed`. Cosine to the query is ~1.0 and every such point occupies
    /// a distinct location (no duplicate cluster to trap the traversal), so
    /// the small graphs stay well-connected and HNSW returns them with
    /// essentially full recall. `mag` sets the (small) distance from the
    /// query: smaller = nearer. The tests never depend on HNSW surfacing
    /// genuinely far nodes, which it does not guarantee on tiny graphs.
    fn near_at(seed: usize, mag: f32) -> [f32; VECTOR_DIM] {
        let mut v = zeros();
        v[0] = 1.0;
        v[1 + (seed % (VECTOR_DIM - 1))] = mag;
        v
    }

    // The params / insert / search / tombstone / rebuild matrix is covered
    // by the strictly-richer suite in `statement_hnsw.rs`. Only the
    // entity-specific `is_tombstoned` round-trip and the search-widening
    // escalation live here.

    #[test]
    fn is_tombstoned_round_trip() {
        let mut idx = EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap();
        let id = EntityId::new();
        idx.insert(id, &one_hot(0)).unwrap();
        assert!(!idx.is_tombstoned(id));
        idx.mark_tombstoned(id).unwrap();
        assert!(idx.is_tombstoned(id));
    }

    #[test]
    fn search_widens_past_tombstone_attrition() {
        // A dense hub of 24 tombstoned entities occupies the query's nearest
        // ring — far more than the initial fetch window (k*OVER_FACTOR =
        // 5*2 = 10) — so a single fixed fetch lands entirely on tombstoned
        // neighbours and collapses to zero live results. A comfortable
        // surplus of 12 live entities sits in a slightly farther ring of the
        // same tight cluster (each ~cosine 1.0 to the query, all on distinct
        // dimensions so the graph is densely connected and HNSW recalls them
        // reliably). Escalation must widen the fetch past the tombstoned
        // front until the k live results survive.
        //
        // Density is load-bearing: a large, well-connected cluster (as in the
        // reliable statement-question widening test) lets the widened search
        // reach the live ring; a sparse handful of points can leave the far
        // ring unreachable on the approximate graph.
        let mut idx = EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap();
        let tombstoned: Vec<EntityId> = (0..24).map(|_| EntityId::new()).collect();
        for (i, id) in tombstoned.iter().enumerate() {
            idx.insert(*id, &near_at(i, 0.01)).unwrap();
        }
        let live: Vec<EntityId> = (0..12).map(|_| EntityId::new()).collect();
        for (i, id) in live.iter().enumerate() {
            idx.insert(*id, &near_at(30 + i, 0.02)).unwrap();
        }
        for id in &tombstoned {
            idx.mark_tombstoned(*id).unwrap();
        }

        let r = idx.search(&query(), 5).unwrap();
        assert_eq!(r.len(), 5, "escalation should still return k live results");
        let got: Vec<EntityId> = r.iter().map(|(id, _)| *id).collect();
        for id in &tombstoned {
            assert!(!got.contains(id), "tombstoned id surfaced after widening");
        }
        for id in &got {
            assert!(live.contains(id), "unexpected non-live entity in results");
        }
    }

    #[test]
    fn search_exhausts_cleanly_when_fewer_than_k_live() {
        // Only 3 live of 6 total; search(k=5) can never reach k, so it must
        // terminate cleanly (no infinite escalation loop) and return only live
        // survivors — never fabricate up to k, never leak a tombstoned entry.
        // The 3 live entities sit in a tight cluster around the query (cosine
        // ~1.0) while the 3 tombstoned entities are orthogonal to it (cosine
        // ~0, distinct far dimensions), so the live trio is unambiguously the
        // query's nearest neighbourhood.
        //
        // The assertions bound the exhaustion behaviour — a non-empty subset
        // of the live set, strictly fewer than k, with no tombstoned leak —
        // rather than demanding exact full recall. Exactly how many of the
        // few live points HNSW surfaces from a tiny graph is an approximate-
        // recall property of the index itself (even ef widened to its maximum
        // occasionally drops one node on a near-degenerate graph), not of the
        // exhaustion/termination logic this test exists to pin down.
        let mut idx = EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap();
        let live: Vec<EntityId> = (0..3).map(|_| EntityId::new()).collect();
        for (i, id) in live.iter().enumerate() {
            idx.insert(*id, &near_at(i, 0.01)).unwrap();
        }
        let tombstoned: Vec<EntityId> = (0..3).map(|_| EntityId::new()).collect();
        for (i, id) in tombstoned.iter().enumerate() {
            idx.insert(*id, &one_hot(100 + i)).unwrap();
            idx.mark_tombstoned(*id).unwrap();
        }

        let r = idx.search(&query(), 5).unwrap();
        assert!(!r.is_empty(), "live survivors must remain reachable");
        assert!(
            r.len() <= live.len(),
            "must not return more than the live count on exhaustion"
        );
        assert!(
            r.len() < 5,
            "cannot reach k when fewer than k live entries exist"
        );
        let got: Vec<EntityId> = r.iter().map(|(id, _)| *id).collect();
        for id in &got {
            assert!(live.contains(id), "returned id must be a live survivor");
        }
        for id in &tombstoned {
            assert!(!got.contains(id), "tombstoned id must not surface");
        }
    }
}
