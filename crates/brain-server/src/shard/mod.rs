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
//! (layers), §01/05 (hardware: io_uring, CPU pinning),
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
pub mod llm_setup;
pub mod tantivy_recovery;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use brain_core::{ShardId, SlotVersion};
use brain_embed::{Dispatcher, VECTOR_DIM};
use brain_index::entity_hnsw::{EntityHnswIndex, EntityHnswParams};
use brain_index::statement_hnsw::{StatementHnswIndex, StatementHnswParams};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::error::OpError;
use brain_ops::subscribe::EventEnvelope;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::RequestBody;
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

use self::adapters::{ArenaRebuildSource, ShardSnapshotSource, WalDirRetentionSource};
use flume::{Receiver, Sender};
use glommio::{ExecutorJoinHandle, LocalExecutorBuilder, Placement};
use tracing::{error, info, warn};

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
    ///
    /// `caller` carries the authenticated agent from the
    /// connection's `ConnPhase::Established.agent`. The shard
    /// passes it to `brain_ops::dispatch`, which stamps it onto
    /// the per-request `ExecutorContext` so the writer-built Ops
    /// know who they belong to.
    DispatchOp {
        req: Box<RequestBody>,
        caller: brain_ops::RequestCaller,
        reply_tx: Sender<Result<brain_ops::DispatchOutcome, OpError>>,
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
    /// Trigger a synchronous snapshot; sub-task 10.9.
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
    /// `EXTRACT_BACKFILL`: enqueue existing memories onto the
    /// per-shard ExtractorWorker channel for re-extraction. Used by
    /// `brain-cli extract --backfill` after a fresh schema upload or
    /// after enabling the worker on a populated shard.
    ExtractBackfill {
        selector: brain_protocol::BackfillSelector,
        reply_tx: Sender<Result<ExtractBackfillReport, String>>,
    },
    /// Auto-abort every Active txn owned by `session_id`. Fanned out
    /// by the connection layer the moment a TCP/TLS connection drops
    /// before TXN_COMMIT (spec §05/04, §01/03 §8). Reply carries the
    /// count of aborted entries for connection-layer logging; the
    /// individual `TxnId`s stay on the shard.
    AbortOrphanedTxns {
        session_id: [u8; 16],
        reply_tx: Sender<usize>,
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
    /// Phase B: per-shard auto-edge worker knobs. Defaults registered
    /// every shard with a 100 ms tick, top_k=5, threshold=0.85,
    /// channel cap 1024. Set `enabled=false` to skip registration
    /// entirely (no worker, no channel, encodes see a `None` sender).
    pub auto_edge: AutoEdgeSpawnConfig,
    /// Phase E: per-shard extractor pipeline knobs. Same shape as
    /// `auto_edge` — `enabled=false` skips registration entirely.
    pub extractor: ExtractorSpawnConfig,
    /// Phase T: per-shard temporal-edge worker knobs. Same shape as
    /// `auto_edge`; `enabled=false` skips registration entirely.
    pub temporal_edge: TemporalEdgeSpawnConfig,
    /// Phase C: per-shard causal-edge worker knobs. Extractor-driven;
    /// `enabled=false` skips registration entirely (no worker, no
    /// channel, the extractor's enqueue path stays `None`).
    pub causal_edge: CausalEdgeSpawnConfig,
    /// Text → vector dispatcher used inside the shard's executor.
    ///
    /// The same `Arc` is cloned into every shard at process startup so
    /// the ~130 MiB BERT weights are loaded once and shared across all
    /// N shards ("weights shared via Arc<Model>"). In
    /// production this is a `CachingDispatcher<CpuDispatcher>`; tests
    /// inject a file-local stub.
    pub dispatcher: Arc<dyn Dispatcher>,
    /// Operator gate on the cross-encoder rerank capability. Operator
    /// flips `enabled = false` to opt out — request-time opt-ins then
    /// surface as `CapabilityNotEnabled`. Enabled-but-fails-to-load is
    /// a hard spawn failure (see [`ShardError::CrossEncoderInitFailed`]).
    pub rerank: RerankSpawnConfig,
    /// Per-tier extractor gates. `Disabled` skips the materialiser
    /// for that tier; rows of that kind never make it into the
    /// registry. `Enabled` lets the materialiser run; init-time
    /// errors there propagate as [`ShardError::ExtractorInitFailed`].
    pub extractors: ExtractorTierSpawnConfig,
    /// Provider credentials / model overrides for the LLM extractor
    /// tier, ferried from `Config.llm`. Resolved env-first /
    /// config-fallback at shard spawn (`llm_setup::build_llm_deps`).
    pub llm: LlmSpawnConfig,
}

/// Knobs ferried from `Config.llm` into the spawn path: provider
/// credentials + model overrides for the LLM extractor tier. Local to
/// the shard module (mirrors the other `*SpawnConfig` types) so the
/// `#[path]`-mounted integration tests don't pull in `crate::config`.
/// Empty / `None` fields fall back to the environment at resolution
/// time.
#[derive(Clone, Debug, Default)]
pub struct LlmSpawnConfig {
    pub openai_api_key: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub openai_model: Option<String>,
    pub anthropic_model: Option<String>,
}

/// Knobs ferried from `Config.rerank` into the spawn path.
#[derive(Clone, Debug)]
pub struct RerankSpawnConfig {
    pub enabled: bool,
}

impl Default for RerankSpawnConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Knobs ferried from `Config.extractors` into the spawn path. One
/// `enabled` bit per tier; the materialiser honours each in turn.
#[derive(Clone, Copy, Debug)]
pub struct ExtractorTierSpawnConfig {
    pub pattern_enabled: bool,
    pub classifier_enabled: bool,
    pub llm_enabled: bool,
}

impl Default for ExtractorTierSpawnConfig {
    fn default() -> Self {
        Self {
            pattern_enabled: true,
            classifier_enabled: true,
            llm_enabled: true,
        }
    }
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
#[derive(Clone, Debug)]
pub struct ExtractorSpawnConfig {
    pub enabled: bool,
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
            enabled: true,
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
    pub cross_context: bool,
    /// Cosine similarity floor for the topical gate. See
    /// [`brain_workers::TemporalEdgeKnobs::topical_threshold`].
    /// Server config materialises this from
    /// `BRAIN_TEMPORAL_EDGE_TOPICAL_THRESHOLD` (via
    /// `brain_workers::resolved_topical_threshold`) so the env var
    /// override flows through unchanged.
    pub topical_threshold: f32,
}

impl Default for TemporalEdgeSpawnConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_ms: 100,
            batch_size: 256,
            window_seconds: 300,
            weight_min: 0.1,
            channel_capacity: 1024,
            cross_context: false,
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
            dispatcher,
            rerank: RerankSpawnConfig::default(),
            extractors: ExtractorTierSpawnConfig::default(),
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
    /// Cross-shard event-feed (sub-task 9.11). The shard's
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
    /// Phase B metrics shared with the writer + AutoEdgeWorker for
    /// this shard. `None` when the worker is disabled in spawn
    /// config. The `/metrics` exposition reads this directly (no
    /// channel hop — the atomics are `Send + Sync` and the handle
    /// itself is shared by `Arc`).
    auto_edge_metrics: Option<Arc<brain_ops::AutoEdgeMetrics>>,
    /// Phase E metrics. Same shape as [`Self::auto_edge_metrics`].
    extractor_metrics: Option<Arc<brain_ops::ExtractorMetrics>>,
    /// Phase T (TemporalEdgeWorker) metrics. Same shape.
    temporal_edge_metrics: Option<Arc<brain_ops::TemporalEdgeMetrics>>,
    /// Phase C (CausalEdgeWorker) metrics. Same shape.
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
}

impl ShardHandle {
    #[must_use]
    pub fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    /// Read-only handle to the Phase B (AutoEdgeWorker) metric
    /// state for this shard. `None` when the worker was disabled in
    /// spawn config (no-schema deployments / tests).
    #[must_use]
    pub fn auto_edge_metrics(&self) -> Option<Arc<brain_ops::AutoEdgeMetrics>> {
        self.auto_edge_metrics.clone()
    }

    /// Read-only handle to the Phase E (ExtractorWorker) metric
    /// state for this shard.
    #[must_use]
    pub fn extractor_metrics(&self) -> Option<Arc<brain_ops::ExtractorMetrics>> {
        self.extractor_metrics.clone()
    }

    /// Read-only handle to the Phase T (TemporalEdgeWorker) metric
    /// state for this shard.
    #[must_use]
    pub fn temporal_edge_metrics(&self) -> Option<Arc<brain_ops::TemporalEdgeMetrics>> {
        self.temporal_edge_metrics.clone()
    }

    /// Read-only handle to the Phase C (CausalEdgeWorker) metric
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
    /// snapshot's id on success; sub-task 10.9.
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

    /// Trigger an immediate full HNSW rebuild. Returns the new
    /// entry count + elapsed time; sub-task 10.10.
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
    ///
    /// `caller` carries the authenticated agent from the
    /// connection's `ConnPhase::Established.agent`. The shard
    /// passes it through to `brain_ops::dispatch`, which stamps it
    /// onto the per-request `ExecutorContext` so the writer-built
    /// Ops know who they belong to — closing the multi-tenant leak
    /// on shared shards.
    pub async fn dispatch_op(
        &self,
        req: RequestBody,
        caller: brain_ops::RequestCaller,
    ) -> Result<brain_ops::DispatchOutcome, DispatchError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::DispatchOp {
                req: Box::new(req),
                caller,
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

    /// Auto-abort every Active txn this shard holds for the given
    /// wire session. The connection layer invokes this on every shard
    /// in the topology when a TCP/TLS connection drops before the
    /// client committed (spec §05/04: "On TXN_ABORT or connection
    /// drop before commit, none of the operations take effect"). The
    /// shard does the work synchronously inside its Glommio executor;
    /// the reply carries the number of entries swept so the
    /// connection-layer logger can summarise.
    ///
    /// Returns `Ok(0)` when no txns belonged to that session (the
    /// common case — most connections don't open a txn).
    pub async fn abort_orphaned_for_session(
        &self,
        session_id: [u8; 16],
    ) -> Result<usize, DispatchError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send_async(ShardRequest::AbortOrphanedTxns {
                session_id,
                reply_tx,
            })
            .await
            .map_err(|_| DispatchError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| DispatchError::ShardDisconnected)
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
    hnsw_shared: SharedHnsw,
}

/// Register every Phase-8 worker against `scheduler`. Sub-task 9.8
/// plugs in real adapters for `RebuildSource`, `WalRetentionSource`,
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
    let lexical_retriever: Arc<dyn brain_index::LexicalRetriever> = Arc::new(
        brain_index::TantivyLexicalRetriever::new(tantivy_shard.clone())
            .map_err(|source| ShardError::LexicalRetrieverInitFailed { source })?,
    );

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
    let auto_edge_spawn_cfg_for_closure = cfg.auto_edge.clone();
    let extractor_spawn_cfg_for_closure = cfg.extractor.clone();
    let temporal_edge_spawn_cfg_for_closure = cfg.temporal_edge.clone();
    let causal_edge_spawn_cfg_for_closure = cfg.causal_edge.clone();

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
                brain_ops::CrossEncoderSlot::Enabled(Arc::new(
                    brain_rerank::RerankService::spawn(encoder),
                ))
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

    // Per-tier extractor gate — used inside the closure when the
    // materialiser walks persisted definitions. Disabled tiers skip
    // materialisation silently (operator opt-out, not a degradation);
    // enabled tiers that fail to init surface as
    // `ShardError::ExtractorInitFailed` (the materialiser already
    // returns per-row errors; we promote any LLM / classifier error
    // there into a hard spawn failure when the tier is enabled).
    let tier_gate = brain_extractors::TierGate {
        pattern: brain_extractors::TierState::from_enabled(cfg.extractors.pattern_enabled),
        classifier: brain_extractors::TierState::from_enabled(cfg.extractors.classifier_enabled),
        llm: brain_extractors::TierState::from_enabled(cfg.extractors.llm_enabled),
    };
    let tier_gate_for_closure = tier_gate;
    // Whether the LLM extractor tier is enabled in config. Captured for
    // the executor closure so it can warn (after `build_llm_deps`) when
    // the tier is on but no provider key is in the environment — that
    // combination silently yields zero statements/relations otherwise.
    let llm_tier_enabled_for_closure = cfg.extractors.llm_enabled;
    // Provider credentials / model overrides for the LLM extractor
    // tier, resolved env-first / config-fallback inside the closure.
    let llm_config_for_closure = cfg.llm.clone();
    // Construct the Phase B / Phase E metric handles up-front so we
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
        if cfg.extractor.enabled {
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
    // Clone the process-wide dispatcher Arc into the executor closure.
    // The CachingDispatcher<CpuDispatcher> built once in main.rs is
    // shared across every shard so the BERT weights live in memory
    // exactly once no matter how many shards spawn.
    let dispatcher_for_closure = cfg.dispatcher.clone();
    let wal_dir_for_executor = wal_dir.clone();
    let arena_path_for_executor = arena_path.clone();
    let metadata_path_for_executor = metadata_path.clone();
    let snapshots_root_for_executor = dir.join("snapshots");
    // 21.5: the shard dir is also home to the per-shard LLM
    // extractor response cache (`<shard_dir>/llm_cache.redb`).
    let shard_dir_for_executor = dir.clone();
    // Tantivy is opened above and propagated into the closure pre-built.
    // The closure can no longer downgrade lexical retrieval to `None`.
    let tantivy_for_closure = tantivy_shard.clone();
    let lexical_retriever_for_closure = lexical_retriever.clone();
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
            // Per-shard semantic retriever. Reuses the executor's
            // embedder + the shared memory HNSW reader. The statement
            // HNSW handle lets the retriever fan out to the statement
            // corpus when `SemanticScope::Statement` or
            // `SemanticScope::Both` is requested.
            let semantic_retriever_for_ops: Arc<dyn brain_index::SemanticRetriever> = Arc::new(
                brain_ops::index::semantic_retriever::BrainSemanticRetriever::new(
                    dispatcher.clone(),
                    hnsw_shared.clone(),
                    Some(statement_hnsw_for_shard.clone()),
                    metadata.clone(),
                ),
            );
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

            // Phase B: per-shard AutoEdgeWorker channel. The sender
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

            // Phase E: per-shard ExtractorWorker channel. Same shape
            // as auto-edge — disabled means no channel, no worker, no
            // overhead. The writer stores the Sender; the Receiver
            // moves into the worker we register below.
            let extractor_spawn_cfg = extractor_spawn_cfg_for_closure.clone();
            let (extractor_sender, extractor_receiver) = if extractor_spawn_cfg.enabled {
                let (tx, rx) = flume::bounded::<brain_ops::ExtractorEnqueue>(
                    extractor_spawn_cfg.channel_capacity.max(1),
                );
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };

            // Phase T: per-shard TemporalEdgeWorker channel. Mirrors
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

            // Phase C: per-shard CausalEdgeWorker channel. Driven by
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

            // Phase 20.7: materialise the persisted `EXTRACTORS_TABLE`
            // rows (seeded by the system-schema bootstrap at
            // MetadataDb::open) into a runtime ExtractorRegistry.
            //
            // Phase 21.5 lights up the LLM-tier deps the materializer
            // needs: a `ModelRouter` built from provider keys (env
            // ANTHROPIC_API_KEY / OPENAI_API_KEY first, then the `[llm]`
            // config section) and the per-shard `llm_cache.redb`. Both
            // slots default to `None` so shards started without an LLM
            // cache or any key configured stay unchanged.
            let llm_deps =
                llm_setup::build_llm_deps(&shard_dir_for_executor, &llm_config_for_closure);
            // The LLM tier is on (all tiers default to enabled) but no
            // provider client could be built — no OPENAI_API_KEY /
            // ANTHROPIC_API_KEY in the env and no `[llm]` key in config.
            // This is the expected out-of-box state, not a
            // misconfiguration: pattern + classifier still extract
            // entities; only statements/relations (LLM-only) are
            // skipped. So it's INFO, not WARN — the server boots clean.
            // The note stays discoverable for anyone wondering why a
            // memory has entities but no knowledge.
            if llm_tier_enabled_for_closure && llm_deps.router.is_none() {
                tracing::info!(
                    target: "brain_server::shard",
                    "LLM extractor tier has no provider key (set OPENAI_API_KEY / \
                     ANTHROPIC_API_KEY or `[llm] openai_api_key` in config); entities \
                     still extract, statements/relations are skipped until a key is set",
                );
            }
            let llm_cache_for_ops = llm_deps.cache.clone();
            // Snapshot the disambiguator before `llm_deps` is consumed
            // by `into_materialize_deps` below — the extractor worker
            // wires it directly into the resolver path so ambiguous-
            // band partial matches get a second opinion.
            let entity_disambiguator_for_worker = llm_deps.disambiguator.clone();

            // Tantivy is opened (and any post-recovery rebuilds run)
            // before the executor closure spawns — see "Tantivy open
            // + recovery" above. We just consume the pre-built handle
            // here. `IndexStatus::NeedsRebuild` is *not* a spawn
            // failure: the maintenance worker handles steady-state
            // rebuilds while the live indexes keep serving reads.
            let tantivy_for_ops = tantivy_for_closure;

            // Resolve the classifier model config. Cascades through
            // `BRAIN_NER_MODEL_PATH` (operator override) and the XDG
            // default location populated by
            // `scripts/bootstrap-model.sh`, mirroring the bootstrap
            // script's own resolution order so an operator who ran the
            // script gets a working classifier on the next boot without
            // exporting an env var. Built once and reused: the loaded
            // `Arc<dyn ClassifierModel>` feeds the `MaterializeDeps`
            // (so classifier-kind extractor rows decode into wired
            // extractors instead of degraded ones) and the same
            // `ClassifierConfig` stays on the OpsContext for diagnostic
            // reporting.
            let classifier_config = brain_extractors::ClassifierConfig::auto_discover();

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
                         BRAIN_NER_MODEL_PATH); only the pattern tier will contribute",
                    );
                    None
                };

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
                let (reg, errors) = brain_extractors::build_registry_with_gate(
                    &defs,
                    &materialize_deps,
                    tier_gate_for_closure,
                );
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

            // 22.3 + 22.4: spawn the per-shard text indexer drain
            // tasks and install their dispatchers. The writer holds
            // the memory dispatcher so single-op ENCODE and TXN
            // batches share one dispatch point; OpsContext also
            // carries it so FORGET (which doesn't go through the
            // writer's post-commit hook) can tombstone the row.
            // No-schema deployments (no tantivy handle) skip
            // both.
            let (memory_text_dispatcher_for_ops, statement_text_dispatcher_for_ops) = {
                let policy = brain_ops::index::text_indexer::CommitPolicy::from_env();

                let memory_dispatcher = {
                    let (dispatcher, receiver) =
                        brain_ops::index::text_indexer::MemoryTextDispatcher::default_channel();
                    match brain_ops::index::text_indexer::memory::spawn_memory_text_indexer_local(
                        tantivy_for_ops.memory_text.clone(),
                        receiver,
                        policy,
                    ) {
                        Ok(()) => Some(Arc::new(dispatcher)),
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
                    match brain_ops::index::text_indexer::statement::spawn_statement_text_indexer_local(
                        tantivy_for_ops.statements.clone(),
                        receiver,
                        policy,
                    ) {
                        Ok(()) => Some(Arc::new(dispatcher)),
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

            let mut real_writer = RealWriterHandle::new(metadata.clone(), hnsw_writer)
                .with_shard_id(shard_id)
                .with_event_bus(event_bus.clone())
                .with_wal_sink(wal_sink);
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
            let writer: Arc<dyn WriterHandle> = Arc::new(real_writer);
            let executor_ctx = ExecutorContext::new(
                dispatcher.clone(),
                hnsw_shared.clone(),
                metadata.clone(),
                writer,
            );

            // The lexical retriever was constructed alongside the
            // tantivy open above and propagated in here pre-built.
            // Hybrid query (phase 23) consumes it via
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
                .with_wal_sink(Some(wal_sink_for_ops)),
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
            // crashes (4).
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
            glommio::spawn_local(async move {
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
            })
            .detach();

            // Build real Phase-8 adapters (sub-task 9.8).
            let rebuild_source: Arc<dyn RebuildSource<{ VECTOR_DIM }>> = Arc::new(
                ArenaRebuildSource::<{ VECTOR_DIM }>::new(shard_id, arena_cell.clone()),
            );
            // Keep a clone for the admin `rebuild-ann` route (10.10).
            let rebuild_source_for_shard = rebuild_source.clone();

            // Recovery step 6 (spec 08.04 §8): restore the memory HNSW.
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
                                let codebook = brain_index::bootstrap_codebook();
                                let (idx, _) = brain_index::rebuild::rebuild_impl::<8, _>(
                                    params, codebook, combined,
                                )?;
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

            // Recovery: rebuild the entity HNSW (resolver tier-3 embedding
            // tie-break) from the metadata store. Like the memory HNSW it's
            // in-RAM only and not persisted; without this the resolver loses
            // its embedding tie-break after restart and over-creates
            // duplicate entities until each surface is re-extracted.
            //
            // Prefer the durable vector written at entity-create time
            // (`spec/09/06_persistence.md §6`): a stored vector goes
            // straight into the HNSW with no embedder call. Rows
            // without a stored vector (pre-feature data, or a partial
            // write) fall back to re-embedding the canonical name.
            match metadata.read_txn() {
                Ok(rtxn) => match brain_metadata::entity::ops::entity_iter_all_live_with_vectors(
                    &rtxn,
                ) {
                    Ok(entities) if !entities.is_empty() => {
                        let count = entities.len();
                        let mut pairs: Vec<(brain_core::EntityId, [f32; VECTOR_DIM])> =
                            Vec::with_capacity(count);
                        let mut from_stored = 0usize;
                        let mut from_reembed = 0usize;
                        let mut embed_failures = 0usize;
                        for (id, name, stored) in entities {
                            if let Some(v) = stored {
                                pairs.push((id, v));
                                from_stored += 1;
                            } else {
                                match dispatcher.embed(&name) {
                                    Ok(v) => {
                                        pairs.push((id, v));
                                        from_reembed += 1;
                                    }
                                    Err(_) => embed_failures += 1,
                                }
                            }
                        }
                        match entity_hnsw_for_shard.write().rebuild(pairs) {
                            Ok(_) => info!(
                                shard_id,
                                rebuilt = count,
                                from_stored,
                                from_reembed,
                                embed_failures,
                                "entity HNSW rebuilt from metadata on startup"
                            ),
                            Err(e) => error!(
                                shard_id,
                                error = ?e,
                                "entity HNSW startup rebuild failed; entity resolution degraded"
                            ),
                        }
                    }
                    Ok(_) => {}
                    Err(e) => error!(
                        shard_id,
                        error = ?e,
                        "entity HNSW startup rebuild: metadata scan failed"
                    ),
                },
                Err(e) => error!(
                    shard_id,
                    error = ?e,
                    "entity HNSW startup rebuild: read_txn failed"
                ),
            }

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

            // LLM cache sweeper. Only register when the shard actually
            // has an LLM cache — without one the worker is a no-op and
            // wiring it adds nothing but a wakeup every hour.
            if ops.llm_cache.is_some() {
                let sweeper = LlmCacheSweeper::new()
                    .with_metrics(llm_cache_sweep_metrics_for_closure.clone());
                scheduler
                    .register(Arc::new(sweeper), ops.clone())
                    .expect("register LlmCacheSweeper");
            }

            // StatementEmbedWorker — drains the redb embed queue, runs
            // each pending statement through the BGE dispatcher, and
            // populates the per-shard StatementHnswIndex. Without this
            // worker the hybrid query path's statement-corpus semantic
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
                .with_metrics(statement_embed_metrics_for_closure.clone());
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
            // Registered unconditionally: substrate-only shards find an
            // empty STATEMENTS_TABLE every cycle and return 0 with
            // negligible cost.
            {
                let worker = brain_workers::ConfidenceSweepWorker::new(metadata.clone())
                    .with_metrics(confidence_sweep_metrics_for_closure.clone());
                scheduler
                    .register(Arc::new(worker), ops.clone())
                    .expect("register ConfidenceSweepWorker");
            }

            // Phase B: register the AutoEdgeWorker when the channel was
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

            // Phase T: register the TemporalEdgeWorker when its
            // channel was created above. Drains the writer's post-
            // encode channel, looks up the agent's prior memory, and
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
                    cross_context: temporal_edge_spawn_cfg.cross_context,
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

            // Phase E: register the ExtractorWorker when its channel was
            // created above (i.e. when `cfg.extractor.enabled` is true).
            // The worker drains the writer's post-encode channel and
            // runs the three-tier extractor pipeline against each
            // memory's text, writing entities / statements / relations /
            // mention edges through brain-metadata.
            if let Some(rx) = extractor_receiver {
                let worker_cfg = WorkerConfig {
                    enabled: extractor_spawn_cfg.enabled,
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
                };
                let mut extractor_worker = ExtractorWorker::new(rx)
                    .with_config(worker_cfg)
                    .with_knobs(knobs)
                    .with_embed_deps(brain_extractors::resolver::EmbeddingDeps {
                        hnsw: entity_hnsw_for_shard.clone(),
                        embedder: dispatcher.clone(),
                    });
                if let Some(d) = entity_disambiguator_for_worker.clone() {
                    extractor_worker = extractor_worker.with_entity_disambiguator(d);
                }
                if let Some(m) = extractor_metrics_for_closure.clone() {
                    extractor_worker = extractor_worker.with_metrics(m);
                }
                // Phase C: when the CausalEdgeWorker is also enabled
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
                scheduler
                    .register(Arc::new(extractor_worker), ops.clone())
                    .expect("register ExtractorWorker");
            }

            // Phase C: register the CausalEdgeWorker when its channel
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
            };
            shard_main_loop(shard, rx).await;
        })
        .map_err(|e| ShardError::Spawn(e.to_string()))?;
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
            ShardRequest::ExtractBackfill { selector, reply_tx } => {
                let out = run_extract_backfill(&shard, selector);
                if reply_tx.send_async(out).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "ExtractBackfill reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::RebuildHnsw { reply_tx } => {
                let start = std::time::Instant::now();
                let result = match shard.rebuild_source.snapshot_vectors().await {
                    Ok(vectors) => {
                        let params = shard.hnsw_shared.params();
                        let codebook = brain_index::bootstrap_codebook();
                        match brain_index::rebuild::rebuild_impl::<8, _>(params, codebook, vectors)
                        {
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
            ShardRequest::AbortOrphanedTxns {
                session_id,
                reply_tx,
            } => {
                // Synchronous on the shard executor: `TxnStore` is a
                // parking_lot::Mutex over a HashMap, so the sweep is
                // a single bounded pass. Returning the count (not the
                // ids) keeps the cross-runtime reply trivially Send.
                let aborted = shard
                    .ops
                    .txn_store
                    .abort_orphaned_for_session(session_id)
                    .len();
                if reply_tx.send_async(aborted).await.is_err() {
                    warn!(
                        shard_id = shard.shard_id,
                        "AbortOrphanedTxns reply dropped (caller gone)"
                    );
                }
            }
            ShardRequest::DispatchOp {
                req,
                caller,
                reply_tx,
            } => {
                // `brain_ops::dispatch` is async and runs entirely
                // within the per-shard Glommio executor: it touches
                // `OpsContext` (which is !Send post-9.7a) and yields
                // through Glommio-aware I/O. Awaiting here is sound —
                // the main loop is single-threaded and processes one
                // request at a time, the same shape as
                // `AppendWalRecord`.
                let out = brain_ops::dispatch::dispatch(*req, caller, &shard.ops).await;
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
// Extract-backfill helper
// ---------------------------------------------------------------------------

/// Walk the per-shard `memories` + `texts` redb tables and push each
/// matching memory onto the `WriterHandle`'s extractor channel. Runs
/// inside the shard executor; the metadata read txn is short-lived and
/// dropped before each enqueue so the writer can take a separate
/// `try_send`. No `.await` points — the whole sweep is synchronous
/// against redb + the bounded flume queue.
fn run_extract_backfill(
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
            for entry in memories.iter().map_err(|e| format!("memories.iter: {e}"))? {
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
            for entry in memories.iter().map_err(|e| format!("memories.iter: {e}"))? {
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

/// Snapshot the active schema's entity-type names as qnames
/// (`brain:Person`, etc). Returned in stable id-order so the labels
/// pass we hand to the classifier is deterministic across reopens
/// and shards. Used by `materialize_classifier_extractor` to
/// populate the per-extractor `target_labels` field.
fn snapshot_entity_type_qnames(rtxn: &redb::ReadTransaction) -> Result<Vec<String>, redb::Error> {
    use brain_metadata::tables::entity_type::ENTITY_TYPES_TABLE;
    use redb::ReadableTable;

    let t = rtxn.open_table(ENTITY_TYPES_TABLE)?;
    let mut rows: Vec<(u32, String)> = Vec::new();
    for entry in t.iter()? {
        let (k, v) = entry?;
        rows.push((k.value(), v.value().name));
    }
    rows.sort_by_key(|(id, _)| *id);
    // System schema's entity-type registry pre-dates the namespace
    // scheme; rows store bare names ("Person") and the implicit
    // namespace is `brain:`. Prefix here so GLiNER's labels and the
    // resolver's expected qname shape align.
    Ok(rows
        .into_iter()
        .map(|(_, name)| format!("brain:{name}"))
        .collect())
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
        let id: u64 = match path.file_name().and_then(|n| n.to_str()).and_then(|s| s.parse().ok()) {
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

    /// Sub-task 15.3 — spawning a shard must leave the knowledge-layer
    /// tantivy directories present on disk so phases 16/22 can open
    /// them without a separate mkdir step.
    #[test]
    fn spawn_creates_knowledge_directories() {
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

        // 22.1: spawn now opens the tantivy indexes via
        // `TantivyShard::open`, which calls `Index::create_in_dir`
        // on a fresh shard. The presence of `meta.json` is the
        // observable proof that this happened (a bare mkdir from
        // §15.3 leaves the directory empty).
        assert!(
            paths.memory_text_tantivy().join("meta.json").exists(),
            "memory_text.tantivy/meta.json should exist after spawn (sub-task 22.1)"
        );
        assert!(
            paths.statements_tantivy().join("meta.json").exists(),
            "statements.tantivy/meta.json should exist after spawn (sub-task 22.1)"
        );
    }
}
