//! Per-shard Glommio executor + on-disk arena.
//!
//! One OS thread per shard hosts a `glommio::LocalExecutor` (single-threaded,
//! io_uring-driven) that owns the shard's `ArenaFile` + `SlotAllocator`. The
//! Tokio connection layer talks to a shard through a `flume::Sender<ShardRequest>`;
//! replies come back through per-call `flume::Sender<...>` carried in the
//! request. Flume's `send_async` / `recv_async` are reactor-agnostic — both
//! ends `.await` natively under whichever runtime drives them.
//!
//! On-disk layout (spec §12/01 §2):
//!
//! ```text
//!   <data_dir>/<shard_id>/
//!     arena.bin       mmap'd by ArenaFile
//!     shard.uuid      16 raw bytes; generated once on first open
//! ```
//!
//! Lifecycle is a two-handle split:
//!
//! ```text
//!   spawn_shard() ─▶ (ShardHandle, ShardJoiner)
//!                       │              │
//!                       │              │  (single-ownership;
//!                       │              │   not cloneable)
//!                       ▼              ▼
//!                 clone freely;   used by graceful
//!                 each clone      shutdown to await
//!                 owns a Sender   the thread's exit
//!                       │
//!                       ▼  (drop every clone)
//!                 channel closes ─▶ shard_main_loop exits ─▶ joiner.join() returns
//! ```
//!
//! Spec §01/04 (layers), §01/05 (hardware: io_uring, CPU pinning),
//! §10/02 (single writer per shard), §12/01 §3 (shard UUID permanence).
//! Audit `phase-09-glommio-port.md` §7 locks flume as the boundary primitive;
//! §8.2 defers the in-shard `Rc<Cell<bool>>` shutdown flag to 9.7.

#![cfg(target_os = "linux")]
// OpsContext is intentionally `!Send + !Sync` post-9.7 (audit §4). The
// per-shard Glommio executor is the containment boundary; `Arc<OpsContext>`
// is used in the shard's main loop without crossing threads.
#![allow(clippy::arc_with_non_send_sync)]

use std::path::{Path, PathBuf};

use std::sync::Arc;

use brain_core::{ShardId, SlotVersion};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_storage::arena::{
    AllocError, ArenaFile, ArenaOpenError, SlotAllocator, DEFAULT_INITIAL_CAPACITY_SLOTS,
};
use brain_storage::recovery::{recover, RecoveryError};
use brain_storage::wal::{Wal, WalConfig, WalError, WalRecord};
use brain_workers::{
    AccessBoostWorker, CacheEvictionWorker, ConsolidationWorker, CounterReconcileWorker,
    DecayWorker, DisabledCacheEvictionSource, DisabledRebuildSource, DisabledSnapshotSource,
    DisabledSummarizer, DisabledWalRetentionSource, EdgeScrubWorker, HnswMaintenanceWorker,
    IdempotencyCleanupWorker, SlotReclamationWorker, SnapshotWorker, StatisticsUpdateWorker,
    WalRetentionWorker, WorkerScheduler,
};
use flume::{Receiver, Sender};
use glommio::{ExecutorJoinHandle, LocalExecutorBuilder, Placement};
use parking_lot::Mutex;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Request type — extended by 9.10 with `Frame { req, reply_tx }`.
// ---------------------------------------------------------------------------

pub(crate) enum ShardRequest {
    /// Trivial round-trip. The shard replies with `()`.
    Ping { reply_tx: Sender<()> },
    /// Allocate a fresh slot. Returns `(slot_idx, slot_version)`.
    AllocSlot {
        reply_tx: Sender<Result<(u64, SlotVersion), ShardOpError>>,
    },
    /// Append a pre-built record to the WAL. Returns the durable LSN.
    /// Stub op for 9.6 — 9.7's `RealWriterHandle` wraps the real
    /// encode/forget/link payload construction inside a higher-level op.
    AppendWalRecord {
        record: WalRecord,
        reply_tx: Sender<Result<u64, ShardOpError>>,
    },
}

// ---------------------------------------------------------------------------
// Spawn config
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ShardSpawnConfig {
    pub channel_capacity: usize,
    pub pin_cpu: Option<usize>,
    /// Root data directory. Per-shard subdir is `<data_dir>/<shard_id>/`.
    pub data_dir: PathBuf,
    /// Initial arena capacity in slots. The arena grows on demand via
    /// `ArenaFile::grow_to` (wired in a later sub-task).
    pub arena_initial_capacity_slots: u64,
    /// WAL configuration (group commit window, segment size limit, ...).
    pub wal_config: WalConfig,
}

impl ShardSpawnConfig {
    /// Construct with arena under `data_dir`, all other knobs defaulted.
    #[must_use]
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            channel_capacity: 1024,
            pin_cpu: None,
            data_dir: data_dir.into(),
            arena_initial_capacity_slots: DEFAULT_INITIAL_CAPACITY_SLOTS,
            wal_config: WalConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Spawn-time / lifecycle errors. Returned by `spawn_shard` and the
/// handle's send/recv helpers.
#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    #[error("shard has shut down or is unreachable")]
    ShardDisconnected,

    #[error("failed to launch Glommio executor: {0}")]
    Spawn(String),

    #[error("failed to join shard executor thread: {0}")]
    Join(String),

    #[error("failed to open arena: {0}")]
    ArenaOpen(#[from] ArenaOpenError),

    #[error("failed to create shard directory at {path}: {source}")]
    DirCreate {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read/write shard.uuid at {path}: {source}")]
    UuidFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("WAL recovery failed: {0}")]
    Recovery(#[from] RecoveryError),

    #[error("WAL init failed: {0}")]
    WalInit(#[from] WalError),

    #[error("metadata open failed: {0}")]
    MetadataOpen(#[from] brain_metadata::MetadataDbError),
}

impl ShardError {
    fn dir_create(path: PathBuf, source: std::io::Error) -> Self {
        Self::DirCreate { path, source }
    }
    fn uuid_file(path: PathBuf, source: std::io::Error) -> Self {
        Self::UuidFile { path, source }
    }
}

/// In-shard, op-time errors. Sent back through `reply_tx` for per-request
/// failures (vs. `ShardError` which is spawn-time). Future variants:
/// `MetadataConflict`, ...
#[derive(Debug, thiserror::Error)]
pub enum ShardOpError {
    #[error("arena allocation failed: {0}")]
    ArenaFull(#[from] AllocError),
    #[error("WAL append failed: {0}")]
    Wal(#[from] WalError),
}

// ---------------------------------------------------------------------------
// Public handle types
// ---------------------------------------------------------------------------

/// Cloneable, `Send + Sync` handle the connection layer (Tokio) holds.
/// Each clone holds a `flume::Sender`. When every clone drops, the
/// shard's request channel closes and the executor's main loop exits.
/// The thread itself is awaited through [`ShardJoiner::join`].
#[derive(Clone)]
pub struct ShardHandle {
    shard_id: ShardId,
    tx: Sender<ShardRequest>,
}

impl ShardHandle {
    #[must_use]
    pub fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    /// Round-trip Ping. Returns once the shard has replied.
    pub async fn ping(&self) -> Result<(), ShardError> {
        let (reply_tx, reply_rx) = flume::bounded::<()>(1);
        self.tx
            .send_async(ShardRequest::Ping { reply_tx })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        Ok(())
    }

    /// Ask the shard's allocator for a fresh slot. Returns the slot
    /// index and its version stamp.
    pub async fn alloc_slot(&self) -> Result<(u64, SlotVersion), AllocSlotError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::AllocSlot { reply_tx })
            .await
            .map_err(|_| AllocSlotError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| AllocSlotError::ShardDisconnected)?
            .map_err(AllocSlotError::Op)
    }

    /// Append a pre-built `WalRecord` to the shard's WAL. Returns the
    /// record's durable LSN once the kernel has acknowledged the fsync.
    pub async fn append_wal_record(&self, record: WalRecord) -> Result<u64, AppendWalError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::AppendWalRecord { record, reply_tx })
            .await
            .map_err(|_| AppendWalError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| AppendWalError::ShardDisconnected)?
            .map_err(AppendWalError::Op)
    }
}

/// Caller-facing error for [`ShardHandle::alloc_slot`]. Either the shard
/// is gone (lifecycle) or the allocator declined the request (op-time).
#[derive(Debug, thiserror::Error)]
pub enum AllocSlotError {
    #[error("shard has shut down or is unreachable")]
    ShardDisconnected,
    #[error(transparent)]
    Op(#[from] ShardOpError),
}

/// Caller-facing error for [`ShardHandle::append_wal_record`].
#[derive(Debug, thiserror::Error)]
pub enum AppendWalError {
    #[error("shard has shut down or is unreachable")]
    ShardDisconnected,
    #[error(transparent)]
    Op(#[from] ShardOpError),
}

/// One-shot ownership of the shard's OS thread. Returned alongside
/// [`ShardHandle`] from [`spawn_shard`]. Call [`ShardJoiner::join`] *after*
/// every `ShardHandle` clone has been dropped to wait for the executor
/// thread to exit cleanly. Forgetting to call `join()` leaks the thread.
pub struct ShardJoiner {
    shard_id: ShardId,
    handle: Option<ExecutorJoinHandle<()>>,
}

impl ShardJoiner {
    /// Block the current thread until the shard's executor exits.
    pub fn join(mut self) -> Result<(), ShardError> {
        let Some(h) = self.handle.take() else {
            return Ok(());
        };
        match h.join() {
            Ok(()) => {
                info!(shard_id = self.shard_id, "shard joined cleanly");
                Ok(())
            }
            Err(e) => Err(ShardError::Join(e.to_string())),
        }
    }
}

impl Drop for ShardJoiner {
    fn drop(&mut self) {
        if self.handle.is_some() {
            warn!(
                shard_id = self.shard_id,
                "ShardJoiner dropped without calling join(); thread will leak"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Per-shard owned state
// ---------------------------------------------------------------------------

struct Shard {
    shard_id: ShardId,
    arena: ArenaFile,
    allocator: SlotAllocator,
    /// Wrapped in `Option` so the main-loop shutdown path can `.take()`
    /// before awaiting `shutdown_in_place` — the borrow checker would
    /// otherwise prevent calling `&mut self`-incompatible async methods.
    wal: Option<Wal>,
    /// Per-shard OpsContext — embedder, index, metadata, writer.
    /// Constructed inside the executor in sub-task 9.7b.
    #[allow(dead_code)] // consumed by the frame dispatcher in sub-task 9.10
    ops: Arc<OpsContext>,
    /// Per-shard worker scheduler. `Option` so shutdown can `.take()`.
    scheduler: Option<WorkerScheduler>,
}

/// Stub dispatcher used until 9.10 wires the config-driven CpuDispatcher.
/// Returns zero vectors + an all-zero fingerprint — sufficient for the
/// 9.7b smoke tests (which don't exercise encode/recall correctness).
struct NopDispatcher;

impl Dispatcher for NopDispatcher {
    fn embed(&self, _: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok([0.0; VECTOR_DIM])
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0; 16]
    }
}

/// Register every Phase-8 worker against `scheduler`. Phase-8 seams
/// (Summarizer, RebuildSource, CacheEvictionSource, WalRetentionSource,
/// SnapshotSource) use the `Disabled*` defaults — real adapters land in
/// 9.8 / 9.15.
fn register_phase8_workers(
    scheduler: &mut WorkerScheduler,
    ops: Arc<OpsContext>,
) -> Result<(), brain_workers::WorkerError> {
    scheduler.register(Arc::new(AccessBoostWorker::new()), ops.clone())?;
    scheduler.register(Arc::new(DecayWorker::new()), ops.clone())?;
    scheduler.register(
        Arc::new(ConsolidationWorker::new(Arc::new(DisabledSummarizer))),
        ops.clone(),
    )?;
    scheduler.register(
        Arc::new(HnswMaintenanceWorker::new(Arc::new(DisabledRebuildSource))),
        ops.clone(),
    )?;
    scheduler.register(Arc::new(IdempotencyCleanupWorker::new()), ops.clone())?;
    scheduler.register(Arc::new(EdgeScrubWorker::new()), ops.clone())?;
    scheduler.register(Arc::new(SlotReclamationWorker::new()), ops.clone())?;
    scheduler.register(Arc::new(StatisticsUpdateWorker::new()), ops.clone())?;
    scheduler.register(Arc::new(CounterReconcileWorker::new()), ops.clone())?;
    scheduler.register(
        Arc::new(CacheEvictionWorker::new(Arc::new(
            DisabledCacheEvictionSource,
        ))),
        ops.clone(),
    )?;
    scheduler.register(
        Arc::new(WalRetentionWorker::new(Arc::new(
            DisabledWalRetentionSource,
        ))),
        ops.clone(),
    )?;
    scheduler.register(
        Arc::new(SnapshotWorker::new(Arc::new(DisabledSnapshotSource))),
        ops,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Public spawn entry point
// ---------------------------------------------------------------------------

/// Open the shard's data directory + arena, then launch its `LocalExecutor`
/// on a dedicated OS thread.
pub fn spawn_shard(
    shard_id: ShardId,
    cfg: ShardSpawnConfig,
) -> Result<(ShardHandle, ShardJoiner), ShardError> {
    // ---- 1. Directory layout ------------------------------------------------
    let dir = cfg.data_dir.join(shard_id.to_string());
    std::fs::create_dir_all(&dir).map_err(|e| ShardError::dir_create(dir.clone(), e))?;

    // ---- 2. UUID (generate or read existing) -------------------------------
    let uuid_path = dir.join("shard.uuid");
    let shard_uuid = read_or_generate_uuid(&uuid_path)?;

    // ---- 3. Arena open / create -------------------------------------------
    let arena_path = dir.join("arena.bin");
    let mut arena = ArenaFile::open(&arena_path, shard_uuid, cfg.arena_initial_capacity_slots)?;
    info!(
        shard_id,
        path = %arena_path.display(),
        capacity = arena.capacity_slots(),
        "arena opened"
    );

    // ---- 4. MetadataDb open + WAL recovery against the real sink ----------
    //
    // Per the audit, `recover()` is sync (mmap-based, reads only — io_uring
    // brings nothing). Sub-task 9.7b replaces 9.6's InMemoryMetadataSink
    // stand-in with the durable redb-backed MetadataDb.
    let metadata_path = dir.join("metadata.redb");
    let mut metadata_db = MetadataDb::open(&metadata_path)?;

    let wal_dir = dir.join("wal");
    std::fs::create_dir_all(&wal_dir).map_err(|e| ShardError::dir_create(wal_dir.clone(), e))?;
    let segments_present = wal_dir
        .read_dir()
        .map_err(|e| ShardError::dir_create(wal_dir.clone(), e))?
        .any(|entry| {
            entry
                .as_ref()
                .ok()
                .and_then(|e| e.path().extension().map(|s| s.to_owned()))
                .map(|ext| ext == "wal")
                .unwrap_or(false)
        });
    let next_lsn_after_recovery: u64;
    let allocator = if segments_present {
        let (report, alloc) = recover(&mut arena, &wal_dir, shard_uuid, &mut metadata_db)?;
        info!(
            shard_id,
            records_replayed = report.records_replayed,
            records_skipped = report.records_skipped,
            records_discarded = report.records_discarded,
            next_lsn = report.next_lsn,
            "WAL recovery complete"
        );
        next_lsn_after_recovery = report.next_lsn;
        alloc
    } else {
        next_lsn_after_recovery = 1;
        SlotAllocator::rebuild_from_arena(&arena)
    };
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(metadata_db));

    // ---- 5. Spawn the Glommio executor + build the rest of the stack -----
    let (tx, rx) = flume::bounded::<ShardRequest>(cfg.channel_capacity);
    let placement = match cfg.pin_cpu {
        Some(cpu) => Placement::Fixed(cpu),
        None => Placement::Unbound,
    };
    let wal_config = cfg.wal_config;
    let wal_dir_for_executor = wal_dir;
    let join_handle = LocalExecutorBuilder::new(placement)
        .name(&format!("brain-shard-{shard_id}"))
        .spawn(move || async move {
            // Build per-shard HNSW; tombstones rebuilt by HnswMaintenanceWorker.
            let (hnsw_shared, hnsw_writer) =
                SharedHnsw::<{ VECTOR_DIM }>::new(IndexParams::default_v1())
                    .expect("SharedHnsw::new");
            // Stub dispatcher — 9.10's frame dispatcher swaps in a real CpuDispatcher.
            let dispatcher: Arc<dyn Dispatcher> = Arc::new(NopDispatcher);
            // Per-shard writer wraps metadata + hnsw_writer.
            let writer: Arc<dyn WriterHandle> =
                Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
            let executor_ctx =
                ExecutorContext::new(dispatcher, hnsw_shared, metadata.clone(), writer);
            let ops = Arc::new(OpsContext::new(executor_ctx));

            // Open or create the WAL.
            let wal = if segments_present {
                Wal::open_existing(
                    &wal_dir_for_executor,
                    shard_uuid,
                    next_lsn_after_recovery,
                    wal_config,
                )
                .await
                .expect("Wal::open_existing (post-recovery)")
            } else {
                Wal::create_with_config(&wal_dir_for_executor, shard_uuid, wal_config)
                    .await
                    .expect("Wal::create_with_config")
            };

            // Spawn the per-shard scheduler + register all 12 Phase-8 workers.
            let mut scheduler = WorkerScheduler::new();
            register_phase8_workers(&mut scheduler, ops.clone()).expect("register Phase-8 workers");
            info!(
                shard_id,
                workers = scheduler.len(),
                "per-shard scheduler online"
            );

            let shard = Shard {
                shard_id,
                arena,
                allocator,
                wal: Some(wal),
                ops,
                scheduler: Some(scheduler),
            };
            shard_main_loop(shard, rx).await;
        })
        .map_err(|e| ShardError::Spawn(e.to_string()))?;
    let handle = ShardHandle { shard_id, tx };
    let joiner = ShardJoiner {
        shard_id,
        handle: Some(join_handle),
    };
    Ok((handle, joiner))
}

// ---------------------------------------------------------------------------
// Shard main loop
// ---------------------------------------------------------------------------

async fn shard_main_loop(mut shard: Shard, rx: Receiver<ShardRequest>) {
    info!(
        shard_id = shard.shard_id,
        "shard executor entering main loop"
    );
    while let Ok(req) = rx.recv_async().await {
        match req {
            ShardRequest::Ping { reply_tx } => {
                if reply_tx.send_async(()).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "Ping reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::AllocSlot { reply_tx } => {
                let out = shard
                    .allocator
                    .alloc(&mut shard.arena)
                    .map_err(ShardOpError::from);
                if reply_tx.send_async(out).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "AllocSlot reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::AppendWalRecord { record, reply_tx } => {
                let out = match shard.wal.as_ref() {
                    Some(wal) => wal
                        .append(record)
                        .await
                        .map(|lsn| lsn.raw())
                        .map_err(ShardOpError::from),
                    None => Err(ShardOpError::Wal(WalError::DirectoryNotEmpty {
                        dir: std::path::PathBuf::new(),
                    })),
                };
                if reply_tx.send_async(out).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "AppendWalRecord reply dropped (caller gone)"
                    );
                }
            }
        }
    }
    // Clean shutdown: drain worker scheduler → WAL committer → arena msync.
    // Order matters: workers may have in-flight WAL appends; let them drain
    // before closing the WAL so the fsync acks land.
    if let Some(scheduler) = shard.scheduler.take() {
        if let Err(e) = scheduler.shutdown().await {
            warn!(
                shard_id = shard.shard_id,
                error = %e,
                "scheduler shutdown failed"
            );
        }
    }
    if let Some(wal) = shard.wal.take() {
        if let Err(e) = wal.shutdown().await {
            warn!(
                shard_id = shard.shard_id,
                error = %e,
                "wal shutdown failed"
            );
        }
    }
    if let Err(e) = shard.arena.msync_all() {
        warn!(
            shard_id = shard.shard_id,
            error = %e,
            "msync_all at shutdown failed"
        );
    }
    info!(
        shard_id = shard.shard_id,
        "shard main loop exiting (channel closed)"
    );
}

// ---------------------------------------------------------------------------
// UUID helper
// ---------------------------------------------------------------------------

fn read_or_generate_uuid(path: &Path) -> Result<[u8; 16], ShardError> {
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() == 16 => {
            let mut out = [0u8; 16];
            out.copy_from_slice(&bytes);
            Ok(out)
        }
        Ok(other) => Err(ShardError::uuid_file(
            path.to_owned(),
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("shard.uuid expected 16 bytes, got {}", other.len()),
            ),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let uuid = uuid::Uuid::now_v7();
            let bytes = *uuid.as_bytes();
            std::fs::write(path, bytes)
                .map_err(|source| ShardError::uuid_file(path.to_owned(), source))?;
            Ok(bytes)
        }
        Err(source) => Err(ShardError::uuid_file(path.to_owned(), source)),
    }
}

// ---------------------------------------------------------------------------
// Compile-time invariants
// ---------------------------------------------------------------------------

const _: fn() = || {
    fn require_send_sync<T: Send + Sync>() {}
    require_send_sync::<ShardHandle>();
    require_send_sync::<Sender<ShardRequest>>();
    fn require_send<T: Send>() {}
    require_send::<ShardJoiner>();
};

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn shard_handle_is_send_sync_compile_check() {
        // Statically asserted above; this test exists so the file's
        // intent is discoverable from `cargo test` output.
    }

    #[test]
    fn shard_spawn_config_new_uses_arena_default_capacity() {
        let cfg = ShardSpawnConfig::new("/tmp/example");
        assert_eq!(cfg.channel_capacity, 1024);
        assert_eq!(cfg.pin_cpu, None);
        assert_eq!(
            cfg.arena_initial_capacity_slots,
            DEFAULT_INITIAL_CAPACITY_SLOTS
        );
    }

    #[test]
    fn read_or_generate_uuid_creates_file_when_absent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("shard.uuid");
        let uuid = read_or_generate_uuid(&path).expect("generate");
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk.len(), 16);
        assert_eq!(&on_disk[..], &uuid[..]);
    }

    #[test]
    fn read_or_generate_uuid_returns_existing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("shard.uuid");
        let canonical = [0xAB_u8; 16];
        std::fs::write(&path, canonical).unwrap();
        let uuid = read_or_generate_uuid(&path).expect("read existing");
        assert_eq!(uuid, canonical);
    }

    #[test]
    fn read_or_generate_uuid_rejects_short_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("shard.uuid");
        std::fs::write(&path, b"short").unwrap();
        let err = read_or_generate_uuid(&path).expect_err("should reject");
        assert!(matches!(err, ShardError::UuidFile { .. }));
    }

    #[test]
    fn spawn_unbound_and_join() {
        let dir = TempDir::new().unwrap();
        let cfg = ShardSpawnConfig::new(dir.path());
        let (handle, joiner) =
            spawn_shard(0, cfg).expect("Glommio spawn should succeed with Unbound placement");
        assert_eq!(handle.shard_id(), 0);
        drop(handle);
        joiner.join().expect("shard should join cleanly");
    }
}
