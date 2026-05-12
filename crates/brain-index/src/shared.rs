//! Concurrent wrapper around `HnswIndex`.
//!
//! See `spec/06_ann_index/08_concurrency.md` and SD-4.8-1 in
//! `docs/spec-deviations.md`.
//!
//! ## Reader / writer split
//!
//! - [`SharedHnsw<D>`] is the **reader handle**: `Clone`, all methods
//!   take `&self`. Multiple clones can search the same index from
//!   different threads concurrently.
//! - [`Writer<D>`] is the **writer handle**: not `Clone`, mutation
//!   methods take `&mut self`. Constructed exactly once alongside
//!   the reader via [`SharedHnsw::new`] — the type system enforces
//!   single-writer-per-shard (spec §06/08 §1) at compile time.
//!
//! ## RwLock not ArcSwap (SD-4.8-1)
//!
//! Spec §06/08 §3 mandates lock-free reads via `ArcSwap<HnswState>`
//! and a pending-insert buffer that periodically rebuilds and
//! publishes a new state. That model requires cloning the HNSW
//! graph cheaply; `hnsw_rs::Hnsw` doesn't implement `Clone`, and at
//! 1M nodes a deep clone would cost ~150 MB and seconds — way past
//! the spec's 100 ms flush cadence.
//!
//! v1 ships with `Arc<parking_lot::RwLock<HnswIndex<D>>>` instead:
//! concurrent readers, exclusive writes. Not lock-free, but readers
//! aren't serialised against each other. Write latency dips reader
//! latency briefly during inserts (~1-3 ms at 1M per spec §06/03 §4),
//! which we accept for v1. Reconciliation: future Phase 11+ work
//! either patches hnsw_rs to expose a clone-aware mutation model or
//! replaces it with a custom HNSW.

use std::path::Path;
use std::sync::Arc;

use brain_core::MemoryId;
use parking_lot::RwLock;

use crate::hnsw::{HnswError, HnswIndex};
use crate::params::IndexParams;
use crate::rebuild::RebuildReport;

/// Cloneable reader handle. All methods are concurrent (RwLock read).
#[derive(Clone)]
pub struct SharedHnsw<const D: usize> {
    inner: Arc<RwLock<HnswIndex<D>>>,
}

/// Single-writer handle. Not `Clone`; mutation methods take `&mut
/// self`. Only obtained via [`SharedHnsw::new`] or
/// [`SharedHnsw::load_snapshot`].
pub struct Writer<const D: usize> {
    inner: Arc<RwLock<HnswIndex<D>>>,
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
        let inner = Arc::new(RwLock::new(idx));
        let reader = Self {
            inner: inner.clone(),
        };
        let writer = Writer { inner };
        (reader, writer)
    }

    /// Rebuild a shared index from an iterator (spec §06/06 §2).
    /// Convenience around [`HnswIndex::rebuild`] + `from_index`.
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

    // ----- Reader-only methods ----------------------------------------

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
        let guard = self.inner.read();
        guard.search(query, k, ef, filter)
    }

    #[must_use]
    pub fn search_active(
        &self,
        query: &[f32; D],
        k: usize,
        ef: Option<usize>,
    ) -> Vec<(MemoryId, f32)> {
        let guard = self.inner.read();
        guard.search_active(query, k, ef)
    }

    #[must_use]
    pub fn contains(&self, memory_id: MemoryId) -> bool {
        self.inner.read().contains(memory_id)
    }

    #[must_use]
    pub fn is_tombstoned(&self, memory_id: MemoryId) -> bool {
        self.inner.read().is_tombstoned(memory_id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    #[must_use]
    pub fn tombstone_count(&self) -> usize {
        self.inner.read().tombstone_count()
    }

    #[must_use]
    pub fn params(&self) -> IndexParams {
        self.inner.read().params()
    }

    /// Save a snapshot. Acquires the read lock for the duration of
    /// the write (writes block, readers don't).
    pub fn save_snapshot(
        &self,
        dir: &Path,
        basename: &str,
        taken_at_lsn: u64,
        shard_uuid: [u8; 16],
    ) -> Result<(), HnswError> {
        let guard = self.inner.read();
        guard.save_snapshot(dir, basename, taken_at_lsn, shard_uuid)
    }

    /// Atomically replace the inner index with `new`. Used by the
    /// HNSW maintenance worker (sub-task 8.5) when a full rebuild
    /// completes; spec §11/04 §5 describes the operation as an
    /// "ArcSwap" — our `Arc<RwLock<HnswIndex>>` realises the same
    /// semantics: readers complete on the old index, the brief
    /// write-lock acquisition is microsecond-scale, new reads see
    /// the replacement.
    ///
    /// **Discipline**: only one task should call `swap` at a time.
    /// The scheduler's single-worker-per-name guarantee (sub-task
    /// 8.1) is what enforces this at runtime.
    pub fn swap(&self, new: HnswIndex<D>) {
        let mut guard = self.inner.write();
        *guard = new;
    }
}

impl<const D: usize> Writer<D> {
    /// Insert a vector. Takes `&mut self` — the type system rejects
    /// concurrent writes through the same `Writer`. The `Writer`
    /// itself isn't `Clone`, so only one exists per shard
    /// (spec §06/08 §1's single-writer-per-shard discipline).
    ///
    /// Acquires the RwLock's write lock briefly (~1-3 ms at 1M per
    /// spec §06/03 §4); concurrent readers wait this out.
    pub fn insert(&mut self, memory_id: MemoryId, vector: &[f32; D]) -> Result<(), HnswError> {
        let mut guard = self.inner.write();
        guard.insert(memory_id, vector)
    }

    /// Mark a memory tombstoned. Same locking discipline as `insert`.
    pub fn mark_tombstoned(&mut self, memory_id: MemoryId) -> Result<(), HnswError> {
        let mut guard = self.inner.write();
        guard.mark_tombstoned(memory_id)
    }
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
        // Both reader clones see the post-insert state.
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
        // RwLock writes commit before unlock — read-after-write is
        // immediate, no flush hint needed.
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
    fn concurrent_readers_during_writer_no_panic() {
        // The big one: 8 reader threads + 1 writer in std::thread::scope
        // (spec §06/08 §1's lock-free-reads is the goal; RwLock gives us
        // concurrent reads but not lock-free). The test asserts:
        // - no panic
        // - no data race (RwLock prevents)
        // - final len() reflects all writes
        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();

        const N_INSERTS: u64 = 100;
        const N_READERS: usize = 8;
        const READS_PER_THREAD: usize = 500;

        // Pre-seed one vector so readers have something to query.
        writer.insert(mid(0), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();

        thread::scope(|s| {
            // Spawn N readers.
            let mut reader_handles = Vec::new();
            for tid in 0..N_READERS {
                let r = reader.clone();
                let h = s.spawn(move || {
                    let q = vec4(1.0, 0.0, 0.0, 0.0);
                    for i in 0..READS_PER_THREAD {
                        let results = r.search_active(&q, 5, None);
                        // Sanity: len > 0 always (we pre-seeded).
                        assert!(!results.is_empty(), "thread {tid} iter {i}: empty results");
                    }
                });
                reader_handles.push(h);
            }

            // Spawn the writer.
            s.spawn(|| {
                for i in 1..=N_INSERTS {
                    writer
                        .insert(mid(i), &vec4(i as f32, 0.5, 0.0, 0.0))
                        .expect("insert");
                }
            });

            // Join readers (writer is joined by scope drop).
            for h in reader_handles {
                h.join().unwrap();
            }
        });

        // After the scope, all threads have completed.
        assert_eq!(reader.len(), (N_INSERTS as usize) + 1);
    }

    #[test]
    fn shared_save_snapshot_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let shard_uuid = [0xAB; 16];

        let (reader, mut writer) = SharedHnsw::<4>::new(IndexParams::default_v1()).unwrap();
        writer.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        writer.insert(mid(2), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
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
        // Writer still works on the rebuilt index.
        writer.insert(mid(3), &vec4(0.0, 0.0, 1.0, 0.0)).unwrap();
        assert_eq!(reader.len(), 3);
    }
}
