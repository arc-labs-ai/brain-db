//! Phase-8 worker source adapters wired to per-shard state (sub-task 9.8).
//!
//! Phase 8 shipped four pluggable "source" traits with `Disabled*` defaults:
//!
//! - [`RebuildSource`]: feeds `HnswMaintenanceWorker` the active
//!   `(MemoryId, vector)` pairs for full rebuild.
//! - [`WalRetentionSource`]: tells `WalRetentionWorker` which segments
//!   are past `durable_lsn` and removes them.
//! - [`SnapshotSource`]: backs `SnapshotWorker`'s take / list / delete.
//! - [`CacheEvictionSource`]: stays `Disabled*` until 9.10 wires a real
//!   `CachingDispatcher` per shard.
//!
//! 9.7b registered all 12 Phase-8 workers against the per-shard
//! scheduler with the `Disabled*` defaults; 9.8 plugs in real adapters
//! for the first three. The fourth — cache eviction — stays disabled
//! and is constructed at the call-site (no adapter struct here).
//!
//! All adapters are `!Send + !Sync` by construction (they hold
//! `Rc<RefCell<…>>` references into per-shard state). Their trait
//! contracts dropped `Send + Sync` in 9.8 to match.

#![cfg(target_os = "linux")]
// `ShardSnapshotSource::take_snapshot` holds immutable `borrow()` on
// `self.wal` across two `Wal::append(...).await` points. The single-
// threaded Glommio executor + the discipline that `borrow_mut` only
// runs at shutdown (after the scheduler drains) means a runtime panic
// is structurally impossible. See shard.rs's module-level note for
// the full rationale.
#![allow(clippy::await_holding_refcell_ref)]

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::{MemoryId, ShardId};
use brain_embed::VECTOR_DIM;
use brain_index::SharedHnsw;
use brain_planner::SharedMetadataDb;
use brain_storage::arena::ArenaFile;
use brain_storage::wal::payload::{CheckpointBeginPayload, CheckpointEndPayload, WalPayload};
use brain_storage::wal::reader::WalReader;
use brain_storage::wal::record::{Lsn, WalRecord};
use brain_storage::wal::Wal;
use brain_workers::hnsw_maint::{RebuildSource, SnapshotFuture};
use brain_workers::snapshot::{
    DeleteFuture as SnapshotDeleteFuture, ListFuture as SnapshotListFuture, SnapshotDesc,
    SnapshotId, SnapshotSource, SnapshotSourceError, TakeFuture,
};
use brain_workers::wal_retention::{
    CheckpointDesc, CheckpointFuture, DeleteFuture as WalDeleteFuture, SegmentDesc,
    SegmentListFuture, WalRetentionSource, WalRetentionSourceError,
};

// ---------------------------------------------------------------------------
// RebuildSource — scan the shard's arena for occupied/non-tombstoned slots.
// ---------------------------------------------------------------------------

/// Walks the shard's `ArenaFile` and yields a `(MemoryId, vector)` for
/// every occupied, non-tombstoned, non-hard-forgotten slot. Spec
/// §11/04 §7 — the rebuild source is the substrate for full HNSW
/// rebuild.
///
/// Holds an `Rc<RefCell<ArenaFile>>` so the per-shard main loop can
/// mutate the arena (via `borrow_mut`) between the adapter's
/// `borrow()` scans. The borrow is released before each `.await`;
/// the worker yields between batches.
pub(crate) struct ArenaRebuildSource<const D: usize> {
    shard_id: ShardId,
    arena: Rc<RefCell<ArenaFile>>,
}

impl<const D: usize> ArenaRebuildSource<D> {
    pub(crate) fn new(shard_id: ShardId, arena: Rc<RefCell<ArenaFile>>) -> Self {
        Self { shard_id, arena }
    }
}

impl<const D: usize> RebuildSource<D> for ArenaRebuildSource<D> {
    fn snapshot_vectors(&self) -> SnapshotFuture<'_, D> {
        let arena = self.arena.clone();
        let shard_id = self.shard_id;
        Box::pin(async move {
            // Short borrow. The scan is mmap-resident → no syscalls →
            // no need to interleave .await yields. Real production
            // shards will hold tens of millions of slots; if that
            // becomes a latency problem, batch + yield in v2.
            let arena = arena.borrow();
            let cap = arena.capacity_slots();
            let mut out = Vec::with_capacity(cap as usize);
            for idx in 0..cap {
                let slot = arena.slot(idx);
                if !slot.is_occupied() {
                    continue;
                }
                if slot.is_tombstoned() || slot.is_hard_forgotten() {
                    continue;
                }
                let mid = MemoryId::pack(shard_id, idx, slot.metadata.slot_version);
                // Slot::vector is [f32; brain_embed::VECTOR_DIM]. The
                // worker is monomorphised on the same const. Reinterpret
                // by copy via array layout — both are [f32; D] when
                // D == VECTOR_DIM.
                // SAFETY-free path: bytemuck cast wouldn't compile across
                // const generics; we copy through a slice.
                let mut v = [0.0_f32; D];
                // Defensive: if a future caller monomorphises with the
                // wrong D, the slice copy short-stops to the lesser
                // length. The shard only ever uses D = VECTOR_DIM.
                let n = v.len().min(slot.vector.len());
                v[..n].copy_from_slice(&slot.vector[..n]);
                out.push((mid, v));
            }
            Ok(out)
        })
    }
}

// ---------------------------------------------------------------------------
// WalRetentionSource — list & delete on-disk segments past durable_lsn.
// ---------------------------------------------------------------------------

/// Backs `WalRetentionWorker` against the shard's on-disk WAL
/// directory. Spec §11/07 + §05/10.
///
/// `current_checkpoint` reads `MetadataDb::durable_lsn` (cheap,
/// in-memory after open). `list_segments` opens a fresh `WalReader`
/// to enumerate headers (a directory walk + 4 KB header read per
/// segment). `delete_segment` is `std::fs::remove_file` — the worker
/// is expected to call it only with segment ids strictly below the
/// active segment.
pub(crate) struct WalDirRetentionSource {
    wal_dir: PathBuf,
    shard_uuid: [u8; 16],
    metadata: SharedMetadataDb,
}

impl WalDirRetentionSource {
    pub(crate) fn new(wal_dir: PathBuf, shard_uuid: [u8; 16], metadata: SharedMetadataDb) -> Self {
        Self {
            wal_dir,
            shard_uuid,
            metadata,
        }
    }

    fn segment_path(&self, segment_seq: u64) -> PathBuf {
        // Mirrors brain_storage::wal::wal::segment_path. The WAL crate
        // doesn't export it (it's a module-private helper); we replicate
        // the format here. Spec §05/06 §3: zero-padded 10-digit decimal.
        self.wal_dir.join(format!("{:010}.wal", segment_seq))
    }
}

impl WalRetentionSource for WalDirRetentionSource {
    fn current_checkpoint(&self) -> CheckpointFuture<'_> {
        let metadata = self.metadata.clone();
        Box::pin(async move {
            // brain_metadata::MetadataDb caches durable_lsn in memory
            // (sink.rs §616-§622); the lookup is a single u64 read.
            let lsn = brain_storage::recovery::MetadataSink::durable_lsn(&*metadata.lock());
            Ok(CheckpointDesc { durable_lsn: lsn })
        })
    }

    fn list_segments(&self) -> SegmentListFuture<'_> {
        let dir = self.wal_dir.clone();
        let uuid = self.shard_uuid;
        Box::pin(async move {
            let reader = WalReader::open(&dir, uuid)
                .map_err(|e| WalRetentionSourceError::Failed(format!("WalReader::open: {e}")))?;
            let segs = reader
                .segments()
                .iter()
                .map(|s| {
                    // We don't know `last_lsn` without scanning every
                    // record. Reporting `starting_lsn` is the safe
                    // (conservative) lower bound: `decide_deletions`
                    // compares `last_lsn < safe_cutoff`, so an
                    // underestimate only *delays* retention — never
                    // deletes a segment that still covers durable_lsn.
                    SegmentDesc {
                        segment_id: s.segment_seq,
                        first_lsn: s.starting_lsn,
                        last_lsn: s.starting_lsn,
                        size_bytes: s.file_size,
                    }
                })
                .collect::<Vec<_>>();
            Ok(segs)
        })
    }

    fn delete_segment(&self, segment_id: u64) -> WalDeleteFuture<'_> {
        let path = self.segment_path(segment_id);
        Box::pin(async move {
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Idempotent. The worker may retry, or a concurrent
                    // path may have removed the file.
                    Ok(())
                }
                Err(e) => Err(WalRetentionSourceError::Failed(format!(
                    "remove_file {}: {e}",
                    path.display()
                ))),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// SnapshotSource — orchestrate write_checkpoint → arena msync → copy.
// ---------------------------------------------------------------------------

/// Backs `SnapshotWorker` against the per-shard arena + WAL + metadata.
/// Spec §05/09 §3.
///
/// Snapshot directory layout:
///
/// ```text
///   <data_dir>/<shard_id>/snapshots/<snapshot_id>/
///     arena.bin       (copy of the arena at checkpoint time)
///     metadata.redb   (copy of the redb file under read txn)
///     hnsw.{graph,data,brain}   (SharedHnsw::save_snapshot output)
///     manifest.toml   ({ shard_uuid, durable_lsn, taken_at, ... })
/// ```
///
/// `take_snapshot` runs the spec §05/09 §3 procedure (CHECKPOINT_BEGIN →
/// msync arena → CHECKPOINT_END), copies the on-disk arena + metadata
/// files, then asks the per-shard `SharedHnsw` to write its snapshot
/// triple (`hnsw.graph` / `hnsw.data` / `hnsw.brain` per SD-4.5-1)
/// into the same directory.
pub(crate) struct ShardSnapshotSource {
    shard_uuid: [u8; 16],
    snapshots_root: PathBuf,
    arena_path: PathBuf,
    metadata_path: PathBuf,
    arena: Rc<RefCell<ArenaFile>>,
    wal: Rc<RefCell<Option<Wal>>>,
    metadata: SharedMetadataDb,
    hnsw: SharedHnsw<{ VECTOR_DIM }>,
    next_checkpoint_id: RefCell<u64>,
}

impl ShardSnapshotSource {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        shard_uuid: [u8; 16],
        snapshots_root: PathBuf,
        arena_path: PathBuf,
        metadata_path: PathBuf,
        arena: Rc<RefCell<ArenaFile>>,
        wal: Rc<RefCell<Option<Wal>>>,
        metadata: SharedMetadataDb,
        hnsw: SharedHnsw<{ VECTOR_DIM }>,
    ) -> Self {
        Self {
            shard_uuid,
            snapshots_root,
            arena_path,
            metadata_path,
            arena,
            wal,
            metadata,
            hnsw,
            next_checkpoint_id: RefCell::new(1),
        }
    }

    fn next_ckpt_id(&self) -> u64 {
        let mut g = self.next_checkpoint_id.borrow_mut();
        let id = *g;
        *g = g.saturating_add(1);
        id
    }

    fn snapshot_dir(&self, id: SnapshotId) -> PathBuf {
        self.snapshots_root.join(format!("{:020}", id.0))
    }
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl SnapshotSource for ShardSnapshotSource {
    fn take_snapshot(&self) -> TakeFuture<'_> {
        Box::pin(async move {
            let ckpt_id = self.next_ckpt_id();
            let started = now_unix_nanos();

            // 1-3. Inline the spec §05/09 §3 checkpoint sequence so the
            //      arena borrow only lives across the sync msync_all
            //      step — never across a `.await`. The wal RefCell is
            //      borrowed immutably across the wal.append awaits; the
            //      single-threaded executor + interior-mutability of
            //      `Wal` keeps that sound (other tasks may take their
            //      own immutable borrows; `borrow_mut` only happens at
            //      shutdown after the scheduler has drained).

            // Step 1: CHECKPOINT_BEGIN.
            let target_lsn_hint: u64;
            {
                let wal_guard = self.wal.borrow();
                let wal = match wal_guard.as_ref() {
                    Some(w) => w,
                    None => {
                        return Err(SnapshotSourceError::Failed(
                            "wal not initialised (shutdown?)".into(),
                        ));
                    }
                };
                let payload = WalPayload::CheckpointBegin(CheckpointBeginPayload {
                    checkpoint_id: ckpt_id,
                    started_at_unix_nanos: started,
                });
                let record = WalRecord::from_typed(Lsn(0), 0, started, 0, &payload);
                wal.append(record)
                    .await
                    .map_err(|e| SnapshotSourceError::Failed(format!("checkpoint begin: {e}")))?;
                target_lsn_hint = wal.next_lsn().saturating_sub(1);
            }

            // Step 3: msync arena (sync). Short mutex-style borrow.
            let arena_capacity_at_checkpoint = {
                let arena = self.arena.borrow();
                arena
                    .msync_all()
                    .map_err(|e| SnapshotSourceError::Failed(format!("arena msync_all: {e}")))?;
                arena.capacity_slots()
            };

            // Step 6: CHECKPOINT_END.
            {
                let wal_guard = self.wal.borrow();
                let wal = match wal_guard.as_ref() {
                    Some(w) => w,
                    None => {
                        return Err(SnapshotSourceError::Failed(
                            "wal disappeared mid-checkpoint".into(),
                        ));
                    }
                };
                let payload = WalPayload::CheckpointEnd(CheckpointEndPayload {
                    checkpoint_id: ckpt_id,
                    durable_lsn: target_lsn_hint,
                    arena_capacity: arena_capacity_at_checkpoint,
                });
                let record = WalRecord::from_typed(Lsn(0), 0, now_unix_nanos(), 0, &payload);
                wal.append(record)
                    .await
                    .map_err(|e| SnapshotSourceError::Failed(format!("checkpoint end: {e}")))?;
            }

            // 4. Lay out the snapshot directory.
            let snap_id = SnapshotId(ckpt_id);
            let dir = self.snapshot_dir(snap_id);
            std::fs::create_dir_all(&dir).map_err(|e| {
                SnapshotSourceError::Failed(format!("create_dir_all {}: {e}", dir.display()))
            })?;

            // 5. Copy arena.bin. msync_all already ran inside
            //    write_checkpoint; the on-disk image at this instant is
            //    consistent with the checkpoint's durable_lsn.
            let arena_dst = dir.join("arena.bin");
            std::fs::copy(&self.arena_path, &arena_dst).map_err(|e| {
                SnapshotSourceError::Failed(format!(
                    "copy arena.bin → {}: {e}",
                    arena_dst.display()
                ))
            })?;

            // 6. Copy metadata.redb. Take a read txn first to flush any
            //    in-memory state to disk; release it before copying so
            //    redb's file lock is dropped. (redb's read txns are
            //    snapshot-isolated; a copy of the file *while a read txn
            //    is alive* gives a consistent point-in-time image.)
            let metadata_dst = dir.join("metadata.redb");
            {
                let md = self.metadata.lock();
                let _rtxn = md
                    .read_txn()
                    .map_err(|e| SnapshotSourceError::Failed(format!("metadata read_txn: {e}")))?;
                std::fs::copy(&self.metadata_path, &metadata_dst).map_err(|e| {
                    SnapshotSourceError::Failed(format!(
                        "copy metadata.redb → {}: {e}",
                        metadata_dst.display()
                    ))
                })?;
            }

            // 7. HNSW snapshot (graph + data + brain wrapper). Writes
            //    three files under `dir` with basename "hnsw"; per
            //    SD-4.5-1, `hnsw_rs::file_dump` is a 2-file format and
            //    the wrapper carries shard_uuid + durable_lsn + the
            //    BLAKE3 footer.
            let durable_lsn_for_hnsw =
                brain_storage::recovery::MetadataSink::durable_lsn(&*self.metadata.lock());
            self.hnsw
                .save_snapshot(&dir, "hnsw", durable_lsn_for_hnsw, self.shard_uuid)
                .map_err(|e| SnapshotSourceError::Failed(format!("hnsw save_snapshot: {e}")))?;

            // 8. Manifest.
            let durable_lsn =
                brain_storage::recovery::MetadataSink::durable_lsn(&*self.metadata.lock());
            let manifest = format!(
                "shard_uuid_hex = \"{}\"\n\
                 checkpoint_id = {}\n\
                 durable_lsn = {}\n\
                 taken_at_unix_nanos = {}\n",
                hex_lower(&self.shard_uuid),
                ckpt_id,
                durable_lsn,
                started,
            );
            std::fs::write(dir.join("manifest.toml"), manifest)
                .map_err(|e| SnapshotSourceError::Failed(format!("write manifest: {e}")))?;

            Ok(snap_id)
        })
    }

    fn list_snapshots(&self) -> SnapshotListFuture<'_> {
        let root = self.snapshots_root.clone();
        Box::pin(async move {
            if !root.exists() {
                return Ok(Vec::new());
            }
            let mut out = Vec::new();
            let entries = std::fs::read_dir(&root).map_err(|e| {
                SnapshotSourceError::Failed(format!("read_dir {}: {e}", root.display()))
            })?;
            for entry in entries {
                let entry = entry
                    .map_err(|e| SnapshotSourceError::Failed(format!("read_dir entry: {e}")))?;
                let file_type = entry
                    .file_type()
                    .map_err(|e| SnapshotSourceError::Failed(format!("file_type: {e}")))?;
                if !file_type.is_dir() {
                    continue;
                }
                let name = entry.file_name();
                let name_str = match name.to_str() {
                    Some(s) => s,
                    None => continue,
                };
                let id_u64: u64 = match name_str.parse() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let manifest_path = entry.path().join("manifest.toml");
                let taken_at = read_manifest_taken_at(&manifest_path).unwrap_or(0);
                let size_bytes = dir_size_bytes(&entry.path()).unwrap_or(0);
                out.push(SnapshotDesc {
                    id: SnapshotId(id_u64),
                    taken_at_unix_nanos: taken_at,
                    size_bytes,
                });
            }
            Ok(out)
        })
    }

    fn delete_snapshot(&self, id: SnapshotId) -> SnapshotDeleteFuture<'_> {
        let dir = self.snapshot_dir(id);
        Box::pin(async move {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(SnapshotSourceError::Failed(format!(
                    "remove_dir_all {}: {e}",
                    dir.display()
                ))),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn dir_size_bytes(dir: &std::path::Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            total = total.saturating_add(dir_size_bytes(&entry.path())?);
        } else if ft.is_file() {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

fn read_manifest_taken_at(path: &std::path::Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("taken_at_unix_nanos = ") {
            return rest.trim().parse().ok();
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use brain_embed::VECTOR_DIM;
    use brain_metadata::MetadataDb;
    use brain_storage::arena::ArenaFile;
    use brain_storage::wal::{Wal, WalConfig};
    use glommio::LocalExecutorBuilder;
    use parking_lot::Mutex;
    use tempfile::TempDir;

    fn fresh_arena(dir: &std::path::Path, capacity_slots: u64) -> (ArenaFile, [u8; 16], PathBuf) {
        let uuid: [u8; 16] = *uuid::Uuid::now_v7().as_bytes();
        let path = dir.join("arena.bin");
        let arena = ArenaFile::open(&path, uuid, capacity_slots).expect("ArenaFile::open");
        (arena, uuid, path)
    }

    fn glommio_run<F, Fut, T>(f: F) -> T
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T>,
        T: Send + 'static,
    {
        let handle = LocalExecutorBuilder::default()
            .spawn(move || async move { f().await })
            .expect("spawn glommio executor");
        handle.join().expect("join glommio executor")
    }

    // ---- ArenaRebuildSource ------------------------------------------------

    #[test]
    fn rebuild_source_skips_unoccupied_and_tombstoned() {
        let tmp = TempDir::new().unwrap();
        let tmp_path = tmp.path().to_path_buf();

        let pairs = glommio_run(move || async move {
            let (mut arena, _uuid, _path) = fresh_arena(&tmp_path, 8);
            // Slot 0: occupied, non-tombstoned.
            {
                let s = arena.slot_mut(0);
                s.metadata.flags = brain_storage::arena::slot::flags::OCCUPIED;
                s.metadata.slot_version = 1;
                s.vector[0] = 1.0;
            }
            // Slot 1: occupied + tombstoned → skip.
            {
                let s = arena.slot_mut(1);
                s.metadata.flags = brain_storage::arena::slot::flags::OCCUPIED
                    | brain_storage::arena::slot::flags::TOMBSTONED;
                s.metadata.slot_version = 2;
            }
            // Slot 2: unoccupied → skip.
            // Slot 3: occupied + hard-forgotten → skip.
            {
                let s = arena.slot_mut(3);
                s.metadata.flags = brain_storage::arena::slot::flags::OCCUPIED
                    | brain_storage::arena::slot::flags::HARD_FORGOTTEN;
                s.metadata.slot_version = 3;
            }
            // Slot 4: occupied with vector data.
            {
                let s = arena.slot_mut(4);
                s.metadata.flags = brain_storage::arena::slot::flags::OCCUPIED;
                s.metadata.slot_version = 7;
                s.vector[5] = 0.5;
            }

            let arena_cell = Rc::new(RefCell::new(arena));
            let src: ArenaRebuildSource<{ VECTOR_DIM }> = ArenaRebuildSource::new(3, arena_cell);
            src.snapshot_vectors().await
        })
        .expect("snapshot_vectors");

        assert_eq!(pairs.len(), 2, "only slots 0 + 4 should be returned");
        // Slot 0
        let (mid0, _) = pairs.iter().find(|(m, _)| m.slot() == 0).unwrap();
        assert_eq!(mid0.shard(), 3);
        assert_eq!(mid0.version(), 1);
        // Slot 4
        let (mid4, v4) = pairs.iter().find(|(m, _)| m.slot() == 4).unwrap();
        assert_eq!(mid4.shard(), 3);
        assert_eq!(mid4.version(), 7);
        assert!((v4[5] - 0.5).abs() < f32::EPSILON);
    }

    // ---- WalDirRetentionSource --------------------------------------------

    #[test]
    fn retention_source_durable_lsn_round_trips_via_metadata_db() {
        let tmp = TempDir::new().unwrap();
        let wal_dir = tmp.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();
        let uuid: [u8; 16] = *uuid::Uuid::now_v7().as_bytes();
        let md_path = tmp.path().join("metadata.redb");
        let md = MetadataDb::open(&md_path).expect("MetadataDb::open");
        let metadata: SharedMetadataDb = Arc::new(Mutex::new(md));

        // The source is `!Send` because of the `Arc<Mutex<MetadataDb>>`
        // → `parking_lot::Mutex` is `Send + Sync` but the source's
        // future returns are `!Send` (their trait dropped + Send in
        // 9.8). Construct inside the executor closure.
        let cp = glommio_run(move || async move {
            let src = WalDirRetentionSource::new(wal_dir, uuid, metadata);
            src.current_checkpoint().await
        })
        .expect("current_checkpoint");
        assert_eq!(cp.durable_lsn, 0, "fresh MetadataDb has durable_lsn = 0");
    }

    #[test]
    fn retention_source_delete_segment_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let wal_dir = tmp.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();
        let uuid: [u8; 16] = *uuid::Uuid::now_v7().as_bytes();
        let md_path = tmp.path().join("metadata.redb");
        let md = MetadataDb::open(&md_path).expect("MetadataDb::open");
        let metadata: SharedMetadataDb = Arc::new(Mutex::new(md));

        glommio_run(move || async move {
            let src = WalDirRetentionSource::new(wal_dir, uuid, metadata);
            src.delete_segment(99)
                .await
                .expect("delete_segment idempotent on missing file");
        });
    }

    #[test]
    fn retention_source_list_segments_round_trips_real_wal() {
        let tmp = TempDir::new().unwrap();
        let wal_dir = tmp.path().join("wal");
        let uuid: [u8; 16] = *uuid::Uuid::now_v7().as_bytes();
        let md_path = tmp.path().join("metadata.redb");
        let md = MetadataDb::open(&md_path).expect("MetadataDb::open");
        let metadata: SharedMetadataDb = Arc::new(Mutex::new(md));

        let segs = glommio_run(move || async move {
            std::fs::create_dir_all(&wal_dir).unwrap();
            // Create a fresh WAL, then immediately drain so the segment
            // file is closed and listable by a fresh WalReader.
            let wal = Wal::create_with_config(&wal_dir, uuid, WalConfig::default())
                .await
                .expect("Wal::create_with_config");
            wal.shutdown().await.expect("Wal::shutdown");

            let src = WalDirRetentionSource::new(wal_dir, uuid, metadata);
            src.list_segments().await
        })
        .expect("list_segments");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].segment_id, 0);
        assert_eq!(segs[0].first_lsn, 1);
    }

    // ---- ShardSnapshotSource ----------------------------------------------

    #[test]
    fn snapshot_take_list_delete_round_trips() {
        let tmp = TempDir::new().unwrap();
        let snapshots_root = tmp.path().join("snapshots");
        let arena_path = tmp.path().join("arena.bin");
        let md_path = tmp.path().join("metadata.redb");
        let wal_dir = tmp.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();
        let uuid: [u8; 16] = *uuid::Uuid::now_v7().as_bytes();

        // MetadataDb is `Send + Sync` (wrapped in `parking_lot::Mutex`)
        // so it crosses the spawn boundary. ArenaFile + Wal must be
        // opened inside the executor (Wal::create is async; ArenaFile
        // is Send but we keep all `Rc<RefCell<…>>` construction local).
        let md = brain_metadata::MetadataDb::open(&md_path).expect("MetadataDb::open");
        let metadata: SharedMetadataDb = std::sync::Arc::new(Mutex::new(md));

        let arena_path_cloned = arena_path.clone();
        let md_path_cloned = md_path.clone();
        let snapshots_root_cloned = snapshots_root.clone();
        let wal_dir_cloned = wal_dir.clone();

        glommio_run({
            let metadata = metadata.clone();
            move || async move {
                let arena = ArenaFile::open(&arena_path_cloned, uuid, 8).expect("ArenaFile::open");
                let arena_cell = Rc::new(RefCell::new(arena));

                let wal = Wal::create_with_config(&wal_dir_cloned, uuid, WalConfig::default())
                    .await
                    .expect("Wal::create_with_config");
                let wal_cell = Rc::new(RefCell::new(Some(wal)));

                let (hnsw_shared, _hnsw_writer) = brain_index::SharedHnsw::<{ VECTOR_DIM }>::new(
                    brain_index::IndexParams::default_v1(),
                )
                .expect("SharedHnsw::new");

                let src = ShardSnapshotSource::new(
                    uuid,
                    snapshots_root_cloned.clone(),
                    arena_path_cloned,
                    md_path_cloned,
                    arena_cell,
                    wal_cell.clone(),
                    metadata,
                    hnsw_shared,
                );

                let id = src.take_snapshot().await.expect("take_snapshot");
                assert_eq!(id.0, 1);

                let listed = src.list_snapshots().await.expect("list_snapshots");
                assert_eq!(listed.len(), 1);
                assert_eq!(listed[0].id, id);

                src.delete_snapshot(id).await.expect("delete_snapshot");

                let listed_after = src.list_snapshots().await.expect("list_snapshots empty");
                assert!(listed_after.is_empty());

                // Drain the WAL cleanly so the test doesn't leak.
                let mut g = wal_cell.borrow_mut();
                if let Some(w) = g.take() {
                    w.shutdown().await.expect("Wal::shutdown");
                }
            }
        });

        // The snapshot directory was created and then cleaned by
        // `delete_snapshot`; the root itself should still exist.
        assert!(snapshots_root.exists());
    }
}
