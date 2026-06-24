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
//! Like the HyPE index the mapping is **many-to-one** (a statement owns
//! several question points), so [`StatementQuestionHnswIndex::search`]
//! collapses raw question hits to the best similarity per statement.
//!
//! - In-memory only; the vectors persist in redb
//!   (`statement_question_vectors`) and this index is rebuilt on boot.
//! - Single-owner; the shard wraps it in `Arc<RwLock<_>>`.

use std::collections::HashMap;

use brain_core::StatementId;
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
    /// Internal u32 point id → owning `StatementId`.
    forward: Vec<StatementId>,
    /// `StatementId` → the internal point ids it owns.
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

    /// Insert one question `vector` owned by `statement_id`.
    pub fn insert(&mut self, statement_id: StatementId, vector: &[f32; VECTOR_DIM]) {
        let internal_id = u32::try_from(self.forward.len())
            .expect("invariant: statement-question point count never reaches u32::MAX");
        self.forward.push(statement_id);
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

    /// Search the top-`k` nearest **statements** to `query`, collapsing raw
    /// question hits to the best similarity per statement. Returns
    /// `(StatementId, similarity)` sorted descending. Tombstoned points
    /// excluded.
    pub fn search(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
    ) -> Result<Vec<(StatementId, f32)>, StatementQuestionHnswError> {
        self.search_with_ef(query, k, None)
    }

    /// Variant of [`Self::search`] with an explicit `ef_search` override.
    pub fn search_with_ef(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
        ef: Option<usize>,
    ) -> Result<Vec<(StatementId, f32)>, StatementQuestionHnswError> {
        if k == 0 || self.forward.is_empty() {
            return Ok(Vec::new());
        }
        let fetch_k = k.saturating_mul(OVER_FACTOR).min(self.forward.len());
        let ef = match ef {
            None => self.params.ef_search.max(fetch_k),
            Some(v) => {
                if v > self.params.ef_search_max {
                    return Err(StatementQuestionHnswError::EfSearchTooLarge {
                        ef: v,
                        max: self.params.ef_search_max,
                    });
                }
                v.max(fetch_k)
            }
        };

        let neighbours: Vec<Neighbour> = self.inner.search(query.as_slice(), fetch_k, ef);
        let mut best: HashMap<StatementId, f32> = HashMap::new();
        for n in neighbours {
            let Ok(internal_id) = u32::try_from(n.d_id) else {
                continue;
            };
            if self.tombstones.is_set(internal_id) {
                continue;
            }
            let Some(statement_id) = self.forward.get(internal_id as usize).copied() else {
                continue;
            };
            let sim = 1.0 - n.distance;
            best.entry(statement_id)
                .and_modify(|cur| {
                    if sim > *cur {
                        *cur = sim;
                    }
                })
                .or_insert(sim);
        }

        let mut out: Vec<(StatementId, f32)> = best.into_iter().collect();
        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.to_bytes().cmp(&b.0.to_bytes()))
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

    /// Discard the current index and re-insert every `(StatementId,
    /// vector)` from `points`. Callers pre-filter points of tombstoned /
    /// superseded statements.
    pub fn rebuild<I>(&mut self, points: I) -> RebuildReport
    where
        I: IntoIterator<Item = (StatementId, [f32; VECTOR_DIM])>,
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
        for (statement_id, vector) in points {
            self.insert(statement_id, &vector);
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

    fn sid(seed: u8) -> StatementId {
        let mut b = [0u8; 16];
        b[0] = seed;
        StatementId::from_bytes(b)
    }

    #[test]
    fn insert_search_collapse_and_tombstone() {
        let mut idx = StatementQuestionHnswIndex::new(statement_question_default_params()).unwrap();
        let a = sid(1);
        idx.insert(a, &one_hot(1));
        idx.insert(a, &one_hot(200)); // same statement, two questions
        idx.insert(sid(2), &one_hot(50));
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.statement_count(), 2);

        let r = idx.search(&one_hot(1), 5).unwrap();
        assert_eq!(r[0].0, a, "best-matching statement first");
        assert!(r.iter().filter(|(s, _)| *s == a).count() == 1, "collapsed per statement");

        idx.mark_statement_tombstoned(a);
        let r = idx.search(&one_hot(1), 5).unwrap();
        assert!(!r.iter().any(|(s, _)| *s == a), "tombstoned statement excluded");
    }

    #[test]
    fn rebuild_discards_prior_entries_and_loads_new_set() {
        let mut idx = StatementQuestionHnswIndex::new(statement_question_default_params()).unwrap();
        idx.insert(sid(1), &one_hot(1));
        let rep = idx.rebuild([(sid(2), one_hot(2)), (sid(2), one_hot(3)), (sid(3), one_hot(4))]);
        assert_eq!(rep.inserted, 3);
        assert_eq!(rep.statements, 2);
        assert!(!idx.contains_statement(sid(1)));
        assert!(idx.contains_statement(sid(2)));
    }
}
