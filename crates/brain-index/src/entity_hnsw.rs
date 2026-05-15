//! `EntityHnswIndex` — per-shard HNSW over entity embeddings.
//!
//! Sub-task 16.3. Distinct from the substrate's [`crate::HnswIndex`]
//! over memory embeddings:
//!
//! | Index | M | ef_construction | ef_search | capacity_hint |
//! |---|---|---|---|---|
//! | Memory (existing) | 16 | 200 | 64 | 1024 |
//! | Entity (this module) | 16 | **100** | 64 | **256** |
//!
//! Spec §18/02 "Entity embedding HNSW" — entity counts are typically
//! 10–100× smaller than memory counts per shard, so the index is
//! initialized smaller and its `ef_construction` is lower.
//!
//! ## Surface (16.3 only)
//!
//! - In-memory only; no `entity.hnsw` persistence (deferred — phase
//!   plan 16.3 F-2).
//! - Single-owner; no concurrency wrapper (deferred to 16.5+ when
//!   the resolver needs concurrent reads).
//! - Inlined `EntityId ↔ u32` mapping (Vec + HashMap) — does NOT
//!   reuse the substrate's `MemoryId`-typed [`crate::IdMap`].
//!   Generalizing the id-map is a phase-16 follow-up.

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
/// [`Self::default_v1`] match `spec/18_entities/02_storage.md`:
/// `M=16, ef_construction=100, ef_search=64`, capacity hint 256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityHnswParams {
    /// Max edges per non-bottom-layer node. Spec §06/02 §1 range:
    /// 4..=64. Same ranges as memory HNSW.
    pub m: usize,
    /// Search width during insertion. Spec §06/02 §1 range: 50..=500.
    /// Entity HNSW uses 100 (lower than memory's 200) — entities are
    /// fewer per shard, the marginal gain of a wider construction
    /// search is small.
    pub ef_construction: usize,
    /// Default search width per query. Spec §06/02 §1 range: 10..=500.
    /// Per-query overrides are clamped to `[k, ef_search_max]`.
    pub ef_search: usize,
    /// Cap on per-query `ef_search` overrides.
    pub ef_search_max: usize,
    /// Initial `max_elements` hint to `hnsw_rs::Hnsw::new`. The crate
    /// uses this only to pre-size internal tables; it doesn't cap
    /// insert count. Spec §18/02 says "typically 10K–100K entities
    /// per shard"; 256 is the small-test footprint.
    pub capacity_hint: usize,
}

impl EntityHnswParams {
    /// Per spec §18/02. Differences vs memory HNSW are commented
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

    /// Validate fields lie in the spec §06/02 ranges (entity HNSW
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

    /// `entity_id` was already inserted. Spec §18/02 — duplicate
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
/// **Single-writer** by `&mut self` discipline (CLAUDE.md §5 inv. 2).
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
            .expect("entity HNSW id-space exhausted (> u32::MAX entities per shard)");
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
    /// `Some(v)` is clamped to `[k, ef_search_max]` per spec §06/02.
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

        let fetch_k = k.saturating_mul(OVER_FACTOR).min(self.forward.len());
        let neighbours: Vec<Neighbour> = self.inner.search(query.as_slice(), fetch_k, ef);
        let mut out: Vec<(EntityId, f32)> = Vec::with_capacity(k);
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
                .expect("entity HNSW id-space exhausted during rebuild");
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

    // ----- Params -------------------------------------------------------

    #[test]
    fn params_default_matches_spec() {
        let p = EntityHnswParams::default_v1();
        assert_eq!(p.m, 16);
        assert_eq!(p.ef_construction, 100, "spec §18/02 — lower than memory");
        assert_eq!(p.ef_search, 64);
        assert_eq!(p.ef_search_max, 500);
        assert_eq!(p.capacity_hint, 256, "spec §18/02 — smaller than memory");
    }

    #[test]
    fn params_validate_rejects_out_of_range() {
        let mut p = EntityHnswParams::default_v1();
        p.m = 3;
        assert!(matches!(
            p.validate(),
            Err(IndexParamsError::MOutOfRange(3))
        ));

        p = EntityHnswParams::default_v1();
        p.ef_construction = 49;
        assert!(matches!(
            p.validate(),
            Err(IndexParamsError::EfConstructionOutOfRange(49))
        ));

        p = EntityHnswParams::default_v1();
        p.ef_search = 501;
        assert!(matches!(
            p.validate(),
            Err(IndexParamsError::EfSearchOutOfRange(501))
        ));

        p = EntityHnswParams {
            m: 16,
            ef_construction: 100,
            ef_search: 64,
            ef_search_max: 32, // < ef_search
            capacity_hint: 256,
        };
        assert!(matches!(
            p.validate(),
            Err(IndexParamsError::EfSearchMaxBelowDefault { .. })
        ));
    }

    // ----- Insert + contains --------------------------------------------

    #[test]
    fn insert_then_contains() {
        let mut idx = EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap();
        let id = EntityId::new();
        let other = EntityId::new();
        assert!(!idx.contains(id));
        idx.insert(id, &one_hot(0)).unwrap();
        assert!(idx.contains(id));
        assert!(!idx.contains(other));
        assert_eq!(idx.len(), 1);
        assert!(!idx.is_empty());
    }

    #[test]
    fn insert_rejects_duplicate() {
        let mut idx = EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap();
        let id = EntityId::new();
        idx.insert(id, &one_hot(0)).unwrap();
        let err = idx.insert(id, &one_hot(1)).expect_err("dup");
        assert!(matches!(err, EntityHnswError::DuplicateEntity(x) if x == id));
        // Index unchanged on the duplicate path.
        assert_eq!(idx.len(), 1);
    }

    // ----- Search -------------------------------------------------------

    #[test]
    fn search_empty_returns_empty() {
        let idx = EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap();
        let r = idx.search(&one_hot(0), 5).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn search_returns_inserted_with_high_similarity() {
        let mut idx = EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap();
        let id = EntityId::new();
        let v = one_hot(7);
        idx.insert(id, &v).unwrap();

        let r = idx.search(&v, 1).unwrap();
        assert_eq!(r.len(), 1);
        let (got_id, similarity) = r[0];
        assert_eq!(got_id, id);
        assert!(
            similarity > 0.99,
            "self-search similarity should be ~1.0; got {similarity}"
        );
    }

    #[test]
    fn search_topk_bounded_by_k() {
        let mut idx = EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap();
        for i in 0..10 {
            idx.insert(EntityId::new(), &one_hot(i)).unwrap();
        }
        let r = idx.search(&one_hot(0), 5).unwrap();
        assert!(r.len() <= 5, "got {} results", r.len());
    }

    #[test]
    fn search_with_ef_above_max_errors() {
        let idx = EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap();
        let err = idx
            .search_with_ef(&one_hot(0), 5, Some(1000))
            .expect_err("over max");
        assert!(matches!(
            err,
            EntityHnswError::EfSearchTooLarge { ef: 1000, max: 500 }
        ));
    }

    // ----- Tombstones ---------------------------------------------------

    #[test]
    fn mark_tombstoned_excludes_from_search() {
        let mut idx = EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap();
        let a = EntityId::new();
        let b = EntityId::new();
        let c = EntityId::new();
        idx.insert(a, &one_hot(0)).unwrap();
        idx.insert(b, &one_hot(1)).unwrap();
        idx.insert(c, &one_hot(2)).unwrap();

        idx.mark_tombstoned(b).unwrap();
        assert!(idx.is_tombstoned(b));
        assert_eq!(idx.tombstone_count(), 1);

        let r = idx.search(&one_hot(0), 3).unwrap();
        let ids: Vec<EntityId> = r.iter().map(|(id, _)| *id).collect();
        assert!(!ids.contains(&b), "tombstoned id surfaced in search");
        assert!(ids.contains(&a), "expected a in results");
    }

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
    fn mark_tombstoned_unknown_errors() {
        let mut idx = EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap();
        let id = EntityId::new();
        let err = idx.mark_tombstoned(id).expect_err("unknown");
        assert!(matches!(err, EntityHnswError::UnknownEntity(x) if x == id));
    }

    // ----- Rebuild ------------------------------------------------------

    #[test]
    fn rebuild_drops_tombstones_and_resets_state() {
        let mut idx = EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap();
        let ids: Vec<EntityId> = (0..5).map(|_| EntityId::new()).collect();
        for (i, id) in ids.iter().enumerate() {
            idx.insert(*id, &one_hot(i)).unwrap();
        }
        idx.mark_tombstoned(ids[0]).unwrap();
        idx.mark_tombstoned(ids[1]).unwrap();
        assert_eq!(idx.tombstone_count(), 2);

        // Rebuild with 3 fresh entities (the surviving ones — caller
        // pre-filters tombstoned).
        let fresh: Vec<EntityId> = (0..3).map(|_| EntityId::new()).collect();
        let input: Vec<_> = fresh.iter().enumerate().map(|(i, id)| (*id, one_hot(i + 100))).collect();
        let report = idx.rebuild(input).unwrap();
        assert_eq!(report.inserted, 3);
        assert_eq!(report.duplicates_skipped, 0);
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.tombstone_count(), 0);
        for old in &ids {
            assert!(!idx.contains(*old));
        }
        for f in &fresh {
            assert!(idx.contains(*f));
        }
    }

    #[test]
    fn rebuild_skips_duplicate_input_ids() {
        let mut idx = EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap();
        let id = EntityId::new();
        let report = idx
            .rebuild(vec![(id, one_hot(0)), (id, one_hot(1))])
            .unwrap();
        assert_eq!(report.inserted, 1);
        assert_eq!(report.duplicates_skipped, 1);
        assert!(idx.contains(id));
    }
}
