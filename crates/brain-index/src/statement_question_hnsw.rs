//! `StatementQuestionHnswIndex` — per-shard HNSW over per-statement
//! question-bridge embeddings.
//!
//! The per-statement analogue of [`crate::hype_hnsw::HypeHnswIndex`]: at
//! write time the embed worker turns each current statement into a few
//! templated questions whose answer is that statement ("what is
//! {subject}'s {predicate}?"), embeds them, and inserts them here. At read
//! time the user's query vector probes this pool and a hit maps back to the
//! owning [`StatementId`] — whose evidence memory is the answer. Embedding a
//! full question (not a bare predicate name) is what keeps this off the
//! confident-wrong-answer trap of short-name cosine.
//!
//! Each question point is tagged with the [`Slot`] of the reified fact it
//! leaves unbound, so a hit yields `(StatementId, Slot)` — the read path can
//! then project the matched slot. The mapping is **many-to-one per
//! (statement, slot)** (a statement owns several question points per slot,
//! and several slots), so [`StatementQuestionHnswIndex::search`] collapses
//! raw question hits to the best similarity per `(StatementId, Slot)` pair.
//!
//! - In-memory only; the vectors persist in redb
//!   (`statement_question_vectors`) and this index is rebuilt on boot.
//! - Single-owner; the shard wraps it in `Arc<RwLock<_>>`.

use std::collections::HashMap;

use brain_core::{Slot, StatementId};
use hnsw_rs::prelude::{DistCosine, Hnsw, Neighbour};
use thiserror::Error;

use crate::entity_hnsw::EntityHnswParams;
use crate::params::{MAX_LAYER, VECTOR_DIM};
use crate::tombstones::TombstoneBitmap;

/// Over-fetch multiplier for `search` — several question points collapse to
/// one statement, so pull enough raw points that `k` statements survive.
const OVER_FACTOR: usize = 8;

/// Default HNSW knobs for the statement-question pool. Reuses
/// [`EntityHnswParams`]; capacity sized for several points per statement.
#[must_use]
pub fn statement_question_default_params() -> EntityHnswParams {
    EntityHnswParams {
        m: 16,
        ef_construction: 100,
        ef_search: 64,
        ef_search_max: 500,
        capacity_hint: 4096,
    }
}

#[derive(Debug, Error)]
pub enum StatementQuestionHnswError {
    #[error("invalid params: {0}")]
    InvalidParams(#[from] crate::params::IndexParamsError),

    #[error("ef_search {ef} above ef_search_max {max}")]
    EfSearchTooLarge { ef: usize, max: usize },
}

/// Outcome of [`StatementQuestionHnswIndex::rebuild`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildReport {
    /// Number of question points re-inserted.
    pub inserted: usize,
    /// Number of distinct statements represented.
    pub statements: usize,
}

/// Per-shard HNSW over per-statement question embeddings (384-dim,
/// BGE-small). Many question points map to one [`StatementId`].
///
/// **Single-writer** by `&mut self` discipline.
pub struct StatementQuestionHnswIndex {
    inner: Hnsw<'static, f32, DistCosine>,
    params: EntityHnswParams,
    /// Internal u32 point id → the `(StatementId, Slot)` the point answers.
    /// A statement legitimately owns several slots (Object / Subject /
    /// Time), each with its own question points, so the target carries the
    /// slot alongside the statement.
    forward: Vec<(StatementId, Slot)>,
    /// `StatementId` → the internal point ids it owns, across ALL slots.
    /// Kept statement-scoped (not slot-scoped) so a tombstone / FORGET /
    /// supersession cascade drops every one of a statement's points at once.
    by_statement: HashMap<StatementId, Vec<u32>>,
    tombstones: TombstoneBitmap,
}

impl StatementQuestionHnswIndex {
    /// Construct an empty index with the given parameters.
    pub fn new(params: EntityHnswParams) -> Result<Self, StatementQuestionHnswError> {
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
            by_statement: HashMap::new(),
            tombstones: TombstoneBitmap::new(),
        })
    }

    /// Insert one question `vector` owned by `statement_id`, tagged with the
    /// `slot` of the reified fact the question leaves unbound.
    pub fn insert(&mut self, statement_id: StatementId, slot: Slot, vector: &[f32; VECTOR_DIM]) {
        let internal_id = u32::try_from(self.forward.len())
            .expect("invariant: statement-question point count never reaches u32::MAX");
        self.forward.push((statement_id, slot));
        self.by_statement
            .entry(statement_id)
            .or_default()
            .push(internal_id);
        self.inner
            .insert_slice((vector.as_slice(), internal_id as usize));
    }

    /// Whether `statement_id` already owns at least one point.
    #[must_use]
    pub fn contains_statement(&self, statement_id: StatementId) -> bool {
        self.by_statement.contains_key(&statement_id)
    }

    /// Search the top-`k` nearest **(statement, slot)** targets to `query`,
    /// collapsing raw question hits to the best similarity per
    /// `(StatementId, Slot)` pair. A single statement's Object / Subject /
    /// Time questions are DISTINCT targets and each survives independently —
    /// collapse is per-pair, not per-statement. Returns
    /// `(StatementId, Slot, similarity)` sorted descending. Tombstoned
    /// points excluded.
    pub fn search(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
    ) -> Result<Vec<(StatementId, Slot, f32)>, StatementQuestionHnswError> {
        self.search_with_ef(query, k, None)
    }

    /// Variant of [`Self::search`] with an explicit `ef_search` override.
    pub fn search_with_ef(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
        ef: Option<usize>,
    ) -> Result<Vec<(StatementId, Slot, f32)>, StatementQuestionHnswError> {
        if k == 0 || self.forward.is_empty() {
            return Ok(Vec::new());
        }
        let base_ef = match ef {
            None => self.params.ef_search,
            Some(v) => {
                if v > self.params.ef_search_max {
                    return Err(StatementQuestionHnswError::EfSearchTooLarge {
                        ef: v,
                        max: self.params.ef_search_max,
                    });
                }
                v
            }
        };

        // Escalate the fetch width (and `ef`) when tombstone attrition —
        // FORGET / supersession cascades that tombstone a statement's whole
        // point set — starves the collapsed result. Many question points map
        // to one `(StatementId, Slot)` target, and a single fixed fetch can
        // collapse to fewer than `k` live targets if the nearest raw points
        // are mostly tombstoned; widen until we have `k` live targets or the
        // graph is exhausted. Termination is guaranteed: `fetch_k` is capped
        // at the node count and `ef` at `ef_search_max`, and each iteration
        // advances at least one of them until both saturate.
        let total_nodes = self.forward.len();
        let mut fetch_multiplier = OVER_FACTOR;
        let mut ef = base_ef.min(self.params.ef_search_max);
        let mut best: HashMap<(StatementId, Slot), f32> = HashMap::new();
        loop {
            best.clear();
            let fetch_k = k.saturating_mul(fetch_multiplier).min(total_nodes);
            let effective_ef = ef.max(fetch_k).min(self.params.ef_search_max);
            let neighbours: Vec<Neighbour> =
                self.inner.search(query.as_slice(), fetch_k, effective_ef);
            for n in neighbours {
                let Ok(internal_id) = u32::try_from(n.d_id) else {
                    continue;
                };
                if self.tombstones.is_set(internal_id) {
                    continue;
                }
                let Some(target) = self.forward.get(internal_id as usize).copied() else {
                    continue;
                };
                let sim = 1.0 - n.distance;
                best.entry(target)
                    .and_modify(|cur| {
                        if sim > *cur {
                            *cur = sim;
                        }
                    })
                    .or_insert(sim);
            }

            if best.len() >= k {
                break;
            }
            let fetch_saturated = fetch_k >= total_nodes;
            let ef_saturated = effective_ef >= self.params.ef_search_max;
            if fetch_saturated && ef_saturated {
                break;
            }
            if !fetch_saturated {
                fetch_multiplier = fetch_multiplier.saturating_mul(2);
            }
            if !ef_saturated {
                ef = ef.saturating_mul(2).min(self.params.ef_search_max);
            }
        }

        let mut out: Vec<(StatementId, Slot, f32)> = best
            .into_iter()
            .map(|((id, slot), sim)| (id, slot, sim))
            .collect();
        out.sort_by(|a, b| {
            b.2.partial_cmp(&a.2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.to_bytes().cmp(&b.0.to_bytes()))
                .then_with(|| a.1.as_u8().cmp(&b.1.as_u8()))
        });
        out.truncate(k);
        Ok(out)
    }

    /// Tombstone every question point owned by `statement_id` (the
    /// supersession / FORGET cascade analogue).
    pub fn mark_statement_tombstoned(&mut self, statement_id: StatementId) {
        if let Some(ids) = self.by_statement.get(&statement_id) {
            for id in ids {
                self.tombstones.set(*id);
            }
        }
    }

    /// Number of question points (including tombstoned).
    #[must_use]
    pub fn len(&self) -> usize {
        self.forward.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    /// Number of distinct statements with at least one point.
    #[must_use]
    pub fn statement_count(&self) -> usize {
        self.by_statement.len()
    }

    #[must_use]
    pub fn tombstone_count(&self) -> usize {
        self.tombstones.count()
    }

    #[must_use]
    pub fn params(&self) -> EntityHnswParams {
        self.params
    }

    /// Discard the current index and re-insert every `(StatementId, Slot,
    /// vector)` from `points`. Callers pre-filter points of tombstoned /
    /// superseded statements.
    pub fn rebuild<I>(&mut self, points: I) -> RebuildReport
    where
        I: IntoIterator<Item = (StatementId, Slot, [f32; VECTOR_DIM])>,
    {
        self.inner = Hnsw::<f32, DistCosine>::new(
            self.params.m,
            self.params.capacity_hint,
            MAX_LAYER,
            self.params.ef_construction,
            DistCosine,
        );
        self.forward.clear();
        self.by_statement.clear();
        self.tombstones.clear();

        let mut report = RebuildReport::default();
        for (statement_id, slot, vector) in points {
            self.insert(statement_id, slot, &vector);
            report.inserted += 1;
        }
        report.statements = self.by_statement.len();
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_hot(seed: usize) -> [f32; VECTOR_DIM] {
        let mut v = [0.0; VECTOR_DIM];
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
        let mut v = [0.0; VECTOR_DIM];
        v[0] = 1.0;
        v[1 + (seed % (VECTOR_DIM - 1))] = mag;
        v
    }

    fn sid(seed: u8) -> StatementId {
        let mut b = [0u8; 16];
        b[0] = seed;
        StatementId::from_bytes(b)
    }

    #[test]
    fn insert_search_collapse_and_tombstone() {
        let mut idx = StatementQuestionHnswIndex::new(statement_question_default_params()).unwrap();
        let a = sid(1);
        idx.insert(a, Slot::Object, &one_hot(1));
        idx.insert(a, Slot::Object, &one_hot(200)); // same (statement, slot), two questions
        idx.insert(sid(2), Slot::Object, &one_hot(50));
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.statement_count(), 2);

        let r = idx.search(&one_hot(1), 5).unwrap();
        assert_eq!(r[0].0, a, "best-matching statement first");
        assert_eq!(r[0].1, Slot::Object, "slot tag preserved");
        assert!(
            r.iter()
                .filter(|(s, sl, _)| *s == a && *sl == Slot::Object)
                .count()
                == 1,
            "collapsed per (statement, slot)"
        );

        idx.mark_statement_tombstoned(a);
        let r = idx.search(&one_hot(1), 5).unwrap();
        assert!(
            !r.iter().any(|(s, _, _)| *s == a),
            "tombstoned statement excluded"
        );
    }

    #[test]
    fn same_statement_distinct_slots_survive_independently() {
        // The same statement owns an Object question and a Time question at
        // different vectors; each must surface as its own target — collapse
        // is per (statement, slot), not per statement.
        let mut idx = StatementQuestionHnswIndex::new(statement_question_default_params()).unwrap();
        let a = sid(7);
        idx.insert(a, Slot::Object, &one_hot(1));
        idx.insert(a, Slot::Time, &one_hot(300));
        assert_eq!(idx.statement_count(), 1);

        let obj = idx.search(&one_hot(1), 5).unwrap();
        assert!(obj.iter().any(|(s, sl, _)| *s == a && *sl == Slot::Object));
        let time = idx.search(&one_hot(300), 5).unwrap();
        assert!(time.iter().any(|(s, sl, _)| *s == a && *sl == Slot::Time));
    }

    #[test]
    fn rebuild_discards_prior_entries_and_loads_new_set() {
        let mut idx = StatementQuestionHnswIndex::new(statement_question_default_params()).unwrap();
        idx.insert(sid(1), Slot::Object, &one_hot(1));
        let rep = idx.rebuild([
            (sid(2), Slot::Object, one_hot(2)),
            (sid(2), Slot::Time, one_hot(3)),
            (sid(3), Slot::Object, one_hot(4)),
        ]);
        assert_eq!(rep.inserted, 3);
        assert_eq!(rep.statements, 2);
        assert!(!idx.contains_statement(sid(1)));
        assert!(idx.contains_statement(sid(2)));
    }

    #[test]
    fn search_widens_past_tombstone_attrition() {
        // One tombstoned statement owns 24 DISTINCT nearest question points —
        // exactly the initial fetch window (k*OVER_FACTOR = 3*8 = 24). A
        // single fixed fetch therefore lands entirely on that statement's
        // tombstoned points and collapses to zero live targets. Five live
        // statements sit slightly farther in the same tight near-cluster
        // (each ~cosine 1.0 to the query, distinct locations so HNSW recalls
        // them reliably). Escalation must widen the fetch past the tombstoned
        // front until the k live targets survive.
        //
        // Distinct locations (not a duplicate cluster) are load-bearing: a
        // dense pile of identical tombstoned vectors traps the HNSW traversal
        // and the live points never surface however wide the fetch.
        let mut idx = StatementQuestionHnswIndex::new(statement_question_default_params()).unwrap();
        let tombstoned = sid(1);
        for p in 0..24 {
            idx.insert(tombstoned, Slot::Object, &near_at(p, 0.02));
        }
        let live: Vec<StatementId> = (0..6).map(|i| sid(i as u8 + 100)).collect();
        for (i, id) in live.iter().enumerate() {
            idx.insert(*id, Slot::Object, &near_at(50 + i, 0.025));
        }
        idx.mark_statement_tombstoned(tombstoned);

        let r = idx.search(&query(), 3).unwrap();
        assert_eq!(r.len(), 3, "escalation should still return k live targets");
        let got: Vec<StatementId> = r.iter().map(|(id, _, _)| *id).collect();
        assert!(
            !got.contains(&tombstoned),
            "tombstoned statement surfaced after widening"
        );
        for (id, _, _) in &r {
            assert!(
                live.contains(id),
                "unexpected non-live statement in results"
            );
        }
    }

    #[test]
    fn search_exhausts_cleanly_when_fewer_than_k_live() {
        // Only 3 live of 6 total; search(k=5) can never reach k, so the test
        // pins the exhaustion/termination PROPERTY (not exact recall — hnsw_rs
        // gives only approximate recall on tiny graphs and may nondeterministically
        // drop a far node): search terminates, returns at most the live count,
        // strictly fewer than k, only live statements, and never a tombstoned one.
        let mut idx = StatementQuestionHnswIndex::new(statement_question_default_params()).unwrap();
        let live: Vec<StatementId> = (0..3).map(|i| sid(i as u8 + 1)).collect();
        for (i, id) in live.iter().enumerate() {
            idx.insert(*id, Slot::Object, &near_at(i, 0.02));
        }
        let tombstoned: Vec<StatementId> = (0..3).map(|i| sid(i as u8 + 100)).collect();
        for (i, id) in tombstoned.iter().enumerate() {
            idx.insert(*id, Slot::Object, &near_at(50 + i, 0.05));
            idx.mark_statement_tombstoned(*id);
        }

        let r = idx.search(&query(), 5).unwrap();
        assert!(
            !r.is_empty(),
            "should return the live survivors on exhaustion"
        );
        assert!(r.len() <= live.len(), "returned more than the live count");
        assert!(r.len() < 5, "returned k despite fewer than k live");
        for (id, _, _) in &r {
            assert!(
                live.contains(id),
                "non-live statement in exhaustion results"
            );
            assert!(
                !tombstoned.contains(id),
                "tombstoned statement surfaced on exhaustion"
            );
        }
    }
}
