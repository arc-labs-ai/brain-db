//! Background-worker source adapters wired to per-shard state.
//!
//! There are four pluggable "source" traits with `Disabled*` defaults:
//!
//! - `RebuildSource`: feeds `HnswMaintenanceWorker` the active
//!   `(MemoryId, vector)` pairs for full rebuild.
//! - `WalRetentionSource`: tells `WalRetentionWorker` which segments
//!   are past `durable_lsn` and removes them.
//! - `SnapshotSource`: backs `SnapshotWorker`'s take / list / delete.
//! - `CacheEvictionSource`: stays `Disabled*` until a real
//!   `CachingDispatcher` is wired per shard.
//!
//! Every worker is registered against the per-shard scheduler with the
//! `Disabled*` defaults; real adapters are plugged in for the first
//! three. The fourth — cache eviction — stays disabled and is
//! constructed at the call-site (no adapter struct here).
//!
//! All adapters are `!Send + !Sync` by construction (they hold
//! `Rc<RefCell<…>>` references into per-shard state). Their trait
//! contracts dropped `Send + Sync` to match.

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

use crate::shard::snapshot_manifest::{blake3_hex, FileDigest, SnapshotManifest, MANIFEST_FILE};

// ---------------------------------------------------------------------------
// RebuildSource — scan the shard's arena for occupied/non-tombstoned slots.
// ---------------------------------------------------------------------------

/// Walks the shard's `ArenaFile` and yields a `(MemoryId, vector)` for
/// every occupied, non-tombstoned, non-hard-forgotten slot. The
/// rebuild source is the substrate for full HNSW rebuild.
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
/// directory.
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
        // the format here: zero-padded 10-digit decimal.
        self.wal_dir.join(format!("{:010}.wal", segment_seq))
    }
}

impl WalRetentionSource for WalDirRetentionSource {
    fn current_checkpoint(&self) -> CheckpointFuture<'_> {
        let metadata = self.metadata.clone();
        Box::pin(async move {
            // brain_metadata::MetadataDb caches durable_lsn in memory;
            // the lookup is a single u64 read.
            let lsn = brain_storage::recovery::MetadataSink::durable_lsn(metadata.as_ref());
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
/// `take_snapshot` runs the procedure (CHECKPOINT_BEGIN →
/// msync arena → CHECKPOINT_END), copies the on-disk arena + metadata
/// files, then asks the per-shard `SharedHnsw` to write its snapshot
/// triple (`hnsw.graph` / `hnsw.data` / `hnsw.brain` per SD-4.5-1)
/// into the same directory.
pub(crate) struct ShardSnapshotSource {
    shard_uuid: [u8; 16],
    snapshots_root: PathBuf,
    arena_path: PathBuf,
    metadata_path: PathBuf,
    wal_dir: PathBuf,
    arena: Rc<RefCell<ArenaFile>>,
    wal: Rc<RefCell<Option<Wal>>>,
    metadata: SharedMetadataDb,
    hnsw: SharedHnsw,
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
        hnsw: SharedHnsw,
    ) -> Self {
        // metadata.redb lives at the shard root; the WAL directory is a
        // sibling. Derive it through ShardPaths so the snapshot bundle's
        // WAL-tail copy reads from the same layout the writer uses.
        let wal_dir = metadata_path
            .parent()
            .map(|root| brain_storage::ShardPaths::at(root).wal_dir())
            .unwrap_or_else(|| metadata_path.clone());
        Self {
            shard_uuid,
            snapshots_root,
            arena_path,
            metadata_path,
            wal_dir,
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

            // 1-3. Inline the checkpoint sequence so the
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

            // 5. Reflink arena.bin. msync_all already ran inside the
            //    checkpoint sequence; the on-disk image at this instant
            //    is consistent with the checkpoint's durable_lsn. The
            //    reflink (FICLONE) shares blocks copy-on-write where the
            //    filesystem supports it, falling back to a full copy.
            let arena_dst = dir.join("arena.bin");
            brain_storage::reflink_or_copy(&self.arena_path, &arena_dst).map_err(|e| {
                SnapshotSourceError::Failed(format!(
                    "reflink arena.bin → {}: {e}",
                    arena_dst.display()
                ))
            })?;

            // 6. Reflink metadata.redb. Take a read txn first to flush
            //    any in-memory state to disk; release it before copying
            //    so redb's file lock is dropped. (redb's read txns are
            //    snapshot-isolated; a copy of the file *while a read txn
            //    is alive* gives a consistent point-in-time image.)
            let metadata_dst = dir.join("metadata.redb");
            {
                let _rtxn = self
                    .metadata
                    .read_txn()
                    .map_err(|e| SnapshotSourceError::Failed(format!("metadata read_txn: {e}")))?;
                brain_storage::reflink_or_copy(&self.metadata_path, &metadata_dst).map_err(
                    |e| {
                        SnapshotSourceError::Failed(format!(
                            "reflink metadata.redb → {}: {e}",
                            metadata_dst.display()
                        ))
                    },
                )?;
            }

            // 6a. Copy shard.uuid so the bundle is self-describing and a
            //     restore onto a freshly-laid-out data dir lands the
            //     identity file too. Best-effort: the manifest's
            //     shard_uuid is the authoritative identity, so a missing
            //     uuid file here doesn't fail the snapshot.
            let uuid_src = self
                .metadata_path
                .parent()
                .map(|root| brain_storage::ShardPaths::at(root).shard_uuid());
            if let Some(uuid_src) = uuid_src {
                if uuid_src.exists() {
                    let uuid_dst = dir.join(brain_storage::layout::SHARD_UUID_FILE);
                    if let Err(e) = brain_storage::reflink_or_copy(&uuid_src, &uuid_dst) {
                        tracing::warn!(
                            error = %e,
                            "snapshot: copying shard.uuid into bundle failed (non-fatal)"
                        );
                    }
                }
            }

            // 6b. Copy the WAL tail. A snapshot that can't be replayed
            //     back to its LSN is useless, so the bundle must carry
            //     every segment that covers [.. durable_lsn]. We copy
            //     each segment whose starting_lsn <= durable_lsn — that
            //     set always includes the segment containing durable_lsn
            //     and every earlier one, so recovery can replay the WAL
            //     to the snapshot LSN. Segments live in `<dir>/wal/`.
            let wal_dst_dir = dir.join("wal");
            std::fs::create_dir_all(&wal_dst_dir).map_err(|e| {
                SnapshotSourceError::Failed(format!(
                    "create_dir_all {}: {e}",
                    wal_dst_dir.display()
                ))
            })?;
            let mut wal_segment_rel_paths: Vec<String> = Vec::new();
            {
                let reader = WalReader::open(&self.wal_dir, self.shard_uuid).map_err(|e| {
                    SnapshotSourceError::Failed(format!("WalReader::open for snapshot tail: {e}"))
                })?;
                for seg in reader.segments() {
                    if seg.starting_lsn > target_lsn_hint {
                        // Segment begins after the snapshot LSN — its
                        // records are entirely post-snapshot. Skip so the
                        // bundle is a true point-in-time view.
                        continue;
                    }
                    let name = format!("{:010}.wal", seg.segment_seq);
                    let src = self.wal_dir.join(&name);
                    let dst = wal_dst_dir.join(&name);
                    brain_storage::reflink_or_copy(&src, &dst).map_err(|e| {
                        SnapshotSourceError::Failed(format!(
                            "reflink wal segment {} → {}: {e}",
                            src.display(),
                            dst.display()
                        ))
                    })?;
                    wal_segment_rel_paths.push(format!("wal/{name}"));
                }
            }

            // 7. HNSW snapshot (graph + data + brain wrapper). Writes
            //    three files under `dir` with basename "hnsw"; per
            //    SD-4.5-1, `hnsw_rs::file_dump` is a 2-file format and
            //    the wrapper carries shard_uuid + durable_lsn + the
            //    BLAKE3 footer.
            let durable_lsn_for_hnsw =
                brain_storage::recovery::MetadataSink::durable_lsn(self.metadata.as_ref());
            self.hnsw
                .save_snapshot(&dir, "hnsw", durable_lsn_for_hnsw, self.shard_uuid)
                .map_err(|e| SnapshotSourceError::Failed(format!("hnsw save_snapshot: {e}")))?;

            // 8. Manifest. The snapshot's LSN is the checkpoint's
            //    durable_lsn — the point recovery replays the bundled WAL
            //    up to. Hash every bundle file (arena, metadata, each WAL
            //    segment) with BLAKE3 so restore can verify integrity
            //    before swapping files into the live data dir. The HNSW
            //    triple is intentionally excluded: it's rebuilt on
            //    restore, never trusted from the bundle.
            let mut files = std::collections::BTreeMap::new();
            for rel in std::iter::once("arena.bin".to_string())
                .chain(std::iter::once("metadata.redb".to_string()))
                .chain(wal_segment_rel_paths.iter().cloned())
            {
                let path = dir.join(&rel);
                let size = std::fs::metadata(&path)
                    .map_err(|e| {
                        SnapshotSourceError::Failed(format!("stat {}: {e}", path.display()))
                    })?
                    .len();
                let blake3 = blake3_hex(&path).map_err(|e| {
                    SnapshotSourceError::Failed(format!("blake3 {}: {e}", path.display()))
                })?;
                files.insert(rel, FileDigest { size, blake3 });
            }

            let manifest = SnapshotManifest {
                snapshot_lsn: target_lsn_hint,
                checkpoint_id: ckpt_id,
                shard_uuid: hex_lower(&self.shard_uuid),
                taken_at_unix_nanos: started,
                files,
            };
            manifest
                .write_to(&dir.join(MANIFEST_FILE))
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
                let manifest_path = entry.path().join(MANIFEST_FILE);
                let taken_at = SnapshotManifest::read_from(&manifest_path)
                    .map(|m| m.taken_at_unix_nanos)
                    .unwrap_or(0);
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
        let metadata: SharedMetadataDb = Arc::new(md);

        // The source's future returns are `!Send` (their trait dropped
        // Send), so construct it inside the executor closure
        // rather than across the spawn boundary.
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
        let metadata: SharedMetadataDb = Arc::new(md);

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
        let metadata: SharedMetadataDb = Arc::new(md);

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

    /// Take a snapshot of a populated shard (≥1 HNSW vector + ≥1 WAL
    /// record) and assert the bundle is complete: arena.bin,
    /// metadata.redb, ≥1 WAL segment, and a manifest.json whose BLAKE3
    /// digests match the on-disk files. Then list + delete it.
    #[test]
    fn snapshot_bundle_is_complete_and_blake3_matches() {
        use crate::shard::snapshot_manifest::{blake3_hex, SnapshotManifest, MANIFEST_FILE};
        use brain_core::MemoryId;

        let tmp = TempDir::new().unwrap();
        let snapshots_root = tmp.path().join("snapshots");
        // metadata.redb at the root, wal/ as a sibling — the layout
        // ShardSnapshotSource::new derives the WAL dir from.
        let arena_path = tmp.path().join("arena.bin");
        let md_path = tmp.path().join("metadata.redb");
        let wal_dir = tmp.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();
        let uuid: [u8; 16] = *uuid::Uuid::now_v7().as_bytes();

        let md = brain_metadata::MetadataDb::open(&md_path).expect("MetadataDb::open");
        let metadata: SharedMetadataDb = std::sync::Arc::new(md);

        let arena_path_cloned = arena_path.clone();
        let md_path_cloned = md_path.clone();
        let snapshots_root_cloned = snapshots_root.clone();
        let wal_dir_cloned = wal_dir.clone();

        let snap_dir = glommio_run({
            let metadata = metadata.clone();
            move || async move {
                let mut arena =
                    ArenaFile::open(&arena_path_cloned, uuid, 8).expect("ArenaFile::open");
                // Occupy a slot so the arena image isn't all-zero.
                {
                    let s = arena.slot_mut(0);
                    s.metadata.flags = brain_storage::arena::slot::flags::OCCUPIED;
                    s.metadata.slot_version = 1;
                    s.vector[0] = 0.25;
                }
                arena.msync_all().expect("arena msync");
                let arena_cell = Rc::new(RefCell::new(arena));

                let wal = Wal::create_with_config(&wal_dir_cloned, uuid, WalConfig::default())
                    .await
                    .expect("Wal::create_with_config");
                let wal_cell = Rc::new(RefCell::new(Some(wal)));

                // Populate the HNSW so save_snapshot isn't a no-op.
                let (hnsw_shared, _hnsw_writer) =
                    brain_index::SharedHnsw::new(brain_index::IndexParams::default_v1())
                        .expect("SharedHnsw::new");
                let mut v = [0.0_f32; VECTOR_DIM];
                v[0] = 1.0;
                let mid = MemoryId::pack(0, 0, 1);
                hnsw_shared.insert_recovery(mid, &v);
                // Publish pending into the main epoch so save_snapshot
                // (which snapshots `main`) isn't an empty-graph no-op.
                let params = hnsw_shared.params();
                hnsw_shared
                    .flush_with_rebuild(move |pending| {
                        let pairs: Vec<_> =
                            pending.iter().map(|e| (e.memory_id, e.vector)).collect();
                        let (idx, _) = brain_index::rebuild::rebuild_impl(params, pairs)?;
                        Ok(idx)
                    })
                    .expect("flush_with_rebuild publishes the vector");
                assert!(!hnsw_shared.is_empty(), "HNSW main must be non-empty");

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

                let dir = snapshots_root_cloned.join(format!("{:020}", id.0));

                // Drain the WAL cleanly so the test doesn't leak.
                let mut g = wal_cell.borrow_mut();
                if let Some(w) = g.take() {
                    w.shutdown().await.expect("Wal::shutdown");
                }
                dir
            }
        });

        // Bundle assertions (sync, outside the executor).
        assert!(snap_dir.join("arena.bin").is_file(), "arena.bin present");
        assert!(
            snap_dir.join("metadata.redb").is_file(),
            "metadata.redb present"
        );
        let wal_segs: Vec<_> = std::fs::read_dir(snap_dir.join("wal"))
            .expect("bundle wal/ dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("wal"))
            .collect();
        assert!(
            !wal_segs.is_empty(),
            "bundle must carry ≥1 WAL segment (the checkpoint tail)"
        );
        assert!(
            snap_dir.join("hnsw.brain").is_file(),
            "non-empty HNSW snapshot marker present"
        );

        let manifest =
            SnapshotManifest::read_from(&snap_dir.join(MANIFEST_FILE)).expect("manifest.json");
        assert!(manifest.files.contains_key("arena.bin"));
        assert!(manifest.files.contains_key("metadata.redb"));
        assert!(
            manifest.files.keys().any(|k| k.starts_with("wal/")),
            "manifest lists ≥1 wal segment"
        );
        // Every manifest digest matches the on-disk file.
        for (rel, digest) in &manifest.files {
            let path = snap_dir.join(rel);
            assert_eq!(
                std::fs::metadata(&path).unwrap().len(),
                digest.size,
                "size mismatch for {rel}"
            );
            assert_eq!(
                blake3_hex(&path).unwrap(),
                digest.blake3,
                "blake3 mismatch for {rel}"
            );
        }

        // Delete cleans the directory; the root persists.
        glommio_run({
            let metadata = metadata.clone();
            let snapshots_root = snapshots_root.clone();
            let arena_path = arena_path.clone();
            let md_path = md_path.clone();
            move || async move {
                let arena = ArenaFile::open(&arena_path, uuid, 8).expect("reopen arena");
                let arena_cell = Rc::new(RefCell::new(arena));
                let wal_cell = Rc::new(RefCell::new(None));
                let (hnsw_shared, _w) =
                    brain_index::SharedHnsw::new(brain_index::IndexParams::default_v1()).unwrap();
                let src = ShardSnapshotSource::new(
                    uuid,
                    snapshots_root,
                    arena_path,
                    md_path,
                    arena_cell,
                    wal_cell,
                    metadata,
                    hnsw_shared,
                );
                src.delete_snapshot(SnapshotId(1))
                    .await
                    .expect("delete_snapshot");
                let after = src.list_snapshots().await.expect("list after delete");
                assert!(after.is_empty());
            }
        });
        assert!(snapshots_root.exists());
    }
}
