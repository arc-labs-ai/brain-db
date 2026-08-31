//! Per-shard Glommio executor + on-disk arena.
//!
//! One OS thread per shard hosts a `glommio::LocalExecutor` (single-threaded,
//! io_uring-driven) that owns the shard's `ArenaFile` + `SlotAllocator`. The
//! Tokio connection layer talks to a shard through a `flume::Sender<ShardRequest>`;
//! replies come back through per-call `flume::Sender<...>` carried in the
//! request. Flume's `send_async` / `recv_async` are reactor-agnostic — both
//! ends `.await` natively under whichever runtime drives them.
//!
//! On-disk layout:
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
//! flume is the boundary primitive between the connection layer and the
//! shard; a per-shard `Rc<Cell<bool>>` flag drives in-shard shutdown.

#![cfg(target_os = "linux")]
// OpsContext is intentionally `!Send + !Sync`. The
// per-shard Glommio executor is the containment boundary; `Arc<OpsContext>`
// is used in the shard's main loop without crossing threads.
#![allow(clippy::arc_with_non_send_sync)]
// `shard.wal` is `Rc<RefCell<Option<Wal>>>`. The main loop's
// `AppendWalRecord` handler takes an *immutable* `borrow()` on the
// outer cell across `Wal::append(...).await`. The snapshot
// adapter also takes immutable borrows. The single-threaded Glommio
// executor + the discipline that the *only* `borrow_mut()` site is the
// shutdown path (after the scheduler has drained) guarantee no
// runtime panic. Without this allow, clippy's `await_holding_refcell_ref`
// rejects the per-shard refactor en masse.
#![allow(clippy::await_holding_refcell_ref)]

pub mod adapters;
pub mod llm_setup;
pub mod rebuild;
pub mod restore;
pub mod snapshot_manifest;
pub mod tantivy_recovery;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use brain_core::{BackfillId, BackfillProgress, BackfillRequest, MemoryId, ShardId, SlotVersion};
use brain_embed::{Dispatcher, VECTOR_DIM};
use brain_index::entity_hnsw::{EntityHnswIndex, EntityHnswParams};
use brain_index::hype_hnsw::HypeHnswIndex;
use brain_index::statement_hnsw::{StatementHnswIndex, StatementHnswParams};
use brain_index::statement_question_hnsw::StatementQuestionHnswIndex;
use brain_index::{IndexParams, PendingEntry, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::error::OpError;
use brain_ops::subscribe::EventEnvelope;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{ForgetMode, RequestBody};
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
    AccessBoostWorker, AutoEdgeKnobs, AutoEdgeWorker, CacheEvictionWorker, ConsolidationWorker,
    CounterReconcileWorker, DecayWorker, DisabledCacheEvictionSource, DisabledSummarizer,
    EdgeScrubWorker, ExtractorKnobs, ExtractorWorker, HnswMaintenanceWorker,
    IdempotencyCleanupWorker, LlmCacheSweeper, MetricsSnapshot, SlotReclamationWorker,
    SnapshotWorker, StatisticsUpdateWorker, Summarizer, WalRetentionWorker, WorkerConfig,
    WorkerKind, WorkerScheduler,
};

use self::adapters::{
    ArenaRebuildSource, ArenaSpaceVectorSource, RedbRebuildSource, ShardSnapshotSource,
    WalDirRetentionSource,
};
use flume::{Receiver, Sender};
use glommio::{ExecutorJoinHandle, LocalExecutorBuilder, Placement};
use tracing::{error, info, warn, Instrument as _};

// ---------------------------------------------------------------------------
// Request type.
// ---------------------------------------------------------------------------

pub(crate) enum ShardRequest {
    /// Trivial round-trip. The shard replies with `()`.
    Ping { reply_tx: Sender<()> },
    /// Resolve a memory's embedding vector by id, space-walled to
    /// `space`. Used once at SUBSCRIBE registration to fetch the
    /// reference vector for a `similar_to` filter — a one-time,
    /// pure-data round-trip so the per-event filter never reaches into
    /// shard state. The reply is `Some([f32; VECTOR_DIM])` for a live
    /// memory owned by `space`, `None` when the memory is missing,
    /// tombstoned, stale (slot-version mismatch), or belongs to another
    /// space.
    GetMemoryVector {
        space: brain_core::SpaceId,
        memory_id: brain_core::MemoryId,
        reply_tx: Sender<Option<[f32; VECTOR_DIM]>>,
    },
    /// Allocate a fresh slot. Returns `(slot_idx, slot_version)`.
    AllocSlot {
        reply_tx: Sender<Result<(u64, SlotVersion), ShardOpError>>,
    },
    /// Dispatch a wire `RequestBody` through `brain_ops::dispatch` and
    /// return the resulting `ResponseBody`. The
    /// frame-dispatcher's primary boundary primitive.
    ///
    /// `caller` carries the authenticated space from the
    /// connection's `ConnPhase::Established.space`. The shard
    /// passes it to `brain_ops::dispatch`, which stamps it onto
    /// the per-request `ExecutorContext` so the writer-built Ops
    /// know who they belong to.
    DispatchOp {
        req: Box<RequestBody>,
        caller: brain_ops::RequestCaller,
        reply_tx: Sender<Result<brain_ops::DispatchOutcome, OpError>>,
        /// The connection-layer `client.request` span. `tracing::Span` is a
        /// `Send + Sync` handle, so it rides the channel unchanged; the shard
        /// re-enters it via `.instrument()` so the `brain.encode` span nests
        /// under it even though span context is thread-local and does not
        /// follow the Tokio→Glommio hop on its own.
        parent_span: tracing::Span,
    },
    /// Append a pre-built record to the WAL. Returns the durable LSN.
    /// Low-level op — `RealWriterHandle` wraps the real
    /// encode/forget/link payload construction inside a higher-level op.
    AppendWalRecord {
        record: WalRecord,
        reply_tx: Sender<Result<u64, ShardOpError>>,
    },
    /// Snapshot every per-shard worker's metrics. Used by the admin
    /// `/metrics` endpoint.
    SchedulerSnapshot {
        reply_tx: Sender<Vec<(&'static str, WorkerKind, MetricsSnapshot)>>,
    },
    /// Trigger a synchronous snapshot.
    /// The reply carries the snapshot id (mapped from
    /// `brain_workers::snapshot::SnapshotId.0`).
    TakeSnapshot {
        reply_tx: Sender<Result<u64, String>>,
    },
    /// List all on-disk snapshots for this shard.
    ListSnapshots {
        reply_tx: Sender<Result<Vec<SnapshotInfo>, String>>,
    },
    /// Delete a single snapshot by id.
    DeleteSnapshot {
        id: u64,
        reply_tx: Sender<Result<(), String>>,
    },
    /// Trigger an immediate memory-HNSW rebuild on this shard. Retained
    /// as the `/v1/rebuild-ann` back-compat path; equivalent to
    /// `RebuildIndex { target: MemoryHnsw }`.
    RebuildHnsw {
        reply_tx: Sender<Result<RebuildReport, String>>,
    },
    /// Rebuild a chosen derived index from authoritative redb state
    /// (admin `POST /v1/rebuild?index=<target>`). Runs on the shard
    /// executor and shares its implementation with the boot-recovery
    /// path (`shard::rebuild`).
    RebuildIndex {
        target: rebuild::RebuildTarget,
        reply_tx: Sender<Result<RebuildReport, String>>,
    },
    /// Snapshot the HNSW index counts. Used by the admin `/metrics`
    /// path to emit `brain_hnsw_*` families.
    HnswSnapshot { reply_tx: Sender<HnswCounts> },
    /// Sample the shard's on-disk storage footprint. Used by the
    /// admin `/metrics` path to emit `brain_wal_*`,
    /// `brain_metadata_size_bytes`, and `brain_arena_*` families. The
    /// handler does blocking `fs::metadata`, acceptable on the
    /// dedicated per-core thread at scrape cadence.
    StorageStats {
        reply_tx: Sender<StorageStatsSnapshot>,
    },
    /// Pause / resume / run-now a single background worker.
    /// Replies with `true` iff the named worker exists.
    WorkerControl {
        name: String,
        action: WorkerAction,
        reply_tx: Sender<bool>,
    },
    /// `EXTRACT_BACKFILL`: enqueue existing memories onto the
    /// per-shard ExtractorWorker channel for re-extraction. Operators
    /// drive this after a fresh schema upload or after enabling the
    /// worker on a populated shard.
    ExtractBackfill {
        selector: brain_protocol::BackfillSelector,
        reply_tx: Sender<Result<ExtractBackfillReport, String>>,
    },
    /// Submit a resumable backfill run to this shard's `BackfillWorker`
    /// (admin `POST /v1/backfill`). Distinct from `ExtractBackfill`,
    /// which is a one-shot synchronous re-enqueue: this drives the
    /// durable, checkpointed, cancellable worker. The `Err(String)`
    /// reply is returned when the worker isn't provisioned on this
    /// shard, surfaced to the HTTP layer as a 500.
    BackfillSubmit {
        request: BackfillRequest,
        reply_tx: Sender<Result<BackfillId, String>>,
    },
    /// Flag the in-flight resumable backfill run matching `id` for
    /// cancellation (admin `DELETE /v1/backfill/<id>`). Reply is
    /// `Ok(true)` iff a matching run was flagged.
    BackfillCancel {
        id: BackfillId,
        reply_tx: Sender<Result<bool, String>>,
    },
    /// Snapshot this shard's most-recent resumable backfill progress
    /// (admin `GET /v1/backfill`).
    BackfillProgressSnapshot {
        reply_tx: Sender<Result<BackfillProgress, String>>,
    },
    /// Auto-abort every Active txn owned by `connection_id`. Fanned out
    /// by the connection layer the moment a TCP/TLS connection drops
    /// before TXN_COMMIT. Reply carries the
    /// count of aborted entries for connection-layer logging; the
    /// individual `TxnId`s stay on the shard.
    AbortOrphanedTxns {
        connection_id: [u8; 16],
        reply_tx: Sender<usize>,
    },
    /// Restore (un-tombstone) a soft-forgotten memory on this shard
    /// (admin `POST /v1/memories/{id}/restore`). Validates the id + owning
    /// namespace, rejects a hard-forgotten or past-grace memory, submits a
    /// WAL-durable `Phase::RestoreMemory` through the shard writer, and
    /// (via the writer's post-commit fan-out) enqueues the FORGET-cascade
    /// revert. The admin handler fans this out to every shard; a non-owning
    /// shard reports `NotFound`. The `Err(String)` reply is a real failure
    /// (writer / metadata error), surfaced to HTTP as `500`.
    RestoreMemory {
        memory_id: brain_core::MemoryId,
        namespace: String,
        reply_tx: Sender<Result<brain_ops::AdminRestoreOutcome, String>>,
    },
    /// Query the historical audit tables (`GET /v1/audit`). Runs on the
    /// shard executor, which owns the `metadata.redb` handle. Reads one
    /// index page (`limit` rows, resuming after `cursor`) and returns
    /// the decoded rows plus the cursor to continue from. Deployment-wide
    /// operator surface — no tenant scoping (the admin token owns the
    /// deployment).
    AuditQuery {
        selector: AuditSelector,
        limit: usize,
        cursor: Option<AuditCursor>,
        reply_tx: Sender<Result<AuditPage, String>>,
    },
}

/// Per-shard counts surfaced by [`ShardRequest::ExtractBackfill`]. The
/// admin handler sums these across every shard before replying to the
/// CLI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExtractBackfillReport {
    /// Memories the handler successfully pushed onto the queue.
    pub enqueued: u64,
    /// Memories considered but not enqueued — channel full, missing
    /// text row, tombstoned, or (for `Memory(id)`) not found on this
    /// shard.
    pub skipped: u64,
}

/// Action verbs for [`ShardRequest::WorkerControl`].
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

/// On-disk storage footprint surfaced by
/// `ShardRequest::StorageStats`. Pure data type so it crosses the
/// Tokio↔Glommio boundary without further plumbing. All sizes are
/// sampled lazily on the scrape that asks for them — there is no
/// background tick.
#[derive(Clone, Copy, Debug, Default)]
pub struct StorageStatsSnapshot {
    /// Sum of bytes of every `wal/*.wal` segment file.
    pub wal_size_bytes: u64,
    /// Count of `wal/*.wal` segment files.
    pub wal_segments: u64,
    /// Byte size of the shard's `metadata.redb` file.
    pub metadata_size_bytes: u64,
    /// Addressable arena capacity in bytes (`capacity_slots * 1600`).
    pub arena_capacity_bytes: u64,
    /// Bytes backing currently-allocated slots (occupied + tombstoned).
    pub arena_used_bytes: u64,
    /// Currently-allocated slots (occupied + tombstoned).
    pub arena_slots_used: u64,
    /// Reclaimed slots sitting on the free list, ready to reuse.
    pub arena_slots_free: u64,
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

/// Report returned by `rebuild-ann`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RebuildReport {
    /// Number of entries in the new index after rebuild.
    pub entries: usize,
    /// Wall-clock duration of the rebuild, in milliseconds.
    pub elapsed_ms: u64,
}

/// Which audit index an audit-log query walks. Pure data — crosses the
/// Tokio↔Glommio channel unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditSelector {
    /// Extraction-audit rows for one memory (`EXTRACTOR_AUDIT_BY_MEMORY`).
    Memory([u8; 16]),
    /// Extraction-audit rows produced by one extractor
    /// (`EXTRACTOR_AUDIT_BY_EXTRACTOR`).
    Extractor(u32),
    /// Extraction-audit rows whose `started_at_unix_nanos` falls in
    /// `[since, until]` (`EXTRACTOR_AUDIT_BY_TIME`).
    Time { since: u64, until: u64 },
    /// Entity-resolution-audit rows whose `created_at_unix_nanos` falls in
    /// `[since, until]`. The resolution table has no secondary index, so
    /// the scan walks the primary key (UUIDv7 ≈ creation order) and
    /// filters on the window.
    Resolution { since: u64, until: u64 },
}

/// Opaque pagination position: the last index key returned in the prior
/// page. `ts` is the leading key component for the `Time` selector; it is
/// ignored for `Memory` / `Extractor` / `Resolution`, whose scans have a
/// fixed (or absent) leading component and resume on `audit_id` alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditCursor {
    pub ts: u64,
    pub audit_id: [u8; 16],
}

/// One page of audit rows plus the cursor to resume after the last row.
/// `next` is `Some` iff at least one further row exists past this page.
#[derive(Clone, Debug)]
pub enum AuditPage {
    Extraction {
        rows: Vec<brain_metadata::tables::audit::ExtractionAudit>,
        next: Option<AuditCursor>,
    },
    Resolution {
        rows: Vec<brain_metadata::tables::audit::ResolutionAudit>,
        next: Option<AuditCursor>,
    },
}

// ---------------------------------------------------------------------------
// Spawn config
// ---------------------------------------------------------------------------

/// `Debug` is not derived — `Arc<dyn Summarizer>` doesn't
/// implement `Debug`. Tests that previously printed the spawn
/// config can format individual fields directly.
#[derive(Clone)]
pub struct ShardSpawnConfig {
    pub channel_capacity: usize,
    pub pin_cpu: Option<usize>,
    /// Root data directory. Per-shard subdir is `<data_dir>/<shard_id>/`.
    pub data_dir: PathBuf,
    /// Initial arena capacity in slots. The arena grows on demand via
    /// `ArenaFile::grow_to` (not yet wired).
    pub arena_initial_capacity_slots: u64,
    /// WAL configuration (group commit window, segment size limit, ...).
    pub wal_config: WalConfig,
    /// Consolidation worker's Summarizer. Defaults to
    /// [`DisabledSummarizer`] so existing tests + non-LLM deployments
    /// keep working. `main.rs::linux_main::run` injects an LLM-backed
    /// impl when `cfg.summarizer.backend != Disabled`.
    pub summarizer: Arc<dyn Summarizer>,
    /// Per-shard auto-edge worker knobs. Defaults registered
    /// every shard with a 100 ms tick, top_k=5, threshold=0.85,
    /// channel cap 1024. Set `enabled=false` to skip registration
    /// entirely (no worker, no channel, encodes see a `None` sender).
    pub auto_edge: AutoEdgeSpawnConfig,
    /// Per-shard extractor pipeline knobs. Same shape as
    /// `auto_edge` — `enabled=false` skips registration entirely.
    pub extractor: ExtractorSpawnConfig,
    /// Per-shard temporal-edge worker knobs. Same shape as
    /// `auto_edge`; `enabled=false` skips registration entirely.
    pub temporal_edge: TemporalEdgeSpawnConfig,
    /// Per-shard causal-edge worker knobs. Extractor-driven;
    /// `enabled=false` skips registration entirely (no worker, no
    /// channel, the extractor's enqueue path stays `None`).
    pub causal_edge: CausalEdgeSpawnConfig,
    /// Retracted-statement GC worker knobs. Off by default.
    pub statement_reclaim: StatementReclaimSpawnConfig,
    /// Superseded-statement GC worker knobs. Off by default.
    pub supersession_sweeper: SupersessionSweeperSpawnConfig,
    /// Entity merge-review-queue sweeper cadence.
    pub ambiguity_resolver: AmbiguityResolverSpawnConfig,
    /// Statement confidence-refresh sweep cadence.
    pub confidence_sweep: ConfidenceSweepSpawnConfig,
    /// LLM extractor response-cache TTL sweep cadence.
    pub llm_cache_sweep: LlmCacheSweepSpawnConfig,
    /// Extractor pipeline tuning (resolver / classifier / HyPE).
    pub extractor_tuning: ExtractorTuningSpawnConfig,
    /// Index-pipeline tuning (tantivy commit cadence).
    pub index: IndexSpawnConfig,
    /// Text → vector dispatcher used inside the shard's executor.
    ///
    /// The same `Arc` is cloned into every shard at process startup so
    /// the ~130 MiB BERT weights are loaded once and shared across all
    /// N shards ("weights shared via `Arc<Model>`"). In
    /// production this is a `CachingDispatcher<CpuDispatcher>`; tests
    /// inject a file-local stub.
    pub dispatcher: Arc<dyn Dispatcher>,
    /// Operator gate on the cross-encoder rerank capability. Operator
    /// flips `enabled = false` to opt out — request-time opt-ins then
    /// surface as `CapabilityNotEnabled`. Enabled-but-fails-to-load is
    /// a hard spawn failure (see [`ShardError::CrossEncoderInitFailed`]).
    pub rerank: RerankSpawnConfig,
    /// Provider credentials / model overrides for the LLM extractor
    /// tier, ferried from `Config.llm`. Resolved env-first /
    /// config-fallback at shard spawn (`llm_setup::build_llm_deps`).
    pub llm: LlmSpawnConfig,
}

/// Knobs ferried from `Config.llm` into the spawn path: the single
/// provider credential + model id for the LLM extractor tier. Local to
/// the shard module (mirrors the other `*SpawnConfig` types) so the
/// `#[path]`-mounted integration tests don't pull in `crate::config`.
/// Empty / `None` fields fall back to the environment at resolution
/// time. The provider is derived from the model id, not configured.
#[derive(Clone, Debug, Default)]
pub struct LlmSpawnConfig {
    pub api_key: Option<String>,
    pub model: Option<String>,
}

/// Knobs ferried from `Config.rerank` into the spawn path.
///
/// The derived `Default` is `enabled: false` (model-free): the cross-encoder
/// needs an on-disk model an operator provides, and an enabled-but-unloadable
/// capability is a hard shard-spawn failure. Production overrides this
/// explicitly from `Config.rerank.enabled` (see main.rs); the default only
/// feeds `ShardSpawnConfig::new`, used by tests, so it must spawn without models.
#[derive(Clone, Debug, Default)]
pub struct RerankSpawnConfig {
    pub enabled: bool,
}

/// Knobs ferried from `Config.workers.auto_edge` into the spawn path.
/// Lives here (vs. server::config) so the shard crate doesn't have to
/// depend on the parent server crate's TOML wrapper.
#[derive(Clone, Debug)]
pub struct AutoEdgeSpawnConfig {
    pub enabled: bool,
    pub interval_ms: u64,
    pub batch_size: usize,
    pub similarity_threshold: f32,
    pub top_k: usize,
    pub ef_search: usize,
    pub channel_capacity: usize,
}

impl Default for AutoEdgeSpawnConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_ms: 100,
            batch_size: 256,
            similarity_threshold: 0.85,
            top_k: 5,
            ef_search: 64,
            channel_capacity: 1024,
        }
    }
}

/// Knobs ferried from `Config.workers.extractor` into the spawn path.
/// Tuning for the extraction pipeline worker. The worker is always
/// provisioned — extraction is a non-configurable, always-on capability
/// (like the embedder) — so there is no separate `enabled` knob here.
#[derive(Clone, Debug)]
pub struct ExtractorSpawnConfig {
    pub interval_ms: u64,
    pub drain_per_cycle: usize,
    pub llm_budget_per_cycle_micro_usd: u64,
    pub channel_capacity: usize,
    pub skip_already_extracted: bool,
    /// Memories the extractor worker batches into one classifier
    /// forward pass per cycle iteration.
    pub batch_size: usize,
}

impl Default for ExtractorSpawnConfig {
    fn default() -> Self {
        Self {
            interval_ms: 1000,
            drain_per_cycle: 32,
            llm_budget_per_cycle_micro_usd: 50_000,
            channel_capacity: 1024,
            skip_already_extracted: true,
            batch_size: brain_workers::DEFAULT_EXTRACTOR_BATCH_SIZE,
        }
    }
}

/// Knobs ferried from `Config.workers.temporal_edge` into the spawn
/// path. Mirrors `AutoEdgeSpawnConfig` but with temporal-specific
/// fields.
#[derive(Clone, Debug)]
pub struct TemporalEdgeSpawnConfig {
    pub enabled: bool,
    pub interval_ms: u64,
    pub batch_size: usize,
    pub window_seconds: u64,
    pub weight_min: f32,
    pub channel_capacity: usize,
    pub cross_session: bool,
    /// Cosine similarity floor for the topical gate. See
    /// [`brain_workers::TemporalEdgeKnobs::topical_threshold`].
    /// Ferried from `[workers.temporal_edge] topical_threshold`.
    pub topical_threshold: f32,
}

impl Default for TemporalEdgeSpawnConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_ms: 100,
            batch_size: 256,
            // 30 minutes: a 5-minute window fragmented a single
            // conversational session so consecutive turns never linked.
            window_seconds: 1800,
            weight_min: 0.1,
            channel_capacity: 1024,
            cross_session: false,
            topical_threshold: brain_workers::DEFAULT_TEMPORAL_EDGE_TOPICAL_THRESHOLD,
        }
    }
}

/// Knobs ferried from `Config.workers.causal_edge` into the spawn
/// path. Mirrors `TemporalEdgeSpawnConfig` but with causal-specific
/// fields (whitelist, per-statement fan-out caps, confidence floor).
#[derive(Clone, Debug)]
pub struct CausalEdgeSpawnConfig {
    pub enabled: bool,
    pub interval_ms: u64,
    pub batch_size: usize,
    pub min_confidence: f32,
    /// `(namespace, name)` pairs. Empty list → worker still spawns
    /// but produces no edges (no causal vocabulary).
    pub whitelist_qnames: Vec<(String, String)>,
    pub max_effect_memories_per_statement: usize,
    pub max_cause_memories_per_statement: usize,
    pub max_related_statements_per_entity: usize,
    pub channel_capacity: usize,
}

impl Default for CausalEdgeSpawnConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_ms: 200,
            batch_size: 64,
            min_confidence: 0.6,
            whitelist_qnames: brain_workers::DEFAULT_WHITELIST_QNAMES
                .iter()
                .map(|(ns, name)| ((*ns).to_owned(), (*name).to_owned()))
                .collect(),
            max_effect_memories_per_statement: brain_workers::DEFAULT_MAX_EFFECT_MEMORIES,
            max_cause_memories_per_statement: brain_workers::DEFAULT_MAX_CAUSE_MEMORIES,
            max_related_statements_per_entity: brain_workers::DEFAULT_MAX_RELATED_STATEMENTS,
            channel_capacity: 1024,
        }
    }
}

/// Knobs ferried from `Config.workers.statement_reclaim` into the spawn
/// path. The retracted-statement GC worker is off by default.
#[derive(Clone, Copy, Debug)]
pub struct StatementReclaimSpawnConfig {
    pub enabled: bool,
    pub grace_seconds: u64,
    pub period_seconds: u64,
}

impl Default for StatementReclaimSpawnConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            grace_seconds: brain_workers::workers::statement_reclaim::DEFAULT_GRACE_SECONDS,
            period_seconds: brain_workers::workers::statement_reclaim::DEFAULT_PERIOD_SECONDS,
        }
    }
}

/// Knobs ferried from `Config.workers.supersession_sweeper` into the
/// spawn path. The superseded-statement GC worker is off by default
/// (`retention_seconds == 0`): superseded rows are filtered from every
/// read regardless, so this only controls physical disk reclamation.
#[derive(Clone, Copy, Debug)]
pub struct SupersessionSweeperSpawnConfig {
    pub enabled: bool,
    pub retention_seconds: u64,
    pub period_seconds: u64,
    pub dry_run: bool,
}

impl Default for SupersessionSweeperSpawnConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            retention_seconds: 0,
            period_seconds: brain_workers::workers::supersession_sweeper::DEFAULT_PERIOD_SECONDS,
            dry_run: false,
        }
    }
}

/// Knobs ferried from `Config.workers.ambiguity_resolver`.
#[derive(Clone, Copy, Debug)]
pub struct AmbiguityResolverSpawnConfig {
    pub enabled: bool,
    pub interval_secs: u64,
}

impl Default for AmbiguityResolverSpawnConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: brain_workers::workers::ambiguity_resolver::DEFAULT_INTERVAL_SECS,
        }
    }
}

/// Knobs ferried from `Config.workers.confidence_sweep`.
#[derive(Clone, Copy, Debug)]
pub struct ConfidenceSweepSpawnConfig {
    pub enabled: bool,
    pub interval_secs: u64,
}

impl Default for ConfidenceSweepSpawnConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: brain_workers::workers::confidence_sweep::DEFAULT_INTERVAL_SECS,
        }
    }
}

/// Knobs ferried from `Config.workers.llm_cache_sweep`.
#[derive(Clone, Copy, Debug)]
pub struct LlmCacheSweepSpawnConfig {
    pub enabled: bool,
    pub interval_secs: u64,
}

impl Default for LlmCacheSweepSpawnConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: brain_workers::workers::llm_cache_sweeper::DEFAULT_INTERVAL_SECS,
        }
    }
}

/// Knobs ferried from `Config.extractors.resolver` + `.classifier` +
/// `.hype` into the spawn path. Tunes the extractor pipeline's
/// resolution / classification / HyPE behaviour.
#[derive(Clone, Debug)]
pub struct ExtractorTuningSpawnConfig {
    /// Tier-3 entity-resolution cosine floor.
    pub resolver_embed_threshold: f32,
    /// Explicit NER model directory override (`None` → XDG discovery).
    pub classifier_model_path: Option<String>,
    /// GLiNER post-sigmoid acceptance threshold.
    pub classifier_threshold: f32,
    /// Hypothetical questions generated per memory at write time.
    pub hype_num_questions: usize,
}

impl Default for ExtractorTuningSpawnConfig {
    fn default() -> Self {
        Self {
            resolver_embed_threshold: brain_extractors::resolver::EMBED_RESOLVE_THRESHOLD,
            classifier_model_path: None,
            classifier_threshold: brain_extractors::classifier::DEFAULT_GLINER_THRESHOLD,
            hype_num_questions: 6,
        }
    }
}

/// Knobs ferried from `Config.index` into the spawn path. Tantivy
/// group-commit cadence.
#[derive(Clone, Copy, Debug)]
pub struct IndexSpawnConfig {
    pub tantivy_commit_n: usize,
    pub tantivy_commit_ms: u64,
}

impl Default for IndexSpawnConfig {
    fn default() -> Self {
        Self {
            tantivy_commit_n: brain_ops::index::text_indexer::DEFAULT_COMMIT_N,
            tantivy_commit_ms: brain_ops::index::text_indexer::DEFAULT_COMMIT_MS,
        }
    }
}

impl ShardSpawnConfig {
    /// Construct with arena under `data_dir`, every other knob
    /// defaulted. The caller supplies the embedding `dispatcher`
    /// because a real `CpuDispatcher` requires a ~130 MiB model load
    /// that can't reasonably default; tests pass in their own stub.
    #[must_use]
    pub fn new(data_dir: impl Into<PathBuf>, dispatcher: Arc<dyn Dispatcher>) -> Self {
        Self {
            channel_capacity: 1024,
            pin_cpu: None,
            data_dir: data_dir.into(),
            arena_initial_capacity_slots: DEFAULT_INITIAL_CAPACITY_SLOTS,
            wal_config: WalConfig::default(),
            summarizer: Arc::new(DisabledSummarizer),
            auto_edge: AutoEdgeSpawnConfig::default(),
            extractor: ExtractorSpawnConfig::default(),
            temporal_edge: TemporalEdgeSpawnConfig::default(),
            causal_edge: CausalEdgeSpawnConfig::default(),
            statement_reclaim: StatementReclaimSpawnConfig::default(),
            supersession_sweeper: SupersessionSweeperSpawnConfig::default(),
            ambiguity_resolver: AmbiguityResolverSpawnConfig::default(),
            confidence_sweep: ConfidenceSweepSpawnConfig::default(),
            llm_cache_sweep: LlmCacheSweepSpawnConfig::default(),
            extractor_tuning: ExtractorTuningSpawnConfig::default(),
            index: IndexSpawnConfig::default(),
            dispatcher,
            rerank: RerankSpawnConfig::default(),
            llm: LlmSpawnConfig::default(),
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

    #[error("backfill control failed: {0}")]
    Backfill(String),

    #[error("audit query failed: {0}")]
    AuditQuery(String),

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

    /// Lexical retrieval is a core capability — a shard that can't
    /// open its tantivy indexes can't serve recalls correctly, so we
    /// refuse to spawn rather than degrade silently.
    #[error("tantivy init failed: {source}")]
    TantivyInitFailed {
        #[source]
        source: brain_index::TantivyShardError,
    },

    /// Snapshot-restore + rebuild on tantivy open failed. Same
    /// rationale as `TantivyInitFailed` — the spawn must abort so
    /// the operator sees the problem.
    #[error("tantivy recovery failed: {source}")]
    TantivyRecoveryFailed {
        #[source]
        source: crate::shard::tantivy_recovery::RecoveryError,
    },

    /// `TantivyLexicalRetriever::new` failed against an open
    /// `TantivyShard`. Treat the same as `TantivyInitFailed`: the
    /// shard can't serve recalls correctly, so spawn aborts.
    #[error("lexical retriever init failed: {source}")]
    LexicalRetrieverInitFailed {
        #[source]
        source: brain_index::LexicalError,
    },

    /// Operator left `rerank.enabled = true` (the default) but the
    /// cross-encoder model failed to load. We refuse to spawn rather
    /// than silently degrade — an opt-in rerank request against a
    /// silently-degraded shard would produce wrong results.
    #[error("cross-encoder init failed: {0}")]
    CrossEncoderInitFailed(String),

    /// An enabled extractor tier failed to initialise at shard spawn.
    /// Disabled-by-config tiers never raise this; only tiers the
    /// operator explicitly opted into.
    #[error("extractor tier \"{tier}\" init failed: {source}")]
    ExtractorInitFailed {
        tier: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
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
    /// Cross-shard event-feed. The shard's
    /// `fanout_task` drains `OpsContext::events` (brain-ops's
    /// in-process broadcast bus) and publishes each envelope through
    /// this channel. The connection layer's `SubscriptionRegistry`
    /// owns the single Receiver clone; per-subscription tasks
    /// observe events via a connection-side `tokio::sync::broadcast`
    /// bridge fed from this Receiver.
    events: Receiver<EventEnvelope>,
    /// Absolute path to this shard's WAL directory. Surfaced so the
    /// connection layer's subscribe-replay path (`run_subscription_task`'s
    /// replay prologue) can open a [`brain_storage::wal::reader::WalReader`]
    /// without round-tripping through the executor for every read.
    wal_dir: std::path::PathBuf,
    /// Shard UUID — required to validate WAL segment headers during
    /// subscribe-replay. Same value that's stamped in every WAL
    /// segment + arena slot.
    shard_uuid: [u8; 16],
    /// AutoEdgeWorker metrics shared with the writer for
    /// this shard. `None` when the worker is disabled in spawn
    /// config. The `/metrics` exposition reads this directly (no
    /// channel hop — the atomics are `Send + Sync` and the handle
    /// itself is shared by `Arc`).
    auto_edge_metrics: Option<Arc<brain_ops::AutoEdgeMetrics>>,
    /// ExtractorWorker metrics. Same shape as [`Self::auto_edge_metrics`].
    extractor_metrics: Option<Arc<brain_ops::ExtractorMetrics>>,
    /// TemporalEdgeWorker metrics. Same shape.
    temporal_edge_metrics: Option<Arc<brain_ops::TemporalEdgeMetrics>>,
    /// CausalEdgeWorker metrics. Same shape.
    causal_edge_metrics: Option<Arc<brain_ops::CausalEdgeMetrics>>,
    /// LLM cache sweeper metrics. `None` when the shard has no LLM
    /// cache configured (no API keys / lock contention at startup).
    llm_cache_sweep_metrics: Option<Arc<brain_ops::LlmCacheSweepMetrics>>,
    /// StatementEmbedWorker metrics. Always wired — the worker is
    /// unconditional. `/metrics` exposition reads this directly.
    statement_embed_metrics: Arc<brain_ops::StatementEmbedMetrics>,
    /// ConfidenceSweepWorker metrics. Always wired — the worker is
    /// unconditional (drains an empty STATEMENTS table on substrate-
    /// only shards). `/metrics` exposition reads this directly.
    confidence_sweep_metrics: Arc<brain_ops::ConfidenceSweepMetrics>,
    /// Read-path per-retriever metrics. Always wired — recall runs on
    /// every shard. Shared by `Arc` with the shard's `OpsContext`, which
    /// records into it after each `execute`; `/metrics` reads it here.
    retriever_metrics: Arc<brain_ops::RetrieverMetrics>,
    /// End-to-end RECALL (query) metrics. Same shared-by-`Arc` shape as
    /// [`Self::retriever_metrics`].
    query_metrics: Arc<brain_ops::QueryMetrics>,
}

impl ShardHandle {
    /// Test-only constructor: builds a `ShardHandle` around a caller-owned
    /// request channel so tests can observe the `ShardRequest`s the
    /// connection layer sends (e.g. the disconnect-time orphan-txn sweep)
    /// without spawning a real Glommio executor. The caller drains `tx`'s
    /// receiver to count / reply to requests.
    #[cfg(test)]
    pub(crate) fn new_for_test(shard_id: ShardId, tx: Sender<ShardRequest>) -> Self {
        let (_events_tx, events_rx) = flume::bounded::<EventEnvelope>(1);
        Self {
            shard_id,
            tx,
            events: events_rx,
            wal_dir: std::path::PathBuf::new(),
            shard_uuid: [0u8; 16],
            auto_edge_metrics: None,
            extractor_metrics: None,
            temporal_edge_metrics: None,
            causal_edge_metrics: None,
            llm_cache_sweep_metrics: None,
            statement_embed_metrics: Arc::new(brain_ops::StatementEmbedMetrics::new()),
            confidence_sweep_metrics: Arc::new(brain_ops::ConfidenceSweepMetrics::new()),
            retriever_metrics: Arc::new(brain_ops::RetrieverMetrics::new()),
            query_metrics: Arc::new(brain_ops::QueryMetrics::new()),
        }
    }

    #[must_use]
    pub fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    /// Whether this shard's executor loop is still draining requests.
    ///
    /// The request channel's receiver lives on the shard executor; if
    /// that thread exits (panic, leaked drain on a hung shutdown), the
    /// flume `Sender` reports `is_disconnected()` and no further request
    /// can ever be served. The readiness probe (`GET /readyz`) flips the
    /// node to `503` when any shard fails this check so a load balancer
    /// drains it instead of routing to a dead shard.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        !self.tx.is_disconnected()
    }

    /// Number of [`ShardRequest`]s queued on this shard's request
    /// channel but not yet drained by the executor loop — the
    /// dispatch-queue depth.
    ///
    /// Because the shard is single-writer (the executor drains the
    /// channel serially), this is the count of requests waiting their
    /// turn. It reads the flume channel length directly, without a
    /// round-trip through the executor, so it is safe to call from the
    /// admin (Tokio) side and cheap enough for a per-scrape gauge.
    #[must_use]
    pub fn queue_depth(&self) -> usize {
        self.tx.len()
    }

    /// Read-only handle to the AutoEdgeWorker metric
    /// state for this shard. `None` when the worker was disabled in
    /// spawn config (no-schema deployments / tests).
    #[must_use]
    pub fn auto_edge_metrics(&self) -> Option<Arc<brain_ops::AutoEdgeMetrics>> {
        self.auto_edge_metrics.clone()
    }

    /// Read-only handle to the ExtractorWorker metric
    /// state for this shard.
    #[must_use]
    pub fn extractor_metrics(&self) -> Option<Arc<brain_ops::ExtractorMetrics>> {
        self.extractor_metrics.clone()
    }

    /// Read-only handle to the TemporalEdgeWorker metric
    /// state for this shard.
    #[must_use]
    pub fn temporal_edge_metrics(&self) -> Option<Arc<brain_ops::TemporalEdgeMetrics>> {
        self.temporal_edge_metrics.clone()
    }

    /// Read-only handle to the CausalEdgeWorker metric
    /// state for this shard.
    #[must_use]
    pub fn causal_edge_metrics(&self) -> Option<Arc<brain_ops::CausalEdgeMetrics>> {
        self.causal_edge_metrics.clone()
    }

    /// Read-only handle to the LLM cache sweeper's metric state.
    /// `None` when no LLM cache was opened on this shard (no API
    /// keys, or another process held the redb lock).
    #[must_use]
    pub fn llm_cache_sweep_metrics(&self) -> Option<Arc<brain_ops::LlmCacheSweepMetrics>> {
        self.llm_cache_sweep_metrics.clone()
    }

    /// Read-only handle to the StatementEmbedWorker's metric state.
    /// Always wired — the worker is unconditional. `/metrics`
    /// exposition reads this directly.
    #[must_use]
    pub fn statement_embed_metrics(&self) -> Arc<brain_ops::StatementEmbedMetrics> {
        self.statement_embed_metrics.clone()
    }

    /// Read-only handle to the ConfidenceSweepWorker's metric state.
    /// Always wired — the worker is unconditional. `/metrics`
    /// exposition reads this directly.
    #[must_use]
    pub fn confidence_sweep_metrics(&self) -> Arc<brain_ops::ConfidenceSweepMetrics> {
        self.confidence_sweep_metrics.clone()
    }

    /// Read-only handle to the read-path per-retriever metric state.
    /// Always wired — recall runs on every shard. `/metrics` exposition
    /// reads this directly.
    #[must_use]
    pub fn retriever_metrics(&self) -> Arc<brain_ops::RetrieverMetrics> {
        self.retriever_metrics.clone()
    }

    /// Read-only handle to the end-to-end RECALL (query) metric state.
    /// Always wired. `/metrics` exposition reads this directly.
    #[must_use]
    pub fn query_metrics(&self) -> Arc<brain_ops::QueryMetrics> {
        self.query_metrics.clone()
    }

    /// Per-shard event feed. Cloning the Receiver shares the underlying
    /// queue (flume Receivers are SPMC-safe); the connection layer
    /// typically clones once and bridges into a tokio `broadcast`.
    #[must_use]
    pub fn events(&self) -> Receiver<EventEnvelope> {
        self.events.clone()
    }

    /// Absolute path to this shard's WAL directory. The connection
    /// layer's subscribe-replay opens a [`brain_storage::wal::reader::WalReader`]
    /// from this path to project records into events.
    #[must_use]
    pub fn wal_dir(&self) -> std::path::PathBuf {
        self.wal_dir.clone()
    }

    /// Shard UUID — used by the WAL reader to validate segment headers.
    #[must_use]
    pub fn shard_uuid(&self) -> [u8; 16] {
        self.shard_uuid
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

    /// Resolve a memory's embedding vector by id, space-walled to
    /// `space`. One-time round-trip used by SUBSCRIBE to fetch the
    /// reference vector for a `similar_to` filter. Returns `Ok(None)`
    /// when the memory is missing, tombstoned, stale, or owned by a
    /// different space; `Err` only if the shard is unreachable.
    pub async fn get_memory_vector(
        &self,
        space: brain_core::SpaceId,
        memory_id: brain_core::MemoryId,
    ) -> Result<Option<[f32; VECTOR_DIM]>, ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::GetMemoryVector {
                space,
                memory_id,
                reply_tx,
            })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)
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
    /// snapshot's id on success.
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
    /// admin `/metrics` exposition path. Cheap; reads
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

    /// Sample this shard's on-disk storage footprint for the admin
    /// `/metrics` exposition. The shard handler stats the WAL
    /// directory and `metadata.redb`, and reads the arena
    /// capacity / allocator occupancy in-process.
    pub async fn storage_stats(&self) -> Result<StorageStatsSnapshot, ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::StorageStats { reply_tx })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)
    }

    /// Pause / resume / run-now a named background worker on
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

    /// Enqueue existing memories on this shard for re-extraction.
    /// (extractor backfill). Returns the count of
    /// memories successfully pushed onto the per-shard ExtractorWorker
    /// channel along with the count that were considered but skipped
    /// (channel full, missing text row, tombstoned, not found).
    ///
    /// `Ok((0, 0))` for a `Memory(id)` selector whose id isn't on this
    /// shard is the contract — the admin handler fans the call out to
    /// every shard, and only the shard that owns the id will report a
    /// hit.
    pub async fn extract_backfill(
        &self,
        selector: brain_protocol::BackfillSelector,
    ) -> Result<ExtractBackfillReport, ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::ExtractBackfill { selector, reply_tx })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)?
            .map_err(ShardError::Snapshot)
    }

    /// Restore (un-tombstone) a soft-forgotten memory on this shard.
    /// Backs the admin `POST /v1/memories/{id}/restore` route. The admin
    /// handler fans this out to every shard; a shard that doesn't own the
    /// id reports [`brain_ops::AdminRestoreOutcome::NotFound`]. `Err` is a
    /// real per-shard failure (writer / metadata error).
    pub async fn restore_memory(
        &self,
        memory_id: brain_core::MemoryId,
        namespace: String,
    ) -> Result<brain_ops::AdminRestoreOutcome, ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::RestoreMemory {
                memory_id,
                namespace,
                reply_tx,
            })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)?
            .map_err(ShardError::Snapshot)
    }

    /// Submit a resumable backfill run to this shard's `BackfillWorker`
    /// and return its id. Backs the admin `POST /v1/backfill` route.
    /// Errors if the worker isn't provisioned on this shard.
    pub async fn backfill_submit(
        &self,
        request: BackfillRequest,
    ) -> Result<BackfillId, ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::BackfillSubmit { request, reply_tx })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)?
            .map_err(ShardError::Backfill)
    }

    /// Flag this shard's in-flight resumable backfill run matching `id`
    /// for cancellation. Backs the admin `DELETE /v1/backfill/<id>`
    /// route. Returns `true` iff a matching run was flagged.
    pub async fn backfill_cancel(&self, id: BackfillId) -> Result<bool, ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::BackfillCancel { id, reply_tx })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)?
            .map_err(ShardError::Backfill)
    }

    /// Snapshot this shard's most-recent resumable backfill progress.
    /// Backs the admin `GET /v1/backfill` route.
    pub async fn backfill_progress(&self) -> Result<BackfillProgress, ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::BackfillProgressSnapshot { reply_tx })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)?
            .map_err(ShardError::Backfill)
    }

    /// Trigger an immediate full HNSW rebuild. Returns the new
    /// entry count + elapsed time.
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

    /// Rebuild a chosen derived index from authoritative redb state.
    /// Backs the admin `POST /v1/rebuild?index=<target>` route. For
    /// `RebuildTarget::All` the returned report aggregates the per-target
    /// entry counts (sum) and total elapsed time.
    pub(crate) async fn rebuild_index(
        &self,
        target: rebuild::RebuildTarget,
    ) -> Result<RebuildReport, ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::RebuildIndex { target, reply_tx })
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
    /// `brain_ops::dispatch`). The frame-dispatcher's
    /// boundary primitive.
    ///
    /// `caller` carries the authenticated space from the
    /// connection's `ConnPhase::Established.space`. The shard
    /// passes it through to `brain_ops::dispatch`, which stamps it
    /// onto the per-request `ExecutorContext` so the writer-built
    /// Ops know who they belong to — closing the multi-tenant leak
    /// on shared shards.
    pub async fn dispatch_op(
        &self,
        req: RequestBody,
        caller: brain_ops::RequestCaller,
        parent_span: tracing::Span,
    ) -> Result<brain_ops::DispatchOutcome, DispatchError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::DispatchOp {
                req: Box::new(req),
                caller,
                reply_tx,
                parent_span,
            })
            .await
            .map_err(|_| DispatchError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| DispatchError::ShardDisconnected)?
            .map_err(DispatchError::Op)
    }

    /// Auto-abort every Active txn this shard holds for the given
    /// wire session. The connection layer invokes this on every shard
    /// in the topology when a TCP/TLS connection drops before the
    /// client committed: on TXN_ABORT or connection drop before commit,
    /// none of the operations take effect. The
    /// shard does the work synchronously inside its Glommio executor;
    /// the reply carries the number of entries swept so the
    /// connection-layer logger can summarise.
    ///
    /// Returns `Ok(0)` when no txns belonged to that session (the
    /// common case — most connections don't open a txn).
    pub async fn abort_orphaned_for_connection(
        &self,
        connection_id: [u8; 16],
    ) -> Result<usize, DispatchError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::AbortOrphanedTxns {
                connection_id,
                reply_tx,
            })
            .await
            .map_err(|_| DispatchError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| DispatchError::ShardDisconnected)
    }

    /// Read one page of the historical audit tables. Backs the admin
    /// `GET /v1/audit` + `/v1/audit/export` routes. `limit` bounds the
    /// page; `cursor` resumes strictly after a prior page's last row.
    /// The returned [`AuditPage::next`] is `Some` when more rows remain.
    pub async fn audit_query(
        &self,
        selector: AuditSelector,
        limit: usize,
        cursor: Option<AuditCursor>,
    ) -> Result<AuditPage, ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::AuditQuery {
                selector,
                limit,
                cursor,
                reply_tx,
            })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)?
            .map_err(ShardError::AuditQuery)
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
    /// The shard this joiner belongs to. Used by
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
    /// `Rc<RefCell<…>>` so the shard's worker adapters
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
    /// Constructed inside the executor.
    #[allow(dead_code)] // consumed by the frame dispatcher
    ops: Arc<OpsContext>,
    /// Per-shard worker scheduler. `Option` so shutdown can `.take()`.
    scheduler: Option<WorkerScheduler>,
    /// Snapshot source for the admin HTTP routes.
    /// Cloned from the same `Arc` the `SnapshotWorker` holds.
    snapshot_source: Arc<dyn SnapshotSource>,
    /// Rebuild source for the admin `rebuild-ann` route.
    /// Same `Arc` the `HnswMaintenanceWorker` holds.
    rebuild_source: Arc<dyn RebuildSource<{ VECTOR_DIM }>>,
    /// The shared HNSW handle. `rebuild-ann` swaps a freshly-
    /// rebuilt index in via `SharedHnsw::swap()`.
    hnsw_shared: SharedHnsw,
    /// Entity resolver HNSW. Same `Arc` the resolver + boot rebuild
    /// hold, so an on-demand `RebuildIndex { EntityHnsw }` reinserts in
    /// place and is immediately visible on the resolve path.
    entity_hnsw: Arc<parking_lot::RwLock<EntityHnswIndex>>,
    /// HyPE question pool. Same `Arc` the semantic retriever probes.
    hype_hnsw: Arc<parking_lot::RwLock<HypeHnswIndex>>,
    /// Per-statement question-bridge pool. Same `Arc` the retriever
    /// probes on slot / temporal reads.
    statement_question_hnsw: Arc<parking_lot::RwLock<StatementQuestionHnswIndex>>,
    /// The per-shard event-fanout task. Held (not detached) so the
    /// drain path can cancel it: it captures its own broadcast
    /// `EventBus` sender clone, so its `recv()` never observes
    /// `Closed` and the task would otherwise stay alive forever,
    /// keeping the Glommio executor from terminating after the main
    /// loop returns.
    fanout_task: Option<glommio::Task<()>>,
    /// The per-shard WAL-drain task. Same rationale as `fanout_task`:
    /// cancelled explicitly at drain so no live detached task blocks
    /// executor teardown.
    wal_drain_task: Option<glommio::Task<()>>,
    /// The per-shard lexical (tantivy) indexer tasks, with the signal
    /// that tells each to flush and exit.
    ///
    /// Unlike `fanout_task` / `wal_drain_task` these are **joined, not
    /// cancelled**: their final `commit()` is durable work. Detaching
    /// them lost that commit — a Glommio executor drops pending tasks
    /// once its main future returns — which left a hard-FORGETten
    /// memory's text on disk and keyword-searchable.
    memory_text_task: Option<(flume::Sender<()>, glommio::Task<()>)>,
    statement_text_task: Option<(flume::Sender<()>, glommio::Task<()>)>,
    /// Concrete lexical retriever handle, kept alongside the
    /// `Arc<dyn LexicalRetriever>` in `ops` so the hot tantivy rebuild
    /// (`do_rebuild_tantivy`) can atomically swap its open-index bundle
    /// via [`TantivyLexicalRetriever::swap_shard`] without disturbing the
    /// stable trait handle every reader holds.
    lexical_retriever: Arc<brain_index::TantivyLexicalRetriever>,
    /// Shard directory — root of the `memory_text.tantivy/` and
    /// `statements.tantivy/` index directories the hot rebuild
    /// reconstructs from authoritative redb and swaps in place.
    tantivy_dir: std::path::PathBuf,
    /// Control-plane senders to the two text indexers, used by the hot
    /// rebuild to `Quiesce` (drop the writer, release the per-directory
    /// lock) and `Resume` (rebuild the writer on the reopened index).
    /// `None` when the corresponding indexer failed to spawn.
    memory_text_control: Option<flume::Sender<brain_ops::index::text_indexer::IndexerControl>>,
    statement_text_control: Option<flume::Sender<brain_ops::index::text_indexer::IndexerControl>>,
}

/// Load a page of `ExtractionAudit` rows from the primary audit table
/// given an ascending iterator of candidate `audit_id`s (produced by
/// walking one of the three extractor-audit indexes). Collects at most
/// `limit` rows; when a further candidate exists past the page, the
/// returned cursor points at the last row actually returned so the caller
/// can resume strictly after it. Dangling index entries (no primary row)
/// are skipped without consuming page budget.
fn collect_extraction_page<I, T>(
    audit_ids: I,
    primary: &T,
    limit: usize,
) -> Result<
    (
        Vec<brain_metadata::tables::audit::ExtractionAudit>,
        Option<AuditCursor>,
    ),
    String,
>
where
    I: IntoIterator<Item = Result<[u8; 16], String>>,
    T: redb::ReadableTable<[u8; 16], brain_metadata::tables::audit::ExtractionAudit>,
{
    let mut rows = Vec::new();
    let mut last: Option<AuditCursor> = None;
    let mut next = None;
    for audit_id in audit_ids {
        let audit_id = audit_id?;
        if rows.len() >= limit {
            // One candidate beyond the page → there is a next page.
            next = last;
            break;
        }
        if let Some(v) = primary.get(&audit_id).map_err(|e| format!("get: {e}"))? {
            let row = v.value();
            last = Some(AuditCursor {
                ts: row.started_at_unix_nanos,
                audit_id: row.audit_id_bytes,
            });
            rows.push(row);
        }
    }
    Ok((rows, next))
}

impl Shard {
    /// Count the live rows in `MEMORIES_TABLE`. There is exactly one
    /// arena slot per memory row (occupied + tombstoned until reclaimed),
    /// so this is the authoritative source for arena occupancy on the
    /// `/metrics` path. A read failure degrades to 0 so a transient redb
    /// hiccup never fails the scrape.
    fn memory_row_count(&self) -> u64 {
        use brain_metadata::tables::memory::MEMORIES_TABLE;
        use redb::ReadableTableMetadata;

        let Ok(rtxn) = self.ops.executor.metadata.read_txn() else {
            return 0;
        };
        let Ok(table) = rtxn.open_table(MEMORIES_TABLE) else {
            return 0;
        };
        table.len().unwrap_or(0)
    }

    /// Resolve a memory's full-precision embedding vector by id,
    /// space-walled to `space`. Returns `None` when the row is missing,
    /// tombstoned, belongs to another space, or has no stored text.
    /// Never a silent cross-tenant read: the caller (SUBSCRIBE
    /// similarity registration) rejects `None` with `InvalidRequest`.
    ///
    /// The vector is produced by re-embedding the memory's stored text
    /// (`TEXTS_TABLE`) rather than reading the arena: the arena is
    /// populated only by WAL recovery on shard restart, so a memory
    /// encoded in the current run has no arena slot yet. The embedder is
    /// deterministic, so re-embedding reproduces the exact vector indexed
    /// at encode time.
    fn memory_vector_for(
        &self,
        space: brain_core::SpaceId,
        memory_id: brain_core::MemoryId,
    ) -> Option<[f32; VECTOR_DIM]> {
        use brain_metadata::tables::memory::MEMORIES_TABLE;
        use brain_metadata::tables::text::TEXTS_TABLE;

        let rtxn = self.ops.executor.metadata.read_txn().ok()?;
        let table = rtxn.open_table(MEMORIES_TABLE).ok()?;
        let row = table.get(memory_id.to_be_bytes()).ok().flatten()?.value();
        // Space wall + liveness: a subscriber may only anchor similarity
        // on a live memory its own (effective) space owns.
        if !row.is_active() || row.space_id_bytes != space.0.into_bytes() {
            return None;
        }
        // Resolve the reference vector by re-embedding the memory's stored
        // text, NOT by reading the arena. The arena is populated only by
        // WAL recovery on shard restart, so a memory encoded in the
        // current run has no arena slot yet and would resolve to None —
        // which silently rejected every same-run similarity subscription.
        // The embedder is deterministic, so re-embedding the stored text
        // reproduces the exact vector that was indexed at encode time.
        let texts = rtxn.open_table(TEXTS_TABLE).ok()?;
        let stored = texts.get(memory_id.to_be_bytes()).ok().flatten()?;
        let text = std::str::from_utf8(stored.value()).ok()?;
        self.ops.executor.embedder.embed(text).ok()
    }

    /// Read one page of the historical audit tables under a single redb
    /// read transaction. Walks the index matching `selector`, resuming
    /// strictly after `cursor`, and loads at most `limit` rows. Returns
    /// [`AuditPage::next`] = `Some` iff at least one further row exists.
    ///
    /// Runs on the shard executor (the sole owner of the `metadata.redb`
    /// handle). Deployment-wide operator surface — no tenant scoping.
    fn run_audit_query(
        &self,
        selector: AuditSelector,
        limit: usize,
        cursor: Option<AuditCursor>,
    ) -> Result<AuditPage, String> {
        use brain_metadata::tables::audit::{
            ENTITY_RESOLUTION_AUDIT_TABLE, EXTRACTOR_AUDIT_BY_EXTRACTOR_TABLE,
            EXTRACTOR_AUDIT_BY_MEMORY_TABLE, EXTRACTOR_AUDIT_BY_TIME_TABLE, EXTRACTOR_AUDIT_TABLE,
        };
        use std::ops::Bound;

        // A `limit` of 0 would loop forever below (never fills a page yet
        // never terminates the "one-past" check); the handler caps it, but
        // guard here too so the executor can never wedge.
        let limit = limit.max(1);
        let rtxn = self
            .ops
            .executor
            .metadata
            .read_txn()
            .map_err(|e| format!("read txn: {e}"))?;

        // Shared page-accumulation over an extractor-audit index: `entries`
        // yields `(leading, audit_id)` in ascending key order; each hit is
        // loaded from the primary table. `next` is set to the last row we
        // actually returned when a further entry exists past the page.
        match selector {
            AuditSelector::Memory(mem) => {
                let idx = rtxn
                    .open_table(EXTRACTOR_AUDIT_BY_MEMORY_TABLE)
                    .map_err(|e| format!("open by-memory index: {e}"))?;
                let primary = rtxn
                    .open_table(EXTRACTOR_AUDIT_TABLE)
                    .map_err(|e| format!("open audit table: {e}"))?;
                let lo = match cursor {
                    Some(c) => Bound::Excluded((mem, c.audit_id)),
                    None => Bound::Included((mem, [0u8; 16])),
                };
                let hi = Bound::Included((mem, [0xffu8; 16]));
                let range = idx.range((lo, hi)).map_err(|e| format!("range: {e}"))?;
                let (rows, next) = collect_extraction_page(
                    range.map(|e| e.map(|(k, _)| k.value().1).map_err(|e| format!("row: {e}"))),
                    &primary,
                    limit,
                )?;
                Ok(AuditPage::Extraction { rows, next })
            }
            AuditSelector::Extractor(ext) => {
                let idx = rtxn
                    .open_table(EXTRACTOR_AUDIT_BY_EXTRACTOR_TABLE)
                    .map_err(|e| format!("open by-extractor index: {e}"))?;
                let primary = rtxn
                    .open_table(EXTRACTOR_AUDIT_TABLE)
                    .map_err(|e| format!("open audit table: {e}"))?;
                let lo = match cursor {
                    Some(c) => Bound::Excluded((ext, c.audit_id)),
                    None => Bound::Included((ext, [0u8; 16])),
                };
                let hi = Bound::Included((ext, [0xffu8; 16]));
                let range = idx.range((lo, hi)).map_err(|e| format!("range: {e}"))?;
                let (rows, next) = collect_extraction_page(
                    range.map(|e| e.map(|(k, _)| k.value().1).map_err(|e| format!("row: {e}"))),
                    &primary,
                    limit,
                )?;
                Ok(AuditPage::Extraction { rows, next })
            }
            AuditSelector::Time { since, until } => {
                let idx = rtxn
                    .open_table(EXTRACTOR_AUDIT_BY_TIME_TABLE)
                    .map_err(|e| format!("open by-time index: {e}"))?;
                let primary = rtxn
                    .open_table(EXTRACTOR_AUDIT_TABLE)
                    .map_err(|e| format!("open audit table: {e}"))?;
                let lo = match cursor {
                    Some(c) => Bound::Excluded((c.ts, c.audit_id)),
                    None => Bound::Included((since, [0u8; 16])),
                };
                let hi = Bound::Included((until, [0xffu8; 16]));
                let range = idx.range((lo, hi)).map_err(|e| format!("range: {e}"))?;
                let (rows, next) = collect_extraction_page(
                    range.map(|e| e.map(|(k, _)| k.value().1).map_err(|e| format!("row: {e}"))),
                    &primary,
                    limit,
                )?;
                Ok(AuditPage::Extraction { rows, next })
            }
            AuditSelector::Resolution { since, until } => {
                let table = rtxn
                    .open_table(ENTITY_RESOLUTION_AUDIT_TABLE)
                    .map_err(|e| format!("open resolution table: {e}"))?;
                let lo = match cursor {
                    Some(c) => Bound::Excluded(c.audit_id),
                    None => Bound::Included([0u8; 16]),
                };
                let hi = Bound::Included([0xffu8; 16]);
                let mut rows = Vec::new();
                let mut last: Option<AuditCursor> = None;
                let mut next = None;
                for entry in table.range((lo, hi)).map_err(|e| format!("range: {e}"))? {
                    let (_k, v) = entry.map_err(|e| format!("row: {e}"))?;
                    let row = v.value();
                    // Primary-key scan is creation-ordered (UUIDv7) but the
                    // window filter is applied in memory since no time index
                    // exists for the resolution table.
                    if row.created_at_unix_nanos < since || row.created_at_unix_nanos > until {
                        continue;
                    }
                    if rows.len() >= limit {
                        next = last;
                        break;
                    }
                    last = Some(AuditCursor {
                        ts: row.created_at_unix_nanos,
                        audit_id: row.audit_id_bytes,
                    });
                    rows.push(row);
                }
                Ok(AuditPage::Resolution { rows, next })
            }
        }
    }

    /// Sample the shard's on-disk storage footprint for `/metrics`.
    /// WAL + metadata sizes are stat'd off disk; arena capacity comes
    /// from the live arena header. Occupancy is the metadata memory-row
    /// count, not the arena `SlotAllocator`: the writer allocates slots
    /// from its own in-process counter, so the `SlotAllocator` never
    /// advances and would always report 0. Blocking `fs::metadata` is
    /// fine here — this runs on the shard's own core at scrape cadence,
    /// and a missing/rotated file degrades to 0 rather than failing the
    /// scrape.
    fn storage_stats(&self) -> StorageStatsSnapshot {
        let metadata_path = self.ops.executor.metadata.path().to_path_buf();
        let metadata_size_bytes = std::fs::metadata(&metadata_path)
            .map(|m| m.len())
            .unwrap_or(0);

        // The shard root is metadata.redb's parent; derive the wal dir
        // from it so we go through the same layout the writer uses.
        let wal_dir = metadata_path
            .parent()
            .map(|root| brain_storage::ShardPaths::at(root).wal_dir())
            .unwrap_or_else(|| metadata_path.clone());
        let (wal_size_bytes, wal_segments) = brain_storage::wal_segment_stats(&wal_dir);

        let capacity_slots = self.arena.borrow().capacity_slots();
        let arena_capacity_bytes = capacity_slots * brain_storage::SLOT_SIZE_BYTES as u64;
        let arena_slots_used = self.memory_row_count();
        // The free-list reclamation worker isn't wired yet, so there are
        // no reclaimed-and-reusable slots to report.
        let arena_slots_free = 0;
        let arena_used_bytes = arena_slots_used * brain_storage::SLOT_SIZE_BYTES as u64;

        StorageStatsSnapshot {
            wal_size_bytes,
            wal_segments,
            metadata_size_bytes,
            arena_capacity_bytes,
            arena_used_bytes,
            arena_slots_used,
            arena_slots_free,
        }
    }

    /// Rebuild the memory HNSW from the authoritative redb vector
    /// snapshot, folding in the pending buffer, and publish atomically
    /// via `SharedHnsw::flush_with_rebuild` so a failed rebuild leaves
    /// the prior index intact. Shared by the `RebuildHnsw` and
    /// `RebuildIndex { MemoryHnsw }` handlers.
    async fn do_rebuild_memory(&self) -> Result<RebuildReport, String> {
        let start = std::time::Instant::now();
        let vectors = self
            .rebuild_source
            .snapshot_vectors()
            .await
            .map_err(|e| format!("rebuild source: {e}"))?;
        let params = self.hnsw_shared.params();
        // Fold the redb snapshot together with the pending buffer and
        // publish atomically. A raw `swap` would clear pending, discarding
        // a live vector not yet folded into main — the sole home of a
        // same-run encode between its ENCODE and the next flush.
        let flush = self.hnsw_shared.flush_with_rebuild(move |pending| {
            let combined = fold_pending_into(vectors, pending);
            let (idx, _) = brain_index::rebuild::rebuild_impl(params, combined)?;
            Ok(idx)
        });
        match flush {
            Ok(report) => Ok(RebuildReport {
                entries: report.main_len_after,
                elapsed_ms: start.elapsed().as_millis() as u64,
            }),
            Err(e) => Err(format!("rebuild: {e:?}")),
        }
    }

    /// Rebuild one derived index from authoritative redb state. The HNSW
    /// helpers reinsert in place under the index's write lock, so the
    /// rebuild is immediately visible on the serve path and a failure
    /// leaves the prior index intact. `All` runs each HNSW target in turn
    /// and returns an aggregate report (summed entries, total elapsed).
    async fn do_rebuild_index(
        &self,
        target: rebuild::RebuildTarget,
    ) -> Result<RebuildReport, String> {
        use rebuild::RebuildTarget as T;
        let metadata = &self.ops.executor.metadata;
        let embedder = self.ops.executor.embedder.as_ref();
        match target {
            T::MemoryHnsw => self.do_rebuild_memory().await,
            T::EntityHnsw => {
                let start = std::time::Instant::now();
                let entries = rebuild::rebuild_entity_hnsw(
                    &self.entity_hnsw,
                    metadata,
                    embedder,
                    self.shard_id,
                )?;
                Ok(RebuildReport {
                    entries,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                })
            }
            T::HypeHnsw => {
                let start = std::time::Instant::now();
                let entries = rebuild::rebuild_hype_hnsw(&self.hype_hnsw, metadata, self.shard_id)?;
                Ok(RebuildReport {
                    entries,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                })
            }
            T::StatementQuestionHnsw => {
                let start = std::time::Instant::now();
                let entries = rebuild::rebuild_statement_question_hnsw(
                    &self.statement_question_hnsw,
                    metadata,
                    self.shard_id,
                )?;
                Ok(RebuildReport {
                    entries,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                })
            }
            T::All => {
                let start = std::time::Instant::now();
                let mut entries = 0usize;
                // Memory first (async snapshot), then the in-place HNSW
                // rebuilds. Any failure short-circuits with the prior
                // indexes intact (each target swaps atomically).
                entries += self.do_rebuild_memory().await?.entries;
                entries += rebuild::rebuild_entity_hnsw(
                    &self.entity_hnsw,
                    metadata,
                    embedder,
                    self.shard_id,
                )?;
                entries += rebuild::rebuild_hype_hnsw(&self.hype_hnsw, metadata, self.shard_id)?;
                entries += rebuild::rebuild_statement_question_hnsw(
                    &self.statement_question_hnsw,
                    metadata,
                    self.shard_id,
                )?;
                Ok(RebuildReport {
                    entries,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                })
            }
            // Tantivy rebuilds live via the quiesce → rebuild → swap dance.
            T::TantivyMemory | T::TantivyStatement => self.do_rebuild_tantivy(target).await,
        }
    }

    /// Hot-rebuild the two tantivy (lexical) indexes from authoritative
    /// redb while the shard keeps serving, then live-swap the read side
    /// and the indexer writers onto the rebuilt indexes.
    ///
    /// A tantivy index cannot be rebuilt in place: the running indexer
    /// holds tantivy's exclusive per-directory writer lock, and the
    /// retriever's cached readers are bound to the `Index` opened at spawn.
    /// The dance below resolves both:
    ///
    /// 1. **Quiesce** both indexers — each drops its writer, releasing the
    ///    lock; ops keep buffering on their channels.
    /// 2. **Rebuild** both on-disk indexes from redb into `<live>.rebuild`
    ///    and atomically rename them over `<live>` (the offline rebuild the
    ///    boot path already uses; it is safe now that no writer holds the
    ///    live lock).
    /// 3. **Reopen** the shard from disk and **swap** the retriever's
    ///    open-index bundle onto it in one atomic publish.
    /// 4. **Resume** both indexers with fresh writers on the reopened
    ///    index; their buffered ops re-drain idempotently.
    ///
    /// Both indexes are rebuilt regardless of the requested `target`: the
    /// reopen + retriever swap are whole-shard, and reconstructing both
    /// from redb keeps the swap atomic and lossless (a quiesced indexer
    /// discards its uncommitted batch, which is only safe when that index
    /// is itself reconstructed from the authoritative rows). At no instant
    /// does a read observe a partial or stale-mixed index: the whole method
    /// runs on the single-threaded shard main loop, so no read is dispatched
    /// between quiesce and resume, and the retriever swap is atomic
    /// (invariant #7).
    async fn do_rebuild_tantivy(
        &self,
        target: rebuild::RebuildTarget,
    ) -> Result<RebuildReport, String> {
        let start = std::time::Instant::now();
        let metadata = self.ops.executor.metadata.as_ref();

        // The quiesce → rebuild → reopen/swap → resume orchestration lives in
        // `drive_tantivy_rebuild`, which guarantees BOTH indexers are resumed
        // on every exit path — including one whose own quiesce ack timed out
        // while its drain loop had already dropped its writer and parked. A
        // quiesced-but-never-resumed indexer is wedged forever and every later
        // ENCODE/FORGET silently loses its lexical op (invariant #7). The
        // middle closure runs synchronously between quiesce and resume.
        let entries = drive_tantivy_rebuild(
            self.memory_text_control.as_ref(),
            self.statement_text_control.as_ref(),
            |mem_quiesced, stmt_quiesced| {
                // 2. Rebuild each on-disk index from authoritative redb, gated
                //    on ITS OWN indexer having quiesced: an un-quiesced indexer
                //    still holds the per-directory writer lock, so the on-disk
                //    replace could not complete cleanly. Resume is still
                //    guaranteed for both below regardless.
                let rebuild_result: Result<u64, String> = (|| {
                    let mem = if mem_quiesced {
                        brain_ops::index::text_indexer::rebuild_memory_text(
                            &self.tantivy_dir,
                            metadata,
                        )
                        .map_err(|e| format!("memory text rebuild: {e}"))?
                        .rows_processed
                    } else {
                        0
                    };
                    let stmt = if stmt_quiesced {
                        brain_ops::index::text_indexer::rebuild_statements(
                            &self.tantivy_dir,
                            metadata,
                        )
                        .map_err(|e| format!("statement text rebuild: {e}"))?
                        .rows_processed
                    } else {
                        0
                    };
                    Ok(mem + stmt)
                })();

                // 3. Reopen the shard from disk and swap the retriever's cached
                //    readers onto it. `TantivyShard::open` reconciles any
                //    interrupted swap, so even a mid-rebuild failure yields a
                //    valid index (the completed rebuild or the restored prior
                //    one). Capture — never swallow — the swap error: a failed
                //    swap leaves the retriever bound to the pre-rebuild `Index`
                //    whose segment files the completed on-disk rebuild has
                //    already deleted, so reads would serve from unlinked
                //    segments (stale / vanishing data), exactly what invariant
                //    #7 forbids. A swap failure must therefore fail-stop.
                let reopened = brain_index::TantivyShard::open(&self.tantivy_dir)
                    .map_err(|e| format!("reopen tantivy after rebuild: {e}"));
                let mut swap_err: Option<String> = None;
                let (reopen_err, resume_handles) = match &reopened {
                    Ok(startup) => {
                        let new_shard = startup.shard.clone();
                        if let Err(e) = self.lexical_retriever.swap_shard(new_shard.clone()) {
                            tracing::error!(
                                shard_id = self.shard_id,
                                error = %e,
                                "lexical retriever swap failed during tantivy rebuild",
                            );
                            swap_err = Some(format!("lexical retriever swap after rebuild: {e}"));
                        }
                        (
                            None,
                            Some((new_shard.memory_text.clone(), new_shard.statements.clone())),
                        )
                    }
                    // Reopen failed: fall back to the pre-rebuild handles so the
                    // indexers can still resume (writes keep flowing); the read
                    // side keeps its prior bundle.
                    Err(e) => (
                        Some(e.clone()),
                        self.ops
                            .tantivy
                            .as_ref()
                            .map(|s| (s.memory_text.clone(), s.statements.clone())),
                    ),
                };

                RebuildMiddle {
                    rebuild_result,
                    reopen_err,
                    swap_err,
                    resume_handles,
                }
            },
        )
        .await?;

        tracing::info!(
            shard_id = self.shard_id,
            ?target,
            entries,
            elapsed_ms = start.elapsed().as_millis() as u64,
            "tantivy indexes rebuilt live and swapped",
        );
        Ok(RebuildReport {
            entries: entries as usize,
            elapsed_ms: start.elapsed().as_millis() as u64,
        })
    }
}

/// How long the hot rebuild waits for an indexer to acknowledge a control
/// message before giving up. Generous: the ack rides the same
/// single-threaded executor and is normally near-instant; the bound only
/// guards against a dead/wedged indexer task.
const INDEXER_CONTROL_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Middle phase of the hot tantivy rebuild, produced by the caller between
/// quiesce and resume: the on-disk rebuild outcome plus the reopen/swap
/// results and the handles to resume the indexers onto.
#[cfg(target_os = "linux")]
struct RebuildMiddle {
    /// Rows reindexed across the (possibly-skipped) memory + statement
    /// rebuilds, or the first rebuild error.
    rebuild_result: Result<u64, String>,
    /// Error reopening the shard from disk after the rebuild, if any.
    reopen_err: Option<String>,
    /// Error swapping the retriever onto the reopened index, if any.
    swap_err: Option<String>,
    /// Handles to resume both indexers onto — the reopened index, or the
    /// pre-rebuild fallback. `None` only when no index is available at all.
    resume_handles: Option<(brain_index::IndexHandle, brain_index::IndexHandle)>,
}

/// Drive the quiesce → rebuild → reopen/swap → resume dance for the two
/// lexical indexers, returning the rows reindexed or the first fail-stop.
///
/// The ordering is invariant #7's load-bearing part: once an indexer receives
/// `Quiesce` it drops its writer and parks, so a quiesced-but-never-resumed
/// indexer is wedged forever and every later ENCODE/FORGET silently loses its
/// lexical op. Therefore NOTHING between the first quiesce and the resume
/// region may short-circuit — the bug this replaced `?`-returned on the FIRST
/// quiesce, stranding an already-parked `memory_text` indexer whenever its ack
/// timed out. Instead BOTH quiesce results are recorded (never `?`), `middle`
/// runs synchronously to rebuild/reopen/swap, and BOTH indexers are resumed
/// independently — even the one whose own quiesce ack failed (the `Quiesce`
/// was still delivered; the indexer parked and is alive to be resumed). Only
/// after both are resumed are the recorded failures surfaced, in dependency
/// order, as fail-stop.
#[cfg(target_os = "linux")]
async fn drive_tantivy_rebuild<M>(
    memory_control: Option<&flume::Sender<brain_ops::index::text_indexer::IndexerControl>>,
    statement_control: Option<&flume::Sender<brain_ops::index::text_indexer::IndexerControl>>,
    middle: M,
) -> Result<u64, String>
where
    M: FnOnce(bool, bool) -> RebuildMiddle,
{
    // 1. Quiesce both indexers (release the per-directory writer locks).
    //    Record — never `?` — BOTH quiesce results. An early return here would
    //    strand an already-parked indexer with its writer dropped.
    let mem_quiesce_err = quiesce_indexer(memory_control, "memory_text").await.err();
    let stmt_quiesce_err = quiesce_indexer(statement_control, "statements").await.err();

    // 2-3. Rebuild each on-disk index (gated on its own quiesce), reopen the
    //       shard, and swap the retriever — all synchronous, no `?`.
    let RebuildMiddle {
        rebuild_result,
        reopen_err,
        swap_err,
        resume_handles,
    } = middle(mem_quiesce_err.is_none(), stmt_quiesce_err.is_none());

    // 4. Resume BOTH indexers on the resolved handles, independently: a failed
    //    first resume must never skip the second. This region runs on every
    //    path above so no quiesced indexer is left parked.
    let mut resume_err: Option<String> = None;
    if let Some((mem_handle, stmt_handle)) = resume_handles {
        if let Err(e) = resume_indexer(memory_control, mem_handle, "memory_text").await {
            resume_err.get_or_insert(e);
        }
        if let Err(e) = resume_indexer(statement_control, stmt_handle, "statements").await {
            resume_err.get_or_insert(e);
        }
    }

    // Both indexers are resumed (or their revival failure recorded). Only now
    // surface the first failure, in dependency order, as fail-stop.
    if let Some(e) = mem_quiesce_err {
        return Err(e);
    }
    if let Some(e) = stmt_quiesce_err {
        return Err(e);
    }
    let entries = rebuild_result?;
    if let Some(e) = reopen_err {
        return Err(e);
    }
    if let Some(e) = swap_err {
        return Err(e);
    }
    if let Some(e) = resume_err {
        return Err(e);
    }
    Ok(entries)
}

/// Send `Quiesce` to an indexer (if it is running) and await its ack. The
/// indexer drops its writer, releasing tantivy's per-directory lock, so the
/// rebuild can replace the directory. A `None` control channel means the
/// indexer never spawned — nothing holds the lock, so this is a no-op.
#[cfg(target_os = "linux")]
async fn quiesce_indexer(
    control: Option<&flume::Sender<brain_ops::index::text_indexer::IndexerControl>>,
    label: &str,
) -> Result<(), String> {
    let Some(control) = control else {
        return Ok(());
    };
    let (ack_tx, ack_rx) = flume::bounded::<()>(1);
    control
        .send_async(brain_ops::index::text_indexer::IndexerControl::Quiesce { ack: ack_tx })
        .await
        .map_err(|_| format!("{label} indexer control channel closed (quiesce)"))?;
    await_ack(&ack_rx, label, "quiesce").await
}

/// Send `Resume` with a fresh handle on the reopened index and await the
/// ack. The indexer rebuilds its writer against `handle` and resumes
/// draining. A `None` control channel is a no-op.
#[cfg(target_os = "linux")]
async fn resume_indexer(
    control: Option<&flume::Sender<brain_ops::index::text_indexer::IndexerControl>>,
    handle: brain_index::IndexHandle,
    label: &str,
) -> Result<(), String> {
    let Some(control) = control else {
        return Ok(());
    };
    let (ack_tx, ack_rx) = flume::bounded::<()>(1);
    control
        .send_async(brain_ops::index::text_indexer::IndexerControl::Resume {
            handle,
            ack: ack_tx,
        })
        .await
        .map_err(|_| format!("{label} indexer control channel closed (resume)"))?;
    await_ack(&ack_rx, label, "resume").await
}

/// Await a control ack with a bounded timeout so a dead indexer task can
/// never wedge the rebuild indefinitely.
#[cfg(target_os = "linux")]
async fn await_ack(ack_rx: &flume::Receiver<()>, label: &str, phase: &str) -> Result<(), String> {
    // `Err` = the deadline elapsed; `Ok(false)` = the ack sender was
    // dropped (the indexer task died). Only `Ok(true)` is a real ack.
    let res = glommio::timer::timeout(INDEXER_CONTROL_ACK_TIMEOUT, async {
        Ok::<bool, glommio::GlommioError<()>>(ack_rx.recv_async().await.is_ok())
    })
    .await;
    if matches!(res, Ok(true)) {
        Ok(())
    } else {
        Err(format!(
            "{label} indexer {phase} ack timed out or task died"
        ))
    }
}

/// Register every background worker against `scheduler`, plugging in
/// real adapters for `RebuildSource`, `WalRetentionSource`,
/// and `SnapshotSource`. `Summarizer` is injected by `main.rs` (OpenAI
/// / Ollama if configured, `DisabledSummarizer` otherwise).
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
// Fanout lag observability
// ---------------------------------------------------------------------------

/// Process-global count of shard-fanout events skipped because a broadcast
/// subscriber lagged behind. A non-zero value means at least one live
/// subscriber missed an LSN and must resync: the per-shard broadcast can no
/// longer honour the "every event or an explicit resync signal" contract for
/// that subscriber. Kept observable (never silently swallowed) so the gap is
/// detectable; PromQL `rate()` over this surfaces sustained overload.
static FANOUT_LAGGED_EVENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record `skipped` fanout events lost to a lagging broadcast subscriber:
/// bump the process-global counter and emit a warning. Called from the
/// per-shard fanout task's `Lagged` arm instead of silently continuing, so a
/// dropped event is always at least logged and counted.
fn record_fanout_lag(shard_id: ShardId, skipped: u64) {
    FANOUT_LAGGED_EVENTS.fetch_add(skipped, std::sync::atomic::Ordering::Relaxed);
    warn!(
        shard_id,
        skipped,
        "shard fanout lagged: broadcast subscriber dropped events; live subscribers see an LSN gap and must resync"
    );
}

/// Read the process-global fanout-lag counter. Test/introspection hook.
#[cfg(test)]
fn fanout_lagged_events() -> u64 {
    FANOUT_LAGGED_EVENTS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Interpret the WAL-readiness signal the shard executor closure sends back
/// after it opens (or fails to open) its WAL.
///
/// - `Ok(Ok(()))` — WAL open; the shard is building the rest of its stack.
/// - `Ok(Err(e))` — WAL IO failure; the closure returned early. Surfaced as
///   [`ShardError::WalInit`] so the spawn fails fast instead of serving a
///   dead shard.
/// - `Err(_)` — the sender was dropped before signalling: the executor
///   thread exited or unwound (e.g. an earlier in-closure `.expect()`
///   panicked) before reaching WAL init. Also a dead shard, so fail the spawn.
fn interpret_wal_ready(
    signal: Result<Result<(), WalError>, flume::RecvError>,
) -> Result<(), ShardError> {
    match signal {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(ShardError::WalInit(err)),
        Err(_) => Err(ShardError::Spawn(
            "shard executor exited before WAL init completed".to_string(),
        )),
    }
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
    // `ensure_dirs` mkdir-p's the shard root, `wal/`, and
    // the opaque-body tantivy directories. It's idempotent over
    // existing substrate shards (no-op when present) and bootstraps fresh
    // ones. typed-graph *files* (entity.hnsw, statement.hnsw,
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
    // `recover()` is sync (mmap-based, reads only — io_uring
    // brings nothing). The durable redb-backed MetadataDb is the sink.
    let metadata_path = paths.metadata_db();
    let mut metadata_db = MetadataDb::open(&metadata_path)?;

    // The LLM extractor cache (`llm_cache.redb`) is opened exactly
    // once per shard — inside the Glommio executor closure below via
    // `llm_setup::build_llm_deps`. redb's lock is process-wide and
    // inode-keyed; pre-opening here and dropping would race the
    // executor's open (the closure runs concurrently with the rest
    // of this function) and produce "Database already open".
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
    // Physical byte length the active WAL segment must be truncated to
    // before reopening for append — the tail recovery validated. Passed to
    // `Wal::open_existing` so a crash-left torn tail or never-committed
    // dangling-transaction prefix is dropped rather than appended past.
    let recovered_tail_offset: u64;
    let allocator = if segments_present {
        let (report, alloc) = recover(&mut arena, &wal_dir, shard_uuid, &mut metadata_db)?;
        info!(
            shard_id,
            records_replayed = report.records_replayed,
            records_skipped = report.records_skipped,
            records_discarded = report.records_discarded,
            next_lsn = report.next_lsn,
            active_tail_offset = report.active_tail_offset,
            active_segment_seq = ?report.active_segment_seq,
            "WAL recovery complete"
        );
        next_lsn_after_recovery = report.next_lsn;
        recovered_tail_offset = report.active_tail_offset;
        alloc
    } else {
        next_lsn_after_recovery = 1;
        recovered_tail_offset = 0;
        SlotAllocator::rebuild_from_arena(&arena)
    };
    let metadata: SharedMetadataDb = Arc::new(metadata_db);

    // ---- 4b. Tantivy open + recovery (must succeed at spawn) -------------
    //
    // Lexical retrieval is a core capability. A shard that can't open or
    // recover its tantivy indexes can't serve recalls correctly, so we
    // refuse to spawn rather than silently flip to a degraded mode. We
    // do this *outside* the Glommio executor closure so the error can
    // bubble through `spawn_shard`'s `Result` directly.
    let tantivy_shard: Arc<brain_index::TantivyShard> = {
        let startup = brain_index::TantivyShard::open(&dir)
            .map_err(|source| ShardError::TantivyInitFailed { source })?;
        crate::shard::tantivy_recovery::recover_tantivy_on_open(&dir, metadata.as_ref(), startup)
            .map_err(|source| ShardError::TantivyRecoveryFailed { source })?
    };
    // Build the lexical retriever from the same `TantivyShard` the
    // indexer workers will write to. Constructing it here (outside the
    // closure) propagates the failure through `spawn_shard`'s `Result`
    // just like the open above.
    // Keep the concrete retriever so the hot rebuild can call
    // `swap_shard`; hand `OpsContext` the trait object cloned from it, so
    // the read path and the rebuild path share one retriever instance.
    let lexical_retriever_concrete: Arc<brain_index::TantivyLexicalRetriever> = Arc::new(
        brain_index::TantivyLexicalRetriever::new(tantivy_shard.clone())
            .map_err(|source| ShardError::LexicalRetrieverInitFailed { source })?,
    );
    let lexical_retriever: Arc<dyn brain_index::LexicalRetriever> =
        lexical_retriever_concrete.clone();

    // ---- 5. Spawn the Glommio executor + build the rest of the stack -----
    let (tx, rx) = flume::bounded::<ShardRequest>(cfg.channel_capacity);
    // Cross-shard event feed. The Glommio closure spawns a
    // fanout_task that drains `ops.events` into this channel; the
    // connection layer reads the Receiver via `ShardHandle::events()`.
    let (events_tx, events_rx) = flume::bounded::<EventEnvelope>(1024);
    let placement = match cfg.pin_cpu {
        Some(cpu) => Placement::Fixed(cpu),
        None => Placement::Unbound,
    };
    let wal_config = cfg.wal_config;
    let summarizer = cfg.summarizer;
    let auto_edge_spawn_cfg_for_closure = cfg.auto_edge.clone();
    let extractor_spawn_cfg_for_closure = cfg.extractor.clone();
    let temporal_edge_spawn_cfg_for_closure = cfg.temporal_edge.clone();
    let causal_edge_spawn_cfg_for_closure = cfg.causal_edge.clone();
    let statement_reclaim_spawn_cfg = cfg.statement_reclaim;
    let supersession_sweeper_spawn_cfg = cfg.supersession_sweeper;
    let ambiguity_resolver_spawn_cfg = cfg.ambiguity_resolver;
    let confidence_sweep_spawn_cfg = cfg.confidence_sweep;
    let llm_cache_sweep_spawn_cfg = cfg.llm_cache_sweep;
    let extractor_tuning_spawn_cfg = cfg.extractor_tuning.clone();
    let index_spawn_cfg = cfg.index;

    // ---- Cross-encoder (rerank) capability gate ----------------------------
    //
    // Loaded outside the Glommio executor so failures map to a clean
    // `ShardError::CrossEncoderInitFailed` instead of an in-closure panic.
    // The `Arc<CrossEncoder>` is then cloned into the closure and lives
    // on `OpsContext` via `CrossEncoderSlot::Enabled`. When the operator
    // turns rerank off in config, the slot is `Disabled` and request-time
    // opt-ins surface as `CapabilityNotEnabled` — clients learn the
    // capability isn't available without falling back to RRF silently.
    let cross_encoder_slot: brain_ops::CrossEncoderSlot = if cfg.rerank.enabled {
        match brain_rerank::try_load() {
            Ok(Some(encoder)) => {
                tracing::info!(
                    target: "brain_server::shard",
                    shard_id,
                    "cross-encoder loaded; rerank capability online",
                );
                // Move the encoder onto its own thread: the forward
                // pass is heavy CPU work that must not block the shard
                // core. The shard awaits scores over a channel instead.
                brain_ops::CrossEncoderSlot::Enabled(Arc::new(brain_rerank::RerankService::spawn(
                    encoder,
                )))
            }
            Ok(None) => {
                // Operator left `rerank.enabled = true` but no model
                // is on disk. We treat this as a misconfiguration —
                // an opt-in rerank request would silently fall back
                // to RRF otherwise, and the operator wouldn't notice
                // the rerank capability is dead.
                return Err(ShardError::CrossEncoderInitFailed(
                    "no cross-encoder model found (set BRAIN_RERANK_MODEL_DIR or place \
                     weights at the XDG default path); set [rerank] enabled = false to \
                     opt out explicitly"
                        .to_string(),
                ));
            }
            Err(err) => {
                return Err(ShardError::CrossEncoderInitFailed(format!(
                    "cross-encoder load failed: {err}",
                )));
            }
        }
    } else {
        tracing::info!(
            target: "brain_server::shard",
            shard_id,
            "rerank disabled by config; opt-in requests will return CapabilityNotEnabled",
        );
        brain_ops::CrossEncoderSlot::Disabled
    };
    let cross_encoder_for_closure = cross_encoder_slot.clone();

    // Extraction is always-on. All three tiers materialise
    // unconditionally — extraction populates the typed graph that reads
    // fuse, so a shard serving with extraction off would return incoherent
    // graph-backed reads. There is no per-tier config gate (like the
    // embedder and HyPE). A materialiser init error surfaces as
    // `ShardError::ExtractorInitFailed` (hard spawn failure); a tier whose
    // dep (GLiNER model / LLM client) is absent materialises degraded and
    // emits `SkippedDisabled` audit rows rather than failing to spawn.
    let tier_gate = brain_extractors::TierGate::all_enabled();
    let tier_gate_for_closure = tier_gate;
    // The extraction pipeline (worker + queue drain) is always provisioned:
    // extraction is a non-configurable always-on capability.
    let extractor_pipeline_enabled = true;
    // Provider credentials / model overrides for the LLM extractor
    // tier, resolved env-first / config-fallback inside the closure.
    let llm_config_for_closure = cfg.llm.clone();
    // Construct the AutoEdge / Extractor metric handles up-front so we
    // can both stash them on `ShardHandle` (for /metrics exposition)
    // and inject them into the writer + worker (so both sides bump
    // the same atomics). `None` when the worker is disabled in
    // spawn config — exposition simply skips that family.
    let auto_edge_metrics_for_handle: Option<Arc<brain_ops::AutoEdgeMetrics>> =
        if cfg.auto_edge.enabled {
            Some(Arc::new(brain_ops::AutoEdgeMetrics::new()))
        } else {
            None
        };
    let extractor_metrics_for_handle: Option<Arc<brain_ops::ExtractorMetrics>> =
        if extractor_pipeline_enabled {
            Some(Arc::new(brain_ops::ExtractorMetrics::new()))
        } else {
            None
        };
    let temporal_edge_metrics_for_handle: Option<Arc<brain_ops::TemporalEdgeMetrics>> =
        if cfg.temporal_edge.enabled {
            Some(Arc::new(brain_ops::TemporalEdgeMetrics::new()))
        } else {
            None
        };
    let causal_edge_metrics_for_handle: Option<Arc<brain_ops::CausalEdgeMetrics>> =
        if cfg.causal_edge.enabled {
            Some(Arc::new(brain_ops::CausalEdgeMetrics::new()))
        } else {
            None
        };
    let auto_edge_metrics_for_closure = auto_edge_metrics_for_handle.clone();
    let extractor_metrics_for_closure = extractor_metrics_for_handle.clone();
    let temporal_edge_metrics_for_closure = temporal_edge_metrics_for_handle.clone();
    let causal_edge_metrics_for_closure = causal_edge_metrics_for_handle.clone();
    // LLM cache sweep metrics are always constructed: whether the
    // sweeper actually runs depends on `OpsContext.llm_cache` (set
    // inside the executor closure once `build_llm_deps` completes),
    // and we want `/metrics` exposition wired even for "cache
    // configured but no rows swept yet" shards.
    let llm_cache_sweep_metrics_for_handle: Arc<brain_ops::LlmCacheSweepMetrics> =
        Arc::new(brain_ops::LlmCacheSweepMetrics::new());
    let llm_cache_sweep_metrics_for_closure = llm_cache_sweep_metrics_for_handle.clone();
    // StatementEmbed metrics are unconditionally constructed: the
    // worker itself is unconditional (drains an empty queue with
    // negligible cost on no-schema shards), and `/metrics` should
    // surface zeroed counters rather than miss the family.
    let statement_embed_metrics_for_handle: Arc<brain_ops::StatementEmbedMetrics> =
        Arc::new(brain_ops::StatementEmbedMetrics::new());
    let statement_embed_metrics_for_closure = statement_embed_metrics_for_handle.clone();
    // ConfidenceSweep metrics are unconditionally constructed for the
    // same reason as StatementEmbed: a substrate-only shard registers
    // the worker, finds an empty STATEMENTS_TABLE every hour, and
    // returns 0 — `/metrics` surfaces zeroed counters rather than
    // skipping the family.
    let confidence_sweep_metrics_for_handle: Arc<brain_ops::ConfidenceSweepMetrics> =
        Arc::new(brain_ops::ConfidenceSweepMetrics::new());
    let confidence_sweep_metrics_for_closure = confidence_sweep_metrics_for_handle.clone();
    // Read-path metric families are unconditionally constructed: recall
    // runs on every shard. One `Arc` is injected into the shard's
    // `OpsContext` (the RECALL handler records into it) and the twin is
    // stashed on `ShardHandle` for `/metrics` exposition.
    let retriever_metrics_for_handle: Arc<brain_ops::RetrieverMetrics> =
        Arc::new(brain_ops::RetrieverMetrics::new());
    let retriever_metrics_for_closure = retriever_metrics_for_handle.clone();
    let query_metrics_for_handle: Arc<brain_ops::QueryMetrics> =
        Arc::new(brain_ops::QueryMetrics::new());
    let query_metrics_for_closure = query_metrics_for_handle.clone();
    // Clone the process-wide dispatcher Arc into the executor closure.
    // The CachingDispatcher<CpuDispatcher> built once in main.rs is
    // shared across every shard so the BERT weights live in memory
    // exactly once no matter how many shards spawn.
    let dispatcher_for_closure = cfg.dispatcher.clone();
    let wal_dir_for_executor = wal_dir.clone();
    let arena_path_for_executor = arena_path.clone();
    let metadata_path_for_executor = metadata_path.clone();
    let snapshots_root_for_executor = dir.join("snapshots");
    // The shard dir is also home to the per-shard LLM
    // extractor response cache (`<shard_dir>/llm_cache.redb`).
    let shard_dir_for_executor = dir.clone();
    // Tantivy is opened above and propagated into the closure pre-built.
    // The closure can no longer downgrade lexical retrieval to `None`.
    let tantivy_for_closure = tantivy_shard.clone();
    let lexical_retriever_for_closure = lexical_retriever.clone();
    // Concrete retriever + shard dir, captured for the shard's hot
    // tantivy rebuild (`do_rebuild_tantivy`).
    let lexical_retriever_concrete_for_closure = lexical_retriever_concrete.clone();
    let tantivy_dir_for_closure = dir.clone();
    // WAL open/create runs *inside* the Glommio executor closure below —
    // it needs the executor's io_uring reactor to `.await`, and it depends
    // on `next_lsn_after_recovery` / `recovered_tail_offset` produced by the
    // recovery pass above. That means it can't be hoisted outside the closure
    // like the tantivy/rerank init. To keep a WAL IO failure (permissions,
    // ENOSPC, an FS without O_DIRECT/io_uring support) from panicking the
    // shard thread *after* `spawn` already returned `Ok` — which would leave
    // `spawn_shard` reporting success while the shard is dead — the closure
    // reports the outcome of WAL init back over this channel. `spawn_shard`
    // blocks on it below and fails the spawn (fail-fast) instead of serving a
    // dead shard. A `RecvError` (sender dropped) means the closure unwound
    // before signalling — e.g. an earlier in-closure panic — which is also
    // treated as a spawn failure.
    let (wal_ready_tx, wal_ready_rx) = flume::bounded::<Result<(), WalError>>(1);
    let join_handle = LocalExecutorBuilder::new(placement)
        .name(&format!("brain-shard-{shard_id}"))
        .spawn(move || async move {
            // Build per-shard HNSW; tombstones rebuilt by HnswMaintenanceWorker.
            let (hnsw_shared, hnsw_writer) =
                SharedHnsw::new(IndexParams::default_v1())
                    .expect("SharedHnsw::new");
            let dispatcher: Arc<dyn Dispatcher> = dispatcher_for_closure;
            // Per-shard StatementHnswIndex. Populated by the
            // StatementEmbedWorker draining `STATEMENT_EMBED_QUEUE_TABLE`
            // (registered below) and read by the SemanticRetriever in
            // its statement-corpus mode. In-memory only — on restart
            // the queue replays the still-pending rows; rows already
            // embedded fall through the worker's idempotent
            // `contains`-check no-op.
            let statement_hnsw_for_shard: Arc<parking_lot::RwLock<StatementHnswIndex>> = Arc::new(
                parking_lot::RwLock::new(
                    StatementHnswIndex::new(StatementHnswParams::default_v1())
                        .expect("StatementHnswIndex::new"),
                ),
            );
            // Per-shard EntityHnswIndex. Read by the extractor's
            // resolver Tier 3b (embedding tie-break) and written by
            // the resolver itself on every new entity create. In-
            // memory only — on restart the index reseeds via the
            // resolver's tier-4 path the first time each surface
            // form is re-extracted.
            let entity_hnsw_for_shard: Arc<parking_lot::RwLock<EntityHnswIndex>> = Arc::new(
                parking_lot::RwLock::new(
                    EntityHnswIndex::new(EntityHnswParams::default_v1())
                        .expect("EntityHnswIndex::new"),
                ),
            );
            // Per-shard HyPE pool (hypothetical-question embeddings).
            // Written by the extractor worker's HyPE generator when
            // generation is enabled; probed by the semantic retriever on
            // every memory search. In-memory only — rebuilt below from the
            // durable `hype_question_vectors` rows. Always constructed: a
            // read probe of an empty pool is a cheap no-op, so reads can
            // probe unconditionally even on a shard that isn't generating.
            let hype_hnsw_for_shard: Arc<parking_lot::RwLock<HypeHnswIndex>> = Arc::new(
                parking_lot::RwLock::new(
                    HypeHnswIndex::new(brain_index::hype_default_params())
                        .expect("HypeHnswIndex::new"),
                ),
            );
            // Per-shard per-statement question-bridge pool. Always
            // constructed: the bridge is load-bearing for slot / temporal
            // reads exactly like HyPE — a write step that populates it and a
            // read step that probes it must always agree, so there is no
            // provisioning gate. Rebuilt below from the durable
            // `statement_question_vectors` rows; probing an empty pool is a
            // cheap no-op on a shard that hasn't generated any yet.
            let statement_question_hnsw_for_shard: Arc<
                parking_lot::RwLock<StatementQuestionHnswIndex>,
            > = Arc::new(parking_lot::RwLock::new(
                StatementQuestionHnswIndex::new(
                    brain_index::statement_question_hnsw::statement_question_default_params(),
                )
                .expect("StatementQuestionHnswIndex::new"),
            ));
            // Per-shard semantic retriever. Reuses the executor's
            // embedder + the shared memory HNSW reader. The statement
            // HNSW handle lets the retriever fan out to the statement
            // corpus when `SemanticScope::Statement` or
            // `SemanticScope::Both` is requested.
            let semantic_retriever_concrete =
                brain_ops::index::semantic_retriever::BrainSemanticRetriever::new(
                    dispatcher.clone(),
                    hnsw_shared.clone(),
                    Some(statement_hnsw_for_shard.clone()),
                    metadata.clone(),
                )
                .with_hype_index(hype_hnsw_for_shard.clone())
                .with_statement_question_index(statement_question_hnsw_for_shard.clone());
            let semantic_retriever_for_ops: Arc<dyn brain_index::SemanticRetriever> =
                Arc::new(semantic_retriever_concrete);
            // Per-shard graph retriever. Reads from the entity /
            // relation / statement redb tables.
            let graph_retriever_for_ops: Arc<dyn brain_index::GraphRetriever> = Arc::new(
                brain_ops::index::graph_retriever::BrainGraphRetriever::new(metadata.clone()),
            );
            // Per-shard writer wraps metadata + hnsw_writer. The
            // shard_id stamp on `reserve_memory_id` is required —
            // without it every MemoryId claims shard 0, and
            // dispatch::shard_for_memory routes LINK / UNLINK /
            // FORGET to shard 0 regardless of where the row lives.
            //
            // The shared `event_bus` is also handed to OpsContext
            // below so the writer's commit-time publishes land on
            // the same bus the SubscriptionRegistry listens on —
            // without this link, SUBSCRIBE clients silently see
            // zero events.
            let event_bus = Arc::new(brain_ops::subscribe::EventBus::default());
            // Wire the WAL sink. The sender lives on the writer
            // (Send + Sync), the receiver is drained by a Glommio-
            // local task spawned after the Wal is open (see "WAL
            // drain task" below). Without this link the writer
            // silently falls back to the legacy bus-stamped-LSN
            // path and subscribe --start-lsn finds an empty log.
            let (wal_sink, wal_drain_rx) = brain_ops::writer::channel_wal_sink();
            let wal_sink_for_ops: Arc<dyn brain_ops::writer::WalSink> = wal_sink.clone();

            // Per-shard AutoEdgeWorker channel. The sender
            // lives on the writer; the worker (registered below in
            // register_phase8_workers) drains the receiver every
            // `interval_ms`. We construct the channel here so the
            // writer can be stamped before being wrapped in
            // `Arc<dyn WriterHandle>` (the trait surface intentionally
            // doesn't expose the sender setter).
            let auto_edge_spawn_cfg = auto_edge_spawn_cfg_for_closure.clone();
            let (auto_edge_sender, auto_edge_receiver) = if auto_edge_spawn_cfg.enabled {
                let (tx, rx) = flume::bounded::<brain_ops::AutoEdgeEnqueue>(
                    auto_edge_spawn_cfg.channel_capacity.max(1),
                );
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };

            // Per-shard ExtractorWorker channel. Provisioned iff the extraction
            // pipeline is enabled (≥1 tier on) — no tiers means no channel, no
            // worker, no overhead. The writer stores the Sender; the Receiver
            // moves into the worker we register below.
            let extractor_spawn_cfg = extractor_spawn_cfg_for_closure.clone();
            let (extractor_sender, extractor_receiver) = if extractor_pipeline_enabled {
                let (tx, rx) = flume::bounded::<brain_ops::ExtractorEnqueue>(
                    extractor_spawn_cfg.channel_capacity.max(1),
                );
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };

            // Per-shard TemporalEdgeWorker channel. Mirrors
            // the auto-edge shape; disabled → no channel, no worker.
            let temporal_edge_spawn_cfg = temporal_edge_spawn_cfg_for_closure.clone();
            let (temporal_edge_sender, temporal_edge_receiver) = if temporal_edge_spawn_cfg.enabled
            {
                let (tx, rx) = flume::bounded::<brain_ops::TemporalEdgeEnqueue>(
                    temporal_edge_spawn_cfg.channel_capacity.max(1),
                );
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };

            // Per-shard CausalEdgeWorker channel. Driven by
            // the ExtractorWorker (statement-create post-commit), not
            // the encode-time writer. The channel is created here so
            // the extractor can be stamped with the sender before
            // worker registration moves the receiver into the worker.
            let causal_edge_spawn_cfg = causal_edge_spawn_cfg_for_closure.clone();
            let (causal_edge_sender, causal_edge_receiver) = if causal_edge_spawn_cfg.enabled {
                let (tx, rx) = flume::bounded::<brain_ops::CausalEdgeEnqueue>(
                    causal_edge_spawn_cfg.channel_capacity.max(1),
                );
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };

            // Per-shard ForgetCascadeWorker channel. Unlike the edge /
            // extractor workers above this is a correctness worker, not a
            // feature: every FORGET must re-derive or tombstone the
            // statements citing the forgotten memory, so the channel +
            // worker are always created (no enable flag). The writer
            // enqueues a job post-commit on each `Phase::Tombstone(Memory)`;
            // a full queue drops the job and bumps a metric (the FORGET
            // itself still succeeds), per the drop-on-overflow discipline
            // shared by every non-text-indexer typed-graph worker.
            let forget_cascade_metrics = Arc::new(brain_ops::ForgetCascadeMetrics::new());
            let (forget_cascade_sender, forget_cascade_receiver) =
                flume::bounded::<brain_ops::ForgetCascadeJob>(1024);

            // Per-shard SchemaMigrationWorker channel. Always wired: a
            // SCHEMA_UPLOAD that narrows a namespace must flag the
            // statements/relations now outside it (the OUTSIDE_ACTIVE_SCHEMA
            // sweep), so the writer enqueues a `SchemaFlagSweepJob`
            // post-commit and this worker drains it. Both ends share one
            // metrics Arc so the writer's enqueue-drop counter and the
            // worker's sweep counts surface together.
            let schema_migration_metrics = Arc::new(brain_ops::SchemaMigrationMetrics::new());
            let (schema_flag_sweep_sender, schema_flag_sweep_receiver) =
                flume::bounded::<brain_ops::SchemaFlagSweepJob>(1024);

            // Materialise the persisted `EXTRACTORS_TABLE`
            // rows (seeded by the system-schema bootstrap at
            // MetadataDb::open) into a runtime ExtractorRegistry.
            //
            // The LLM-tier deps the materializer
            // needs: a `ModelRouter` built from the single provider key
            // (`[llm] api_key`, into which `BRAIN__LLM__API_KEY` folds at
            // load) and the per-shard `llm_cache.redb`. Both slots default
            // to `None` so shards started without an LLM cache or any key
            // configured stay unchanged.
            let llm_deps =
                llm_setup::build_llm_deps(&shard_dir_for_executor, &llm_config_for_closure);
            // The LLM is mandatory. The operator-facing hard gate lives at
            // config load (`Config::validate_llm_provider`): a keyless
            // server refuses to boot. `spawn_shard` is the lower-level API
            // beneath that gate; if a caller reaches here without a provider
            // key, HyPE and the write path cannot run, so we warn loudly
            // rather than silently degrade. Production always passes the
            // config gate first, so this WARN never fires there.
            if llm_deps.primary_client.is_none() {
                tracing::warn!(
                    target: "brain_server::shard",
                    shard_id,
                    "no LLM provider key resolved; HyPE (mandatory) and the LLM write \
                     path are inoperative on this shard. Set BRAIN__LLM__API_KEY or \
                     `[llm] api_key`. The server entry point enforces this as a hard \
                     startup error (Config::validate_llm_provider)",
                );
            }
            let llm_cache_for_ops = llm_deps.cache.clone();
            // Snapshot the disambiguator before `llm_deps` is consumed
            // by `into_materialize_deps` below — the extractor worker
            // wires it directly into the resolver path so ambiguous-
            // band partial matches get a second opinion.
            let entity_disambiguator_for_worker = llm_deps.disambiguator.clone();
            // Snapshot the primary LLM client + model for the HyPE
            // generator, before `llm_deps` is consumed below. `None` when
            // no provider key resolved — HyPE generation then stays off.
            let hype_client_for_worker = llm_deps.primary_client.clone();
            let hype_model_for_worker = llm_deps.primary_model.clone();
            // Dedicated cache handle for the HyPE generator: `llm_cache_for_ops`
            // is moved into OpsContext below, so snapshot the Arc now.
            let hype_cache_for_worker = llm_deps.cache.clone();

            // Tantivy is opened (and any post-recovery rebuilds run)
            // before the executor closure spawns — see "Tantivy open
            // + recovery" above. We just consume the pre-built handle
            // here. `IndexStatus::NeedsRebuild` is *not* a spawn
            // failure: the maintenance worker handles steady-state
            // rebuilds while the live indexes keep serving reads.
            let tantivy_for_ops = tantivy_for_closure;

            // Resolve the classifier model config. Cascades through
            // `[extractors.classifier] model_path` (operator override)
            // and the XDG default location populated by
            // `.devcontainer/bootstrap-model.sh`, mirroring the bootstrap
            // script's own resolution order so an operator who ran the
            // script gets a working classifier on the next boot without
            // exporting an env var. Built once and reused: the loaded
            // `Arc<dyn ClassifierModel>` feeds the `MaterializeDeps`
            // (so classifier-kind extractor rows decode into wired
            // extractors instead of degraded ones) and the same
            // `ClassifierConfig` stays on the OpsContext for diagnostic
            // reporting.
            // An explicit `[extractors.classifier] model_path` override
            // wins; otherwise fall back to the XDG-cascade discovery the
            // bootstrap script writes to. The configured acceptance
            // threshold is stamped on whichever config wins.
            let mut classifier_config =
                match &extractor_tuning_spawn_cfg.classifier_model_path {
                    Some(path) if !path.is_empty() => {
                        brain_extractors::ClassifierConfig::with_model_path(
                            std::path::PathBuf::from(path),
                        )
                    }
                    _ => brain_extractors::ClassifierConfig::auto_discover(),
                };
            classifier_config.threshold = extractor_tuning_spawn_cfg.classifier_threshold;

            // Load the NER backbone if the operator configured one.
            // Fail-stop when the path is set but loading fails:
            // silently degrading to the pattern-only tier when the
            // operator asked for the classifier produces wrong audit
            // status on every ENCODE and hides the misconfiguration.
            let classifier_model: Option<Arc<dyn brain_extractors::ClassifierModel>> =
                if classifier_config.has_path() {
                    let m = brain_extractors::GlinerClassifier::load(&classifier_config)
                        .unwrap_or_else(|e| {
                            panic!(
                                "classifier model load failed at {}: {e}",
                                classifier_config.model_path().display()
                            )
                        });
                    tracing::info!(
                        target: "brain_server::shard",
                        model_path = %classifier_config.model_path().display(),
                        "classifier tier wired",
                    );
                    Some(Arc::new(m))
                } else {
                    // Surface where Brain *would* have looked so the
                    // operator can see the install convention at a
                    // glance. Falls back to a hint when neither HOME
                    // nor XDG_DATA_HOME is available — at which point
                    // the explicit env var is the only path in.
                    let expected = brain_extractors::default_xdg_model_dir()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<unknown: set HOME or XDG_DATA_HOME>".to_string());
                    tracing::info!(
                        target: "brain_server::shard",
                        expected = %expected,
                        "classifier tier inactive (no model at default path or via \
                         [extractors.classifier] model_path); only the pattern tier will contribute",
                    );
                    None
                };

            // Captured out of the registry-build block below so the
            // extractor worker can rebuild the registry live on a
            // SCHEMA_UPLOAD without a restart (see
            // `ExtractorWorker::with_registry_rebuild_deps`). Assigned
            // unconditionally inside the block before it returns.
            let extractor_rebuild_deps: brain_extractors::MaterializeDeps;
            let extractor_registry = {
                let rtxn = metadata
                    .read_txn()
                    .expect("read_txn after MetadataDb::open");
                let defs =
                    brain_metadata::extractor_list(&rtxn).expect("extractor_list at shard startup");
                // Snapshot the active schema's entity-type qnames.
                // GLiNER is zero-shot: these are the per-call labels
                // we pass to every `predict()`. Reading once at
                // startup matches every other registry-shaped state
                // on the shard.
                let entity_type_qnames = Arc::new(
                    snapshot_entity_type_qnames(&rtxn)
                        .expect("entity-type snapshot at shard startup"),
                );
                drop(rtxn);

                let materialize_deps = llm_deps
                    .into_materialize_deps(classifier_model, entity_type_qnames);
                // Stash a clone for the worker's live rebuild path; the
                // build below only borrows it.
                extractor_rebuild_deps = materialize_deps.clone();
                let (mut reg, errors) = brain_extractors::build_registry_with_gate(
                    &defs,
                    &materialize_deps,
                    tier_gate_for_closure,
                );
                if !errors.is_empty() {
                    // An extractor tier that fails to materialise is a hard
                    // spawn failure, not a silent degrade: a shard serving
                    // with a quietly-missing tier returns wrong audit status
                    // on every ENCODE and hides the misconfiguration. All
                    // three tiers are always materialised (extraction is
                    // always-on), so every error is a genuine init failure or
                    // a corrupt definition — never an operator opt-out.
                    // Fail-stop, the same convention the classifier-model load
                    // above uses.
                    let detail = errors
                        .iter()
                        .map(|(id, err)| format!("extractor {}: {err}", id.raw()))
                        .collect::<Vec<_>>()
                        .join("; ");
                    panic!(
                        "enabled extractor tier(s) failed to initialise at shard spawn: {detail}"
                    );
                }
                // Register the native built-in temporal-expressions
                // extractor (pattern tier, deterministic date arithmetic
                // — not materialisable from a regex schema def). Always
                // wired; runs under the pattern-tier gate.
                reg.register(std::sync::Arc::new(
                    brain_extractors::TemporalExtractor::new(),
                ));
                reg
            };

            // Spawn the per-shard text indexer drain
            // tasks and install their dispatchers. The writer holds
            // the memory dispatcher so single-op ENCODE and TXN
            // batches share one dispatch point; OpsContext also
            // carries it so FORGET (which doesn't go through the
            // writer's post-commit hook) can tombstone the row.
            // No-schema deployments (no tantivy handle) skip
            // both.
            // Held (not detached) so `shard_main_loop` can flush the
            // lexical indexes before the executor drops pending tasks.
            let mut __memory_text_task: Option<(flume::Sender<()>, glommio::Task<()>)> = None;
            let mut __statement_text_task: Option<(flume::Sender<()>, glommio::Task<()>)> = None;
            // Control-plane senders for the hot tantivy rebuild. `Some`
            // only when the matching indexer spawned.
            let mut __memory_text_control: Option<
                flume::Sender<brain_ops::index::text_indexer::IndexerControl>,
            > = None;
            let mut __statement_text_control: Option<
                flume::Sender<brain_ops::index::text_indexer::IndexerControl>,
            > = None;
            let (memory_text_dispatcher_for_ops, statement_text_dispatcher_for_ops) = {
                let policy = brain_ops::index::text_indexer::CommitPolicy::new(
                    index_spawn_cfg.tantivy_commit_n.max(1),
                    std::time::Duration::from_millis(index_spawn_cfg.tantivy_commit_ms.max(1)),
                );

                let memory_dispatcher = {
                    let (dispatcher, receiver) =
                        brain_ops::index::text_indexer::MemoryTextDispatcher::default_channel();
                    let (stop_tx, stop_rx) = flume::bounded::<()>(1);
                    // Control channel is unbounded-ish (cap 4): the rebuild
                    // sends at most a Quiesce then a Resume at a time.
                    let (control_tx, control_rx) =
                        flume::bounded::<brain_ops::index::text_indexer::IndexerControl>(4);
                    match brain_ops::index::text_indexer::memory::spawn_memory_text_indexer_local(
                        tantivy_for_ops.memory_text.clone(),
                        receiver,
                        policy,
                        stop_rx,
                        control_rx,
                    ) {
                        Ok(task) => {
                            __memory_text_task = Some((stop_tx, task));
                            __memory_text_control = Some(control_tx);
                            Some(Arc::new(dispatcher))
                        }
                        Err(err) => {
                            tracing::error!(
                                target: "brain_server::shard",
                                error = %err,
                                "memory text indexer spawn failed; lexical writes unavailable",
                            );
                            None
                        }
                    }
                };

                let statement_dispatcher = {
                    let (dispatcher, receiver) =
                        brain_ops::index::text_indexer::StatementTextDispatcher::default_channel();
                    let (stop_tx, stop_rx) = flume::bounded::<()>(1);
                    let (control_tx, control_rx) =
                        flume::bounded::<brain_ops::index::text_indexer::IndexerControl>(4);
                    match brain_ops::index::text_indexer::statement::spawn_statement_text_indexer_local(
                        tantivy_for_ops.statements.clone(),
                        receiver,
                        policy,
                        stop_rx,
                        control_rx,
                    ) {
                        Ok(task) => {
                            __statement_text_task = Some((stop_tx, task));
                            __statement_text_control = Some(control_tx);
                            Some(Arc::new(dispatcher))
                        }
                        Err(err) => {
                            tracing::error!(
                                target: "brain_server::shard",
                                error = %err,
                                "statement text indexer spawn failed; lexical writes unavailable",
                            );
                            None
                        }
                    }
                };

                (memory_dispatcher, statement_dispatcher)
            };

            // Per-shard redb-committed-LSN watermark. The writer advances
            // it after each successful `wtxn.commit()`; the snapshot
            // source reads the SAME handle for `CHECKPOINT_END.durable_lsn`
            // (see `ShardSnapshotSource`). Seed it to the post-recovery
            // committed tail: recovery replays every WAL record up to
            // `next_lsn - 1` into redb and commits, so that LSN is durable
            // in metadata at boot. A fresh shard seeds 0.
            let redb_committed_watermark = brain_ops::RedbCommittedWatermark::new();
            redb_committed_watermark.advance_to(next_lsn_after_recovery.saturating_sub(1));

            let mut real_writer = RealWriterHandle::new(metadata.clone(), hnsw_writer)
                .with_shard_id(shard_id)
                .with_event_bus(event_bus.clone())
                .with_wal_sink(wal_sink)
                .with_redb_committed_watermark(redb_committed_watermark.clone());
            // Seed the slot counter from the persisted high-water mark so a
            // restart on a non-empty shard never re-issues a live arena slot.
            // (The counter resets to 1 in-process; without this, restart-reuse
            // collides memory_ids — overwriting rows and skipping extraction.)
            match metadata.read_txn() {
                Ok(rtxn) => {
                    match brain_metadata::tables::slot_version::max_assigned_slot(&rtxn) {
                        Ok(hi) => real_writer.seed_next_slot(hi.saturating_add(1)),
                        Err(e) => tracing::warn!(
                            error = %e,
                            "slot high-water recovery failed; slot counter starts at 1"
                        ),
                    }
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    "slot high-water recovery read_txn failed; slot counter starts at 1"
                ),
            }
            if let Some(tx) = auto_edge_sender {
                real_writer.set_auto_edge_sender(tx);
            }
            if let Some(tx) = extractor_sender {
                real_writer.set_extractor_sender(tx);
            }
            if let Some(tx) = temporal_edge_sender {
                real_writer.set_temporal_edge_sender(tx);
            }
            if let Some(m) = auto_edge_metrics_for_closure.clone() {
                real_writer.set_auto_edge_metrics(m);
            }
            if let Some(m) = extractor_metrics_for_closure.clone() {
                real_writer.set_extractor_metrics(m);
            }
            if let Some(m) = temporal_edge_metrics_for_closure.clone() {
                real_writer.set_temporal_edge_metrics(m);
            }
            if let Some(d) = memory_text_dispatcher_for_ops.clone() {
                real_writer.set_memory_text_dispatcher(d);
            }
            // Always wired: the FORGET cascade is correctness, not an
            // optional feature. Both ends share one metrics Arc so the
            // writer's drop counter and the worker's per-cascade counts
            // surface together.
            real_writer.set_forget_cascade_sender(forget_cascade_sender);
            real_writer.set_forget_cascade_metrics(forget_cascade_metrics.clone());
            // Always wired: the schema-flag sweep is correctness (keeps the
            // OUTSIDE_ACTIVE_SCHEMA flag accurate after a narrowing upload),
            // not an optional feature.
            real_writer.set_schema_flag_sweep_sender(schema_flag_sweep_sender);
            real_writer.set_schema_flag_sweep_metrics(schema_migration_metrics.clone());
            let writer: Arc<dyn WriterHandle> = Arc::new(real_writer);
            // Wrap the arena in `Rc<RefCell<…>>` now (rather than after WAL
            // open below) so the by-slot vector source can be built and
            // handed to the ExecutorContext here. Single-threaded executor →
            // sound; the discipline is "drop the borrow before .await", and
            // `arena` isn't touched again before this point.
            let arena_cell = Rc::new(RefCell::new(arena));

            // Construct the resumable BackfillWorker up front so the same
            // `Arc` can be both threaded onto the executor context (giving
            // the ADMIN_BACKFILL / ADMIN_BACKFILL_CANCEL dispatch path a
            // submit/cancel handle) and registered in the scheduler below
            // (which drives its checkpoint walk). It's a provisioned C2
            // worker — `run_cycle` no-ops when no run is active — and not
            // in the C0 always-on set, so it stays controllable.
            let backfill_worker =
                Arc::new(brain_workers::workers::backfill::BackfillWorker::new());

            let executor_ctx = ExecutorContext::new(
                dispatcher.clone(),
                hnsw_shared.clone(),
                metadata.clone(),
                writer,
            )
            // Per-space brute-force retrieval lane: exact cosine scan of a
            // small single-space query's own arena vectors.
            .with_space_vectors(Rc::new(ArenaSpaceVectorSource::new(arena_cell.clone())))
            // Backfill control handle — same Arc registered in the
            // scheduler below.
            .with_backfill_handle(backfill_worker.clone());

            // The lexical retriever was constructed alongside the
            // tantivy open above and propagated in here pre-built.
            // Retrieval consumes it via
            // `OpsContext.lexical_retriever`.
            let lexical_retriever_for_ops = lexical_retriever_for_closure;

            let ops = Arc::new(
                OpsContext::new(
                    executor_ctx,
                    lexical_retriever_for_ops,
                    semantic_retriever_for_ops,
                    graph_retriever_for_ops,
                )
                .with_event_bus(event_bus.clone())
                .with_extractor_registry(extractor_registry)
                .with_classifier_config(classifier_config)
                .with_llm_cache(llm_cache_for_ops)
                .with_tantivy(Some(tantivy_for_ops))
                .with_memory_text_dispatcher(memory_text_dispatcher_for_ops)
                .with_statement_text_dispatcher(statement_text_dispatcher_for_ops)
                .with_cross_encoder(cross_encoder_for_closure)
                .with_wal_sink(Some(wal_sink_for_ops))
                .with_recall_metrics(
                    retriever_metrics_for_closure.clone(),
                    query_metrics_for_closure.clone(),
                ),
            );

            // Spawn the per-shard fanout task: drains the in-process
            // broadcast EventBus (`ops.events`) into the cross-shard
            // flume Sender we set up before entering the closure. The
            // connection layer reads the matching Receiver via
            // `ShardHandle::events()`.
            //
            // `tokio::sync::broadcast::Receiver` is runtime-agnostic
            // (atomics + Waker, no tokio I/O); polling its `recv()`
            // future inside Glommio is sound. `Lagged` means this
            // fanout receiver fell behind and the broadcast buffer
            // overwrote events before we forwarded them: we can't
            // recover the dropped envelopes, but we must not swallow
            // the gap — `record_fanout_lag` counts + warns so a live
            // subscriber's missing LSN is observable (resync required).
            let mut __fanout_task: Option<glommio::Task<()>> = None;
            {
                let event_bus = ops.events.clone();
                let events_tx = events_tx.clone();
                __fanout_task = Some(glommio::spawn_local(async move {
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
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                                record_fanout_lag(shard_id, skipped);
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }));
            }

            // Open or create the WAL. A genuine IO failure here (bad perms,
            // ENOSPC, an FS without O_DIRECT/io_uring support) must fail the
            // spawn rather than panic this thread behind an already-returned
            // `Ok` from `spawn`. Report the outcome over `wal_ready_tx`;
            // `spawn_shard` blocks on the receiver and turns an `Err` into a
            // spawn failure. On the error path we end the closure early so the
            // executor thread exits cleanly instead of unwinding.
            let wal_open_result = if segments_present {
                Wal::open_existing(
                    &wal_dir_for_executor,
                    shard_uuid,
                    next_lsn_after_recovery,
                    recovered_tail_offset,
                    wal_config,
                )
                .await
            } else {
                Wal::create_with_config(&wal_dir_for_executor, shard_uuid, wal_config).await
            };
            let wal = match wal_open_result {
                Ok(wal) => {
                    // Best-effort: if the receiver is gone the spawn was
                    // already abandoned, so there is nothing to serve.
                    let _ = wal_ready_tx.send(Ok(()));
                    wal
                }
                Err(err) => {
                    let _ = wal_ready_tx.send(Err(err));
                    return;
                }
            };

            // Wrap the WAL in `Rc<RefCell<…>>` so adapters can share the
            // handle with the main loop. `arena_cell` was wrapped earlier
            // (before the ExecutorContext build). Single-threaded executor →
            // sound; the discipline is "drop the borrow before .await".
            let wal_cell = Rc::new(RefCell::new(Some(wal)));

            // WAL drain task: forwards every record the writer's
            // `ChannelWalSink` enqueues to the real `Wal::append`,
            // replying with the assigned LSN over the per-call
            // oneshot. Lives on this executor so it can share the
            // `Rc<RefCell<Option<Wal>>>` with the main loop's
            // `AppendWalRecord` handler — both serialise on the
            // RefCell, and single-threaded scheduling means a
            // `.await` inside the drain task doesn't conflict with
            // the main loop's borrow.
            //
            // The task ends when the sender (held inside the
            // writer's `Arc<dyn WalSink>`) is dropped — which happens
            // at shard shutdown when the `OpsContext` Arc count
            // hits zero.
            let wal_cell_for_drain = wal_cell.clone();
            let __wal_drain_task = glommio::spawn_local(async move {
                while let Ok(msg) = wal_drain_rx.recv_async().await {
                    let outcome = {
                        let guard = wal_cell_for_drain.borrow();
                        match guard.as_ref() {
                            Some(wal) => wal.append_many(msg.records).await.map_err(|e| {
                                brain_ops::writer::WalSinkError::Internal(format!("{e}"))
                            }),
                            None => Err(brain_ops::writer::WalSinkError::Disconnected),
                        }
                    };
                    let _ = msg.reply.send(outcome);
                }
            });

            // Build real worker adapters.
            //
            // Two rebuild sources, by lifecycle phase:
            //
            // - `rebuild_source` (arena) feeds the *boot recovery* rebuild
            //   below. On restart the arena is the freshly-WAL-replayed image
            //   of every durable memory, so it is the authoritative substrate
            //   here.
            // - `redb_rebuild_source` feeds every *runtime* rebuild (the HNSW
            //   maintenance worker + the admin `rebuild-ann` route). The arena
            //   is never written by the live encode path, so a runtime rebuild
            //   from it would silently drop every same-run memory; redb holds
            //   the durable write-time vector for the complete live set.
            let rebuild_source: Arc<dyn RebuildSource<{ VECTOR_DIM }>> = Arc::new(
                ArenaRebuildSource::<{ VECTOR_DIM }>::new(shard_id, arena_cell.clone()),
            );
            let redb_rebuild_source: Arc<dyn RebuildSource<{ VECTOR_DIM }>> =
                Arc::new(RedbRebuildSource::<{ VECTOR_DIM }>::new(metadata.clone()));
            // Keep a clone for the admin `rebuild-ann` route.
            let rebuild_source_for_shard = redb_rebuild_source.clone();

            // Recovery step 6: restore the memory HNSW.
            // Try snapshot-load first; on any failure (missing, CRC /
            // version / shard_uuid mismatch, hnsw_rs deserialization),
            // fall through to a full rebuild from the arena. The
            // fallback is the same code that landed in c500012 — it's
            // the original v1 path and is correct on its own.
            let snapshot_loaded = match find_latest_snapshot_dir(&snapshots_root_for_executor) {
                Some(snap_dir) => match brain_index::SharedHnsw::load_snapshot(
                    &snap_dir,
                    "hnsw",
                    shard_uuid,
                ) {
                    Ok((loaded_idx, taken_at_lsn)) => {
                        let loaded_len = loaded_idx.len();
                        // Capture the snapshot's memory ids BEFORE the swap
                        // moves the index into the published main. Any of
                        // these that redb marks inactive as of the recovered
                        // tail is a memory FORGOTTEN after `taken_at_lsn`:
                        // the snapshot still holds it active, so it must be
                        // re-tombstoned below or it becomes a ghost node (a
                        // live top-k slot diverging from redb indefinitely).
                        let snapshot_ids: Vec<brain_core::MemoryId> = loaded_idx
                            .id_map()
                            .iter_forward()
                            .map(|(bytes, _)| brain_core::MemoryId::from_be_bytes(bytes))
                            .collect();
                        hnsw_shared.swap(loaded_idx);
                        // Tail-replay: any arena entry whose memory_id
                        // isn't in the loaded main is a write that
                        // landed between `taken_at_lsn` and the crash.
                        // Push them into pending via the recovery insert
                        // path — boot is single-threaded so this is safe.
                        let mut tail = 0usize;
                        match rebuild_source.snapshot_vectors().await {
                            Ok(arena_vectors) => {
                                for (mid, v) in arena_vectors {
                                    if !hnsw_shared.contains(mid) {
                                        hnsw_shared.insert_recovery(mid, &v);
                                        tail += 1;
                                    }
                                }
                                info!(
                                    shard_id,
                                    taken_at_lsn,
                                    loaded = loaded_len,
                                    tail_replayed = tail,
                                    snap_dir = %snap_dir.display(),
                                    "memory HNSW: loaded snapshot + tail-replayed arena"
                                );
                            }
                            Err(e) => warn!(
                                shard_id,
                                error = ?e,
                                "memory HNSW: tail-replay arena scan failed; loaded snapshot \
                                 alone may miss writes past taken_at_lsn"
                            ),
                        }
                        // Post-snapshot FORGET reconciliation: re-apply every
                        // delete that landed after `taken_at_lsn`. The arena
                        // tail above only carries ACTIVE memories, so an
                        // inactive redb row for a snapshot id is invisible to
                        // it; tombstone those ids so the HNSW active set
                        // converges to the redb active set.
                        match reconcile_forgotten_memories(
                            &hnsw_shared,
                            &metadata,
                            &snapshot_ids,
                        ) {
                            Ok(retombstoned) if retombstoned > 0 => info!(
                                shard_id,
                                retombstoned,
                                "memory HNSW: re-applied post-snapshot FORGETs"
                            ),
                            Ok(_) => {}
                            Err(e) => warn!(
                                shard_id,
                                error = %e,
                                "memory HNSW: FORGET reconciliation failed; snapshot may \
                                 retain ghost nodes"
                            ),
                        }
                        true
                    }
                    Err(e) => {
                        warn!(
                            shard_id,
                            error = %e,
                            snap_dir = %snap_dir.display(),
                            "memory HNSW: snapshot load failed; falling back to arena rebuild"
                        );
                        false
                    }
                },
                None => false, // no snapshot exists yet — fresh shard or
                                // checkpoint hasn't run; full rebuild
                                // below is the v1 path.
            };

            if !snapshot_loaded {
                // Fallback: full rebuild from the arena. This is the v1
                // recovery path — correct on its own; it just costs
                // O(N·log N) graph-build time vs O(load) for the
                // snapshot path.
                match rebuild_source.snapshot_vectors().await {
                    Ok(vectors) if !vectors.is_empty() => {
                        let params = hnsw_shared.params();
                        let reseeded = vectors.len();
                        let outcome =
                            hnsw_shared.flush_with_rebuild(move |pending_snapshot| {
                                let mut combined = vectors;
                                // Pending is empty pre-serving, but fold
                                // defensively so a stray insert can't be
                                // lost: arena vectors are authoritative.
                                let arena_ids: std::collections::HashSet<brain_core::MemoryId> =
                                    combined.iter().map(|(id, _)| *id).collect();
                                for entry in pending_snapshot {
                                    if !entry.tombstoned
                                        && !arena_ids.contains(&entry.memory_id)
                                    {
                                        combined.push((entry.memory_id, entry.vector));
                                    }
                                }
                                let (idx, _) =
                                    brain_index::rebuild::rebuild_impl(params, combined)?;
                                Ok(idx)
                            });
                        match outcome {
                            Ok(report) => info!(
                                shard_id,
                                reseeded,
                                new_epoch = report.new_epoch,
                                "memory HNSW rebuilt from arena on startup (fallback)"
                            ),
                            Err(e) => error!(
                                shard_id,
                                error = ?e,
                                "memory HNSW startup rebuild failed; semantic recall \
                                 degraded until the next maintenance rebuild"
                            ),
                        }
                    }
                    Ok(_) => {
                        info!(
                            shard_id,
                            "no arena vectors to rebuild; memory HNSW starts empty"
                        );
                    }
                    Err(e) => error!(
                        shard_id,
                        error = ?e,
                        "memory HNSW startup rebuild: arena snapshot failed; semantic \
                         recall degraded"
                    ),
                }
            }

            // Recovery: rebuild the entity / HyPE / statement-question
            // HNSW indexes from the authoritative redb tables. All three
            // are in-RAM only (not persisted), so without this they start
            // empty and the resolver + question-bridge reads degrade until
            // each surface is re-extracted. Boot and the on-demand admin
            // `RebuildIndex` route share one implementation in
            // `shard::rebuild`; boot logs any failure and continues (a
            // degraded index is better than refusing to start).
            self::rebuild::log_boot_result(
                shard_id,
                "entity HNSW",
                self::rebuild::rebuild_entity_hnsw(
                    &entity_hnsw_for_shard,
                    &metadata,
                    dispatcher.as_ref(),
                    shard_id,
                ),
            );
            self::rebuild::log_boot_result(
                shard_id,
                "HyPE HNSW",
                self::rebuild::rebuild_hype_hnsw(&hype_hnsw_for_shard, &metadata, shard_id),
            );
            self::rebuild::log_boot_result(
                shard_id,
                "statement question-bridge",
                self::rebuild::rebuild_statement_question_hnsw(
                    &statement_question_hnsw_for_shard,
                    &metadata,
                    shard_id,
                ),
            );

            // Recovery: re-enqueue every live statement so the
            // StatementEmbedWorker repopulates the (in-RAM, non-persisted)
            // statement HNSW. Runs in the background off the embed queue, so
            // it doesn't block the serve path; statement-scoped semantic
            // search fills in as the worker drains.
            match metadata.write_txn() {
                Ok(wtxn) => {
                    match brain_metadata::statement::statement_embed_queue_seed_all_live(&wtxn) {
                        Ok(seeded) => match wtxn.commit() {
                            Ok(()) => info!(
                                shard_id,
                                seeded,
                                "statement embed queue seeded from live statements on startup"
                            ),
                            Err(e) => error!(
                                shard_id,
                                error = ?e,
                                "statement embed queue seed commit failed"
                            ),
                        },
                        Err(e) => error!(
                            shard_id,
                            error = ?e,
                            "statement embed queue seed failed; statement semantic search degraded"
                        ),
                    }
                }
                Err(e) => error!(
                    shard_id,
                    error = ?e,
                    "statement embed queue seed: write_txn failed"
                ),
            }

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
                redb_committed_watermark.clone(),
            ));
            // CacheEvictionSource stays Disabled* until a
            // real CachingDispatcher is wired per shard.
            let cache_eviction_source: Arc<dyn CacheEvictionSource> =
                Arc::new(DisabledCacheEvictionSource);

            // Spawn the per-shard scheduler + register all background workers.
            let mut scheduler = WorkerScheduler::new();
            register_phase8_workers(
                &mut scheduler,
                ops.clone(),
                redb_rebuild_source,
                wal_retention_source,
                snapshot_source.clone(),
                cache_eviction_source,
                summarizer,
            )
            .expect("register Phase-8 workers");

            // BackfillWorker — provisioned unconditionally (C2). It idles
            // (run_cycle returns 0) until an ADMIN_BACKFILL submits a run
            // via the handle threaded onto the executor context above; the
            // scheduler then drives its checkpoint walk. Registering the
            // same Arc means the dispatch handle and the running worker are
            // one instance, so submit/cancel/progress see the live run.
            scheduler
                .register(backfill_worker.clone(), ops.clone())
                .expect("register BackfillWorker");

            // LLM cache sweeper. Registered when enabled AND the shard
            // actually has an LLM cache — without a cache the worker is a
            // no-op, and the operator can opt out via `enabled = false`.
            if llm_cache_sweep_spawn_cfg.enabled && ops.llm_cache.is_some() {
                let sweeper = LlmCacheSweeper::new()
                    .with_interval_secs(llm_cache_sweep_spawn_cfg.interval_secs)
                    .with_metrics(llm_cache_sweep_metrics_for_closure.clone());
                scheduler
                    .register(Arc::new(sweeper), ops.clone())
                    .expect("register LlmCacheSweeper");
            }

            // StatementEmbedWorker — drains the redb embed queue, runs
            // each pending statement through the BGE dispatcher, and
            // populates the per-shard StatementHnswIndex. Without this
            // worker the retrieval path's statement-corpus semantic
            // retriever returns zero hits and recall degenerates to
            // BM25 + graph only.
            //
            // Registered unconditionally: on a substrate-only shard
            // the queue stays empty (nothing produces statement
            // create / supersede events) and the worker is a 1 s
            // ticking no-op.
            {
                let worker = brain_workers::StatementEmbedWorker::new(
                    metadata.clone(),
                    statement_hnsw_for_shard.clone(),
                    dispatcher.clone(),
                )
                .with_metrics(statement_embed_metrics_for_closure.clone())
                .with_question_bridge(statement_question_hnsw_for_shard.clone());
                scheduler
                    .register(Arc::new(worker), ops.clone())
                    .expect("register StatementEmbedWorker");
            }

            // ConfidenceSweepWorker — periodically re-aggregates the
            // stored confidence on active Statement rows via noisy-OR
            // with kind-specific decay. The query ranker uses this
            // value as a weight, so without the sweep long-running
            // deployments accumulate over-confident stale Facts and
            // Preferences whose evidence has aged out.
            //
            // On by default (low-cost maintenance); operator can opt out via
            // `[workers.confidence_sweep] enabled = false`, which skips
            // registration entirely.
            if confidence_sweep_spawn_cfg.enabled {
                let worker = brain_workers::ConfidenceSweepWorker::new(metadata.clone())
                    .with_interval_secs(confidence_sweep_spawn_cfg.interval_secs)
                    .with_metrics(confidence_sweep_metrics_for_closure.clone());
                scheduler
                    .register(Arc::new(worker), ops.clone())
                    .expect("register ConfidenceSweepWorker");
            }

            // StatementReclaimWorker — physically reclaims retracted
            // statement rows (plus their secondary-index + evidence-
            // overflow entries) after the retract grace period. Off by
            // default (`[workers.statement_reclaim] enabled = false`); gated by
            // skip-registration like every other optional worker — a disabled
            // worker is simply not registered. `set_enabled(true)` overrides the
            // worker's own off-by-default so that, once provisioned, it actually
            // runs. Closes the tombstone-grace-then-reclaim loop on the
            // statement side, mirroring slot reclamation for memories.
            if statement_reclaim_spawn_cfg.enabled {
                let worker = brain_workers::workers::statement_reclaim::StatementReclaimWorker::new()
                    .set_enabled(true)
                    .with_grace_seconds(statement_reclaim_spawn_cfg.grace_seconds)
                    .with_period_seconds(statement_reclaim_spawn_cfg.period_seconds);
                scheduler
                    .register(Arc::new(worker), ops.clone())
                    .expect("register StatementReclaimWorker");
            }

            // SupersessionSweeper — physically reclaims superseded
            // statement rows after the configured retention window. Off
            // by default and gated on `[workers.supersession_sweeper]
            // enabled`: superseded rows are filtered from every read
            // regardless, so this is purely a disk-reclamation opt-in.
            // Mirrors StatementReclaimWorker for the supersession side,
            // closing the supersede-then-reclaim loop instead of letting
            // superseded rows accumulate on disk forever. `retention_seconds`
            // is now tuning (how old before delete), not the on/off gate.
            if supersession_sweeper_spawn_cfg.enabled {
                let mut worker =
                    brain_workers::workers::supersession_sweeper::SupersessionSweeper::new()
                        .with_retention_seconds(supersession_sweeper_spawn_cfg.retention_seconds)
                        .with_period_seconds(supersession_sweeper_spawn_cfg.period_seconds);
                if supersession_sweeper_spawn_cfg.dry_run {
                    worker = worker.dry_run();
                }
                scheduler
                    .register(Arc::new(worker), ops.clone())
                    .expect("register SupersessionSweeper");
            }

            // Register the AutoEdgeWorker when the channel was
            // created above (i.e. when `cfg.auto_edge.enabled` is true).
            // The worker drains the receiver feeding off post-commit
            // encodes and writes `SimilarTo` edges back through the
            // unified edge tables.
            if let Some(rx) = auto_edge_receiver {
                let worker_cfg = WorkerConfig {
                    enabled: auto_edge_spawn_cfg.enabled,
                    interval: std::time::Duration::from_millis(
                        auto_edge_spawn_cfg.interval_ms.max(1),
                    ),
                    batch_size: auto_edge_spawn_cfg.batch_size,
                    max_runtime: std::time::Duration::from_secs(5),
                };
                let knobs = AutoEdgeKnobs {
                    top_k: auto_edge_spawn_cfg.top_k,
                    similarity_threshold: auto_edge_spawn_cfg.similarity_threshold,
                    ef_search: Some(auto_edge_spawn_cfg.ef_search),
                };
                let mut auto_edge_worker = AutoEdgeWorker::new(rx)
                    .with_config(worker_cfg)
                    .with_knobs(knobs);
                if let Some(m) = auto_edge_metrics_for_closure.clone() {
                    auto_edge_worker = auto_edge_worker.with_metrics(m);
                }
                scheduler
                    .register(Arc::new(auto_edge_worker), ops.clone())
                    .expect("register AutoEdgeWorker");
            }

            // Register the TemporalEdgeWorker when its
            // channel was created above. Drains the writer's post-
            // encode channel, looks up the space's prior memory, and
            // writes a decay-weighted `FollowedBy` edge.
            if let Some(rx) = temporal_edge_receiver {
                let worker_cfg = WorkerConfig {
                    enabled: temporal_edge_spawn_cfg.enabled,
                    interval: std::time::Duration::from_millis(
                        temporal_edge_spawn_cfg.interval_ms.max(1),
                    ),
                    batch_size: temporal_edge_spawn_cfg.batch_size,
                    max_runtime: std::time::Duration::from_secs(5),
                };
                let knobs = brain_workers::TemporalEdgeKnobs {
                    window_seconds: temporal_edge_spawn_cfg.window_seconds,
                    weight_min: temporal_edge_spawn_cfg.weight_min,
                    cross_session: temporal_edge_spawn_cfg.cross_session,
                    topical_threshold: temporal_edge_spawn_cfg.topical_threshold,
                };
                let mut temporal_edge_worker = brain_workers::TemporalEdgeWorker::new(rx)
                    .with_config(worker_cfg)
                    .with_knobs(knobs);
                if let Some(m) = temporal_edge_metrics_for_closure.clone() {
                    temporal_edge_worker = temporal_edge_worker.with_metrics(m);
                }
                scheduler
                    .register(Arc::new(temporal_edge_worker), ops.clone())
                    .expect("register TemporalEdgeWorker");
            }

            // Register the ForgetCascadeWorker unconditionally — it drains
            // the cascade channel stamped on the writer above and, for each
            // FORGET, re-derives or tombstones the statements (and edges /
            // relations) citing the forgotten memory. Without it a FORGET
            // tombstones the memory but leaves dependent statements at their
            // pre-FORGET confidence, citing a memory the user deleted.
            // Substrate-only shards never enqueue a job, so the worker is a
            // cheap ticking no-op there.
            {
                let worker = brain_workers::workers::forget_cascade::ForgetCascadeWorker::new(
                    forget_cascade_receiver,
                )
                .with_metrics(forget_cascade_metrics.clone());
                scheduler
                    .register(Arc::new(worker), ops.clone())
                    .expect("register ForgetCascadeWorker");
            }

            // SchemaMigrationWorker — drains the writer's post-commit
            // `SchemaFlagSweepJob` channel and (re)flags statements /
            // relations that fall outside the active schema after a
            // narrowing SCHEMA_UPLOAD. Without it the OUTSIDE_ACTIVE_SCHEMA
            // flag never updates and ADMIN_LIST_STALE_STATEMENTS goes blind.
            // Shares the metrics Arc handed to the writer above.
            {
                let worker = brain_workers::workers::schema_migration::SchemaMigrationWorker::new(
                    schema_flag_sweep_receiver,
                )
                .with_metrics(schema_migration_metrics.clone());
                scheduler
                    .register(Arc::new(worker), ops.clone())
                    .expect("register SchemaMigrationWorker");
            }

            // AuditLogSweeper — enforces the extractor-audit retention
            // window (default 90d). Without it the extractor-audit table
            // grows unbounded on long-running shards.
            {
                let worker = brain_workers::workers::audit_log_sweeper::AuditLogSweeper::new();
                scheduler
                    .register(Arc::new(worker), ops.clone())
                    .expect("register AuditLogSweeper");
            }

            // StaleExtractionDetector — counts statements whose
            // `schema_version` trails the active schema and exposes the
            // total via metrics so operators can see extraction drift after
            // a narrowing SCHEMA_UPLOAD. On by default (read-only counting
            // sweep, hourly cadence); its own `defaults_for` config carries
            // the enabled flag, so the scheduler honours it. Without
            // registration the stale-statement count is never emitted.
            {
                let worker =
                    brain_workers::workers::stale_extraction_detector::StaleExtractionDetector::new();
                scheduler
                    .register(Arc::new(worker), ops.clone())
                    .expect("register StaleExtractionDetector");
            }

            // EntityGcWorker — tombstones orphaned entities past a grace
            // window (default 30d). Off by default: `EntityGcWorker::new()`
            // ships disabled and its `defaults_for(EntityGc)` config carries
            // `enabled = false`, so the scheduler leaves it idle until an
            // operator opts in. Provisioned unconditionally so it *can* run
            // when enabled; without registration it could never run at all.
            {
                let worker = brain_workers::workers::entity_gc::EntityGcWorker::new();
                scheduler
                    .register(Arc::new(worker), ops.clone())
                    .expect("register EntityGcWorker");
            }

            // AmbiguityResolverWorker — promotes / expires entries in the
            // entity-merge review queue using the per-shard entity HNSW +
            // embedder. Without it ambiguous resolutions accumulate and
            // entity-resolution quality decays over time. On by default
            // (low-cost maintenance); operator can opt out via
            // `[workers.ambiguity_resolver] enabled = false`.
            if ambiguity_resolver_spawn_cfg.enabled {
                let worker = brain_workers::AmbiguityResolverWorker::new(
                    metadata.clone(),
                    entity_hnsw_for_shard.clone(),
                    dispatcher.clone(),
                )
                .with_interval_secs(ambiguity_resolver_spawn_cfg.interval_secs);
                scheduler
                    .register(Arc::new(worker), ops.clone())
                    .expect("register AmbiguityResolverWorker");
            }

            // Register the ExtractorWorker when its channel was
            // created above (i.e. when the extraction pipeline is enabled —
            // ≥1 tier on). The worker drains the writer's post-encode channel
            // and runs the three-tier extractor pipeline against each memory's
            // text, writing entities / statements / relations / mention edges
            // through brain-metadata.
            if let Some(rx) = extractor_receiver {
                let worker_cfg = WorkerConfig {
                    // The receiver only exists when the pipeline is enabled, so
                    // the worker is unconditionally enabled within this branch.
                    enabled: true,
                    interval: std::time::Duration::from_millis(
                        extractor_spawn_cfg.interval_ms.max(1),
                    ),
                    batch_size: extractor_spawn_cfg.drain_per_cycle,
                    max_runtime: std::time::Duration::from_secs(5),
                };
                let knobs = ExtractorKnobs {
                    drain_per_cycle: extractor_spawn_cfg.drain_per_cycle,
                    llm_budget_per_cycle_micro_usd: extractor_spawn_cfg
                        .llm_budget_per_cycle_micro_usd,
                    skip_already_extracted: extractor_spawn_cfg.skip_already_extracted,
                    batch_size: extractor_spawn_cfg.batch_size,
                    hype_refresh_per_cycle:
                        brain_workers::DEFAULT_EXTRACTOR_HYPE_REFRESH_PER_CYCLE,
                };
                let mut extractor_worker = ExtractorWorker::new(rx)
                    .with_config(worker_cfg)
                    .with_knobs(knobs)
                    .with_embed_deps(brain_extractors::resolver::EmbeddingDeps {
                        hnsw: entity_hnsw_for_shard.clone(),
                        embedder: dispatcher.clone(),
                        embed_threshold: extractor_tuning_spawn_cfg.resolver_embed_threshold,
                    });
                // Give the worker the deps to rebuild the registry live when a
                // SCHEMA_UPLOAD declares a new extractor — no restart needed.
                // The tier gate is the same one the boot-time build used.
                extractor_worker = extractor_worker.with_registry_rebuild_deps(
                    extractor_rebuild_deps.clone(),
                    tier_gate_for_closure,
                );
                if let Some(d) = entity_disambiguator_for_worker.clone() {
                    extractor_worker = extractor_worker.with_entity_disambiguator(d);
                }
                if let Some(m) = extractor_metrics_for_closure.clone() {
                    extractor_worker = extractor_worker.with_metrics(m);
                }
                // When the CausalEdgeWorker is also enabled
                // we hand the extractor its sender + the qname
                // whitelist so post-commit causal statements fan out.
                // Without this wire, the extractor never enqueues onto
                // the causal channel even if the worker is spawned.
                if let (Some(tx), Some(metrics)) = (
                    causal_edge_sender.clone(),
                    causal_edge_metrics_for_closure.clone(),
                ) {
                    use std::collections::HashSet;
                    let whitelist: HashSet<(String, String)> =
                        causal_edge_spawn_cfg.whitelist_qnames.iter().cloned().collect();
                    let feed = brain_workers::extractor::CausalEdgeFeed {
                        sender: tx,
                        metrics,
                        whitelist_qnames: whitelist,
                    };
                    extractor_worker = extractor_worker.with_causal_edge_feed(feed);
                }
                // Wire the write-time HyPE generator. HyPE is MANDATORY and
                // always-on — there is no flag to disable it. Hypothetical-
                // question embeddings are the cheap-read bridge that lets a
                // user's phrasing match a memory it doesn't lexically
                // resemble, and the write path depends on them. The LLM HyPE
                // needs is a hard startup requirement enforced at config load
                // (`Config::validate_llm_provider`): a keyless server refuses
                // to boot. The only tunable is the number of questions per
                // memory (`[extractors.hype] num_questions`, default 6).
                //
                // The `(client, cache)` slots are `Some` whenever the config
                // gate was honoured (the production entry point). They can
                // only be `None` below that gate — `spawn_shard` reached
                // directly without a provider key/cache — in which case the
                // shard already logged a WARN above; HyPE is then inoperative
                // and we skip wiring it rather than panic the executor thread.
                match (hype_client_for_worker.clone(), hype_cache_for_worker.clone()) {
                    (Some(client), Some(cache)) => {
                        let num_questions = extractor_tuning_spawn_cfg.hype_num_questions.max(1);
                        let generator = brain_workers::HypeGenerator::new(
                            client,
                            hype_model_for_worker.clone(),
                            dispatcher.clone(),
                            hype_hnsw_for_shard.clone(),
                            metadata.clone(),
                            cache,
                            num_questions,
                        );
                        extractor_worker = extractor_worker.with_hype(generator);
                        info!(
                            shard_id,
                            num_questions, "HyPE write-time generation enabled (mandatory)"
                        );
                    }
                    _ => tracing::warn!(
                        target: "brain_server::shard",
                        shard_id,
                        "HyPE (mandatory) is inoperative: no LLM provider client or cache. \
                         The server entry point enforces an LLM as a hard startup \
                         requirement (Config::validate_llm_provider); reaching here means \
                         spawn_shard was used below that gate without a key",
                    ),
                }
                scheduler
                    .register(Arc::new(extractor_worker), ops.clone())
                    .expect("register ExtractorWorker");
            }

            // Register the CausalEdgeWorker when its channel
            // was created above. The worker drains the extractor's
            // post-commit channel, walks the cause/effect mapping, and
            // writes `Caused` edges between memories.
            if let Some(rx) = causal_edge_receiver {
                let worker_cfg = WorkerConfig {
                    enabled: causal_edge_spawn_cfg.enabled,
                    interval: std::time::Duration::from_millis(
                        causal_edge_spawn_cfg.interval_ms.max(1),
                    ),
                    batch_size: causal_edge_spawn_cfg.batch_size,
                    max_runtime: std::time::Duration::from_secs(5),
                };
                let knobs = brain_workers::CausalEdgeKnobs {
                    whitelist_qnames: causal_edge_spawn_cfg.whitelist_qnames.clone(),
                    min_confidence: causal_edge_spawn_cfg.min_confidence,
                    max_effect_memories_per_statement: causal_edge_spawn_cfg
                        .max_effect_memories_per_statement,
                    max_cause_memories_per_statement: causal_edge_spawn_cfg
                        .max_cause_memories_per_statement,
                    max_related_statements_per_entity: causal_edge_spawn_cfg
                        .max_related_statements_per_entity,
                };
                let mut causal_edge_worker = brain_workers::CausalEdgeWorker::new(rx)
                    .with_config(worker_cfg)
                    .with_knobs(knobs);
                if let Some(m) = causal_edge_metrics_for_closure.clone() {
                    causal_edge_worker = causal_edge_worker.with_metrics(m);
                }
                scheduler
                    .register(Arc::new(causal_edge_worker), ops.clone())
                    .expect("register CausalEdgeWorker");
            }

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
                entity_hnsw: entity_hnsw_for_shard.clone(),
                hype_hnsw: hype_hnsw_for_shard.clone(),
                statement_question_hnsw: statement_question_hnsw_for_shard.clone(),
                fanout_task: __fanout_task,
                wal_drain_task: Some(__wal_drain_task),
                memory_text_task: __memory_text_task,
                statement_text_task: __statement_text_task,
                lexical_retriever: lexical_retriever_concrete_for_closure,
                tantivy_dir: tantivy_dir_for_closure,
                memory_text_control: __memory_text_control,
                statement_text_control: __statement_text_control,
            };
            shard_main_loop(shard, rx).await;
        })
        .map_err(|e| ShardError::Spawn(e.to_string()))?;
    // Block until the shard signals its WAL is open (fail-fast readiness).
    // A WAL IO failure, or the executor thread exiting before it signalled,
    // fails the spawn here instead of leaving a dead shard serving requests.
    interpret_wal_ready(wal_ready_rx.recv())?;
    let handle = ShardHandle {
        shard_id,
        tx,
        events: events_rx,
        wal_dir: wal_dir.clone(),
        shard_uuid,
        auto_edge_metrics: auto_edge_metrics_for_handle,
        extractor_metrics: extractor_metrics_for_handle,
        temporal_edge_metrics: temporal_edge_metrics_for_handle,
        causal_edge_metrics: causal_edge_metrics_for_handle,
        llm_cache_sweep_metrics: Some(llm_cache_sweep_metrics_for_handle),
        statement_embed_metrics: statement_embed_metrics_for_handle,
        confidence_sweep_metrics: confidence_sweep_metrics_for_handle,
        retriever_metrics: retriever_metrics_for_handle,
        query_metrics: query_metrics_for_handle,
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

/// How long shard teardown waits for a lexical indexer's final commit
/// before giving up and logging. Generous relative to a tantivy commit
/// of a single batch; the point is to bound the wait, not to race it.
const LEXICAL_FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
            ShardRequest::GetMemoryVector {
                space,
                memory_id,
                reply_tx,
            } => {
                let vector = shard.memory_vector_for(space, memory_id);
                if reply_tx.send_async(vector).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "GetMemoryVector reply dropped (caller gone)"
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
            ShardRequest::StorageStats { reply_tx } => {
                let stats = shard.storage_stats();
                if reply_tx.send_async(stats).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "StorageStats reply dropped (caller gone)"
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
            ShardRequest::ExtractBackfill { selector, reply_tx } => {
                let out = run_extract_backfill(&shard, selector).await;
                if reply_tx.send_async(out).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "ExtractBackfill reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::RestoreMemory {
                memory_id,
                namespace,
                reply_tx,
            } => {
                let out = run_restore_memory(&shard, memory_id, &namespace).await;
                if reply_tx.send_async(out).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "RestoreMemory reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::BackfillSubmit { request, reply_tx } => {
                // Reach the same per-shard worker handle the (now-rejected)
                // wire op used to: the `Arc<dyn BackfillControl>` threaded
                // onto the executor context at shard construction. `submit`
                // is a synchronous `&self` push onto worker-owned state, so
                // no borrow crosses the reply `.await`.
                let out = match shard.ops.executor.backfill_handle.as_ref() {
                    Some(handle) => Ok(handle.submit(request)),
                    None => Err("backfill worker not provisioned on this shard".to_owned()),
                };
                if reply_tx.send_async(out).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "BackfillSubmit reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::BackfillCancel { id, reply_tx } => {
                let out = match shard.ops.executor.backfill_handle.as_ref() {
                    Some(handle) => Ok(handle.cancel(id)),
                    None => Err("backfill worker not provisioned on this shard".to_owned()),
                };
                if reply_tx.send_async(out).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "BackfillCancel reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::BackfillProgressSnapshot { reply_tx } => {
                let out = match shard.ops.executor.backfill_handle.as_ref() {
                    Some(handle) => Ok(handle.progress()),
                    None => Err("backfill worker not provisioned on this shard".to_owned()),
                };
                if reply_tx.send_async(out).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "BackfillProgressSnapshot reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::RebuildHnsw { reply_tx } => {
                let result = shard.do_rebuild_memory().await;
                if reply_tx.send_async(result).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "RebuildHnsw reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::RebuildIndex { target, reply_tx } => {
                let result = shard.do_rebuild_index(target).await;
                if reply_tx.send_async(result).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        ?target,
                        "RebuildIndex reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::AbortOrphanedTxns {
                connection_id,
                reply_tx,
            } => {
                // Synchronous on the shard executor: `TxnStore` is a
                // parking_lot::Mutex over a HashMap, so the sweep is
                // a single bounded pass. Returning the count (not the
                // ids) keeps the cross-runtime reply trivially Send.
                let aborted = shard
                    .ops
                    .txn_store
                    .abort_orphaned_for_connection(connection_id)
                    .len();
                if reply_tx.send_async(aborted).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "AbortOrphanedTxns reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::AuditQuery {
                selector,
                limit,
                cursor,
                reply_tx,
            } => {
                let out = shard.run_audit_query(selector, limit, cursor);
                if reply_tx.send_async(out).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "AuditQuery reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::DispatchOp {
                req,
                caller,
                reply_tx,
                parent_span,
            } => {
                // `brain_ops::dispatch` is async and runs entirely
                // within the per-shard Glommio executor: it touches
                // `OpsContext` (which is !Send) and yields
                // through Glommio-aware I/O. Awaiting here is sound —
                // the main loop is single-threaded and processes one
                // request at a time, the same shape as
                // `AppendWalRecord`.
                //
                // `.instrument(parent_span)` re-enters the connection-layer
                // `client.request` span on this Glommio thread so the
                // `brain.encode` span (and its storage sub-spans) nest under
                // it. We instrument the future rather than holding an
                // `enter()` guard because the dispatch yields at `.await`
                // points; a guard held across `.await` would mis-attribute
                // spans from interleaved work.
                // Peek for a direct (non-transactional) hard FORGET before
                // moving `req` into dispatch. Hard forget promises the
                // plaintext-derived embedding is "no longer recoverable from
                // the file" immediately (invariant #6). The arena is the one
                // at-rest home the apply layer can't reach — it holds only
                // the redb wtxn, whereas the arena lives here on the shard —
                // so we zero it at this boundary once the forget commits.
                // Transactional forgets (`txn_id.is_some()`) are buffered and
                // applied at COMMIT_TXN, and a rollback must not leave a
                // zeroed slot, so they are covered by the recovery replay
                // (which re-zeroes hard-forgotten slots) rather than here.
                let hard_forget_id = match &*req {
                    RequestBody::Forget(f) if f.mode == ForgetMode::Hard && f.txn_id.is_none() => {
                        Some(MemoryId::from_raw(f.memory_id))
                    }
                    _ => None,
                };
                let out = brain_ops::dispatch::dispatch(*req, caller, &shard.ops)
                    .instrument(parent_span)
                    .await;
                // Zero the arena slot only after a successful commit. FORGET
                // is lenient (a missing/stale id is a no-op success) and
                // `hard_forget_slot` is itself guarded on occupancy + slot
                // version, so an over-eager call is a safe no-op: a same-run
                // memory (never in the arena) and a slot since reclaimed by a
                // newer memory (invariant #4) are both left untouched.
                if let (Some(id), Ok(_)) = (hard_forget_id, &out) {
                    let zeroed = {
                        let mut arena = shard.arena.borrow_mut();
                        arena.hard_forget_slot(id.slot(), id.version())
                    };
                    if zeroed {
                        tracing::debug!(
                            shard_id = shard.shard_id,
                            memory_id = ?id,
                            "hard forget: zeroed arena slot at rest"
                        );
                    }
                }
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
    // Clean shutdown: drain worker scheduler → final snapshot → WAL
    // committer → arena msync. Order matters:
    //   - Workers drain first so in-flight WAL appends complete and the
    //     background snapshot worker can't race with the final snapshot.
    //   - Final snapshot runs while the WAL is still alive (it writes
    //     CHECKPOINT_BEGIN/END records) and bounds the next start's
    //     replay tail to "nothing since just now."
    //   - WAL committer closes after the snapshot's BEGIN/END are acked.
    //   - Arena msync runs last so all pre-msync writes (including the
    //     snapshot's redb checkpoint flag) are visible to a fresh process.
    if let Some(scheduler) = shard.scheduler.take() {
        if let Err(e) = scheduler.shutdown().await {
            warn!(
                shard_id = shard.shard_id,
                error = %e,
                "scheduler shutdown failed"
            );
        }
    }
    // Final snapshot, only if there's something to snapshot. Mirrors the
    // worker's empty-HNSW guard: a shard that never received an encode
    // has no semantic state worth checkpointing, and writing
    // CHECKPOINT_BEGIN/END for it would just pollute the WAL and break
    // recovery-tooling assumptions about LSN positions. Best-effort: a
    // failure here doesn't break shutdown; the arena-rebuild fallback on
    // next start keeps correctness intact.
    if !shard.ops.executor.index.is_empty() {
        if let Err(e) = shard.snapshot_source.take_snapshot().await {
            warn!(
                shard_id = shard.shard_id,
                error = ?e,
                "final snapshot at shutdown failed; next start will replay more WAL"
            );
        }
    }
    // Flush the lexical indexes before anything else is torn down.
    //
    // These are JOINED, not cancelled: their final `commit()` is durable
    // work, and a Glommio executor drops still-pending tasks once this
    // future returns. Detaching them lost exactly that commit — a hard
    // FORGET purges the redb `TEXTS` row inside the tombstone's own write
    // txn, but its lexical delete rides the indexer queue, so the text
    // stayed on disk and keyword-searchable after a graceful shutdown.
    //
    // The join is bounded. The op channel's senders sit behind `Arc`s in
    // `OpsContext` and the writer, so this cannot prove the queue is
    // closed; an unbounded wait would trade a stale index entry for a
    // hung shard join, which is strictly worse. On expiry we log and move
    // on — the same posture `bootstrap::shutdown` takes when a shard join
    // times out.
    for (label, slot) in [
        ("memory_text", shard.memory_text_task.take()),
        ("statement_text", shard.statement_text_task.take()),
    ] {
        let Some((stop, task)) = slot else {
            continue;
        };
        // Best-effort: if the loop already exited via `Disconnected`
        // the send fails and the join below returns immediately.
        let _ = stop.send(());
        let flushed = glommio::timer::timeout(LEXICAL_FLUSH_TIMEOUT, async move {
            task.await;
            Ok(())
        })
        .await
        .is_ok();
        if !flushed {
            error!(
                shard_id = shard.shard_id,
                indexer = label,
                timeout_ms = LEXICAL_FLUSH_TIMEOUT.as_millis() as u64,
                "lexical indexer did not flush within timeout; \
                 recently indexed or deleted docs may be missing"
            );
        }
    }

    // Cancel the detached per-shard helper tasks BEFORE reclaiming the WAL.
    //
    // `wal_drain_task` holds a *shared* `RefCell` borrow of `shard.wal`
    // across its `wal.append_many(...).await`. It is an independently
    // spawned Glommio task, not a scheduler-driven adapter, so draining the
    // scheduler above does *not* guarantee it has dropped that borrow: on a
    // single-threaded executor it can be parked mid-append (borrow live)
    // when control returns here. If we called `shard.wal.borrow_mut()` while
    // that shared borrow was outstanding, the `RefCell` would panic
    // (`already borrowed`), aborting the shard thread mid-shutdown and
    // skipping the WAL/arena flush below.
    //
    // `cancel().await` drops the task's future — releasing any live borrow —
    // and joins it, so no shared borrow can be outstanding when we `take()`.
    // The main loop has already exited (its channel is closed) and the
    // scheduler is drained, so no new records can enqueue after this point.
    // Only a not-yet-acked, still-in-flight append is abandoned; every acked
    // write was fsynced before its LSN was returned, so WAL-before-ack holds
    // and no durable work is lost. The fanout task touches no shared state
    // but is cancelled here too so it can't keep the executor runnable.
    if let Some(t) = shard.wal_drain_task.take() {
        t.cancel().await;
    }
    if let Some(t) = shard.fanout_task.take() {
        t.cancel().await;
    }
    // Take the WAL out of its cell. The scheduler is drained and the WAL
    // drain task is cancelled/joined above, so no `Rc` clone of `shard.wal`
    // holds a live borrow. `take()` is therefore safe.
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
// Extract-backfill helper
// ---------------------------------------------------------------------------

/// Merge a rebuild snapshot with the HNSW pending buffer, producing the
/// complete `(MemoryId, vector)` set to feed a fresh index build. Pending
/// entries shadow snapshot entries for the same id (latest vector wins) and
/// new pending ids are appended; tombstoned pending entries are dropped. Used
/// by the admin `rebuild-ann` path so a rebuild never loses a live vector that
/// landed in pending after the snapshot read.
fn fold_pending_into(
    snapshot: Vec<(brain_core::MemoryId, [f32; VECTOR_DIM])>,
    pending: &[PendingEntry],
) -> Vec<(brain_core::MemoryId, [f32; VECTOR_DIM])> {
    let mut combined = snapshot;
    let ids: std::collections::HashSet<brain_core::MemoryId> =
        combined.iter().map(|(id, _)| *id).collect();
    for entry in pending {
        if entry.tombstoned {
            continue;
        }
        if ids.contains(&entry.memory_id) {
            if let Some(slot) = combined.iter_mut().find(|(id, _)| *id == entry.memory_id) {
                slot.1 = entry.vector;
            }
        } else {
            combined.push((entry.memory_id, entry.vector));
        }
    }
    combined
}

/// Rows examined between cooperative yields during a backfill table
/// scan. A full `MEMORIES_TABLE` walk with per-row redb `get` + enqueue
/// would otherwise hold the shard core for the whole scan, starving
/// foreground RECALL/ENCODE on this shard. Yielding every N rows lets
/// those interleave while keeping the per-yield bookkeeping negligible.
const BACKFILL_YIELD_INTERVAL: usize = 256;

/// Whether the scan should cooperatively yield after examining
/// `rows_examined` rows. Yields on every `BACKFILL_YIELD_INTERVAL`-th
/// row (never on row 0, so a tiny scan does no extra work).
#[inline]
fn backfill_should_yield(rows_examined: usize) -> bool {
    rows_examined != 0 && rows_examined.is_multiple_of(BACKFILL_YIELD_INTERVAL)
}

/// Restore (un-tombstone) a soft-forgotten memory on this shard. Runs
/// inside the shard executor. A non-owning shard (the id routes
/// elsewhere) short-circuits to `NotFound` so the admin fan-out reports a
/// clean miss rather than an error. The grace window matches the
/// slot-reclamation worker's ([`brain_workers::workers::slot_reclaim::DEFAULT_FORGET_GRACE`]),
/// so restore-eligibility and reclamation coincide.
async fn run_restore_memory(
    shard: &Shard,
    memory_id: brain_core::MemoryId,
    namespace: &str,
) -> Result<brain_ops::AdminRestoreOutcome, String> {
    // Cheap shard-belongs check: an id that routes elsewhere is a clean
    // miss on this shard (the admin handler fans out to every shard).
    if memory_id.shard() != shard.shard_id {
        return Ok(brain_ops::AdminRestoreOutcome::NotFound);
    }
    let grace_nanos =
        u64::try_from(brain_workers::workers::slot_reclaim::DEFAULT_FORGET_GRACE.as_nanos())
            .unwrap_or(u64::MAX);
    let now_unix_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    brain_ops::handle_admin_restore(
        &shard.ops,
        memory_id,
        namespace,
        grace_nanos,
        now_unix_nanos,
    )
    .await
    .map_err(|e| format!("restore: {e}"))
}

/// Walk the per-shard `memories` + `texts` redb tables and push each
/// matching memory onto the `WriterHandle`'s extractor channel. Runs
/// inside the shard executor; the metadata read txn is held for the
/// scan's duration (a stable snapshot) but the loop yields cooperatively
/// every [`BACKFILL_YIELD_INTERVAL`] rows so foreground ops on this
/// shard aren't starved by a large scan. Enqueue is still a non-blocking
/// `try_send` against the bounded flume queue.
async fn run_extract_backfill(
    shard: &Shard,
    selector: brain_protocol::BackfillSelector,
) -> Result<ExtractBackfillReport, String> {
    use brain_core::MemoryId;
    use brain_metadata::tables::memory::MEMORIES_TABLE;
    use brain_metadata::tables::text::TEXTS_TABLE;
    use brain_protocol::BackfillSelector;
    use redb::ReadableTable;

    let mut report = ExtractBackfillReport::default();
    let metadata = shard.ops.executor.metadata.clone();
    let writer = shard.ops.executor.writer.clone();

    let rtxn = metadata
        .read_txn()
        .map_err(|e| format!("extract_backfill read_txn: {e}"))?;
    let memories = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| format!("open MEMORIES: {e}"))?;
    let texts = rtxn
        .open_table(TEXTS_TABLE)
        .map_err(|e| format!("open TEXTS: {e}"))?;

    // Closure: try to enqueue one memory by its 16-byte id.
    let mut try_one = |key: [u8; 16]| -> Result<(), String> {
        let Some(meta_guard) = memories
            .get(&key)
            .map_err(|e| format!("memories.get: {e}"))?
        else {
            // For `Memory(id)` callers we already know the id; for the
            // table-scan path this never fires (we got the key from the
            // iter). Count as skipped either way.
            report.skipped = report.skipped.saturating_add(1);
            return Ok(());
        };
        let row = meta_guard.value();
        if !row.is_active() || row.is_hard_forgotten() {
            report.skipped = report.skipped.saturating_add(1);
            return Ok(());
        }
        let text_guard = texts.get(&key).map_err(|e| format!("texts.get: {e}"))?;
        let Some(text_guard) = text_guard else {
            report.skipped = report.skipped.saturating_add(1);
            return Ok(());
        };
        let bytes = text_guard.value();
        let text = match std::str::from_utf8(bytes) {
            Ok(s) => s.to_owned(),
            Err(_) => {
                report.skipped = report.skipped.saturating_add(1);
                return Ok(());
            }
        };
        let memory_id = MemoryId::from_be_bytes(key);
        if writer.enqueue_for_extraction(memory_id, &text) {
            report.enqueued = report.enqueued.saturating_add(1);
        } else {
            report.skipped = report.skipped.saturating_add(1);
        }
        Ok(())
    };

    match selector {
        BackfillSelector::Memory(wire_id) => {
            let memory_id: MemoryId = wire_id.into();
            // Cheap shard-belongs check: skip without error when the id
            // routes elsewhere. The admin handler fans out to every
            // shard; only the owning shard reports a hit.
            if memory_id.shard() != shard.shard_id {
                return Ok(report);
            }
            try_one(memory_id.to_be_bytes())?;
        }
        BackfillSelector::Since { since_unix_nanos } => {
            let cutoff_nanos = since_unix_nanos;
            let mut examined = 0usize;
            for entry in memories.iter().map_err(|e| format!("memories.iter: {e}"))? {
                examined += 1;
                if backfill_should_yield(examined) {
                    glommio::executor().yield_if_needed().await;
                }
                let (k, v) = entry.map_err(|e| format!("memories.entry: {e}"))?;
                let key = k.value();
                let row = v.value();
                if row.created_at_unix_nanos < cutoff_nanos {
                    continue;
                }
                if !row.is_active() || row.is_hard_forgotten() {
                    continue;
                }
                try_one(key)?;
            }
        }
        BackfillSelector::All => {
            let mut examined = 0usize;
            for entry in memories.iter().map_err(|e| format!("memories.iter: {e}"))? {
                examined += 1;
                if backfill_should_yield(examined) {
                    glommio::executor().yield_if_needed().await;
                }
                let (k, v) = entry.map_err(|e| format!("memories.entry: {e}"))?;
                let key = k.value();
                let row = v.value();
                if !row.is_active() || row.is_hard_forgotten() {
                    continue;
                }
                try_one(key)?;
            }
        }
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// Entity-type snapshot for zero-shot classifier labels
// ---------------------------------------------------------------------------

/// Snapshot the active schema's entity-type names as classifier labels
/// (`brain:Person`, …) in stable id-order. Used at shard spawn to seed the
/// classifier's *fallback* labels; the worker re-reads the same snapshot every
/// drain cycle (`brain_metadata::entity_type_label_qnames`) so a runtime
/// `SCHEMA_UPLOAD` reaches the classifier without a restart. Delegates to the
/// shared helper so the label convention has one source of truth.
fn snapshot_entity_type_qnames(
    rtxn: &redb::ReadTransaction,
) -> Result<Vec<String>, brain_metadata::EntityTypeOpError> {
    brain_metadata::entity_type_label_qnames(rtxn)
}

// ---------------------------------------------------------------------------
// UUID helper
// ---------------------------------------------------------------------------

/// Scan a snapshots root for the most-recent checkpoint subdirectory.
/// Snapshot worker writes each checkpoint into `<root>/<NN20>/` where
/// `NN20` is a zero-padded 20-digit checkpoint id; the highest id is
/// the freshest. Returns `None` when the root doesn't exist or contains
/// no numeric subdirectories — the caller falls back to a full arena
/// rebuild.
fn find_latest_snapshot_dir(root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id: u64 = match path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|s| s.parse().ok())
        {
            Some(v) => v,
            None => continue,
        };
        match &best {
            Some((cur, _)) if *cur >= id => {}
            _ => best = Some((id, path)),
        }
    }
    best.map(|(_, p)| p)
}

/// Re-apply post-snapshot FORGETs to a freshly-loaded memory HNSW.
///
/// A snapshot captures the graph as of `taken_at_lsn`; a FORGET that landed
/// afterward marks the memory inactive in redb but leaves the loaded main
/// holding the node active — a ghost that occupies a top-k slot and diverges
/// from redb until some later full rebuild fires. For every `snapshot_id`
/// whose redb row is inactive (or entirely absent) this tombstones the node
/// in `hnsw`, so after reconciliation the HNSW active set equals the redb
/// active set for the memory index.
///
/// Returns the number of nodes re-tombstoned, or an error string when the
/// redb read could not be set up (so the caller can warn rather than
/// silently skip reconciliation). A per-row read error is treated as "leave
/// as loaded" — fail-safe toward keeping a genuinely-live node.
fn reconcile_forgotten_memories(
    hnsw: &brain_index::SharedHnsw,
    metadata: &brain_metadata::MetadataDb,
    snapshot_ids: &[brain_core::MemoryId],
) -> Result<usize, String> {
    let rtxn = metadata.read_txn().map_err(|e| format!("read_txn: {e}"))?;
    let table = rtxn
        .open_table(brain_metadata::tables::memory::MEMORIES_TABLE)
        .map_err(|e| format!("open MEMORIES_TABLE: {e}"))?;
    let mut retombstoned = 0usize;
    for mid in snapshot_ids {
        let inactive = match table.get(mid.to_be_bytes()) {
            Ok(Some(row)) => !row.value().is_active(),
            // Row gone entirely → not active.
            Ok(None) => true,
            // Per-row read error → leave the node as loaded.
            Err(_) => false,
        };
        if inactive && !hnsw.is_tombstoned(*mid) {
            hnsw.tombstone_recovery(*mid);
            retombstoned += 1;
        }
    }
    Ok(retombstoned)
}

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
    use brain_embed::EmbedError;
    use tempfile::TempDir;

    /// File-local stub: substrate tests don't exercise embedding
    /// quality and we don't want to load a ~130 MiB BERT model per
    /// `cargo test` invocation. Production paths go through the real
    /// `CachingDispatcher<CpuDispatcher>` built in `main.rs`.
    struct TestStubDispatcher;
    impl Dispatcher for TestStubDispatcher {
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

    fn stub_dispatcher() -> Arc<dyn Dispatcher> {
        Arc::new(TestStubDispatcher)
    }

    #[test]
    fn wal_ready_ok_yields_ok() {
        assert!(interpret_wal_ready(Ok(Ok(()))).is_ok());
    }

    #[test]
    fn queue_depth_tracks_undrained_requests() {
        // The dispatch-queue depth reflects requests queued on the
        // request channel but not yet drained by the executor. With no
        // receiver draining, each send accumulates.
        let (tx, rx) = flume::unbounded::<ShardRequest>();
        let handle = ShardHandle::new_for_test(0, tx);
        assert_eq!(handle.queue_depth(), 0);

        let (reply_tx, _reply_rx) = flume::bounded(1);
        handle
            .tx
            .send(ShardRequest::Ping { reply_tx })
            .expect("send ping");
        assert_eq!(handle.queue_depth(), 1);

        let (reply_tx, _reply_rx) = flume::bounded(1);
        handle
            .tx
            .send(ShardRequest::HnswSnapshot { reply_tx })
            .expect("send snapshot");
        assert_eq!(handle.queue_depth(), 2);

        // Draining one request drops the depth.
        let _ = rx.recv().expect("drain one");
        assert_eq!(handle.queue_depth(), 1);
    }

    #[test]
    fn backfill_yields_on_interval_boundaries_only() {
        // Never yields on the first row (row 0 → 0 examined) or before a
        // full interval elapses; yields on each interval boundary so a
        // large scan can't monopolize the shard core.
        assert!(!backfill_should_yield(0));
        assert!(!backfill_should_yield(1));
        assert!(!backfill_should_yield(BACKFILL_YIELD_INTERVAL - 1));
        assert!(backfill_should_yield(BACKFILL_YIELD_INTERVAL));
        assert!(backfill_should_yield(BACKFILL_YIELD_INTERVAL * 2));
        assert!(!backfill_should_yield(BACKFILL_YIELD_INTERVAL + 1));

        // A scan of `rows` yields floor(rows / interval) times.
        let rows = BACKFILL_YIELD_INTERVAL * 3 + 7;
        let yields = (1..=rows).filter(|n| backfill_should_yield(*n)).count();
        assert_eq!(yields, 3);
    }

    #[test]
    fn wal_ready_io_failure_fails_spawn() {
        // A WAL open/create IO failure reported by the closure must surface
        // as a spawn error, not a silently-dead serving shard.
        let err = WalError::NoSegmentsFound {
            dir: std::path::PathBuf::from("/nonexistent/wal"),
        };
        let out = interpret_wal_ready(Ok(Err(err)));
        assert!(matches!(out, Err(ShardError::WalInit(_))));
    }

    #[test]
    fn wal_ready_sender_dropped_fails_spawn() {
        // If the executor thread exits before signalling (e.g. an earlier
        // in-closure panic drops the sender), the spawn must still fail
        // rather than proceed to build a handle over a dead shard.
        let (tx, rx) = flume::bounded::<Result<(), WalError>>(1);
        drop(tx);
        let out = interpret_wal_ready(rx.recv());
        assert!(matches!(out, Err(ShardError::Spawn(_))));
    }

    /// Spawn config for tests that run without real model files. The
    /// stub dispatcher fakes embeddings; rerank is turned off because
    /// no cross-encoder weights exist in the test environment, and an
    /// enabled-but-missing reranker is a hard spawn failure by design.
    fn stub_spawn_config(dir: impl Into<std::path::PathBuf>) -> ShardSpawnConfig {
        let mut cfg = ShardSpawnConfig::new(dir, stub_dispatcher());
        cfg.rerank.enabled = false;
        cfg
    }

    #[test]
    fn fanout_lag_is_counted_not_silently_swallowed() {
        // The fanout task's `Lagged` arm routes through `record_fanout_lag`
        // instead of a bare `continue`; a dropped event must always leave an
        // observable trace (counter + warn). Assert on a delta because the
        // counter is process-global and other tests may touch it in parallel.
        let before = fanout_lagged_events();
        record_fanout_lag(0, 3);
        record_fanout_lag(0, 5);
        let after = fanout_lagged_events();
        assert_eq!(
            after - before,
            8,
            "fanout lag must accumulate skipped-event counts, not be swallowed"
        );
    }

    /// Regression: a delivered-but-unacked FIRST quiesce must NOT strand the
    /// `memory_text` indexer. Before the fix `do_rebuild_tantivy` `?`-returned
    /// on the first quiesce, skipping the whole resume region — so a
    /// `memory_text` indexer that received `Quiesce` (dropped its writer,
    /// parked) but whose ack never arrived was left parked forever, silently
    /// losing every later ENCODE/FORGET lexical op (invariant #7). The drive
    /// helper must instead resume BOTH indexers and only then surface the
    /// quiesce failure as fail-stop.
    #[cfg(target_os = "linux")]
    #[test]
    fn first_quiesce_failure_still_resumes_memory_text() {
        use brain_ops::index::text_indexer::IndexerControl;
        use std::cell::Cell;
        use std::rc::Rc;

        glommio::LocalExecutorBuilder::default()
            .name("rebuild-quiesce-test")
            .spawn(|| async move {
                // Two control channels standing in for the two live indexers.
                let (mem_tx, mem_rx) = flume::bounded::<IndexerControl>(4);
                let (stmt_tx, stmt_rx) = flume::bounded::<IndexerControl>(4);

                // memory_text fake: on `Quiesce` it records receipt and DROPS
                // its ack without acking — modelling a delivered-but-timed-out
                // quiesce (the drain loop parked with its writer dropped). It
                // stays alive to receive `Resume`, proving it can be revived.
                let mem_quiesced = Rc::new(Cell::new(false));
                let mem_resumed = Rc::new(Cell::new(false));
                let mem_task = {
                    let mem_quiesced = mem_quiesced.clone();
                    let mem_resumed = mem_resumed.clone();
                    glommio::spawn_local(async move {
                        while let Ok(ctl) = mem_rx.recv_async().await {
                            match ctl {
                                IndexerControl::Quiesce { ack } => {
                                    mem_quiesced.set(true);
                                    drop(ack); // parked; never acks
                                }
                                IndexerControl::Resume { ack, .. } => {
                                    mem_resumed.set(true);
                                    let _ = ack.send(());
                                    break;
                                }
                            }
                        }
                    })
                };

                // statements fake: acks both quiesce and resume normally.
                let stmt_resumed = Rc::new(Cell::new(false));
                let stmt_task = {
                    let stmt_resumed = stmt_resumed.clone();
                    glommio::spawn_local(async move {
                        while let Ok(ctl) = stmt_rx.recv_async().await {
                            match ctl {
                                IndexerControl::Quiesce { ack } => {
                                    let _ = ack.send(());
                                }
                                IndexerControl::Resume { ack, .. } => {
                                    stmt_resumed.set(true);
                                    let _ = ack.send(());
                                    break;
                                }
                            }
                        }
                    })
                };

                // Real handles to resume onto (a fresh empty on-disk shard).
                let dir = TempDir::new().expect("tempdir");
                let shard = brain_index::TantivyShard::open(dir.path())
                    .expect("open tantivy shard")
                    .shard;
                let mem_handle = shard.memory_text.clone();
                let stmt_handle = shard.statements.clone();

                // Record which per-index rebuilds the middle was asked to run.
                let mem_rebuild_run = Rc::new(Cell::new(false));
                let stmt_rebuild_run = Rc::new(Cell::new(false));
                let result = {
                    let mem_rebuild_run = mem_rebuild_run.clone();
                    let stmt_rebuild_run = stmt_rebuild_run.clone();
                    drive_tantivy_rebuild(Some(&mem_tx), Some(&stmt_tx), move |mem_ok, stmt_ok| {
                        mem_rebuild_run.set(mem_ok);
                        stmt_rebuild_run.set(stmt_ok);
                        RebuildMiddle {
                            rebuild_result: Ok(0),
                            reopen_err: None,
                            swap_err: None,
                            resume_handles: Some((mem_handle, stmt_handle)),
                        }
                    })
                    .await
                };

                // The rebuild fail-stops (the first quiesce never acked)...
                assert!(
                    result.is_err(),
                    "rebuild must fail-stop when the first quiesce ack does not arrive",
                );
                // ...yet memory_text is still RESUMED, not stranded parked...
                assert!(
                    mem_quiesced.get(),
                    "memory_text quiesce must have been delivered",
                );
                assert!(
                    mem_resumed.get(),
                    "memory_text must be resumed even though its quiesce ack failed",
                );
                // ...and statements was quiesced + resumed normally.
                assert!(stmt_resumed.get(), "statement indexer must be resumed");
                // The memory-text on-disk rebuild was SKIPPED (its indexer
                // never quiesced), while the statement rebuild was allowed.
                assert!(
                    !mem_rebuild_run.get(),
                    "memory rebuild must be skipped when its indexer did not quiesce",
                );
                assert!(
                    stmt_rebuild_run.get(),
                    "statement rebuild must run when its indexer quiesced",
                );

                mem_task.await;
                stmt_task.await;
            })
            .expect("spawn test executor")
            .join()
            .expect("join test executor");
    }

    #[test]
    fn shard_handle_is_send_sync_compile_check() {
        // Statically asserted above; this test exists so the file's
        // intent is discoverable from `cargo test` output.
    }

    #[test]
    fn shard_spawn_config_new_uses_arena_default_capacity() {
        let cfg = ShardSpawnConfig::new("/tmp/example", stub_dispatcher());
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
        let cfg = stub_spawn_config(dir.path());
        let (handle, joiner) =
            spawn_shard(0, cfg).expect("Glommio spawn should succeed with Unbound placement");
        assert_eq!(handle.shard_id(), 0);
        drop(handle);
        joiner.join().expect("shard should join cleanly");
    }

    /// Spawning a shard must leave the opaque-body
    /// tantivy directories present on disk so the owning modules can
    /// open them without a separate mkdir step.
    #[test]
    fn spawn_creates_graph_directories() {
        let dir = TempDir::new().unwrap();
        let cfg = stub_spawn_config(dir.path());
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

        // entity.hnsw / statement.hnsw — NOT created by spawn; the owning
        // modules open them on demand.
        assert!(
            !paths.entity_hnsw().exists(),
            "entity.hnsw is created by phase 16, not by spawn"
        );
        assert!(
            !paths.statement_hnsw().exists(),
            "statement.hnsw is created by phase 17, not by spawn"
        );

        // llm_cache.redb — IS created by spawn.
        assert!(
            paths.llm_cache_db().exists(),
            "llm_cache.redb should be created by spawn (sub-task 15.4)"
        );

        // Spawn opens the tantivy indexes via
        // `TantivyShard::open`, which calls `Index::create_in_dir`
        // on a fresh shard. The presence of `meta.json` is the
        // observable proof that this happened (a bare mkdir
        // leaves the directory empty).
        assert!(
            paths.memory_text_tantivy().join("meta.json").exists(),
            "memory_text.tantivy/meta.json should exist after spawn (sub-task 22.1)"
        );
        assert!(
            paths.statements_tantivy().join("meta.json").exists(),
            "statements.tantivy/meta.json should exist after spawn (sub-task 22.1)"
        );
    }

    #[test]
    fn reconcile_forgotten_tombstones_inactive_snapshot_nodes() {
        // IDX1 convergence: a memory HNSW loaded from a snapshot holds M1..M3
        // active; a post-snapshot FORGET marked M2 inactive in redb.
        // Reconciliation must tombstone exactly M2 so the HNSW active set
        // converges to the redb active set — M1/M3 stay live, no ghost.
        use brain_core::{MemoryId, MemoryKind, NamespaceId, SessionId, SpaceId};
        use brain_index::SharedHnsw;
        use brain_metadata::tables::memory::{flags, MemoryMetadata, MEMORIES_TABLE};

        fn space(b: u8) -> SpaceId {
            let mut x = [0u8; 16];
            x[15] = b;
            x.into()
        }
        fn row(slot: u64) -> MemoryMetadata {
            MemoryMetadata::new_active(
                MemoryId::pack(1, slot, 1),
                NamespaceId::SYSTEM,
                space(slot as u8),
                SessionId(1),
                slot,
                1,
                MemoryKind::Episodic,
                [0xAB; 16],
                0.5,
                10,
                1_700_000_000_000_000_000,
            )
        }

        let dir = TempDir::new().unwrap();
        let md = brain_metadata::MetadataDb::open(dir.path().join("metadata.redb")).unwrap();

        let m1 = MemoryId::pack(1, 1, 1);
        let m2 = MemoryId::pack(1, 2, 1);
        let m3 = MemoryId::pack(1, 3, 1);

        let wtxn = md.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            t.insert(&m1.to_be_bytes(), &row(1)).unwrap();
            // M2: FORGOTTEN after the snapshot — ACTIVE flag cleared.
            let mut r2 = row(2);
            r2.set_flag(flags::ACTIVE, false);
            assert!(r2.is_tombstoned());
            t.insert(&m2.to_be_bytes(), &r2).unwrap();
            t.insert(&m3.to_be_bytes(), &row(3)).unwrap();
        }
        wtxn.commit().unwrap();

        // Simulate the loaded snapshot: all three active in the graph.
        let idx = brain_index::HnswIndex::new(brain_index::params::IndexParams::default_v1())
            .expect("HnswIndex::new");
        let (hnsw, _writer) = SharedHnsw::from_index(idx);
        for (i, mid) in [m1, m2, m3].iter().enumerate() {
            let mut v = [0.0f32; VECTOR_DIM];
            v[i] = 1.0;
            hnsw.insert_recovery(*mid, &v);
        }
        assert!(hnsw.contains(m2), "M2 active before reconciliation");

        let snapshot_ids = vec![m1, m2, m3];
        let n = reconcile_forgotten_memories(&hnsw, &md, &snapshot_ids).unwrap();
        assert_eq!(n, 1, "exactly M2 should be re-tombstoned");

        assert!(
            hnsw.is_tombstoned(m2),
            "M2 must be tombstoned after reconcile"
        );
        assert!(!hnsw.contains(m2), "M2 must no longer be a live node");
        assert!(hnsw.contains(m1), "M1 stays live");
        assert!(hnsw.contains(m3), "M3 stays live");
        assert!(!hnsw.is_tombstoned(m1));
        assert!(!hnsw.is_tombstoned(m3));

        // Idempotent: a second pass tombstones nothing more.
        let n2 = reconcile_forgotten_memories(&hnsw, &md, &snapshot_ids).unwrap();
        assert_eq!(n2, 0, "reconciliation is idempotent");
    }
}
