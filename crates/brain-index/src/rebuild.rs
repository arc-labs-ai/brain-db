//! Build a fresh `HnswIndex` from an external iterator of
//! `(MemoryId, [f32; D])` pairs.
//!
//! See `spec/09_indexing/06_persistence.md` §2 (rebuild procedure)
//! and `spec/09_indexing/07_maintenance.md` §5 (the full-rebuild
//! flow — 4.6 implements only the **Build** phase; Catch-up,
//! Atomic-swap, and Cleanup are the Phase 8 maintenance worker's
//! responsibility).
//!
//! ## Caller owns the filter
//!
//! says tombstoned memories are skipped during
//! rebuild says corrupted vectors are skipped too.
//! Both filters are **upstream** of brain-index: the iterator the
//! caller passes yields only active, valid memories. `rebuild`
//! itself just iterates and inserts.
//!
//! ## Sequential in v1
//!
//! mentions parallel insertion as a perf target. v1
//! ships sequential insertion — simpler, deterministic, fine for
//! shard sizes ≤ 1M (~30 s rebuild). A `rebuild_parallel` is a
//! small additive change later, since hnsw_rs already exposes
//! `parallel_insert_slice`.

use std::time::{Duration, Instant};

use brain_core::MemoryId;

use crate::hnsw::{HnswError, HnswIndex};
use crate::params::IndexParams;

/// Observability snapshot returned alongside a rebuilt index.
/// Used by the Phase 8 maintenance worker for the
/// `last_rebuild_duration_ms` metric.
#[derive(Debug, Clone, Copy)]
pub struct RebuildReport {
    /// Count of memories successfully inserted into the new index.
    pub memories_inserted: u64,
    /// Wall-clock time spent in the build phase.
    pub duration: Duration,
}

/// Build a fresh `HnswIndex<D>` from `source`. The iterator is
/// consumed in order; the iteration order influences HNSW graph
/// quality slightly (recommends metadata-store /
/// MemoryId order — UUIDv7's time-ordered prefix gives roughly
/// insertion-time order, which is a sensible default).
///
/// Returns the fresh index plus a [`RebuildReport`]. Tombstones
/// start empty (the iterator skips them's
/// "compaction" property).
///
/// Errors:
/// - `HnswError::InvalidParams` if `params` doesn't validate.
/// - `HnswError::DuplicateMemoryId` if the iterator yields the same
///   `MemoryId` twice (caller bug).
/// - `HnswError::IdMapExhausted` at `u32::MAX` items.
pub fn rebuild_impl<const D: usize, I>(
    params: IndexParams,
    source: I,
) -> Result<(HnswIndex<D>, RebuildReport), HnswError>
where
    I: IntoIterator<Item = (MemoryId, [f32; D])>,
{
    let started_at = Instant::now();
    let mut idx = HnswIndex::<D>::new(params)?;
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

    fn mid(slot: u64) -> MemoryId {
        MemoryId::pack(1, slot, 1)
    }

    fn vec4(a: f32, b: f32, c: f32, d: f32) -> [f32; 4] {
        let n = (a * a + b * b + c * c + d * d).sqrt();
        [a / n, b / n, c / n, d / n]
    }

    #[test]
    fn rebuild_empty_iterator() {
        let (idx, report) =
            rebuild_impl::<4, _>(IndexParams::default_v1(), std::iter::empty()).unwrap();
        assert_eq!(idx.len(), 0);
        assert!(idx.is_empty());
        assert_eq!(report.memories_inserted, 0);
    }

    #[test]
    fn rebuild_from_iterator_yields_correct_len() {
        let source = vec![
            (mid(1), vec4(1.0, 0.0, 0.0, 0.0)),
            (mid(2), vec4(0.0, 1.0, 0.0, 0.0)),
            (mid(3), vec4(0.0, 0.0, 1.0, 0.0)),
        ];
        let (idx, report) = rebuild_impl::<4, _>(IndexParams::default_v1(), source).unwrap();
        assert_eq!(idx.len(), 3);
        assert_eq!(report.memories_inserted, 3);
    }

    #[test]
    fn rebuild_uses_provided_params() {
        let params = IndexParams {
            m: 32,
            ef_construction: 100,
            ef_search: 50,
            ef_search_max: 250,
        };
        let (idx, _) = rebuild_impl::<4, _>(params, std::iter::empty()).unwrap();
        assert_eq!(idx.params(), params);
    }

    #[test]
    fn rebuild_starts_with_empty_tombstones() {
        let source = vec![
            (mid(1), vec4(1.0, 0.0, 0.0, 0.0)),
            (mid(2), vec4(0.0, 1.0, 0.0, 0.0)),
        ];
        let (idx, _) = rebuild_impl::<4, _>(IndexParams::default_v1(), source).unwrap();
        assert_eq!(idx.tombstone_count(), 0);
        assert!(!idx.is_tombstoned(mid(1)));
        assert!(!idx.is_tombstoned(mid(2)));
    }

    #[test]
    fn rebuild_rejects_duplicate_memory_id() {
        // Two entries with the same MemoryId — caller bug.
        let source = vec![
            (mid(7), vec4(1.0, 0.0, 0.0, 0.0)),
            (mid(7), vec4(0.0, 1.0, 0.0, 0.0)),
        ];
        match rebuild_impl::<4, _>(IndexParams::default_v1(), source) {
            Err(HnswError::DuplicateMemoryId { memory_id_bytes }) => {
                assert_eq!(memory_id_bytes, mid(7).to_be_bytes());
            }
            Err(e) => panic!("expected DuplicateMemoryId, got error {e}"),
            Ok(_) => panic!("expected DuplicateMemoryId, got Ok"),
        }
    }
}
