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
// 9.8: `shard.wal` is `Rc<RefCell<Option<Wal>>>`. The main loop's
// `AppendWalRecord` handler takes an *immutable* `borrow()` on the
// outer cell across `Wal::append(...).await`. The Phase-8 snapshot
// adapter also takes immutable borrows. The single-threaded Glommio
// executor + the discipline that the *only* `borrow_mut()` site is the
// shutdown path (after the scheduler has drained) guarantee no
// runtime panic. Without this allow, clippy's `await_holding_refcell_ref`
// rejects the per-shard refactor en masse.
#![allow(clippy::await_holding_refcell_ref)]

pub mod adapters;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use brain_core::{ShardId, SlotVersion};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::error::OpError;
use brain_ops::subscribe::EventEnvelope;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::request::RequestBody;
use brain_protocol::response::ResponseBody;
use brain_storage::arena::{
    AllocError, ArenaFile, ArenaOpenError, SlotAllocator, DEFAULT_INITIAL_CAPACITY_SLOTS,
};
use brain_storage::recovery::{recover, RecoveryError};
use brain_storage::wal::{Wal, WalConfig, WalError, WalRecord};
use brain_workers::cache_evict::CacheEvictionSource;
use brain_workers::hnsw_maint::RebuildSource;
use brain_workers::snapshot::SnapshotSource;
use brain_workers::wal_retention::WalRetentionSource;
use brain_workers::{
    AccessBoostWorker, CacheEvictionWorker, ConsolidationWorker, CounterReconcileWorker,
    DecayWorker, DisabledCacheEvictionSource, DisabledSummarizer, EdgeScrubWorker,
    HnswMaintenanceWorker, IdempotencyCleanupWorker, MetricsSnapshot, SlotReclamationWorker,
    SnapshotWorker, StatisticsUpdateWorker, Summarizer, WalRetentionWorker, WorkerKind,
    WorkerScheduler,
};

use self::adapters::{ArenaRebuildSource, ShardSnapshotSource, WalDirRetentionSource};
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
    /// Dispatch a wire `RequestBody` through `brain_ops::dispatch` and
    /// return the resulting `ResponseBody`. Added in 9.10 — the
    /// frame-dispatcher's primary boundary primitive.
    DispatchOp {
        req: Box<RequestBody>,
        reply_tx: Sender<Result<ResponseBody, OpError>>,
    },
    /// Append a pre-built record to the WAL. Returns the durable LSN.
    /// Stub op for 9.6 — 9.7's `RealWriterHandle` wraps the real
    /// encode/forget/link payload construction inside a higher-level op.
    AppendWalRecord {
        record: WalRecord,
        reply_tx: Sender<Result<u64, ShardOpError>>,
    },
    /// Snapshot every per-shard worker's metrics. Used by the admin
    /// `/metrics` endpoint (sub-task 9.13).
    SchedulerSnapshot {
        reply_tx: Sender<Vec<(&'static str, WorkerKind, MetricsSnapshot)>>,
    },
    /// Trigger a synchronous snapshot. Spec §14/06 §5; sub-task 10.9.
    /// The reply carries the snapshot id (mapped from
    /// `brain_workers::snapshot::SnapshotId.0`).
    TakeSnapshot {
        reply_tx: Sender<Result<u64, String>>,
    },
    /// List all on-disk snapshots for this shard. Sub-task 10.9.
    ListSnapshots {
        reply_tx: Sender<Result<Vec<SnapshotInfo>, String>>,
    },
    /// Delete a single snapshot by id. Sub-task 10.9.
    DeleteSnapshot {
        id: u64,
        reply_tx: Sender<Result<(), String>>,
    },
    /// Trigger an immediate HNSW rebuild on this shard.
    /// Sub-task 10.10.
    RebuildHnsw {
        reply_tx: Sender<Result<RebuildReport, String>>,
    },
    /// Snapshot the HNSW index counts. Used by the admin `/metrics`
    /// path to emit `brain_hnsw_*` families. Sub-task 12.8.
    HnswSnapshot { reply_tx: Sender<HnswCounts> },
    /// F-13: pause / resume / run-now a single background worker.
    /// Replies with `true` iff the named worker exists.
    WorkerControl {
        name: String,
        action: WorkerAction,
        reply_tx: Sender<bool>,
    },
}

/// F-13 action verbs for [`ShardRequest::WorkerControl`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerAction {
    /// Pause the worker. Loop keeps ticking but skips `run_cycle`.
    Pause,
    /// Resume a paused worker (kicks the wake channel so the next
    /// cycle runs immediately rather than waiting out the current
    /// sleep).
    Resume,
    /// Wake the worker now; run one cycle outside the schedule.
    RunNow,
}

/// Counts surfaced by `ShardRequest::HnswSnapshot`. Pure data type so
/// it crosses the Tokio↔Glommio boundary without further plumbing.
#[derive(Clone, Copy, Debug, Default)]
pub struct HnswCounts {
    pub node_count: u64,
    pub tombstone_count: u64,
}

impl HnswCounts {
    /// Tombstone ratio in `[0, 1]`. Returns 0 when `node_count == 0`.
    #[must_use]
    pub fn tombstone_ratio(self) -> f64 {
        if self.node_count == 0 {
            0.0
        } else {
            self.tombstone_count as f64 / self.node_count as f64
        }
    }
}

/// Owned snapshot descriptor surfaced through `ShardHandle`. Mirrors
/// `brain_workers::snapshot::SnapshotDesc` but with plain types so it
/// can cross the admin HTTP boundary unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub id: u64,
    pub taken_at_unix_nanos: u64,
    pub size_bytes: u64,
}

/// Report returned by `rebuild-ann`. Sub-task 10.10.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RebuildReport {
    /// Number of entries in the new index after rebuild.
    pub entries: usize,
    /// Wall-clock duration of the rebuild, in milliseconds.
    pub elapsed_ms: u64,
}

// ---------------------------------------------------------------------------
// Spawn config
// ---------------------------------------------------------------------------

/// `Debug` was dropped in 9.15 — `Arc<dyn Summarizer>` doesn't
/// implement `Debug`. Tests that previously printed the spawn
/// config can format individual fields directly.
#[derive(Clone)]
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
    /// Consolidation worker's Summarizer (sub-task 9.15). Defaults to
    /// [`DisabledSummarizer`] so existing tests + non-LLM deployments
    /// keep working. `main.rs::linux_main::run` injects an LLM-backed
    /// impl when `cfg.summarizer.backend != Disabled`.
    pub summarizer: Arc<dyn Summarizer>,
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
            summarizer: Arc::new(DisabledSummarizer),
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

    #[error("snapshot operation failed: {0}")]
    Snapshot(String),

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

    #[error("LLM cache open failed: {0}")]
    LlmCache(#[from] brain_metadata::LlmCacheError),
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
    /// Cross-shard event-feed (sub-task 9.11). The shard's
    /// `fanout_task` drains `OpsContext::events` (brain-ops's
    /// in-process broadcast bus) and publishes each envelope through
    /// this channel. The connection layer's `SubscriptionRegistry`
    /// owns the single Receiver clone; per-subscription tasks
    /// observe events via a connection-side `tokio::sync::broadcast`
    /// bridge fed from this Receiver.
    events: Receiver<EventEnvelope>,
}

impl ShardHandle {
    #[must_use]
    pub fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    /// Per-shard event feed. Cloning the Receiver shares the underlying
    /// queue (flume Receivers are SPMC-safe); the connection layer
    /// typically clones once and bridges into a tokio `broadcast`.
    #[must_use]
    pub fn events(&self) -> Receiver<EventEnvelope> {
        self.events.clone()
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

    /// Snapshot every per-worker metric record. Returns
    /// `(name, kind, snapshot)` tuples in HashMap iteration order
    /// (not registration order). The admin `/metrics` endpoint reads
    /// this once per scrape.
    pub async fn scheduler_snapshot(
        &self,
    ) -> Result<Vec<(&'static str, WorkerKind, MetricsSnapshot)>, ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::SchedulerSnapshot { reply_tx })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)
    }

    /// Trigger a synchronous snapshot of this shard. Returns the
    /// snapshot's id on success. Spec §14/06 §5; sub-task 10.9.
    pub async fn take_snapshot(&self) -> Result<u64, ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::TakeSnapshot { reply_tx })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)?
            .map_err(ShardError::Snapshot)
    }

    /// List the snapshots persisted for this shard.
    pub async fn list_snapshots(&self) -> Result<Vec<SnapshotInfo>, ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::ListSnapshots { reply_tx })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)?
            .map_err(ShardError::Snapshot)
    }

    /// Delete a single snapshot by id.
    pub async fn delete_snapshot(&self, id: u64) -> Result<(), ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::DeleteSnapshot { id, reply_tx })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)?
            .map_err(ShardError::Snapshot)
    }

    /// Snapshot the HNSW index counts for this shard. Used by the
    /// admin `/metrics` exposition path (sub-task 12.8). Cheap; reads
    /// two atomics inside the shard executor.
    pub async fn hnsw_snapshot(&self) -> Result<HnswCounts, ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::HnswSnapshot { reply_tx })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)
    }

    /// F-13: pause / resume / run-now a named background worker on
    /// this shard. Returns `Ok(true)` iff the worker exists,
    /// `Ok(false)` if there's no such worker (caller should reply
    /// `404 unknown worker`).
    pub async fn worker_control(
        &self,
        name: String,
        action: WorkerAction,
    ) -> Result<bool, ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::WorkerControl {
                name,
                action,
                reply_tx,
            })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)
    }

    /// Trigger an immediate full HNSW rebuild. Returns the new
    /// entry count + elapsed time. Spec §14/06 §4; sub-task 10.10.
    pub async fn rebuild_hnsw(&self) -> Result<RebuildReport, ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::RebuildHnsw { reply_tx })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)?
            .map_err(ShardError::Snapshot)
    }

    /// Dispatch a fully-decoded wire request through the shard's
    /// `OpsContext`. Returns the wire `ResponseBody` (variant chosen by
    /// `brain_ops::dispatch`). Added in 9.10 as the frame-dispatcher's
    /// boundary primitive.
    pub async fn dispatch_op(&self, req: RequestBody) -> Result<ResponseBody, DispatchError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::DispatchOp {
                req: Box::new(req),
                reply_tx,
            })
            .await
            .map_err(|_| DispatchError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| DispatchError::ShardDisconnected)?
            .map_err(DispatchError::Op)
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

/// Caller-facing error for [`ShardHandle::dispatch_op`]. Either the
/// shard's request channel is closed (lifecycle) or `brain_ops::dispatch`
/// returned a structured `OpError` (op-time).
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("shard has shut down or is unreachable")]
    ShardDisconnected,
    #[error(transparent)]
    Op(#[from] OpError),
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
    /// The shard this joiner belongs to. Used by sub-task 9.14's
    /// `graceful_shutdown_shards` for per-shard timeout logging.
    #[must_use]
    pub fn shard_id(&self) -> ShardId {
        self.shard_id
    }

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
    /// `Rc<RefCell<…>>` so the shard's Phase-8 worker adapters
    /// (`ArenaRebuildSource`, `ShardSnapshotSource`) can hold an
    /// independent handle into the same on-disk arena. The main loop's
    /// `AllocSlot` handler borrows mutably for the duration of one
    /// `allocator.alloc(&mut arena)` call (no `.await` while held);
    /// adapters borrow immutably inside their futures. The single-
    /// threaded Glommio executor guarantees no concurrent borrows.
    arena: Rc<RefCell<ArenaFile>>,
    allocator: SlotAllocator,
    /// `Rc<RefCell<Option<Wal>>>` so `ShardSnapshotSource` can call
    /// `Wal::append` (via `write_checkpoint`) while the main loop also
    /// holds a handle. `Option` so the shutdown path can `.take()`
    /// before awaiting `Wal::shutdown` (which consumes the value).
    wal: Rc<RefCell<Option<Wal>>>,
    /// Per-shard OpsContext — embedder, index, metadata, writer.
    /// Constructed inside the executor in sub-task 9.7b.
    #[allow(dead_code)] // consumed by the frame dispatcher in sub-task 9.10
    ops: Arc<OpsContext>,
    /// Per-shard worker scheduler. `Option` so shutdown can `.take()`.
    scheduler: Option<WorkerScheduler>,
    /// Snapshot source for the admin HTTP routes (sub-task 10.9).
    /// Cloned from the same `Arc` the `SnapshotWorker` holds.
    snapshot_source: Arc<dyn SnapshotSource>,
    /// Rebuild source for the admin `rebuild-ann` route (sub-task
    /// 10.10). Same `Arc` the `HnswMaintenanceWorker` holds.
    rebuild_source: Arc<dyn RebuildSource<{ VECTOR_DIM }>>,
    /// The shared HNSW handle. `rebuild-ann` swaps a freshly-
    /// rebuilt index in via `SharedHnsw::swap()`.
    hnsw_shared: SharedHnsw<{ VECTOR_DIM }>,
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

/// Register every Phase-8 worker against `scheduler`. Sub-task 9.8
/// plugs in real adapters for `RebuildSource`, `WalRetentionSource`,
/// and `SnapshotSource`. `CacheEvictionSource` stays `Disabled*` until
/// 9.10 wires a real `CachingDispatcher` per shard. Sub-task 9.15
/// surfaces the `Summarizer` parameter so `main.rs` can inject an
/// OpenAI / Ollama adapter when configured.
fn register_phase8_workers(
    scheduler: &mut WorkerScheduler,
    ops: Arc<OpsContext>,
    rebuild_source: Arc<dyn RebuildSource<{ VECTOR_DIM }>>,
    wal_retention_source: Arc<dyn WalRetentionSource>,
    snapshot_source: Arc<dyn SnapshotSource>,
    cache_eviction_source: Arc<dyn CacheEvictionSource>,
    summarizer: Arc<dyn Summarizer>,
) -> Result<(), brain_workers::WorkerError> {
    scheduler.register(Arc::new(AccessBoostWorker::new()), ops.clone())?;
    scheduler.register(Arc::new(DecayWorker::new()), ops.clone())?;
    scheduler.register(Arc::new(ConsolidationWorker::new(summarizer)), ops.clone())?;
    scheduler.register(
        Arc::new(HnswMaintenanceWorker::new(rebuild_source)),
        ops.clone(),
    )?;
    scheduler.register(Arc::new(IdempotencyCleanupWorker::new()), ops.clone())?;
    scheduler.register(Arc::new(EdgeScrubWorker::new()), ops.clone())?;
    scheduler.register(Arc::new(SlotReclamationWorker::new()), ops.clone())?;
    scheduler.register(Arc::new(StatisticsUpdateWorker::new()), ops.clone())?;
    scheduler.register(Arc::new(CounterReconcileWorker::new()), ops.clone())?;
    scheduler.register(
        Arc::new(CacheEvictionWorker::new(cache_eviction_source)),
        ops.clone(),
    )?;
    scheduler.register(
        Arc::new(WalRetentionWorker::new(wal_retention_source)),
        ops.clone(),
    )?;
    scheduler.register(Arc::new(SnapshotWorker::new(snapshot_source)), ops)?;
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
    //
    // `ensure_dirs` (sub-task 15.3) mkdir-p's the shard root, `wal/`, and
    // the knowledge-layer tantivy directories. It's idempotent over
    // existing substrate shards (no-op when present) and bootstraps fresh
    // ones. Knowledge-layer *files* (entity.hnsw, statement.hnsw,
    // llm_cache.redb) are created lazily by their owning modules.
    let dir = cfg.data_dir.join(shard_id.to_string());
    brain_storage::ensure_dirs(&dir).map_err(|e| ShardError::dir_create(dir.clone(), e))?;
    let paths = brain_storage::ShardPaths::at(&dir);

    // ---- 2. UUID (generate or read existing) -------------------------------
    let uuid_path = paths.shard_uuid();
    let shard_uuid = read_or_generate_uuid(&uuid_path)?;

    // ---- 3. Arena open / create -------------------------------------------
    let arena_path = paths.arena();
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
    let metadata_path = paths.metadata_db();
    let mut metadata_db = MetadataDb::open(&metadata_path)?;

    // Sub-task 15.4: open (or create) the LLM extractor cache file
    // alongside metadata.redb. The file persists; the handle is dropped
    // at function exit and phase 21 (LLM extractor) re-opens / threads
    // it through the Glommio closure when it wires the cache writer.
    let _llm_cache = brain_metadata::LlmCacheDb::open(paths.llm_cache_db())?;

    let wal_dir = paths.wal_dir();
    // `ensure_dirs` above already created wal_dir; this assertion documents
    // the precondition for the segment scan below.
    debug_assert!(wal_dir.is_dir(), "ensure_dirs must have created wal/");
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
    // 9.11 cross-shard event feed. The Glommio closure spawns a
    // fanout_task that drains `ops.events` into this channel; the
    // connection layer reads the Receiver via `ShardHandle::events()`.
    let (events_tx, events_rx) = flume::bounded::<EventEnvelope>(1024);
    let placement = match cfg.pin_cpu {
        Some(cpu) => Placement::Fixed(cpu),
        None => Placement::Unbound,
    };
    let wal_config = cfg.wal_config;
    let summarizer = cfg.summarizer;
    let wal_dir_for_executor = wal_dir.clone();
    let arena_path_for_executor = arena_path.clone();
    let metadata_path_for_executor = metadata_path.clone();
    let snapshots_root_for_executor = dir.join("snapshots");
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
                ExecutorContext::new(dispatcher, hnsw_shared.clone(), metadata.clone(), writer);

            // Phase 20.7: materialise the persisted `EXTRACTORS_TABLE`
            // rows (seeded by the system-schema bootstrap at
            // MetadataDb::open) into a runtime ExtractorRegistry.
            // The classifier model is unloaded in phase 20.7;
            // operator-provided weights via BRAIN_NER_MODEL_PATH
            // are wired in phase 20.7b.
            let extractor_registry = {
                let db_guard = metadata.lock();
                let rtxn = db_guard
                    .read_txn()
                    .expect("read_txn after MetadataDb::open");
                let defs = brain_metadata::extractor_list(&rtxn)
                    .expect("extractor_list at shard startup");
                drop(rtxn);
                drop(db_guard);

                let (reg, errors) =
                    brain_extractors::build_registry_from_definitions(&defs, None);
                for (id, err) in errors {
                    tracing::warn!(
                        target: "brain_server::shard",
                        extractor_id = id.raw(),
                        error = %err,
                        "extractor materialise failed; skipping",
                    );
                }
                reg
            };

            let classifier_config = match std::env::var("BRAIN_NER_MODEL_PATH") {
                Ok(p) => brain_extractors::ClassifierConfig::with_model_path(p.into()),
                Err(_) => brain_extractors::ClassifierConfig::unloaded(),
            };

            let ops = Arc::new(
                OpsContext::new(executor_ctx)
                    .with_extractor_registry(extractor_registry)
                    .with_classifier_config(classifier_config),
            );

            // Spawn the per-shard fanout task: drains the in-process
            // broadcast EventBus (`ops.events`) into the cross-shard
            // flume Sender we set up before entering the closure. The
            // connection layer reads the matching Receiver via
            // `ShardHandle::events()`. Spec audit §8.1.
            //
            // `tokio::sync::broadcast::Receiver` is runtime-agnostic
            // (atomics + Waker, no tokio I/O); polling its `recv()`
            // future inside Glommio is sound. `Lagged` is treated as
            // a transient skip — slow subscribers see gaps, not
            // crashes (spec §17.4).
            {
                let event_bus = ops.events.clone();
                let events_tx = events_tx.clone();
                glommio::spawn_local(async move {
                    let mut rx = event_bus.receiver();
                    loop {
                        match rx.recv().await {
                            Ok(env) => {
                                if events_tx.send_async(env).await.is_err() {
                                    // Connection layer dropped the Receiver
                                    // (e.g. server shutting down).
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                })
                .detach();
            }

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

            // Wrap arena + WAL in `Rc<RefCell<…>>` so adapters can share
            // handles with the main loop. Single-threaded executor →
            // sound; the discipline is "drop the borrow before .await".
            let arena_cell = Rc::new(RefCell::new(arena));
            let wal_cell = Rc::new(RefCell::new(Some(wal)));

            // Build real Phase-8 adapters (sub-task 9.8).
            let rebuild_source: Arc<dyn RebuildSource<{ VECTOR_DIM }>> = Arc::new(
                ArenaRebuildSource::<{ VECTOR_DIM }>::new(shard_id, arena_cell.clone()),
            );
            // Keep a clone for the admin `rebuild-ann` route (10.10).
            let rebuild_source_for_shard = rebuild_source.clone();
            let wal_retention_source: Arc<dyn WalRetentionSource> =
                Arc::new(WalDirRetentionSource::new(
                    wal_dir_for_executor.clone(),
                    shard_uuid,
                    metadata.clone(),
                ));
            let snapshot_source: Arc<dyn SnapshotSource> = Arc::new(ShardSnapshotSource::new(
                shard_uuid,
                snapshots_root_for_executor,
                arena_path_for_executor,
                metadata_path_for_executor,
                arena_cell.clone(),
                wal_cell.clone(),
                metadata.clone(),
                hnsw_shared.clone(),
            ));
            // CacheEvictionSource stays Disabled* until 9.10 wires the
            // real CachingDispatcher per shard.
            let cache_eviction_source: Arc<dyn CacheEvictionSource> =
                Arc::new(DisabledCacheEvictionSource);

            // Spawn the per-shard scheduler + register all 12 Phase-8 workers.
            let mut scheduler = WorkerScheduler::new();
            register_phase8_workers(
                &mut scheduler,
                ops.clone(),
                rebuild_source,
                wal_retention_source,
                snapshot_source.clone(),
                cache_eviction_source,
                summarizer,
            )
            .expect("register Phase-8 workers");
            info!(
                shard_id,
                workers = scheduler.len(),
                "per-shard scheduler online"
            );

            let shard = Shard {
                shard_id,
                arena: arena_cell,
                allocator,
                wal: wal_cell,
                ops,
                scheduler: Some(scheduler),
                snapshot_source,
                rebuild_source: rebuild_source_for_shard,
                hnsw_shared,
            };
            shard_main_loop(shard, rx).await;
        })
        .map_err(|e| ShardError::Spawn(e.to_string()))?;
    let handle = ShardHandle {
        shard_id,
        tx,
        events: events_rx,
    };
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
                // Borrow mutably for the duration of the (synchronous)
                // allocator call. The borrow is dropped before the
                // following `.await`, satisfying the RefCell discipline.
                let out = {
                    let mut arena = shard.arena.borrow_mut();
                    shard
                        .allocator
                        .alloc(&mut arena)
                        .map_err(ShardOpError::from)
                };
                if reply_tx.send_async(out).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "AllocSlot reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::SchedulerSnapshot { reply_tx } => {
                let snap = shard
                    .scheduler
                    .as_ref()
                    .map(|s| s.metrics_snapshot())
                    .unwrap_or_default();
                if reply_tx.send_async(snap).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "SchedulerSnapshot reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::TakeSnapshot { reply_tx } => {
                let result = match shard.snapshot_source.take_snapshot().await {
                    Ok(id) => Ok(id.0),
                    Err(e) => Err(e.to_string()),
                };
                if reply_tx.send_async(result).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "TakeSnapshot reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::ListSnapshots { reply_tx } => {
                let result = match shard.snapshot_source.list_snapshots().await {
                    Ok(descs) => Ok(descs
                        .into_iter()
                        .map(|d| SnapshotInfo {
                            id: d.id.0,
                            taken_at_unix_nanos: d.taken_at_unix_nanos,
                            size_bytes: d.size_bytes,
                        })
                        .collect()),
                    Err(e) => Err(e.to_string()),
                };
                if reply_tx.send_async(result).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "ListSnapshots reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::DeleteSnapshot { id, reply_tx } => {
                let result = match shard
                    .snapshot_source
                    .delete_snapshot(brain_workers::snapshot::SnapshotId(id))
                    .await
                {
                    Ok(()) => Ok(()),
                    Err(e) => Err(e.to_string()),
                };
                if reply_tx.send_async(result).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "DeleteSnapshot reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::HnswSnapshot { reply_tx } => {
                let counts = HnswCounts {
                    node_count: shard.hnsw_shared.len() as u64,
                    tombstone_count: shard.hnsw_shared.tombstone_count() as u64,
                };
                if reply_tx.send_async(counts).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "HnswSnapshot reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::WorkerControl {
                name,
                action,
                reply_tx,
            } => {
                let applied = match &shard.scheduler {
                    Some(scheduler) => match action {
                        WorkerAction::Pause => scheduler.pause(&name),
                        WorkerAction::Resume => scheduler.resume(&name),
                        WorkerAction::RunNow => scheduler.run_now(&name),
                    },
                    None => false,
                };
                if reply_tx.send_async(applied).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "WorkerControl reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::RebuildHnsw { reply_tx } => {
                let start = std::time::Instant::now();
                let result = match shard.rebuild_source.snapshot_vectors().await {
                    Ok(vectors) => {
                        let params = shard.hnsw_shared.params();
                        match brain_index::HnswIndex::<{ VECTOR_DIM }>::rebuild(params, vectors) {
                            Ok((new_idx, _report)) => {
                                let entries = new_idx.len();
                                shard.hnsw_shared.swap(new_idx);
                                Ok(RebuildReport {
                                    entries,
                                    elapsed_ms: start.elapsed().as_millis() as u64,
                                })
                            }
                            Err(e) => Err(format!("rebuild: {e:?}")),
                        }
                    }
                    Err(e) => Err(format!("rebuild source: {e}")),
                };
                if reply_tx.send_async(result).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "RebuildHnsw reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::DispatchOp { req, reply_tx } => {
                // `brain_ops::dispatch` is async and runs entirely
                // within the per-shard Glommio executor: it touches
                // `OpsContext` (which is !Send post-9.7a) and yields
                // through Glommio-aware I/O. Awaiting here is sound —
                // the main loop is single-threaded and processes one
                // request at a time, the same shape as
                // `AppendWalRecord`.
                let out = brain_ops::dispatch::dispatch(*req, &shard.ops).await;
                if reply_tx.send_async(out).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "DispatchOp reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::AppendWalRecord { record, reply_tx } => {
                // `Wal::append` is itself async (group-commit), so we
                // can't hold the cell borrow across the `.await`. Clone
                // the inner `Wal` reference is impossible (Wal is !Clone);
                // instead, we keep `Wal` inside `Rc<RefCell<Option<…>>>`
                // and take a short borrow to capture an `&Wal` pointer
                // that's owned by the `Rc`. The borrow stays alive for
                // the duration of one append, but releases on .await
                // suspension is unnecessary: a single executor task is
                // running at a time, so re-borrows can't race.
                // Implementation: just `borrow()` for the call window.
                let out = {
                    let wal_guard = shard.wal.borrow();
                    match wal_guard.as_ref() {
                        Some(wal) => {
                            // `wal.append` borrows `&self` (`Wal` uses
                            // interior mutability via RefCell<WalInner>),
                            // so holding the outer RefCell borrow is
                            // sound for the duration of the future.
                            // Single-threaded executor means no other
                            // task on this shard will reborrow.
                            wal.append(record)
                                .await
                                .map(|lsn| lsn.raw())
                                .map_err(ShardOpError::from)
                        }
                        None => Err(ShardOpError::Wal(WalError::DirectoryNotEmpty {
                            dir: std::path::PathBuf::new(),
                        })),
                    }
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
    // Take the WAL out of its cell. The scheduler is already drained
    // above, so any snapshot/retention adapter holding an `Rc` clone
    // of `shard.wal` has finished its last future and dropped its
    // borrow. `take()` is therefore safe.
    let wal = shard.wal.borrow_mut().take();
    if let Some(wal) = wal {
        if let Err(e) = wal.shutdown().await {
            warn!(
                shard_id = shard.shard_id,
                error = %e,
                "wal shutdown failed"
            );
        }
    }
    if let Err(e) = shard.arena.borrow().msync_all() {
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

    /// Sub-task 15.3 — spawning a shard must leave the knowledge-layer
    /// tantivy directories present on disk so phases 16/22 can open
    /// them without a separate mkdir step.
    #[test]
    fn spawn_creates_knowledge_directories() {
        let dir = TempDir::new().unwrap();
        let cfg = ShardSpawnConfig::new(dir.path());
        let (handle, joiner) = spawn_shard(3, cfg).expect("spawn");
        // Stop the executor before inspecting state so the test isn't
        // racing with shard startup.
        drop(handle);
        joiner.join().expect("shard should join cleanly");

        let shard_dir = dir.path().join("3");
        let paths = brain_storage::ShardPaths::at(&shard_dir);
        assert!(paths.wal_dir().is_dir(), "wal/ should exist");
        assert!(
            paths.statements_tantivy().is_dir(),
            "statements.tantivy/ should exist after spawn"
        );
        assert!(
            paths.memory_text_tantivy().is_dir(),
            "memory_text.tantivy/ should exist after spawn"
        );

        // Substrate files are present (arena + metadata + uuid).
        assert!(paths.arena().exists(), "arena.bin should exist");
        assert!(paths.metadata_db().exists(), "metadata.redb should exist");
        assert!(paths.shard_uuid().exists(), "shard.uuid should exist");

        // entity.hnsw / statement.hnsw — NOT created by spawn; owning
        // modules open them on demand in phases 16 / 17.
        assert!(
            !paths.entity_hnsw().exists(),
            "entity.hnsw is created by phase 16, not by spawn"
        );
        assert!(
            !paths.statement_hnsw().exists(),
            "statement.hnsw is created by phase 17, not by spawn"
        );

        // llm_cache.redb — IS created by spawn as of sub-task 15.4.
        assert!(
            paths.llm_cache_db().exists(),
            "llm_cache.redb should be created by spawn (sub-task 15.4)"
        );
    }
}
