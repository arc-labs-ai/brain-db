//! Two-tier lock-free HNSW wrapper.
//!
//! Readers never block on writers. The wrapper splits live state into
//! two tiers:
//!
//! - **Main**: an immutable [`MainEpoch`] holding the HNSW for the
//!   current epoch. Replaced atomically by the maintenance worker via
//!   [`SharedHnsw::flush_with_rebuild`]. Searches against main are
//!   lock-free.
//! - **Pending**: a [`PendingBuffer`] of recent inserts plus a
//!   tombstone overlay. Brute-force scanned on every read under a
//!   shared lock — multiple readers run concurrently; only writers
//!   take the exclusive side.
//!
//! Read-after-write is immediate: every read sees pending ∪ main.
//! Writes go into pending; the HNSW maintenance worker periodically
//! folds pending into a freshly-built main and publishes the new
//! epoch.
//!
//! ## Reader / writer split
//!
//! - [`SharedHnsw<D>`] is the **reader handle**: `Clone`, all methods
//!   take `&self`. Multiple clones can search the same index from
//!   different threads concurrently.
//! - [`Writer<D>`] is the **writer handle**: not `Clone`, mutation
//!   methods take `&mut self`. Constructed exactly once alongside the
//!   reader via [`SharedHnsw::new`] — the type system enforces
//!   single-writer-per-shard at compile time.
//!
//! ## Sizing of pending
//!
//! Under typical load pending stays well below 1000 entries
//! (~100 µs brute-force cost at 1000 × 384 dims). The maintenance
//! worker triggers a flush when the buffer crosses a threshold, when
//! a wall-clock interval elapses, or on demand from the snapshot
//! worker.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use brain_core::MemoryId;
use parking_lot::RwLock;

use crate::hnsw::{HnswError, HnswIndex};
use crate::params::IndexParams;
use crate::rebuild::RebuildReport;

/// An immutable HNSW snapshot for a single published epoch.
///
/// Built by the maintenance worker, swapped in atomically via
/// [`SharedHnsw::flush_with_rebuild`], and never mutated in place.
/// Each new epoch carries a monotonically increasing `epoch_id` for
/// metrics and debugging.
struct MainEpoch<const D: usize> {
    index: HnswIndex<D>,
    epoch_id: u64,
}

/// Recent inserts and tombstones that have not yet been folded into
/// the main HNSW.
///
/// Read paths brute-force scan `entries` and consult `tombstoned` as
/// an overlay on top of the main HNSW's own tombstone bitmap. The
/// `tombstoned` set wins — a memory tombstoned in pending must read
/// as tombstoned regardless of main's state.
struct PendingBuffer<const D: usize> {
    entries: Vec<PendingEntry<D>>,
    tombstoned: HashSet<MemoryId>,
}

impl<const D: usize> PendingBuffer<D> {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            tombstoned: HashSet::new(),
        }
    }
}

/// A vector waiting to be folded into the main HNSW.
///
/// Visible to the maintenance worker via
/// [`SharedHnsw::pending_snapshot`] and the closure passed to
/// [`SharedHnsw::flush_with_rebuild`]. The `tombstoned` flag is set
/// when a memory is inserted into pending and then forgotten before
/// the next flush — the entry stays in the buffer (so the writer
/// doesn't have to scan to remove it) but is filtered out of reads
/// and rebuilds.
#[derive(Clone, Debug)]
pub struct PendingEntry<const D: usize> {
    pub memory_id: MemoryId,
    pub vector: [f32; D],
    pub tombstoned: bool,
}

/// Report returned by [`SharedHnsw::flush_with_rebuild`].
#[derive(Debug, Clone)]
pub struct FlushReport {
    /// Count of entries snapshotted from pending and folded into the
    /// new main. Equals the size of the snapshot passed to the
    /// builder; inserts that arrived during the build are *not*
    /// counted here and survive into the next flush.
    pub entries_flushed: usize,
    /// The newly-published epoch id (was `prev_epoch + 1`).
    pub new_epoch: u64,
    /// `HnswIndex::len()` of the freshly-published main.
    pub main_len_after: usize,
}

/// Cloneable reader handle.
///
/// Reads against the published main HNSW are lock-free (an `ArcSwap`
/// load). Reads against pending take a shared lock for a brief
/// window; multiple reader threads scan pending concurrently.
#[derive(Clone)]
pub struct SharedHnsw<const D: usize> {
    main: Arc<ArcSwap<MainEpoch<D>>>,
    pending: Arc<RwLock<PendingBuffer<D>>>,
}

/// Single-writer handle.
///
/// Writes land in the pending buffer; the main HNSW is never mutated
/// through the `Writer`. The maintenance worker rebuilds main from
/// arena + pending via [`SharedHnsw::flush_with_rebuild`].
///
/// Not `Clone` — the type system enforces single-writer-per-shard.
pub struct Writer<const D: usize> {
    /// Held so the published main outlives the writer regardless of
    /// reader-handle cloning patterns; not read from directly because
    /// writes only touch pending.
    _main: Arc<ArcSwap<MainEpoch<D>>>,
    pending: Arc<RwLock<PendingBuffer<D>>>,
}

impl<const D: usize> SharedHnsw<D> {
    /// Create a fresh shared index and its single writer. Returns the
    /// reader handle (cloneable) and the writer handle (one-shot).
    pub fn new(params: IndexParams) -> Result<(Self, Writer<D>), HnswError> {
        let idx = HnswIndex::<D>::new(params)?;
        Ok(Self::from_index(idx))
    }

    /// Wrap an existing `HnswIndex`, returning the reader/writer pair.
    #[must_use]
    pub fn from_index(idx: HnswIndex<D>) -> (Self, Writer<D>) {
        let epoch = Arc::new(MainEpoch {
            index: idx,
            epoch_id: 0,
        });
        let main = Arc::new(ArcSwap::new(epoch));
        let pending = Arc::new(RwLock::new(PendingBuffer::new()));
        let reader = Self {
            main: main.clone(),
            pending: pending.clone(),
        };
        let writer = Writer {
            _main: main,
            pending,
        };
        (reader, writer)
    }

    /// Rebuild a shared index from an iterator. Convenience around
    /// [`HnswIndex::rebuild`] + `from_index`.
    pub fn rebuild<I>(
        params: IndexParams,
        source: I,
    ) -> Result<(Self, Writer<D>, RebuildReport), HnswError>
    where
        I: IntoIterator<Item = (MemoryId, [f32; D])>,
    {
        let (idx, report) = HnswIndex::<D>::rebuild(params, source)?;
        let (reader, writer) = Self::from_index(idx);
        Ok((reader, writer, report))
    }

    /// Load a shared index from a snapshot. Wraps
    /// [`HnswIndex::load_snapshot`]. Returns the reader/writer pair
    /// plus the `taken_at_lsn` recorded in the snapshot header.
    pub fn load_snapshot(
        dir: &Path,
        basename: &str,
        expected_shard_uuid: [u8; 16],
    ) -> Result<(Self, Writer<D>, u64), HnswError> {
        let (idx, lsn) = HnswIndex::<D>::load_snapshot(dir, basename, expected_shard_uuid)?;
        let (reader, writer) = Self::from_index(idx);
        Ok((reader, writer, lsn))
    }

    // ----- Reader methods --------------------------------------------------

    /// Top-`k` nearest neighbours of `query` with an extra caller
    /// filter (in addition to the always-on tombstone filter).
    ///
    /// Reads pending and main; merges + dedupes; returns at most `k`
    /// results sorted descending by similarity (best match first).
    #[must_use]
    pub fn search<F>(
        &self,
        query: &[f32; D],
        k: usize,
        ef: Option<usize>,
        filter: F,
    ) -> Vec<(MemoryId, f32)>
    where
        F: Fn(MemoryId) -> bool,
    {
        if k == 0 {
            return Vec::new();
        }

        let epoch = self.main.load();
        let (pending_hits, pending_tombstones) = {
            let g = self.pending.read();
            let hits = brute_force_search(&g.entries, query, k * 2, &filter);
            (hits, g.tombstoned.clone())
        };

        let main_hits: Vec<(MemoryId, f32)> = epoch.index.search(query, k * 2, ef, |id| {
            !pending_tombstones.contains(&id) && filter(id)
        });

        merge_dedupe(main_hits, pending_hits, k)
    }

    /// Top-`k` nearest neighbours of `query`, excluding tombstoned
    /// memories. Convenience for the common case.
    #[must_use]
    pub fn search_active(
        &self,
        query: &[f32; D],
        k: usize,
        ef: Option<usize>,
    ) -> Vec<(MemoryId, f32)> {
        self.search(query, k, ef, |_| true)
    }

    /// Is `memory_id` present (and not tombstoned) anywhere in the
    /// two-tier index?
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

    /// Is `memory_id` tombstoned in either tier? The pending tombstone
    /// overlay wins — a just-tombstoned memory reads as tombstoned
    /// before the next flush.
    #[must_use]
    pub fn is_tombstoned(&self, memory_id: MemoryId) -> bool {
        if self.pending.read().tombstoned.contains(&memory_id) {
            return true;
        }
        self.main.load().index.is_tombstoned(memory_id)
    }

    /// Approximate combined size: published main plus pending entries
    /// not already in main.
    ///
    /// Exact dedupe across the two tiers would require scanning both;
    /// the approximation is consistent with how the underlying HNSW
    /// library counts (tombstones still occupy a node).
    #[must_use]
    pub fn len(&self) -> usize {
        let pending = self.pending.read();
        let pending_extra = pending.entries.iter().filter(|e| !e.tombstoned).count();
        self.main.load().index.len() + pending_extra
    }

    /// True iff both tiers are empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        let pending = self.pending.read();
        if pending.entries.iter().any(|e| !e.tombstoned) {
            return false;
        }
        drop(pending);
        self.main.load().index.is_empty()
    }

    /// Combined tombstone count: main's bitmap plus the pending
    /// overlay entries not already counted in main.
    #[must_use]
    pub fn tombstone_count(&self) -> usize {
        let epoch = self.main.load();
        let pending = self.pending.read();
        let mut count = epoch.index.tombstone_count();
        for id in &pending.tombstoned {
            if !epoch.index.is_tombstoned(*id) {
                count += 1;
            }
        }
        count
    }

    /// HNSW parameters of the published main. Pending entries inherit
    /// the same params (they'll be folded into a rebuild using these).
    #[must_use]
    pub fn params(&self) -> IndexParams {
        self.main.load().index.params()
    }

    /// Current epoch id. Increments by 1 on every successful
    /// [`SharedHnsw::flush_with_rebuild`] or [`SharedHnsw::swap`].
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.main.load().epoch_id
    }

    /// Save a snapshot of the **published main** to disk.
    ///
    /// Returns [`HnswError::PendingNotEmpty`] if pending has any
    /// non-tombstoned entries or active tombstone overlays — callers
    /// must flush first so the snapshot reflects the full live state.
    /// The maintenance worker honours this contract by calling
    /// [`SharedHnsw::flush_with_rebuild`] before snapshot prep.
    pub fn save_snapshot(
        &self,
        dir: &Path,
        basename: &str,
        taken_at_lsn: u64,
        shard_uuid: [u8; 16],
    ) -> Result<(), HnswError> {
        {
            let pending = self.pending.read();
            let live_entries = pending.entries.iter().filter(|e| !e.tombstoned).count();
            if live_entries > 0 || !pending.tombstoned.is_empty() {
                return Err(HnswError::PendingNotEmpty {
                    entries: live_entries,
                    tombstones: pending.tombstoned.len(),
                });
            }
        }
        let epoch = self.main.load();
        epoch.index.save_snapshot(dir, basename, taken_at_lsn, shard_uuid)
    }

    /// Atomically replace the published main with `new_index` and
    /// clear pending. Used for bootstrap and snapshot-load paths
    /// where main was rebuilt from a source of truth that already
    /// reflects all writes (so pending is redundant).
    ///
    /// For the regular maintenance flow that folds pending into the
    /// rebuild, use [`SharedHnsw::flush_with_rebuild`] instead.
    pub fn swap(&self, new_index: HnswIndex<D>) {
        let prev = self.main.load();
        let next = Arc::new(MainEpoch {
            index: new_index,
            epoch_id: prev.epoch_id.wrapping_add(1),
        });
        // Order: store the new epoch first, then clear pending. A
        // reader that observes the new epoch but still reads the old
        // pending will see a superset of the truth (some entries
        // appear in both main and pending); `merge_dedupe` handles
        // that case correctly.
        self.main.store(next);
        let mut pending = self.pending.write();
        pending.entries.clear();
        pending.tombstoned.clear();
    }

    /// Snapshot pending entries, hand them to `build` to produce a
    /// new main HNSW, then atomically publish the result and drain
    /// the flushed ids from pending.
    ///
    /// Called by the HNSW maintenance worker. The builder owns the
    /// arena read path (this crate can't reach the arena); it
    /// receives the pending snapshot so the rebuilt main reflects
    /// every write the system has acknowledged.
    ///
    /// Inserts that arrive *during* `build` land in pending alongside
    /// the snapshot. The drain step removes only the ids the builder
    /// folded in; any newer inserts survive into the next flush.
    pub fn flush_with_rebuild<F>(&self, build: F) -> Result<FlushReport, HnswError>
    where
        F: FnOnce(&[PendingEntry<D>]) -> Result<HnswIndex<D>, HnswError>,
    {
        // 1. Snapshot pending under shared lock; drop the lock before
        //    building so concurrent readers (and the writer) keep
        //    making progress.
        let snapshot: Vec<PendingEntry<D>> = self.pending.read().entries.clone();
        let snapshot_count = snapshot.len();

        // 2. Build the new main outside any lock.
        let new_index = build(&snapshot)?;

        // 3. Publish the new epoch and drain the flushed ids. The
        //    exclusive pending lock is held only across the drain;
        //    the build itself ran unlocked.
        let mut pending = self.pending.write();
        let prev_epoch = self.main.load();
        let new_epoch_id = prev_epoch.epoch_id.wrapping_add(1);
        let main_len_after = new_index.len();
        let new_epoch = Arc::new(MainEpoch {
            index: new_index,
            epoch_id: new_epoch_id,
        });
        // Store before drain so any reader seeing the new epoch and
        // the old pending observes a superset; merge_dedupe handles it.
        self.main.store(new_epoch);

        let flushed: HashSet<MemoryId> = snapshot.iter().map(|e| e.memory_id).collect();
        pending.entries.retain(|e| !flushed.contains(&e.memory_id));
        pending.tombstoned.retain(|id| !flushed.contains(id));

        Ok(FlushReport {
            entries_flushed: snapshot_count,
            new_epoch: new_epoch_id,
            main_len_after,
        })
    }

    /// Clone the current pending entries — useful for the maintenance
    /// worker's pre-flush bookkeeping and the bench harness.
    #[must_use]
    pub fn pending_snapshot(&self) -> Vec<PendingEntry<D>> {
        self.pending.read().entries.clone()
    }

    /// Count of live (non-tombstoned) pending entries. Cheap.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.read().entries.iter().filter(|e| !e.tombstoned).count()
    }

}

impl<const D: usize> Writer<D> {
    /// Insert a vector. Lands in pending; visible to readers
    /// immediately. Folded into main on the next flush.
    ///
    /// Re-inserting an existing pending id replaces the prior entry
    /// (and clears any pending tombstone for that id). Re-inserting
    /// an id that lives in main is rejected by the rebuild step, not
    /// here — pending allows the duplicate to sit until flush, when
    /// the builder is responsible for resolving the collision.
    pub fn insert(&mut self, memory_id: MemoryId, vector: &[f32; D]) -> Result<(), HnswError> {
        let mut pending = self.pending.write();
        // A re-insert after tombstone resurrects the entry.
        pending.tombstoned.remove(&memory_id);
        if let Some(slot) = pending.entries.iter_mut().find(|e| e.memory_id == memory_id) {
            slot.vector = *vector;
            slot.tombstoned = false;
        } else {
            pending.entries.push(PendingEntry {
                memory_id,
                vector: *vector,
                tombstoned: false,
            });
        }
        Ok(())
    }

    /// Mark a memory tombstoned. Visible immediately via
    /// [`SharedHnsw::is_tombstoned`] regardless of which tier holds
    /// the underlying vector.
    pub fn mark_tombstoned(&mut self, memory_id: MemoryId) -> Result<(), HnswError> {
        let mut pending = self.pending.write();
        pending.tombstoned.insert(memory_id);
        if let Some(slot) = pending.entries.iter_mut().find(|e| e.memory_id == memory_id) {
            slot.tombstoned = true;
        }
        Ok(())
    }
}

// ===== Search helpers ======================================================

/// Brute-force rank pending entries by dot product against `query`.
/// Returns at most `k` non-tombstoned entries that pass `filter`,
/// sorted descending by similarity.
fn brute_force_search<const D: usize, F>(
    entries: &[PendingEntry<D>],
    query: &[f32; D],
    k: usize,
    filter: &F,
) -> Vec<(MemoryId, f32)>
where
    F: Fn(MemoryId) -> bool,
{
    if entries.is_empty() || k == 0 {
        return Vec::new();
    }
    let mut scored: Vec<(MemoryId, f32)> = entries
        .iter()
        .filter(|e| !e.tombstoned && filter(e.memory_id))
        .map(|e| (e.memory_id, dot(query, &e.vector)))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    scored
}

/// Dot product of two equal-length f32 vectors. With L2-normalised
/// inputs this equals cosine similarity, matching the metric the
/// main HNSW uses.
fn dot<const D: usize>(a: &[f32; D], b: &[f32; D]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Merge main and pending hit lists, dedupe by `MemoryId` (pending
/// score wins on collision because it reflects the latest vector for
/// that id), sort descending by similarity, truncate to `k`.
fn merge_dedupe(
    main_hits: Vec<(MemoryId, f32)>,
    pending_hits: Vec<(MemoryId, f32)>,
    k: usize,
) -> Vec<(MemoryId, f32)> {
    let mut out: Vec<(MemoryId, f32)> = Vec::with_capacity(main_hits.len() + pending_hits.len());
    let mut seen: HashSet<MemoryId> = HashSet::with_capacity(pending_hits.len());
    for (id, sim) in pending_hits {
        seen.insert(id);
        out.push((id, sim));
    }
    for (id, sim) in main_hits {
        if !seen.contains(&id) {
            out.push((id, sim));
        }
    }
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(k);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn vec4(a: f32, b: f32, c: f32, d: f32) -> [f32; 4] {
        let n = (a * a + b * b + c * c + d * d).sqrt();
        [a / n, b / n, c / n, d / n]
    }

    fn mid(slot: u64) -> MemoryId {
        MemoryId::pack(1, slot, 1)
    }

    #[test]
    fn single_threaded_insert_and_search() {
        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();
        writer.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        let results = reader.search_active(&vec4(1.0, 0.0, 0.0, 0.0), 1, None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, mid(1));
    }

    #[test]
    fn reader_clones_share_state() {
        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();
        let r1 = reader.clone();
        let r2 = reader.clone();
        writer.insert(mid(7), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        assert!(r1.contains(mid(7)));
        assert!(r2.contains(mid(7)));
        assert_eq!(r1.len(), 1);
        assert_eq!(r2.len(), 1);
    }

    #[test]
    fn tombstone_visible_after_write() {
        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();
        writer.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        writer.mark_tombstoned(mid(1)).unwrap();
        assert!(reader.is_tombstoned(mid(1)));
        assert_eq!(reader.tombstone_count(), 1);
    }

    #[test]
    fn writer_serialises_sequential_calls() {
        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();
        writer.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        writer.insert(mid(2), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        writer.insert(mid(3), &vec4(0.0, 0.0, 1.0, 0.0)).unwrap();
        assert_eq!(reader.len(), 3);
    }

    #[test]
    fn pending_is_searched_on_every_read() {
        // Insert via writer; read sees the result without any explicit
        // flush. The two-tier model's read-after-write contract.
        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();
        writer.insert(mid(5), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        let results = reader.search_active(&vec4(1.0, 0.0, 0.0, 0.0), 1, None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, mid(5));
        // Pending still holds the entry — no flush has happened.
        assert_eq!(reader.pending_len(), 1);
        assert_eq!(reader.epoch(), 0);
    }

    #[test]
    fn concurrent_readers_during_writer_no_panic() {
        // 8 reader threads + 1 writer in std::thread::scope. With the
        // two-tier model, main reads are lock-free; pending reads use
        // a shared lock that the writer briefly contends.
        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();

        const N_INSERTS: u64 = 100;
        const N_READERS: usize = 8;
        const READS_PER_THREAD: usize = 500;

        writer.insert(mid(0), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();

        thread::scope(|s| {
            let mut reader_handles = Vec::new();
            for tid in 0..N_READERS {
                let r = reader.clone();
                let h = s.spawn(move || {
                    let q = vec4(1.0, 0.0, 0.0, 0.0);
                    for i in 0..READS_PER_THREAD {
                        let results = r.search_active(&q, 5, None);
                        assert!(!results.is_empty(), "thread {tid} iter {i}: empty results");
                    }
                });
                reader_handles.push(h);
            }

            s.spawn(|| {
                for i in 1..=N_INSERTS {
                    writer
                        .insert(mid(i), &vec4(i as f32, 0.5, 0.0, 0.0))
                        .expect("insert");
                }
            });

            for h in reader_handles {
                h.join().unwrap();
            }
        });

        assert_eq!(reader.len(), (N_INSERTS as usize) + 1);
    }

    #[test]
    fn flush_drains_only_snapshotted_entries() {
        // Insert A, B, C; flush with a builder that captures the
        // snapshot. Inside the builder, simulate a mid-flush insert
        // of D by writing to pending directly. After the flush:
        // main holds [A, B, C]; pending holds [D].
        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();
        writer.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        writer.insert(mid(2), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        writer.insert(mid(3), &vec4(0.0, 0.0, 1.0, 0.0)).unwrap();
        assert_eq!(reader.pending_len(), 3);

        let pending_handle = reader.pending.clone();
        let report = reader
            .flush_with_rebuild(|snapshot| {
                assert_eq!(snapshot.len(), 3);
                // Simulate a concurrent insert that lands during the
                // rebuild.
                pending_handle.write().entries.push(PendingEntry {
                    memory_id: mid(4),
                    vector: vec4(0.5, 0.5, 0.0, 0.0),
                    tombstoned: false,
                });
                // Build a fresh HNSW from the snapshot.
                let source: Vec<_> = snapshot
                    .iter()
                    .filter(|e| !e.tombstoned)
                    .map(|e| (e.memory_id, e.vector))
                    .collect();
                let (idx, _) = HnswIndex::<4>::rebuild(IndexParams::default_v1(), source)?;
                Ok(idx)
            })
            .unwrap();

        assert_eq!(report.entries_flushed, 3);
        assert_eq!(report.main_len_after, 3);
        assert_eq!(report.new_epoch, 1);

        // Pending now holds only the mid(4) that arrived during the
        // build.
        let leftover = reader.pending_snapshot();
        assert_eq!(leftover.len(), 1);
        assert_eq!(leftover[0].memory_id, mid(4));
    }

    #[test]
    fn epoch_id_monotonically_increases() {
        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();
        assert_eq!(reader.epoch(), 0);

        writer.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        reader
            .flush_with_rebuild(|snap| {
                let source: Vec<_> = snap.iter().map(|e| (e.memory_id, e.vector)).collect();
                let (idx, _) = HnswIndex::<4>::rebuild(IndexParams::default_v1(), source)?;
                Ok(idx)
            })
            .unwrap();
        assert_eq!(reader.epoch(), 1);

        writer.insert(mid(2), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        reader
            .flush_with_rebuild(|snap| {
                let source: Vec<_> = snap.iter().map(|e| (e.memory_id, e.vector)).collect();
                let (idx, _) = HnswIndex::<4>::rebuild(IndexParams::default_v1(), source)?;
                Ok(idx)
            })
            .unwrap();
        assert_eq!(reader.epoch(), 2);
    }

    #[test]
    fn pending_tombstone_overrides_main() {
        // Flush an insert into main; tombstone via pending; verify
        // is_tombstoned() returns true even though main says false.
        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();
        writer.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        reader
            .flush_with_rebuild(|snap| {
                let source: Vec<_> = snap.iter().map(|e| (e.memory_id, e.vector)).collect();
                let (idx, _) = HnswIndex::<4>::rebuild(IndexParams::default_v1(), source)?;
                Ok(idx)
            })
            .unwrap();
        // mid(1) lives in main, not pending.
        assert_eq!(reader.pending_len(), 0);
        assert!(!reader.is_tombstoned(mid(1)));

        // Tombstone via pending overlay.
        writer.mark_tombstoned(mid(1)).unwrap();
        assert!(reader.is_tombstoned(mid(1)));
        // contains() also reflects the overlay.
        assert!(!reader.contains(mid(1)));
        // tombstone_count picks it up.
        assert_eq!(reader.tombstone_count(), 1);
    }

    #[test]
    fn pending_dedup_within_buffer() {
        // Insert id 1 twice; pending should only have one entry; the
        // second insert replaces the first vector.
        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();
        writer.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        writer.insert(mid(1), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        let snapshot = reader.pending_snapshot();
        assert_eq!(snapshot.len(), 1);
        // The second vector wins.
        assert!((snapshot[0].vector[1] - 1.0).abs() < 1e-6);
        assert!(snapshot[0].vector[0].abs() < 1e-6);
    }

    #[test]
    fn pending_reinsert_after_tombstone_resurrects() {
        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();
        writer.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        writer.mark_tombstoned(mid(1)).unwrap();
        assert!(reader.is_tombstoned(mid(1)));
        writer.insert(mid(1), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        // Re-insert clears the tombstone overlay.
        assert!(!reader.is_tombstoned(mid(1)));
        assert!(reader.contains(mid(1)));
    }

    #[test]
    fn save_snapshot_errors_when_pending_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let shard_uuid = [0xAB; 16];
        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();
        writer.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        match reader.save_snapshot(dir.path(), "snap", 0, shard_uuid) {
            Err(HnswError::PendingNotEmpty {
                entries: 1,
                tombstones: 0,
            }) => {}
            Err(e) => panic!("wrong error: {e}"),
            Ok(()) => panic!("expected PendingNotEmpty"),
        }
        // Flush, then save should succeed.
        reader
            .flush_with_rebuild(|snap| {
                let source: Vec<_> = snap.iter().map(|e| (e.memory_id, e.vector)).collect();
                let (idx, _) = HnswIndex::<4>::rebuild(IndexParams::default_v1(), source)?;
                Ok(idx)
            })
            .unwrap();
        reader
            .save_snapshot(dir.path(), "snap", 7, shard_uuid)
            .expect("save after flush");
    }

    #[test]
    fn shared_save_snapshot_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let shard_uuid = [0xAB; 16];

        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();
        writer.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        writer.insert(mid(2), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        // Flush so pending is empty for save_snapshot.
        reader
            .flush_with_rebuild(|snap| {
                let source: Vec<_> = snap.iter().map(|e| (e.memory_id, e.vector)).collect();
                let (idx, _) = HnswIndex::<4>::rebuild(IndexParams::default_v1(), source)?;
                Ok(idx)
            })
            .unwrap();
        let pre = reader.search_active(&vec4(1.0, 0.0, 0.0, 0.0), 2, None);

        reader
            .save_snapshot(dir.path(), "shr", 42, shard_uuid)
            .unwrap();

        let (loaded_reader, _loaded_writer, lsn) =
            SharedHnsw::<4>::load_snapshot(dir.path(), "shr", shard_uuid).unwrap();
        assert_eq!(lsn, 42);
        let post = loaded_reader.search_active(&vec4(1.0, 0.0, 0.0, 0.0), 2, None);
        assert_eq!(pre.len(), post.len());
        for (a, b) in pre.iter().zip(post.iter()) {
            assert_eq!(a.0, b.0);
        }
    }

    #[test]
    fn rebuild_returns_shared_pair() {
        let source = vec![
            (mid(1), vec4(1.0, 0.0, 0.0, 0.0)),
            (mid(2), vec4(0.0, 1.0, 0.0, 0.0)),
        ];
        let (reader, mut writer, report) =
            SharedHnsw::<4>::rebuild(IndexParams::default_v1(), source).unwrap();
        assert_eq!(report.memories_inserted, 2);
        assert_eq!(reader.len(), 2);
        writer.insert(mid(3), &vec4(0.0, 0.0, 1.0, 0.0)).unwrap();
        assert_eq!(reader.len(), 3);
    }

    #[test]
    fn swap_clears_pending() {
        // swap is for bootstrap / snapshot-load: replaces main and
        // drops pending wholesale. The maintenance worker uses
        // flush_with_rebuild instead.
        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();
        writer.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        writer.mark_tombstoned(mid(99)).unwrap();
        assert_eq!(reader.pending_len(), 1);

        let (replacement, _w) = HnswIndex::<4>::rebuild(
            IndexParams::default_v1(),
            vec![(mid(10), vec4(1.0, 0.0, 0.0, 0.0))],
        )
        .unwrap();
        reader.swap(replacement);

        assert_eq!(reader.pending_len(), 0);
        assert!(!reader.is_tombstoned(mid(99)));
        assert!(reader.contains(mid(10)));
        assert!(!reader.contains(mid(1)));
        assert_eq!(reader.epoch(), 1);
    }

    #[test]
    fn pending_hits_outrank_main_when_closer() {
        // Insert mid(1) at angle ~0° into main; pending holds mid(2)
        // at angle ~5° to the query. Query at 0°. mid(1) wins because
        // main returned an exact match.
        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();
        writer.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        reader
            .flush_with_rebuild(|snap| {
                let source: Vec<_> = snap.iter().map(|e| (e.memory_id, e.vector)).collect();
                let (idx, _) = HnswIndex::<4>::rebuild(IndexParams::default_v1(), source)?;
                Ok(idx)
            })
            .unwrap();
        // Pending: mid(2) close to the query.
        writer.insert(mid(2), &vec4(0.99, 0.1, 0.0, 0.0)).unwrap();
        let q = vec4(1.0, 0.0, 0.0, 0.0);
        let results = reader.search_active(&q, 2, None);
        assert_eq!(results.len(), 2);
        // mid(1) is an exact match → highest similarity → first.
        assert_eq!(results[0].0, mid(1));
        assert_eq!(results[1].0, mid(2));
    }
}
