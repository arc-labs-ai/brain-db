//! Two-tier lock-free wrapper for the PQ-flavour HNSW.
//!
//! Mirrors [`crate::shared::SharedHnsw`]'s shape — immutable
//! [`MainPqEpoch`] swapped via `ArcSwap`, mutable [`PendingPqBuffer`]
//! protected by `RwLock`. Differences:
//!
//! - Vector dim is fixed at [`VECTOR_DIM`] (the PQ codebook is trained
//!   for one shape). The const generic is `M` (subquantiser count)
//!   instead of `D`.
//! - Search composes ADC against main with exact cosine against
//!   pending, then re-ranks the merged candidate set against the
//!   full-precision arena via a caller-supplied closure.
//! - The pending buffer holds full-precision `f32` vectors so pending
//!   inserts are visible at exact cosine immediately; the closure
//!   isn't called for them.
//!
//! Spec: `spec/09_indexing/07_hnsw_pq.md` §§7-9.

use std::collections::HashSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use brain_core::MemoryId;
use parking_lot::RwLock;

use crate::params::{IndexParams, VECTOR_DIM};
use crate::pq::rerank;
use crate::pq_hnsw::{PqHnswError, PqHnswIndex};

/// An immutable PQ-HNSW snapshot for a single published epoch.
struct MainPqEpoch<const M: usize> {
    index: PqHnswIndex<M>,
    epoch_id: u64,
}

/// Recent inserts and tombstones that haven't yet been folded into
/// the main PQ-HNSW. Full-precision vectors live here — encoding
/// happens during the flush rebuild.
struct PendingPqBuffer {
    entries: Vec<PendingPqEntry>,
    tombstoned: HashSet<MemoryId>,
}

impl PendingPqBuffer {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            tombstoned: HashSet::new(),
        }
    }
}

/// A full-precision vector waiting to be PQ-encoded and folded into
/// the main HNSW. The `vector` field is kept verbatim so pending
/// search uses exact cosine.
#[derive(Clone, Debug)]
pub struct PendingPqEntry {
    pub memory_id: MemoryId,
    pub vector: [f32; VECTOR_DIM],
    pub tombstoned: bool,
}

/// Report returned by [`SharedPqHnsw::flush_with_rebuild`]. Same shape
/// as the pure-HNSW flush report so the maintenance worker can branch
/// on the flavour once and emit consistent metrics either way.
#[derive(Debug, Clone)]
pub struct PqFlushReport {
    pub entries_flushed: usize,
    pub new_epoch: u64,
    pub main_len_after: usize,
}

/// Cloneable reader handle for a PQ-flavour shared index.
#[derive(Clone)]
pub struct SharedPqHnsw<const M: usize> {
    main: Arc<ArcSwap<MainPqEpoch<M>>>,
    pending: Arc<RwLock<PendingPqBuffer>>,
}

/// Single-writer handle. Not `Clone` — enforces single-writer-per-shard
/// at the type level, matching [`crate::shared::Writer`].
pub struct PqWriter<const M: usize> {
    /// Kept so the published main outlives the writer regardless of
    /// reader cloning patterns; never read directly.
    _main: Arc<ArcSwap<MainPqEpoch<M>>>,
    pending: Arc<RwLock<PendingPqBuffer>>,
}

impl<const M: usize> SharedPqHnsw<M> {
    /// Wrap an existing [`PqHnswIndex`], returning the reader/writer
    /// pair. The caller has already trained the codebook and built the
    /// initial index.
    #[must_use]
    pub fn from_index(idx: PqHnswIndex<M>) -> (Self, PqWriter<M>) {
        let epoch = Arc::new(MainPqEpoch {
            index: idx,
            epoch_id: 0,
        });
        let main = Arc::new(ArcSwap::new(epoch));
        let pending = Arc::new(RwLock::new(PendingPqBuffer::new()));
        let reader = Self {
            main: main.clone(),
            pending: pending.clone(),
        };
        let writer = PqWriter {
            _main: main,
            pending,
        };
        (reader, writer)
    }

    // ----- Reader methods --------------------------------------------------

    /// Top-`k` nearest neighbours of `query`, with the PQ re-rank pass
    /// applied. Returns `(MemoryId, cosine_similarity)` pairs sorted
    /// descending by similarity — same shape the pure-HNSW search
    /// returns.
    ///
    /// `arena_lookup` resolves a memory id to its full-precision
    /// vector. Called only for candidates whose vector isn't already
    /// in the pending buffer; tombstoned-during-search candidates are
    /// silently dropped (same partial-results contract as
    /// [`crate::shared::SharedHnsw::search`]).
    ///
    /// `filter` runs as an extra predicate alongside the always-on
    /// tombstone filter.
    #[must_use]
    pub fn search<F, A>(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
        ef: Option<usize>,
        filter: F,
        arena_lookup: A,
    ) -> Vec<(MemoryId, f32)>
    where
        F: Fn(MemoryId) -> bool,
        A: Fn(MemoryId) -> Option<[f32; VECTOR_DIM]>,
    {
        if k == 0 {
            return Vec::new();
        }

        // Pending tombstone overlay wins everywhere.
        let pending_tombstones: HashSet<MemoryId> = self.pending.read().tombstoned.clone();

        // 1. Main: PQ-ADC top-K' where K' = k * rerank_factor.
        let epoch = self.main.load();
        let rerank_factor = epoch
            .index
            .params()
            .pq
            .map(|p| usize::from(p.rerank_factor))
            .unwrap_or(4);
        let inflated_k = k.saturating_mul(rerank_factor.max(1));
        let main_candidates = epoch.index.search(query, inflated_k, ef, |id| {
            !pending_tombstones.contains(&id) && filter(id)
        });

        // Re-rank main candidates against the arena. Pending hits are
        // handled separately because their vectors live in memory.
        let main_reranked = rerank::<_>(&main_candidates, query, inflated_k, |id| {
            arena_lookup(id)
        });

        // 2. Pending: brute-force exact cosine. Tombstoned overlay
        //    already excluded above.
        let pending_hits = self.pending_search(query, inflated_k, &filter);

        // 3. Merge + dedupe by MemoryId, prefer pending's score on
        //    collision (latest vector wins).
        merge_dedupe_descending(main_reranked, pending_hits, k)
    }

    /// Top-`k` nearest neighbours, excluding tombstoned memories.
    /// Convenience for the common case.
    #[must_use]
    pub fn search_active<A>(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
        ef: Option<usize>,
        arena_lookup: A,
    ) -> Vec<(MemoryId, f32)>
    where
        A: Fn(MemoryId) -> Option<[f32; VECTOR_DIM]>,
    {
        self.search(query, k, ef, |_| true, arena_lookup)
    }

    /// Is `memory_id` present (and not tombstoned) in either tier?
    #[must_use]
    pub fn contains(&self, memory_id: MemoryId) -> bool {
        let pending = self.pending.read();
        if pending.tombstoned.contains(&memory_id) {
            return false;
        }
        if pending
            .entries
            .iter()
            .any(|e| e.memory_id == memory_id && !e.tombstoned)
        {
            return true;
        }
        drop(pending);
        let epoch = self.main.load();
        epoch.index.contains(memory_id) && !epoch.index.is_tombstoned(memory_id)
    }

    /// Is `memory_id` tombstoned in either tier?
    #[must_use]
    pub fn is_tombstoned(&self, memory_id: MemoryId) -> bool {
        if self.pending.read().tombstoned.contains(&memory_id) {
            return true;
        }
        self.main.load().index.is_tombstoned(memory_id)
    }

    /// Approximate combined size: published main plus pending entries.
    #[must_use]
    pub fn len(&self) -> usize {
        let pending = self.pending.read();
        let pending_extra = pending.entries.iter().filter(|e| !e.tombstoned).count();
        self.main.load().index.len() + pending_extra
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        let pending = self.pending.read();
        if pending.entries.iter().any(|e| !e.tombstoned) {
            return false;
        }
        drop(pending);
        self.main.load().index.is_empty()
    }

    /// Index params of the published main.
    #[must_use]
    pub fn params(&self) -> IndexParams {
        self.main.load().index.params()
    }

    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.main.load().epoch_id
    }

    /// Atomically replace the published main with `new_index` and
    /// clear pending. Used for bootstrap and snapshot-load paths
    /// where main was rebuilt from a source of truth that already
    /// reflects all writes.
    pub fn swap(&self, new_index: PqHnswIndex<M>) {
        let prev = self.main.load();
        let next = Arc::new(MainPqEpoch {
            index: new_index,
            epoch_id: prev.epoch_id.wrapping_add(1),
        });
        self.main.store(next);
        let mut pending = self.pending.write();
        pending.entries.clear();
        pending.tombstoned.clear();
    }

    /// Snapshot pending entries, pass them to `build` to produce a new
    /// main, then atomically publish + drain the flushed ids. Same
    /// shape as [`crate::shared::SharedHnsw::flush_with_rebuild`].
    pub fn flush_with_rebuild<F>(&self, build: F) -> Result<PqFlushReport, PqHnswError>
    where
        F: FnOnce(&[PendingPqEntry]) -> Result<PqHnswIndex<M>, PqHnswError>,
    {
        let snapshot: Vec<PendingPqEntry> = self.pending.read().entries.clone();
        let snapshot_count = snapshot.len();

        let new_index = build(&snapshot)?;

        let mut pending = self.pending.write();
        let prev_epoch = self.main.load();
        let new_epoch_id = prev_epoch.epoch_id.wrapping_add(1);
        let main_len_after = new_index.len();
        let new_epoch = Arc::new(MainPqEpoch {
            index: new_index,
            epoch_id: new_epoch_id,
        });
        self.main.store(new_epoch);

        let flushed: HashSet<MemoryId> = snapshot.iter().map(|e| e.memory_id).collect();
        pending.entries.retain(|e| !flushed.contains(&e.memory_id));
        pending.tombstoned.retain(|id| !flushed.contains(id));

        Ok(PqFlushReport {
            entries_flushed: snapshot_count,
            new_epoch: new_epoch_id,
            main_len_after,
        })
    }

    /// Clone the current pending entries — used by the maintenance
    /// worker's flush prep.
    #[must_use]
    pub fn pending_snapshot(&self) -> Vec<PendingPqEntry> {
        self.pending.read().entries.clone()
    }

    /// Count of live (non-tombstoned) pending entries.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending
            .read()
            .entries
            .iter()
            .filter(|e| !e.tombstoned)
            .count()
    }

    // ----- Private helpers -------------------------------------------------

    /// Brute-force exact-cosine over the pending buffer. Pending holds
    /// full-precision vectors, so no re-rank needed.
    fn pending_search<F>(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
        filter: &F,
    ) -> Vec<(MemoryId, f32)>
    where
        F: Fn(MemoryId) -> bool,
    {
        let pending = self.pending.read();
        if pending.entries.is_empty() || k == 0 {
            return Vec::new();
        }
        let mut scored: Vec<(MemoryId, f32)> = pending
            .entries
            .iter()
            .filter(|e| !e.tombstoned && filter(e.memory_id))
            .map(|e| (e.memory_id, dot(query, &e.vector)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        scored
    }
}

impl<const M: usize> PqWriter<M> {
    /// Insert a full-precision vector. The PQ encode happens at flush
    /// time inside the build closure; until then the vector lives in
    /// pending and reads at exact cosine.
    pub fn insert(
        &mut self,
        memory_id: MemoryId,
        vector: &[f32; VECTOR_DIM],
    ) -> Result<(), PqHnswError> {
        let mut pending = self.pending.write();
        // Re-insert after tombstone resurrects the entry.
        pending.tombstoned.remove(&memory_id);
        if let Some(slot) = pending.entries.iter_mut().find(|e| e.memory_id == memory_id) {
            slot.vector = *vector;
            slot.tombstoned = false;
        } else {
            pending.entries.push(PendingPqEntry {
                memory_id,
                vector: *vector,
                tombstoned: false,
            });
        }
        Ok(())
    }

    /// Mark a memory tombstoned. Visible immediately via
    /// [`SharedPqHnsw::is_tombstoned`].
    pub fn mark_tombstoned(&mut self, memory_id: MemoryId) -> Result<(), PqHnswError> {
        let mut pending = self.pending.write();
        pending.tombstoned.insert(memory_id);
        if let Some(slot) = pending.entries.iter_mut().find(|e| e.memory_id == memory_id) {
            slot.tombstoned = true;
        }
        Ok(())
    }
}

// ===== Helpers =============================================================

/// Dot product of two equal-length `f32` vectors. With L2-normalised
/// inputs (BGE-small output, spec `§04/03 §1`) this equals cosine
/// similarity.
fn dot(a: &[f32; VECTOR_DIM], b: &[f32; VECTOR_DIM]) -> f32 {
    let mut sum = 0.0_f32;
    for i in 0..VECTOR_DIM {
        sum += a[i] * b[i];
    }
    sum
}

/// Merge main and pending hit lists, dedupe by `MemoryId` (pending
/// wins on collision), sort descending by similarity, truncate to `k`.
fn merge_dedupe_descending(
    main: Vec<(MemoryId, f32)>,
    pending: Vec<(MemoryId, f32)>,
    k: usize,
) -> Vec<(MemoryId, f32)> {
    use std::collections::HashMap;
    let mut by_id: HashMap<MemoryId, f32> = HashMap::with_capacity(main.len() + pending.len());
    for (id, score) in main {
        by_id.insert(id, score);
    }
    for (id, score) in pending {
        // Pending overrides main if both present.
        by_id.insert(id, score);
    }
    let mut merged: Vec<(MemoryId, f32)> = by_id.into_iter().collect();
    merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    merged.truncate(k);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pq::{Codebook, PqParams, PQ_CENTROIDS_PER_SUBSPACE};

    fn mid(slot: u8) -> MemoryId {
        MemoryId::pack(1, slot as u64, 1)
    }

    fn arithmetic_codebook<const M: usize>() -> Codebook<M> {
        let sub_dim = VECTOR_DIM / M;
        let mut centroids = vec![0.0_f32; M * PQ_CENTROIDS_PER_SUBSPACE * sub_dim];
        for s in 0..M {
            for k in 0..PQ_CENTROIDS_PER_SUBSPACE {
                let offset = (s * PQ_CENTROIDS_PER_SUBSPACE + k) * sub_dim;
                centroids[offset] = k as f32;
            }
        }
        Codebook::<M>::from_trained(centroids, sub_dim).unwrap()
    }

    fn unit_at_angle(angle_radians: f32) -> [f32; VECTOR_DIM] {
        let mut v = [0.0_f32; VECTOR_DIM];
        v[0] = angle_radians.cos();
        v[1] = angle_radians.sin();
        v
    }

    fn pq_params_default() -> IndexParams {
        IndexParams::default_v1().with_pq(PqParams::default_v1())
    }

    fn build_shared() -> (SharedPqHnsw<8>, PqWriter<8>) {
        let idx = PqHnswIndex::<8>::new(pq_params_default(), arithmetic_codebook::<8>()).unwrap();
        SharedPqHnsw::<8>::from_index(idx)
    }

    #[test]
    fn empty_search_returns_empty() {
        let (reader, _writer) = build_shared();
        let results = reader.search_active(&unit_at_angle(0.0), 5, None, |_| None);
        assert!(results.is_empty());
    }

    #[test]
    fn pending_insert_visible_to_reader_before_flush() {
        let (reader, mut writer) = build_shared();
        let v = unit_at_angle(0.0);
        writer.insert(mid(1), &v).unwrap();
        assert!(reader.contains(mid(1)));
        assert_eq!(reader.pending_len(), 1);

        // Pending hit ranks via exact cosine (1.0 against itself).
        let results = reader.search_active(&v, 1, None, |_id| Some(v));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, mid(1));
        assert!((results[0].1 - 1.0).abs() < 1e-5);
    }

    #[test]
    fn pending_tombstone_hides_from_reader() {
        let (reader, mut writer) = build_shared();
        let v = unit_at_angle(0.0);
        writer.insert(mid(2), &v).unwrap();
        writer.mark_tombstoned(mid(2)).unwrap();
        assert!(!reader.contains(mid(2)));
        assert!(reader.is_tombstoned(mid(2)));

        let results = reader.search_active(&v, 5, None, |_id| Some(v));
        assert!(results.iter().all(|(id, _)| *id != mid(2)));
    }

    #[test]
    fn swap_clears_pending_and_bumps_epoch() {
        let (reader, mut writer) = build_shared();
        writer.insert(mid(1), &unit_at_angle(0.0)).unwrap();
        assert_eq!(reader.pending_len(), 1);
        let before = reader.epoch();

        let replacement =
            PqHnswIndex::<8>::new(pq_params_default(), arithmetic_codebook::<8>()).unwrap();
        reader.swap(replacement);

        assert_eq!(reader.epoch(), before.wrapping_add(1));
        assert_eq!(reader.pending_len(), 0);
        // The swapped main is empty; the inserted memory is gone.
        assert!(!reader.contains(mid(1)));
    }

    #[test]
    fn flush_folds_pending_into_main() {
        let (reader, mut writer) = build_shared();
        let v = unit_at_angle(0.0);
        writer.insert(mid(1), &v).unwrap();
        writer.insert(mid(2), &v).unwrap();
        assert_eq!(reader.pending_len(), 2);

        let codebook_for_build = arithmetic_codebook::<8>();
        let report = reader
            .flush_with_rebuild(|snapshot| {
                let mut new_idx =
                    PqHnswIndex::<8>::new(pq_params_default(), codebook_for_build).unwrap();
                for entry in snapshot {
                    if !entry.tombstoned {
                        new_idx.insert(entry.memory_id, &entry.vector).unwrap();
                    }
                }
                Ok(new_idx)
            })
            .unwrap();
        assert_eq!(report.entries_flushed, 2);
        assert_eq!(report.main_len_after, 2);
        assert_eq!(reader.pending_len(), 0);
        assert!(reader.contains(mid(1)));
        assert!(reader.contains(mid(2)));
    }

    #[test]
    fn merge_dedupe_prefers_pending_on_collision() {
        let main = vec![(mid(1), 0.5), (mid(2), 0.7)];
        let pending = vec![(mid(1), 0.95), (mid(3), 0.6)];
        let merged = merge_dedupe_descending(main, pending, 5);

        // mid(1) appears once with pending's 0.95, not main's 0.5.
        let m1 = merged.iter().find(|(id, _)| *id == mid(1)).unwrap();
        assert!((m1.1 - 0.95).abs() < 1e-6);
        // Sorted descending.
        for w in merged.windows(2) {
            assert!(w[0].1 >= w[1].1);
        }
    }
}
