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

use brain_core::{MemoryId, ShardId, SlotIndex, SlotVersion};
use brain_index::{SharedHnsw, SpaceVectorSource, VECTOR_DIM};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_ops::memory_artifact::get_artifact_vector;
use brain_ops::RedbCommittedWatermark;
use brain_planner::SharedMetadataDb;
use brain_storage::arena::ArenaFile;
use brain_storage::wal::payload::{CheckpointBeginPayload, CheckpointEndPayload, WalPayload};
use brain_storage::wal::reader::WalReader;
use brain_storage::wal::record::{Lsn, WalRecord};
use brain_storage::wal::Wal;
use brain_workers::hnsw_maint::{RebuildSource, RebuildSourceError, SnapshotFuture};
use brain_workers::snapshot::{
    DeleteFuture as SnapshotDeleteFuture, ListFuture as SnapshotListFuture, SnapshotDesc,
    SnapshotId, SnapshotSource, SnapshotSourceError, TakeFuture,
};
use brain_workers::wal_retention::{
    CheckpointDesc, CheckpointFuture, DeleteFuture as WalDeleteFuture, SegmentDesc,
    SegmentListFuture, WalRetentionSource, WalRetentionSourceError,
};
use redb::ReadableTable;

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
// RedbRebuildSource — enumerate live vectors from the authoritative redb store.
// ---------------------------------------------------------------------------

/// Yields a `(MemoryId, vector)` for every live (active, non-hard-forgotten)
/// memory by joining `MEMORIES_TABLE` (membership + tombstone state) with the
/// per-memory `MEMORY_ARTIFACTS_TABLE` (the durable write-time vector).
///
/// This is the rebuild substrate for **runtime** full rebuilds (the HNSW
/// maintenance worker and the admin `rebuild-ann` route). Unlike
/// [`ArenaRebuildSource`], it sees memories encoded in the current run: the
/// arena is populated only by WAL recovery on restart, so a live encode never
/// reaches it, whereas its vector is committed to redb on the ENCODE ack path.
/// Rebuilding from the arena alone would silently drop every same-run memory;
/// rebuilding from redb yields the complete live set.
///
/// Holds the shared `MetadataDb` handle; the scan runs under a single redb
/// read txn (snapshot-isolated), so a concurrent writer on the shard can't
/// tear the enumeration.
pub(crate) struct RedbRebuildSource<const D: usize> {
    metadata: SharedMetadataDb,
}

impl<const D: usize> RedbRebuildSource<D> {
    pub(crate) fn new(metadata: SharedMetadataDb) -> Self {
        Self { metadata }
    }
}

impl<const D: usize> RebuildSource<D> for RedbRebuildSource<D> {
    fn snapshot_vectors(&self) -> SnapshotFuture<'_, D> {
        let metadata = self.metadata.clone();
        Box::pin(async move {
            let rtxn = metadata
                .read_txn()
                .map_err(|e| RebuildSourceError::Failed(format!("read_txn: {e}")))?;
            let table = rtxn
                .open_table(MEMORIES_TABLE)
                .map_err(|e| RebuildSourceError::Failed(format!("open memories: {e}")))?;
            let mut out = Vec::new();
            for entry in table
                .iter()
                .map_err(|e| RebuildSourceError::Failed(format!("memories iter: {e}")))?
            {
                let (key_guard, row_guard) =
                    entry.map_err(|e| RebuildSourceError::Failed(format!("memories row: {e}")))?;
                let row = row_guard.value();
                // A tombstoned (inactive) or hard-forgotten memory must not
                // re-enter the searchable graph. Mirrors ArenaRebuildSource's
                // occupied-and-live filter.
                if !row.is_active() || row.is_hard_forgotten() {
                    continue;
                }
                let key = key_guard.value();
                // The vector lives in the artifact bundle, written on the ack
                // path. A missing bundle means the memory has no stored vector
                // (partial write / pre-feature row) — skip rather than insert
                // a zero vector that would pollute nearest-neighbour scores.
                let Some(vec) = get_artifact_vector(&rtxn, key) else {
                    continue;
                };
                let mut v = [0.0_f32; D];
                // D == VECTOR_DIM in every shard monomorphisation; the min
                // guard keeps a mismatched const from panicking.
                let n = v.len().min(vec.len());
                v[..n].copy_from_slice(&vec[..n]);
                out.push((MemoryId::from_be_bytes(key), v));
            }
            Ok(out)
        })
    }
}

// ---------------------------------------------------------------------------
// ArenaSpaceVectorSource — read a live memory's vector by arena slot.
// ---------------------------------------------------------------------------

/// Backs the per-space brute-force retrieval lane: given an arena slot +
/// the expected slot version, copy that slot's full-precision vector out
/// of the mmap'd arena. The retriever range-scans one space's memory ids
/// from redb, then calls this per surviving id.
///
/// Holds an `Rc<RefCell<ArenaFile>>` shared with the main loop; the read
/// takes a short immutable borrow, copies the vector, and releases it —
/// no `.await` is held across the borrow. `!Send` by construction (the
/// mmap), which is why it reaches the retriever as a borrowed
/// `&dyn SpaceVectorSource`, never stored on the `Send + Sync` retriever.
pub(crate) struct ArenaSpaceVectorSource {
    arena: Rc<RefCell<ArenaFile>>,
}

impl ArenaSpaceVectorSource {
    pub(crate) fn new(arena: Rc<RefCell<ArenaFile>>) -> Self {
        Self { arena }
    }
}

impl SpaceVectorSource for ArenaSpaceVectorSource {
    fn vector_at(
        &self,
        slot: SlotIndex,
        expected_version: SlotVersion,
    ) -> Option<[f32; VECTOR_DIM]> {
        let arena = self.arena.borrow();
        if slot >= arena.capacity_slots() {
            return None;
        }
        let s = arena.slot(slot);
        // A stale id (invariant #4), an unoccupied slot, or a
        // tombstoned / hard-forgotten memory yields no vector — the
        // candidate is dropped from the brute-force set.
        if !s.is_occupied() || s.is_tombstoned() || s.is_hard_forgotten() {
            return None;
        }
        if s.metadata.slot_version != expected_version {
            return None;
        }
        // No per-read CRC. This mirrors `ArenaRebuildSource` — the only
        // other vector-read path, which feeds every HNSW rebuild — so the
        // two are consistent; verify-on-read is a deliberate non-goal on
        // the hot brute-force lane. Copy element-wise: `Slot::vector` and
        // the return type are both `[f32; VECTOR_DIM]` (384).
        let mut v = [0.0_f32; VECTOR_DIM];
        let n = v.len().min(s.vector.len());
        v[..n].copy_from_slice(&s.vector[..n]);
        Some(v)
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
        // The active segment's true highest LSN is only knowable from live
        // WAL state, which this source doesn't hold. `durable_lsn` is a
        // safe floor: it is at least as high as any completed segment's
        // last record and lies within (or just before) the active segment.
        // Under-reporting the active segment here is harmless — the worker
        // never deletes it — but reporting the true last_lsn of every
        // *completed* segment is essential.
        let durable_lsn =
            brain_storage::recovery::MetadataSink::durable_lsn(self.metadata.as_ref());
        Box::pin(async move {
            let reader = WalReader::open(&dir, uuid)
                .map_err(|e| WalRetentionSourceError::Failed(format!("WalReader::open: {e}")))?;
            let infos = reader.segments();
            let mut segs = Vec::with_capacity(infos.len());
            for (i, s) in infos.iter().enumerate() {
                // Segments carry a contiguous, gap-free LSN range (the
                // reader enforces `next.starting_lsn == prev.last + 1`), so
                // a completed segment's true last LSN is the next segment's
                // `starting_lsn - 1`. The active (highest-seq) segment has
                // no successor; use `durable_lsn` as a lower bound. Both
                // are correct for retention: they never *under*-report a
                // completed segment (which would risk deleting a straddler),
                // and the active segment is guarded against deletion anyway.
                let last_lsn = match infos.get(i + 1) {
                    Some(next) => next.starting_lsn.saturating_sub(1),
                    None => durable_lsn,
                };
                segs.push(SegmentDesc {
                    segment_id: s.segment_seq,
                    first_lsn: s.starting_lsn,
                    last_lsn: last_lsn.max(s.starting_lsn),
                    size_bytes: s.file_size,
                });
            }
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
    /// The per-shard writer's redb-committed-LSN watermark. The
    /// checkpoint's `durable_lsn` must be this value (clamped to the
    /// WAL-durable tail at CHECKPOINT_BEGIN), NOT the WAL-appended tail:
    /// the WAL tail advances at enqueue time, before the writer's redb
    /// commit, so stamping it would let recovery skip a WAL-durable
    /// record whose redb commit had not run at power loss.
    redb_committed_watermark: RedbCommittedWatermark,
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
        redb_committed_watermark: RedbCommittedWatermark,
    ) -> Self {
        // metadata.redb lives at the shard root; the WAL directory is a
        // sibling. Derive it through ShardPaths so the snapshot bundle's
        // WAL-tail copy reads from the same layout the writer uses.
        let wal_dir = metadata_path
            .parent()
            .map(|root| brain_storage::ShardPaths::at(root).wal_dir())
            .unwrap_or_else(|| metadata_path.clone());
        // Resume the checkpoint counter past the highest snapshot id that
        // already exists on disk. Snapshot directory names *are* the id
        // (`snapshots/{id:020}`), and `reflink_or_copy` truncates an
        // existing destination — so if we restarted at 1 the next cycle
        // would overwrite the oldest surviving bundle, destroying a valid
        // backup (invariant #7). A missing/empty dir yields 0 → start at 1.
        let next_id = scan_max_snapshot_id(&snapshots_root).saturating_add(1);
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
            next_checkpoint_id: RefCell::new(next_id),
            redb_committed_watermark,
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

/// Highest numeric snapshot id present under `root`, or 0 if the
/// directory is missing, empty, or holds no numerically-named
/// sub-directories. Non-numeric entries are ignored (mirrors
/// `list_snapshots`). A read error is treated as "no snapshots" (0) so a
/// transient stat failure never lets the counter reset and overwrite a
/// prior bundle — the worst case is a delayed id, never a reused one.
fn scan_max_snapshot_id(root: &std::path::Path) -> u64 {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut max_id = 0u64;
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u64>().ok())
        {
            max_id = max_id.max(id);
        }
    }
    max_id
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

            // The checkpoint's durable_lsn is the redb-committed
            // watermark, NOT the WAL-appended tail (`target_lsn_hint`).
            // The watermark only advances after `wtxn.commit()`, so
            // every record at or below it is durable in metadata; a
            // record that is WAL-durable but whose redb commit had not
            // yet run sits strictly above it and is therefore replayed
            // (not skipped) on recovery. Clamp to `target_lsn_hint` so
            // the checkpoint never claims a durable_lsn past the WAL
            // tail bundled with this snapshot — the watermark is always
            // <= the true WAL tail, but a concurrent commit could push
            // it past the tail we sampled at CHECKPOINT_BEGIN, and
            // `min` keeps the checkpoint consistent with the copied WAL.
            let checkpoint_durable_lsn = self.redb_committed_watermark.load().min(target_lsn_hint);

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
                    durable_lsn: checkpoint_durable_lsn,
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

    // ---- RedbRebuildSource ------------------------------------------------

    /// The runtime rebuild source must enumerate the durable redb store, not
    /// the arena: a memory encoded in the current run has its vector in redb
    /// (written on the ENCODE ack path) but never in the arena (populated only
    /// by WAL recovery). This proves the source returns exactly the live
    /// (active, vector-bearing) memories and skips tombstoned rows and rows
    /// with no stored vector.
    #[test]
    fn redb_rebuild_source_enumerates_live_same_run_memories() {
        use brain_core::{MemoryId, MemoryKind, NamespaceId, SessionId, SpaceId};
        use brain_metadata::tables::memory::{flags, MemoryMetadata, MEMORIES_TABLE};
        use brain_ops::memory_artifact::merge_memory_artifact;

        let tmp = TempDir::new().unwrap();
        let md_path = tmp.path().join("metadata.redb");
        let md = MetadataDb::open(&md_path).expect("MetadataDb::open");
        let metadata: SharedMetadataDb = Arc::new(md);

        let space: SpaceId = [0u8; 16].into();
        let row = |slot: u64| {
            MemoryMetadata::new_active(
                MemoryId::pack(0, slot, 1),
                NamespaceId::SYSTEM,
                space,
                SessionId(1),
                slot,
                1,
                MemoryKind::Episodic,
                [0u8; 16],
                0.5,
                8,
                1_700_000_000_000_000_000,
            )
        };
        let live_vec = |first: f32| {
            let mut v = vec![0.0_f32; VECTOR_DIM];
            v[0] = first;
            v
        };

        // slot 1 + 2: active with a stored vector → returned.
        // slot 3: tombstoned (ACTIVE cleared) but has a vector → skipped.
        // slot 4: active but no artifact bundle → skipped.
        {
            let wtxn = metadata.write_txn().expect("write_txn");
            {
                let mut t = wtxn.open_table(MEMORIES_TABLE).expect("open memories");
                let m1 = row(1);
                let m2 = row(2);
                let mut m3 = row(3);
                m3.set_flag(flags::ACTIVE, false);
                let m4 = row(4);
                t.insert(&m1.memory_id_bytes, &m1).unwrap();
                t.insert(&m2.memory_id_bytes, &m2).unwrap();
                t.insert(&m3.memory_id_bytes, &m3).unwrap();
                t.insert(&m4.memory_id_bytes, &m4).unwrap();
            }
            for (slot, first) in [(1u64, 0.25_f32), (2, 0.5), (3, 0.75)] {
                let id = MemoryId::pack(0, slot, 1);
                merge_memory_artifact(&wtxn, id.to_be_bytes(), |b| {
                    b.vector = live_vec(first);
                })
                .expect("merge artifact");
            }
            wtxn.commit().expect("commit");
        }

        let pairs = glommio_run({
            let metadata = metadata.clone();
            move || async move {
                let src: RedbRebuildSource<{ VECTOR_DIM }> = RedbRebuildSource::new(metadata);
                src.snapshot_vectors().await
            }
        })
        .expect("snapshot_vectors");

        assert_eq!(pairs.len(), 2, "only the two live vector-bearing memories");
        let (_m1, v1) = pairs
            .iter()
            .find(|(m, _)| m.slot() == 1)
            .expect("slot 1 present");
        assert!((v1[0] - 0.25).abs() < f32::EPSILON);
        let (_m2, v2) = pairs
            .iter()
            .find(|(m, _)| m.slot() == 2)
            .expect("slot 2 present");
        assert!((v2[0] - 0.5).abs() < f32::EPSILON);
        assert!(
            pairs.iter().all(|(m, _)| m.slot() != 3),
            "tombstoned memory must be excluded"
        );
        assert!(
            pairs.iter().all(|(m, _)| m.slot() != 4),
            "vectorless memory must be excluded"
        );
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
                    // Watermark seeded past the WAL tail so this
                    // bundle-completeness test exercises the normal
                    // (all-committed) checkpoint path.
                    {
                        let wm = RedbCommittedWatermark::new();
                        wm.advance_to(u64::MAX);
                        wm
                    },
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
                    RedbCommittedWatermark::new(),
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

    // ---- HIGH-1: snapshot-id resume across restart ------------------------

    #[test]
    fn scan_max_snapshot_id_handles_missing_empty_and_populated() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("snapshots");
        // Missing dir → 0.
        assert_eq!(scan_max_snapshot_id(&root), 0);
        // Empty dir → 0.
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(scan_max_snapshot_id(&root), 0);
        // Populated {1,2,3} using the 20-digit names take_snapshot writes,
        // plus a stray non-numeric entry that must be ignored.
        for id in [1u64, 2, 3] {
            std::fs::create_dir_all(root.join(format!("{id:020}"))).unwrap();
        }
        std::fs::create_dir_all(root.join("not-a-snapshot")).unwrap();
        assert_eq!(scan_max_snapshot_id(&root), 3);
    }

    /// After a restart the checkpoint counter must resume past the highest
    /// existing snapshot id — restarting at 1 would let the next cycle's
    /// `reflink_or_copy` truncate and overwrite bundle #1 (invariant #7).
    #[test]
    fn new_resumes_checkpoint_counter_past_existing_snapshots() {
        let tmp = TempDir::new().unwrap();
        let tmp_path = tmp.path().to_path_buf();
        let next_id = glommio_run(move || async move {
            let snapshots_root = tmp_path.join("snapshots");
            for id in [1u64, 2, 3] {
                std::fs::create_dir_all(snapshots_root.join(format!("{id:020}"))).unwrap();
            }
            let arena_path = tmp_path.join("arena.bin");
            let md_path = tmp_path.join("metadata.redb");
            let uuid: [u8; 16] = *uuid::Uuid::now_v7().as_bytes();
            let md = MetadataDb::open(&md_path).expect("MetadataDb::open");
            let metadata: SharedMetadataDb = Arc::new(md);
            let arena = ArenaFile::open(&arena_path, uuid, 8).expect("ArenaFile::open");
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
                RedbCommittedWatermark::new(),
            );
            // The next allocated checkpoint id is 4 → bundle #3 is safe.
            src.next_ckpt_id()
        });
        assert_eq!(next_id, 4);
    }

    // ---- CORE-DURABILITY: checkpoint durable_lsn vs redb-committed watermark

    /// A minimal Encode WAL record for a given arena slot. Mirrors the
    /// storage crate's recovery-test fixture: recovery decodes + applies
    /// it, so it counts toward `records_replayed` / `applied()`.
    fn encode_record(slot: u64) -> WalRecord {
        use brain_core::{MemoryId, MemoryKind, NamespaceId, SessionId, SpaceId};
        use brain_storage::wal::payload::{EncodePayload, WalPayload};

        let p = EncodePayload {
            memory_id: MemoryId::pack(1, slot, 1),
            request_id: [0u8; 16].into(),
            space_id: SpaceId::default(),
            namespace_id: NamespaceId::SYSTEM,
            session_id: SessionId(0),
            kind: MemoryKind::Episodic,
            salience_initial: 0.5,
            embedding_model_fp: [0xAB; 16],
            text: "hello".to_string(),
            vector: vec![0.5; VECTOR_DIM],
            edges: vec![],
            request_hash: [0; 32],
            response_payload: vec![],
            deduplicate: false,
            occurred_at_unix_nanos: None,
        };
        WalRecord::from_typed(
            Lsn(0),
            0,
            1_700_000_000_000_000_000,
            0xCAFE,
            &WalPayload::Encode(p),
        )
    }

    /// Append `n_data` Encode records (LSN `1..=n_data`), set the
    /// redb-committed watermark to `watermark`, take exactly one
    /// snapshot, then drain the WAL. Returns `(tempdir, wal_dir, uuid,
    /// arena_path)` for a subsequent recovery pass. The tempdir is
    /// returned so the caller keeps it alive.
    fn snapshot_with_watermark(
        n_data: u64,
        watermark: u64,
    ) -> (TempDir, std::path::PathBuf, [u8; 16], std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        let arena_path = base.join("arena.bin");
        let md_path = base.join("metadata.redb");
        let wal_dir = base.join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();
        let uuid: [u8; 16] = *uuid::Uuid::now_v7().as_bytes();
        let md = MetadataDb::open(&md_path).expect("MetadataDb::open");
        let metadata: SharedMetadataDb = Arc::new(md);

        let wm = RedbCommittedWatermark::new();
        wm.advance_to(watermark);

        {
            let arena_path = arena_path.clone();
            let md_path = md_path.clone();
            let wal_dir = wal_dir.clone();
            let snap_root = base.join("snapshots");
            let metadata = metadata.clone();
            glommio_run(move || async move {
                let arena = ArenaFile::open(&arena_path, uuid, 16).expect("ArenaFile::open");
                let arena_cell = Rc::new(RefCell::new(arena));
                let wal = Wal::create_with_config(&wal_dir, uuid, WalConfig::default())
                    .await
                    .expect("Wal::create_with_config");
                for slot in 0..n_data {
                    wal.append(encode_record(slot)).await.expect("wal append");
                }
                let wal_cell = Rc::new(RefCell::new(Some(wal)));
                let (hnsw, _w) =
                    brain_index::SharedHnsw::new(brain_index::IndexParams::default_v1()).unwrap();
                let src = ShardSnapshotSource::new(
                    uuid,
                    snap_root,
                    arena_path.clone(),
                    md_path,
                    arena_cell,
                    wal_cell.clone(),
                    metadata,
                    hnsw,
                    wm,
                );
                src.take_snapshot().await.expect("take_snapshot");
                let mut g = wal_cell.borrow_mut();
                if let Some(w) = g.take() {
                    w.shutdown().await.expect("Wal::shutdown");
                }
            });
        }
        (tmp, wal_dir, uuid, arena_path)
    }

    /// The gap: three writes are WAL-durable (LSN 1,2,3) but only the
    /// first has been committed to redb (watermark = 1). The periodic
    /// snapshot stamps CHECKPOINT_END while records 2 and 3 are still
    /// uncommitted. The checkpoint's `durable_lsn` MUST be the
    /// redb-committed watermark (1), NOT the WAL tail — otherwise a
    /// crash-then-recovery would skip LSN 2 and 3 and silently drop
    /// WAL-durable data.
    #[test]
    fn checkpoint_durable_lsn_is_redb_watermark_not_wal_tail() {
        use brain_storage::recovery::MetadataSink;
        let (_tmp, wal_dir, uuid, arena_path) = snapshot_with_watermark(3, 1);

        // Pass 1 (fresh sink): reads the CHECKPOINT_END record, which
        // carries durable_lsn = watermark.
        let mut arena = ArenaFile::open(&arena_path, uuid, 16).expect("reopen arena");
        let mut sink = brain_storage::recovery::InMemoryMetadataSink::new();
        let (report1, _alloc) =
            brain_storage::recovery::recover(&mut arena, &wal_dir, uuid, &mut sink)
                .expect("recover pass 1");
        assert_eq!(
            sink.durable_lsn(),
            1,
            "CHECKPOINT_END.durable_lsn must equal the redb-committed watermark (1), \
             not the WAL tail",
        );
        assert!(
            sink.durable_lsn() < report1.next_lsn.saturating_sub(1),
            "durable_lsn ({}) must be strictly below the WAL tail (next_lsn-1 = {})",
            sink.durable_lsn(),
            report1.next_lsn.saturating_sub(1),
        );

        // Pass 2 (sink seeded with the checkpoint's durable_lsn): proves
        // recovery REPLAYS the WAL-durable-but-redb-uncommitted records
        // rather than skipping them.
        let mut arena2 = ArenaFile::open(&arena_path, uuid, 16).expect("reopen arena 2");
        let mut sink2 =
            brain_storage::recovery::InMemoryMetadataSink::with_durable_lsn(sink.durable_lsn());
        let (report2, _alloc2) =
            brain_storage::recovery::recover(&mut arena2, &wal_dir, uuid, &mut sink2)
                .expect("recover pass 2");
        assert!(
            sink2.applied().contains_key(&2),
            "LSN 2 (WAL-durable, redb-uncommitted) must be REPLAYED",
        );
        assert!(
            sink2.applied().contains_key(&3),
            "LSN 3 (WAL-durable, redb-uncommitted) must be REPLAYED",
        );
        assert_eq!(
            report2.records_skipped, 1,
            "only LSN 1 (at/below the watermark) may be skipped",
        );
    }

    /// Normal case: every write has committed to redb (watermark past
    /// the WAL tail). The checkpoint then advances `durable_lsn` to the
    /// latest LSN (clamped to the WAL tail sampled at CHECKPOINT_BEGIN),
    /// so recovery correctly skips the whole already-durable prefix.
    #[test]
    fn checkpoint_durable_lsn_advances_when_all_writes_committed() {
        use brain_storage::recovery::MetadataSink;
        // watermark = u64::MAX → "everything committed"; durable_lsn is
        // clamped to the WAL tail (the CHECKPOINT_BEGIN LSN, 4).
        let (_tmp, wal_dir, uuid, arena_path) = snapshot_with_watermark(3, u64::MAX);

        let mut arena = ArenaFile::open(&arena_path, uuid, 16).expect("reopen arena");
        let mut sink = brain_storage::recovery::InMemoryMetadataSink::new();
        brain_storage::recovery::recover(&mut arena, &wal_dir, uuid, &mut sink).expect("recover");
        // Three data records (1,2,3) + CHECKPOINT_BEGIN (4); the tail
        // sampled at BEGIN is 4, so the checkpoint advances there.
        assert_eq!(
            sink.durable_lsn(),
            4,
            "with all writes committed the checkpoint advances durable_lsn to the WAL tail",
        );
    }

    /// One gap case: `n_data` Encode records are WAL-durable (LSN
    /// `1..=n_data`) but only the first `watermark` (`1 <= watermark <
    /// n_data`) have committed to redb. A snapshot stamps CHECKPOINT_END
    /// while records `watermark+1..=n_data` are still uncommitted.
    ///
    /// Asserts the watermark contract holds end-to-end:
    /// - the checkpoint's `durable_lsn` equals the redb-committed watermark
    ///   (`< n_data`, strictly below the WAL tail);
    /// - recovery seeded with that `durable_lsn` REPLAYS every
    ///   WAL-durable-but-redb-uncommitted record (`watermark+1..=n_data`)
    ///   and skips exactly the `watermark` truly-committed ones — never
    ///   silently dropping a WAL-durable record.
    fn assert_watermark_gap_replays(n_data: u64, watermark: u64) {
        use brain_storage::recovery::MetadataSink;
        assert!(
            (1..n_data).contains(&watermark),
            "gap case requires 1 <= watermark < n_data (got n_data={n_data}, watermark={watermark})",
        );
        let (_tmp, wal_dir, uuid, arena_path) = snapshot_with_watermark(n_data, watermark);

        // Pass 1 (fresh sink): CHECKPOINT_END carries durable_lsn = watermark.
        let mut arena = ArenaFile::open(&arena_path, uuid, 16).expect("reopen arena");
        let mut sink = brain_storage::recovery::InMemoryMetadataSink::new();
        let (report1, _alloc) =
            brain_storage::recovery::recover(&mut arena, &wal_dir, uuid, &mut sink)
                .expect("recover pass 1");
        assert_eq!(
            sink.durable_lsn(),
            watermark,
            "n_data={n_data}: durable_lsn must equal the redb-committed watermark",
        );
        assert!(
            sink.durable_lsn() < report1.next_lsn.saturating_sub(1),
            "n_data={n_data}: durable_lsn ({}) must be strictly below the WAL tail ({})",
            sink.durable_lsn(),
            report1.next_lsn.saturating_sub(1),
        );

        // Pass 2 (sink seeded with the checkpoint's durable_lsn): every
        // uncommitted record is replayed; exactly `watermark` are skipped.
        let mut arena2 = ArenaFile::open(&arena_path, uuid, 16).expect("reopen arena 2");
        let mut sink2 =
            brain_storage::recovery::InMemoryMetadataSink::with_durable_lsn(sink.durable_lsn());
        let (report2, _alloc2) =
            brain_storage::recovery::recover(&mut arena2, &wal_dir, uuid, &mut sink2)
                .expect("recover pass 2");
        for lsn in (watermark + 1)..=n_data {
            assert!(
                sink2.applied().contains_key(&lsn),
                "n_data={n_data}, watermark={watermark}: LSN {lsn} \
                 (WAL-durable, redb-uncommitted) must be REPLAYED",
            );
        }
        assert_eq!(
            report2.records_skipped, watermark,
            "n_data={n_data}, watermark={watermark}: only records at/below the \
             watermark may be skipped",
        );
    }

    /// The S5 scenario, swept across several deterministic watermark/tail
    /// gaps: for each `(n_data, watermark)` the checkpoint's durable_lsn is
    /// the redb-committed watermark and recovery replays every uncommitted
    /// record. Extends the single-case
    /// `checkpoint_durable_lsn_is_redb_watermark_not_wal_tail` with wider
    /// coverage of the gap size.
    #[test]
    fn checkpoint_watermark_gap_replays_uncommitted_records() {
        for (n_data, watermark) in [(3, 1), (4, 2), (4, 3), (5, 1), (5, 4), (6, 3), (8, 5)] {
            assert_watermark_gap_replays(n_data, watermark);
        }
    }

    /// Randomized watermark/tail gaps over a handful of deterministic seeds:
    /// the same replay-never-skip invariant must hold for arbitrary gap
    /// positions, not just the hand-picked ones above.
    #[test]
    fn checkpoint_watermark_gap_replays_uncommitted_records_randomized() {
        // Deterministic xorshift64* — reproducible, no dependency.
        let mut state: u64 = 0x1234_5678_9ABC_DEF0;
        let mut next = || {
            let mut x = state;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            state = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        for _ in 0..10 {
            // n_data in 3..=9; watermark in 1..n_data.
            let n_data = 3 + next() % 7;
            let watermark = 1 + next() % (n_data - 1);
            assert_watermark_gap_replays(n_data, watermark);
        }
    }
}
