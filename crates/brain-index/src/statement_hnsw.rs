//! `StatementHnswIndex` — per-shard HNSW over statement embeddings.
//!
//! Distinct from the substrate's [`crate::HnswIndex`] over
//! memory embeddings and the [`crate::EntityHnswIndex`] over
//! entity embeddings:
//!
//! | Index | M | ef_construction | ef_search | capacity_hint |
//! |---|---|---|---|---|
//! | Memory (substrate) | 16 | 200 | 64 | 1024 |
//! | Entity | 16 | 100 | 64 | 256 |
//! | Statement (this module) | **32** | 200 | **128** | 1024 |
//!
//! Statement counts are typically 0.1–1×
//! memory counts per shard, so the index is sized similarly to memory
//! and the wider `M`+`ef_search` give better recall on the denser
//! semantic neighbourhoods statements form.
//!
//! - In-memory only; no `statement.hnsw` persistence.
//! - Single-owner; no concurrency wrapper. The shard's worker
//!   discipline (one writer per shard) is enough.
//! - Inlined `StatementId ↔ u32` mapping (`Vec` + `HashMap`) — mirrors
//!   the entity HNSW.
//!
//! ## Population
//!
//! The embedding worker produces vectors from
//! `subject_canonical_name + " " + predicate_name + " " + object_text`
//! and subscribes to `STATEMENT_CREATED / _SUPERSEDED / _TOMBSTONED`
//! events.

use std::collections::HashMap;

use brain_core::StatementId;
use hnsw_rs::prelude::{DistCosine, Hnsw, Neighbour};
use thiserror::Error;

use crate::params::{IndexParamsError, MAX_LAYER, VECTOR_DIM};
use crate::tombstones::TombstoneBitmap;

/// Over-fetch multiplier used to amortise post-hoc tombstone
/// filtering. Mirrors `crate::hnsw::OVER_FACTOR` but isolated here so
/// future tuning can diverge.
const OVER_FACTOR: usize = 2;

// ---------------------------------------------------------------------------
// StatementHnswParams.
// ---------------------------------------------------------------------------

/// HNSW knobs for the statement index. Defaults from
/// [`Self::default_v1`]: `M=32, ef_construction=200, ef_search=128`,
/// capacity hint 1024.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementHnswParams {
    /// Max edges per non-bottom-layer node range:
    /// 4..=64.
    pub m: usize,
    /// Search width during insertion range: 50..=500.
    pub ef_construction: usize,
    /// Default search width per query range: 10..=500.
    /// Per-query overrides are clamped to `[k, ef_search_max]`.
    pub ef_search: usize,
    /// Cap on per-query `ef_search` overrides.
    pub ef_search_max: usize,
    /// Initial `max_elements` hint to `hnsw_rs::Hnsw::new`. The crate
    /// uses this only to pre-size internal tables; it doesn't cap
    /// insert count.
    pub capacity_hint: usize,
}

impl StatementHnswParams {
    /// Differences vs entity HNSW are commented inline.
    #[must_use]
    pub const fn default_v1() -> Self {
        Self {
            m: 32,                // ← vs entity's 16
            ef_construction: 200, // ← vs entity's 100
            ef_search: 128,       // ← vs entity's 64
            ef_search_max: 500,
            capacity_hint: 1024,
        }
    }

    /// Validate fields lie in the ranges.
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

impl Default for StatementHnswParams {
    fn default() -> Self {
        Self::default_v1()
    }
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum StatementHnswError {
    #[error("invalid params: {0}")]
    InvalidParams(#[from] IndexParamsError),

    /// `statement_id` was already inserted — duplicate
    /// inserts are caller bugs; we detect rather than letting
    /// `hnsw_rs` silently overwrite the embedding.
    #[error("duplicate statement_id {0:?}")]
    DuplicateStatement(StatementId),

    /// `statement_id` is not present in the index — returned by
    /// [`StatementHnswIndex::mark_tombstoned`] when called on a
    /// nonexistent id.
    #[error("statement {0:?} not present in the index")]
    UnknownStatement(StatementId),

    /// `ef_search` override exceeded `ef_search_max`.
    #[error("ef_search {ef} above ef_search_max {max}")]
    EfSearchTooLarge { ef: usize, max: usize },
}

// ---------------------------------------------------------------------------
// RebuildReport.
// ---------------------------------------------------------------------------

/// Outcome of [`StatementHnswIndex::rebuild`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildReport {
    /// Number of statements re-inserted from the input iterator.
    pub inserted: usize,
    /// Number of input entries skipped because the caller passed a
    /// duplicate StatementId in the rebuild iterator.
    pub duplicates_skipped: usize,
}

// ---------------------------------------------------------------------------
// StatementHnswIndex.
// ---------------------------------------------------------------------------

/// Per-shard HNSW over statement embeddings (384-dim, BGE-small).
///
/// **Single-writer** by `&mut self` discipline.
pub struct StatementHnswIndex {
    inner: Hnsw<'static, f32, DistCosine>,
    params: StatementHnswParams,
    /// Internal u32 id → StatementId. `None` only after rebuild drops
    /// a slot (or initial state).
    forward: Vec<Option<StatementId>>,
    /// StatementId → internal u32 id (the value used by `inner`).
    reverse: HashMap<StatementId, u32>,
    tombstones: TombstoneBitmap,
}

impl StatementHnswIndex {
    /// Construct an empty index with the given parameters.
    pub fn new(params: StatementHnswParams) -> Result<Self, StatementHnswError> {
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

    /// Insert `vector` under `statement_id`.
    ///
    /// Returns [`StatementHnswError::DuplicateStatement`] if
    /// `statement_id` is already present (and the index is left
    /// unchanged — no internal id is burned).
    pub fn insert(
        &mut self,
        statement_id: StatementId,
        vector: &[f32; VECTOR_DIM],
    ) -> Result<(), StatementHnswError> {
        if self.reverse.contains_key(&statement_id) {
            return Err(StatementHnswError::DuplicateStatement(statement_id));
        }
        let internal_id = u32::try_from(self.forward.len())
            .expect("invariant: statement count per shard never reaches u32::MAX");
        self.forward.push(Some(statement_id));
        self.reverse.insert(statement_id, internal_id);
        self.inner
            .insert_slice((vector.as_slice(), internal_id as usize));
        Ok(())
    }

    /// Search the top-`k` nearest statements to `query`. Returns
    /// `(StatementId, similarity)` tuples sorted **descending by
    /// similarity** (best match first). Similarity = `1 - distance`;
    /// for L2-normalised input, similarity is in `[-1, 1]` with `1.0`
    /// being identical.
    ///
    /// Tombstoned entries are always excluded.
    pub fn search(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
    ) -> Result<Vec<(StatementId, f32)>, StatementHnswError> {
        self.search_with_ef(query, k, None)
    }

    /// Variant of [`Self::search`] with an explicit `ef_search`
    /// override. Passing `None` uses
    /// [`StatementHnswParams::ef_search`]; `Some(v)` is clamped to
    /// `[k, ef_search_max]`.
    pub fn search_with_ef(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
        ef: Option<usize>,
    ) -> Result<Vec<(StatementId, f32)>, StatementHnswError> {
        if k == 0 || self.is_empty() {
            return Ok(Vec::new());
        }
        let ef = match ef {
            None => self.params.ef_search,
            Some(v) => {
                if v > self.params.ef_search_max {
                    return Err(StatementHnswError::EfSearchTooLarge {
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
        let mut out: Vec<(StatementId, f32)> = Vec::with_capacity(k);
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
                let Some(Some(statement_id)) = self.forward.get(internal_id as usize) else {
                    tracing::warn!(
                        internal_id,
                        "statement HNSW returned an internal id with no StatementId mapping; dropping"
                    );
                    continue;
                };
                out.push((*statement_id, 1.0 - n.distance));
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
                    "statement HNSW search bailout exhausted; returning partial results"
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

    /// Mark `statement_id` as tombstoned. Search results filter
    /// tombstoned entries out; the underlying HNSW graph is unaffected
    /// until [`Self::rebuild`].
    pub fn mark_tombstoned(&mut self, statement_id: StatementId) -> Result<(), StatementHnswError> {
        let internal_id = self
            .reverse
            .get(&statement_id)
            .copied()
            .ok_or(StatementHnswError::UnknownStatement(statement_id))?;
        self.tombstones.set(internal_id);
        Ok(())
    }

    #[must_use]
    pub fn is_tombstoned(&self, statement_id: StatementId) -> bool {
        match self.reverse.get(&statement_id).copied() {
            Some(internal_id) => self.tombstones.is_set(internal_id),
            None => false,
        }
    }

    #[must_use]
    pub fn contains(&self, statement_id: StatementId) -> bool {
        self.reverse.contains_key(&statement_id)
    }

    /// Number of mapped statements (including tombstoned).
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
    pub fn params(&self) -> StatementHnswParams {
        self.params
    }

    /// Discard the current index and re-insert every `(StatementId,
    /// vector)` from `statements`. Tombstones are cleared; the
    /// underlying `hnsw_rs::Hnsw` is replaced with a fresh instance.
    ///
    /// Duplicate StatementIds in the input are skipped (counted in the
    /// returned report). Callers should pre-filter tombstoned
    /// statements; this function does NOT honor any prior tombstone
    /// state.
    pub fn rebuild<I>(&mut self, statements: I) -> Result<RebuildReport, StatementHnswError>
    where
        I: IntoIterator<Item = (StatementId, [f32; VECTOR_DIM])>,
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
        for (id, vector) in statements {
            if self.reverse.contains_key(&id) {
                report.duplicates_skipped += 1;
                continue;
            }
            let internal_id = u32::try_from(self.forward.len())
                .expect("invariant: rebuilt statement count never reaches u32::MAX");
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
    /// fixtures.
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

    // ----- Params -------------------------------------------------------

    #[test]
    fn params_default_matches_spec() {
        let p = StatementHnswParams::default_v1();
        assert_eq!(p.m, 32, "— higher than entity");
        assert_eq!(p.ef_construction, 200);
        assert_eq!(p.ef_search, 128, "— higher than entity");
        assert_eq!(p.ef_search_max, 500);
        assert_eq!(p.capacity_hint, 1024);
    }

    #[test]
    fn params_validate_rejects_out_of_range() {
        let mut p = StatementHnswParams::default_v1();
        p.m = 3;
        assert!(matches!(
            p.validate(),
            Err(IndexParamsError::MOutOfRange(3))
        ));

        p = StatementHnswParams::default_v1();
        p.ef_construction = 49;
        assert!(matches!(
            p.validate(),
            Err(IndexParamsError::EfConstructionOutOfRange(49))
        ));

        p = StatementHnswParams::default_v1();
        p.ef_search = 501;
        assert!(matches!(
            p.validate(),
            Err(IndexParamsError::EfSearchOutOfRange(501))
        ));

        p = StatementHnswParams {
            m: 32,
            ef_construction: 200,
            ef_search: 128,
            ef_search_max: 32, // < ef_search
            capacity_hint: 1024,
        };
        assert!(matches!(
            p.validate(),
            Err(IndexParamsError::EfSearchMaxBelowDefault { .. })
        ));
    }

    // ----- Insert + contains --------------------------------------------

    #[test]
    fn insert_then_contains() {
        let mut idx = StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap();
        let id = StatementId::new();
        let other = StatementId::new();
        assert!(!idx.contains(id));
        idx.insert(id, &one_hot(0)).unwrap();
        assert!(idx.contains(id));
        assert!(!idx.contains(other));
        assert_eq!(idx.len(), 1);
        assert!(!idx.is_empty());
    }

    #[test]
    fn insert_rejects_duplicate() {
        let mut idx = StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap();
        let id = StatementId::new();
        idx.insert(id, &one_hot(0)).unwrap();
        let err = idx.insert(id, &one_hot(1)).expect_err("dup");
        assert!(matches!(err, StatementHnswError::DuplicateStatement(x) if x == id));
        assert_eq!(idx.len(), 1);
    }

    // ----- Search -------------------------------------------------------

    #[test]
    fn search_empty_returns_empty() {
        let idx = StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap();
        let r = idx.search(&one_hot(0), 5).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn search_returns_inserted_with_high_similarity() {
        let mut idx = StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap();
        let id = StatementId::new();
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
        let mut idx = StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap();
        for i in 0..10 {
            idx.insert(StatementId::new(), &one_hot(i)).unwrap();
        }
        let r = idx.search(&one_hot(0), 5).unwrap();
        assert!(r.len() <= 5, "got {} results", r.len());
    }

    #[test]
    fn search_orders_by_similarity_descending() {
        let mut idx = StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap();
        for i in 0..8 {
            idx.insert(StatementId::new(), &one_hot(i)).unwrap();
        }
        let r = idx.search(&one_hot(0), 5).unwrap();
        for w in r.windows(2) {
            assert!(
                w[0].1 >= w[1].1,
                "results should be descending by similarity: {} < {}",
                w[0].1,
                w[1].1
            );
        }
    }

    #[test]
    fn search_with_ef_above_max_errors() {
        // Must be non-empty for the ef check to fire; empty index
        // short-circuits to `Ok(vec![])` before validating `ef`.
        let mut idx = StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap();
        idx.insert(StatementId::new(), &one_hot(0)).unwrap();
        let err = idx
            .search_with_ef(&one_hot(0), 5, Some(1000))
            .expect_err("over max");
        assert!(matches!(
            err,
            StatementHnswError::EfSearchTooLarge { ef: 1000, max: 500 }
        ));
    }

    // ----- Tombstones ---------------------------------------------------

    #[test]
    fn mark_tombstoned_excludes_from_search() {
        let mut idx = StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap();
        let a = StatementId::new();
        let b = StatementId::new();
        let c = StatementId::new();
        idx.insert(a, &one_hot(0)).unwrap();
        idx.insert(b, &one_hot(1)).unwrap();
        idx.insert(c, &one_hot(2)).unwrap();

        idx.mark_tombstoned(b).unwrap();
        assert!(idx.is_tombstoned(b));
        assert_eq!(idx.tombstone_count(), 1);

        let r = idx.search(&one_hot(0), 3).unwrap();
        let ids: Vec<StatementId> = r.iter().map(|(id, _)| *id).collect();
        assert!(!ids.contains(&b), "tombstoned id surfaced in search");
        assert!(ids.contains(&a), "expected a in results");
    }

    #[test]
    fn search_widens_past_tombstone_attrition() {
        // A dense hub of 24 tombstoned statements occupies the query's
        // nearest ring — far more than the initial fetch window (k*OVER_FACTOR
        // = 5*2 = 10) — so a single fixed fetch lands entirely on tombstoned
        // neighbours and collapses to zero live results. A comfortable surplus
        // of 12 live statements sits in a slightly farther ring of the same
        // tight cluster (each ~cosine 1.0 to the query, all on distinct
        // dimensions so the graph is densely connected and HNSW recalls them
        // reliably). Escalation must widen the fetch past the tombstoned front
        // until the k live results survive.
        //
        // Density is load-bearing: a large, well-connected cluster (as in the
        // reliable statement-question widening test) lets the widened search
        // reach the live ring; a sparse handful of points can leave the far
        // ring unreachable on the approximate graph.
        let mut idx = StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap();
        let tombstoned: Vec<StatementId> = (0..24).map(|_| StatementId::new()).collect();
        for (i, id) in tombstoned.iter().enumerate() {
            idx.insert(*id, &near_at(i, 0.01)).unwrap();
        }
        let live: Vec<StatementId> = (0..12).map(|_| StatementId::new()).collect();
        for (i, id) in live.iter().enumerate() {
            idx.insert(*id, &near_at(30 + i, 0.02)).unwrap();
        }
        for id in &tombstoned {
            idx.mark_tombstoned(*id).unwrap();
        }

        let r = idx.search(&query(), 5).unwrap();
        assert_eq!(r.len(), 5, "escalation should still return k live results");
        let got: Vec<StatementId> = r.iter().map(|(id, _)| *id).collect();
        for id in &tombstoned {
            assert!(!got.contains(id), "tombstoned id surfaced after widening");
        }
        for id in &got {
            assert!(
                live.contains(id),
                "unexpected non-live statement in results"
            );
        }
    }

    #[test]
    fn search_exhausts_cleanly_when_fewer_than_k_live() {
        // Only 3 live of 6 total; search(k=5) can never reach k, so it must
        // terminate cleanly (no infinite escalation loop) and return only live
        // survivors — never fabricate up to k, never leak a tombstoned entry.
        // The 3 live statements sit in a tight cluster around the query (cosine
        // ~1.0) while the 3 tombstoned statements are orthogonal to it (cosine
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
        let mut idx = StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap();
        let live: Vec<StatementId> = (0..3).map(|_| StatementId::new()).collect();
        for (i, id) in live.iter().enumerate() {
            idx.insert(*id, &near_at(i, 0.01)).unwrap();
        }
        let tombstoned: Vec<StatementId> = (0..3).map(|_| StatementId::new()).collect();
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
        let got: Vec<StatementId> = r.iter().map(|(id, _)| *id).collect();
        for id in &got {
            assert!(live.contains(id), "returned id must be a live survivor");
        }
        for id in &tombstoned {
            assert!(!got.contains(id), "tombstoned id must not surface");
        }
    }

    #[test]
    fn mark_tombstoned_idempotent() {
        let mut idx = StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap();
        let id = StatementId::new();
        idx.insert(id, &one_hot(0)).unwrap();
        idx.mark_tombstoned(id).unwrap();
        idx.mark_tombstoned(id).unwrap(); // second call is a no-op
        assert!(idx.is_tombstoned(id));
        assert_eq!(idx.tombstone_count(), 1);
    }

    #[test]
    fn mark_tombstoned_unknown_errors() {
        let mut idx = StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap();
        let id = StatementId::new();
        let err = idx.mark_tombstoned(id).expect_err("unknown");
        assert!(matches!(err, StatementHnswError::UnknownStatement(x) if x == id));
    }

    // ----- Rebuild ------------------------------------------------------

    #[test]
    fn rebuild_drops_tombstones_and_resets_state() {
        let mut idx = StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap();
        let ids: Vec<StatementId> = (0..5).map(|_| StatementId::new()).collect();
        for (i, id) in ids.iter().enumerate() {
            idx.insert(*id, &one_hot(i)).unwrap();
        }
        idx.mark_tombstoned(ids[0]).unwrap();
        idx.mark_tombstoned(ids[1]).unwrap();
        assert_eq!(idx.tombstone_count(), 2);

        // Rebuild with 3 fresh statements (the surviving ones — caller
        // pre-filters tombstoned).
        let survivors = ids[2..]
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, one_hot(i + 2)))
            .collect::<Vec<_>>();
        let report = idx.rebuild(survivors).unwrap();

        assert_eq!(report.inserted, 3);
        assert_eq!(report.duplicates_skipped, 0);
        assert_eq!(idx.tombstone_count(), 0, "rebuild clears tombstones");
        assert_eq!(idx.len(), 3);
        assert!(!idx.contains(ids[0]));
        assert!(!idx.contains(ids[1]));
        assert!(idx.contains(ids[2]));
    }

    #[test]
    fn rebuild_skips_duplicates() {
        let mut idx = StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap();
        let id = StatementId::new();
        let other = StatementId::new();
        let report = idx
            .rebuild(vec![
                (id, one_hot(0)),
                (id, one_hot(1)),
                (other, one_hot(2)),
            ])
            .unwrap();
        assert_eq!(report.inserted, 2);
        assert_eq!(report.duplicates_skipped, 1);
        assert_eq!(idx.len(), 2);
    }
}
