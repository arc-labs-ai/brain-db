//! `StatementSimilaritySource` adapter over the per-shard
//! `StatementHnswIndex`.
//!
//! The metadata-layer
//! [`brain_metadata::statement::TieredSupersedeDecider`] calls
//! `StatementSimilaritySource::nearest` on the candidate vector for
//! Tier 1/2 of the supersession ladder. The metadata crate itself
//! does not depend on `brain-index` — inverting the dep would break
//! the layering — so the adapter lives here where the LLM judge
//! (`LlmExtractor`) and the HNSW index can be wired together cheaply.
//!
//! ## Wiring
//!
//! Pass the per-shard `Arc<RwLock<StatementHnswIndex>>` (already
//! threaded through brain-ops + brain-workers for the embedder
//! worker) plus the metadata db handle. The adapter takes a read
//! lock on the HNSW for the search and a snapshot of the metadata
//! to materialise candidate rows.

use std::sync::Arc;

use brain_core::StatementId;
use brain_index::statement_hnsw::StatementHnswIndex;
use brain_metadata::statement::{
    statement_get, StatementOpError, StatementSimilarityCandidate, StatementSimilaritySource,
};
use parking_lot::RwLock;
use redb::ReadTransaction;

/// Adapter wrapping the per-shard statement HNSW so the metadata
/// decider can query it without taking a direct dep on `brain-index`.
///
/// `nearest` truncates the new vector to the HNSW's fixed dimension
/// when the caller passes a shorter slice (no panic — Tier 0
/// would have caught the obvious case before we got here).
pub struct StatementHnswSource {
    pub hnsw: Arc<RwLock<StatementHnswIndex>>,
}

impl StatementHnswSource {
    pub fn new(hnsw: Arc<RwLock<StatementHnswIndex>>) -> Self {
        Self { hnsw }
    }
}

impl StatementSimilaritySource for StatementHnswSource {
    fn nearest(
        &self,
        rtxn: &ReadTransaction,
        query_vector: &[f32],
        k: usize,
    ) -> Result<Vec<StatementSimilarityCandidate>, StatementOpError> {
        // The HNSW search API takes a fixed-size array reference. If
        // the caller's vector is shorter / longer we return empty —
        // Tier 1/2 short-circuit to Coexist, which is the safe
        // fallback.
        const DIM: usize = brain_index::params::VECTOR_DIM;
        if query_vector.len() != DIM {
            return Ok(Vec::new());
        }
        let arr: &[f32; DIM] = query_vector
            .try_into()
            .expect("invariant: query_vector.len() == DIM, length-checked above");

        // Hold the read lock only for the duration of the HNSW search
        // — re-locking per-candidate would let writers slip in
        // between but the snapshot semantics of the cosine band are
        // already approximate (we accept transient skew).
        let hits = {
            let guard = self.hnsw.read();
            match guard.search(arr, k) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        target: "brain_extractors::supersede_source",
                        error = %e,
                        "StatementHnsw search failed; returning empty candidate set"
                    );
                    return Ok(Vec::new());
                }
            }
        };

        // Materialise the full Statement rows from metadata. A miss
        // here means the row was tombstoned + reclaimed between the
        // HNSW write and now — drop the candidate silently.
        let mut out: Vec<StatementSimilarityCandidate> = Vec::with_capacity(hits.len());
        for (id, score) in hits {
            let s: StatementId = id;
            let Some(stmt) = statement_get(rtxn, s)? else {
                continue;
            };
            out.push(StatementSimilarityCandidate {
                statement_id: s,
                statement: stmt,
                score,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_index::statement_hnsw::StatementHnswParams;

    #[test]
    fn nearest_with_wrong_dim_returns_empty() {
        let hnsw = Arc::new(RwLock::new(
            StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap(),
        ));
        let source = StatementHnswSource::new(hnsw);

        let tmp = tempfile::tempdir().unwrap();
        let db = brain_metadata::MetadataDb::open(tmp.path().join("md.redb")).unwrap();
        let rtxn = db.read_txn().unwrap();

        // Dim 5 ≠ DIM (384) → empty.
        let out = source.nearest(&rtxn, &[1.0; 5], 10).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn empty_hnsw_returns_empty() {
        let hnsw = Arc::new(RwLock::new(
            StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap(),
        ));
        let source = StatementHnswSource::new(hnsw);

        let tmp = tempfile::tempdir().unwrap();
        let db = brain_metadata::MetadataDb::open(tmp.path().join("md.redb")).unwrap();
        let rtxn = db.read_txn().unwrap();

        let q = vec![0.5_f32; brain_index::params::VECTOR_DIM];
        let out = source.nearest(&rtxn, &q, 10).unwrap();
        assert!(out.is_empty());
    }
}
