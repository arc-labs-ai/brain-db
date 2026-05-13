//! # brain-workers
//!
//! Background-worker infrastructure plus the 12 concrete workers
//! (sub-tasks 8.2 – 8.13). v1 runs on the default tokio runtime;
//! Phase 9 swaps in the Glommio shard executor without changing the
//! trait surface.
//!
//! Sub-task 8.1 ships the infrastructure only:
//! - [`Worker`] trait + [`WorkerKind`].
//! - [`WorkerConfig`] with spec §11/01 §11 defaults.
//! - [`WorkerContext`] (handle bag + shutdown signal).
//! - [`WorkerMetrics`] (spec §11/01 §15).
//! - [`WorkerScheduler`] + [`WorkerHandle`].
//! - [`drive_batch`] helper for spec §11/01 §5 / §6 cycle structure.
//!
//! See `spec/11_background_workers/` for the authoritative design.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod config;
pub mod context;
pub mod error;
pub mod metrics;
pub mod scheduler;
pub mod summarizer;
pub mod worker;
pub mod workers;

pub use config::{WorkerConfig, WorkerKind};
pub use context::WorkerContext;
pub use error::WorkerError;
pub use metrics::{Snapshot as MetricsSnapshot, WorkerMetrics};
pub use scheduler::{WorkerHandle, WorkerScheduler};
pub use summarizer::{DisabledSummarizer, Summarizer, SummarizerError};
pub use worker::{drive_batch, Worker};

// Module-level re-exports preserve the pre-refactor public paths
// (`brain_workers::<worker>::<Type>`) so external callers don't churn.
pub use workers::{
    access_boost, cache_evict, consolidation, counter_reconcile, decay, edge_scrub, hnsw_maint,
    idempotency_cleanup, slot_reclaim, snapshot, statistics, wal_retention,
};

pub use workers::access_boost::{
    boosted_salience, AccessBoostWorker, DEFAULT_BOOST_FACTOR, MAX_SALIENCE,
};
pub use workers::cache_evict::{
    CacheEvictionError, CacheEvictionSource, CacheEvictionWorker, DisabledCacheEvictionSource,
    PruneFuture, DEFAULT_CACHE_MAX_AGE,
};
pub use workers::consolidation::{
    cluster_by_similarity, cosine, deterministic_request_id, ClusterCandidate, ConsolidationWorker,
    DEFAULT_INITIAL_SALIENCE, DEFAULT_MIN_CLUSTER_SIZE, DEFAULT_RECENCY_WINDOW,
    DEFAULT_SIMILARITY_THRESHOLD,
};
pub use workers::counter_reconcile::CounterReconcileWorker;
pub use workers::decay::{
    decayed_salience, half_life_days, DecayWorker, CONSOLIDATED_HALF_LIFE_DAYS,
    EPISODIC_HALF_LIFE_DAYS, MIN_DELTA_FOR_WRITE, SEMANTIC_HALF_LIFE_DAYS,
};
pub use workers::edge_scrub::EdgeScrubWorker;
pub use workers::hnsw_maint::{
    decide_action, Action, DisabledRebuildSource, HnswMaintenanceWorker, IndexStats, RebuildSource,
    RebuildSourceError, RebuildThresholds, SnapshotFuture as RebuildSnapshotFuture,
};
pub use workers::idempotency_cleanup::{IdempotencyCleanupWorker, DEFAULT_IDEMPOTENCY_TTL};
pub use workers::slot_reclaim::{SlotReclamationWorker, DEFAULT_FORGET_GRACE};
pub use workers::snapshot::{
    decide_retention, DeleteFuture as SnapshotDeleteFuture, DisabledSnapshotSource,
    ListFuture as SnapshotListFuture, RetentionPolicy, SnapshotDesc, SnapshotId, SnapshotSource,
    SnapshotSourceError, SnapshotWorker, TakeFuture as SnapshotTakeFuture,
};
pub use workers::statistics::{StatisticsUpdateWorker, Stats};
pub use workers::wal_retention::{
    decide_deletions, CheckpointDesc, CheckpointFuture, DeleteFuture as WalDeleteFuture,
    DisabledWalRetentionSource, SegmentDesc, SegmentListFuture, WalRetentionSource,
    WalRetentionSourceError, WalRetentionWorker,
};
