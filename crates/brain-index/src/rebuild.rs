//! Build a fresh `HnswIndex` from an external iterator of
//! `(MemoryId, [f32; VECTOR_DIM])` pairs.
//!
//! See `spec/09_indexing/06_persistence.md` §2 (rebuild procedure)
//! and `spec/09_indexing/07_hnsw_pq.md` §8 (PQ-aware rebuild — the
//! arena vector is encoded against the supplied codebook before
//! landing in the HNSW graph).
//!
//! ## Caller owns the filter
//!
//! Tombstoned memories are skipped upstream of brain-index; the
//! iterator the caller passes yields only active, valid memories.
//! `rebuild` itself just iterates and inserts.
//!
//! ## Sequential in v1
//!
//! v1 ships sequential insertion — simpler, deterministic, fine for
//! shard sizes ≤ 1M. A parallel variant is a small additive change
//! later (hnsw_rs already exposes `parallel_insert_slice`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use brain_core::MemoryId;

use crate::hnsw::{HnswError, HnswIndexImpl};
use crate::params::{IndexParams, VECTOR_DIM};
use crate::pq::Codebook;

/// Observability snapshot returned alongside a rebuilt index.
/// Used by the maintenance worker for the
/// `last_rebuild_duration_ms` metric.
#[derive(Debug, Clone, Copy)]
pub struct RebuildReport {
    /// Count of memories successfully inserted into the new index.
    pub memories_inserted: u64,
    /// Wall-clock time spent in the build phase.
    pub duration: Duration,
}

/// Build a fresh PQ-flavour `HnswIndexImpl<M>` from `source`. Every
/// vector is PQ-encoded against `codebook` before it lands in the
/// graph; the iteration order influences HNSW graph quality
/// slightly.
///
/// Returns the fresh index plus a [`RebuildReport`]. Tombstones
/// start empty (the iterator skips them upstream).
///
/// Errors:
/// - [`HnswError::InvalidParams`] if `params` doesn't validate.
/// - [`HnswError::DuplicateMemoryId`] if the iterator yields the
///   same `MemoryId` twice (caller bug).
/// - [`HnswError::IdMapExhausted`] at `u32::MAX` items.
/// - [`HnswError::Encode`] if any vector contains NaN / infinity.
pub fn rebuild_impl<const M: usize, I>(
    params: IndexParams,
    codebook: Arc<Codebook<M>>,
    source: I,
) -> Result<(HnswIndexImpl<M>, RebuildReport), HnswError>
where
    I: IntoIterator<Item = (MemoryId, [f32; VECTOR_DIM])>,
{
    let started_at = Instant::now();
    let mut idx = HnswIndexImpl::<M>::new(params, (*codebook).clone())?;
    let mut count: u64 = 0;
    for (memory_id, vector) in source {
        idx.insert(memory_id, &vector)?;
        count += 1;
    }
    let report = RebuildReport {
        memories_inserted: count,
        duration: started_at.elapsed(),
    };
    Ok((idx, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pq::bootstrap_codebook;

    fn mid(slot: u64) -> MemoryId {
        MemoryId::pack(1, slot, 1)
    }

    /// Build a 384-dim unit vector with `value` in the first
    /// component, zero elsewhere — normalised. Quick fixture for
    /// rebuild tests that don't need realistic distributions.
    fn unit_first(value: f32) -> [f32; VECTOR_DIM] {
        let mut v = [0.0_f32; VECTOR_DIM];
        v[0] = value.signum();
        v
    }

    #[test]
    fn rebuild_empty_iterator() {
        let cb = bootstrap_codebook();
        let (idx, report) = rebuild_impl::<8, _>(
            IndexParams::default_v1(),
            cb,
            std::iter::empty::<(MemoryId, [f32; VECTOR_DIM])>(),
        )
        .unwrap();
        assert_eq!(idx.len(), 0);
        assert!(idx.is_empty());
        assert_eq!(report.memories_inserted, 0);
    }

    #[test]
    fn rebuild_from_iterator_yields_correct_len() {
        let cb = bootstrap_codebook();
        let source = vec![
            (mid(1), unit_first(1.0)),
            (mid(2), unit_first(-1.0)),
            (mid(3), unit_first(1.0)),
        ];
        let (idx, report) = rebuild_impl::<8, _>(IndexParams::default_v1(), cb, source).unwrap();
        assert_eq!(idx.len(), 3);
        assert_eq!(report.memories_inserted, 3);
    }

    #[test]
    fn rebuild_uses_provided_params() {
        let cb = bootstrap_codebook();
        let params = IndexParams {
            m: 32,
            ef_construction: 100,
            ef_search: 50,
            ef_search_max: 250,
        };
        let (idx, _) = rebuild_impl::<8, _>(
            params,
            cb,
            std::iter::empty::<(MemoryId, [f32; VECTOR_DIM])>(),
        )
        .unwrap();
        assert_eq!(idx.params(), params);
    }

    #[test]
    fn rebuild_rejects_duplicate_memory_id() {
        let cb = bootstrap_codebook();
        let source = vec![(mid(7), unit_first(1.0)), (mid(7), unit_first(-1.0))];
        match rebuild_impl::<8, _>(IndexParams::default_v1(), cb, source) {
            Err(HnswError::DuplicateMemoryId { memory_id_bytes }) => {
                assert_eq!(memory_id_bytes, mid(7).to_be_bytes());
            }
            Err(e) => panic!("expected DuplicateMemoryId, got error {e}"),
            Ok(_) => panic!("expected DuplicateMemoryId, got Ok"),
        }
    }
}
