//! # brain-workers
//!
//! Background-worker infrastructure plus the 12 concrete workers.
//! v1 runs on the default tokio runtime; the Glommio shard executor
//! later swaps in without changing the trait surface.
//!
//! The infrastructure consists of:
//! - [`Worker`] trait + [`WorkerKind`].
//! - [`WorkerConfig`] with defaults.
//! - [`WorkerContext`] (handle bag + shutdown signal).
//! - [`WorkerMetrics`].
//! - [`WorkerScheduler`] + [`WorkerHandle`].
//! - [`drive_batch`] helper for cycle structure.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod config;
pub mod context;
pub mod env;
pub mod error;
pub mod metrics;
pub mod scheduler;
pub mod summarizer;
pub mod worker;
pub mod workers;

pub use config::{WorkerConfig, WorkerKind};
pub use context::WorkerContext;
pub use env::{parse_enabled, parse_interval_override, parse_positive_seconds};
pub use error::WorkerError;
pub use metrics::{MetricsSnapshot, WorkerMetrics};
pub use scheduler::{WorkerHandle, WorkerScheduler};
pub use summarizer::{DisabledSummarizer, Summarizer, SummarizerError};
pub use worker::{drive_batch, Worker};

// Module-level re-exports preserve the pre-refactor public paths
// (`brain_workers::<worker>::<Type>`) so external callers don't churn.
pub use workers::{
    access_boost, ambiguity_resolver, auto_edge, cache_evict, causal_edge, confidence_sweep,
    consolidation, counter_reconcile, decay, edge_scrub, extractor, hnsw_maint,
    idempotency_cleanup, slot_reclaim, snapshot, statement_embed, statistics, temporal_edge,
    wal_retention,
};

pub use workers::ambiguity_resolver::{AmbiguityResolverConfig, AmbiguityResolverWorker};

pub use workers::access_boost::{
    boosted_salience, AccessBoostWorker, DEFAULT_BOOST_FACTOR, MAX_SALIENCE,
};
pub use workers::auto_edge::{
    resolved_threshold, AutoEdgeKnobs, AutoEdgeWorker, AUTO_EDGE_THRESHOLD_ENV,
    DEFAULT_AUTO_EDGE_SIMILARITY_THRESHOLD, DEFAULT_EF_SEARCH, DEFAULT_TOP_K,
};
pub use workers::cache_evict::{
    CacheEvictionError, CacheEvictionSource, CacheEvictionWorker, DisabledCacheEvictionSource,
    PruneFuture, DEFAULT_CACHE_MAX_AGE,
};
pub use workers::causal_edge::{
    CausalEdgeKnobs, CausalEdgeWorker, DEFAULT_MAX_CAUSE_MEMORIES, DEFAULT_MAX_EFFECT_MEMORIES,
    DEFAULT_MAX_RELATED_STATEMENTS, DEFAULT_WHITELIST_QNAMES,
};
pub use workers::confidence_sweep::{ConfidenceSweepKnobs, ConfidenceSweepWorker};
pub use workers::consolidation::{
    cluster_by_similarity, cosine, deterministic_request_id, ClusterCandidate, ConsolidationWorker,
    DEFAULT_CONSOLIDATION_SIMILARITY_THRESHOLD, DEFAULT_INITIAL_SALIENCE, DEFAULT_MIN_CLUSTER_SIZE,
    DEFAULT_RECENCY_WINDOW,
};
pub use workers::counter_reconcile::CounterReconcileWorker;
pub use workers::decay::{
    decayed_salience, half_life_days, DecayWorker, CONSOLIDATED_HALF_LIFE_DAYS,
    EPISODIC_HALF_LIFE_DAYS, MIN_DELTA_FOR_WRITE, SEMANTIC_HALF_LIFE_DAYS,
};
pub use workers::edge_scrub::EdgeScrubWorker;
pub use workers::extractor::{
    ExtractorKnobs, ExtractorWorker, DEFAULT_EXTRACTOR_BATCH_SIZE,
    DEFAULT_EXTRACTOR_DRAIN_PER_CYCLE, DEFAULT_EXTRACTOR_LLM_BUDGET_MICRO_USD,
    DEFAULT_EXTRACTOR_SKIP_AUDITED, EXTRACTOR_BATCH_SIZE_ENV,
};
pub use workers::hnsw_maint::{
    decide_action, Action, DisabledRebuildSource, HnswMaintenanceWorker, IndexStats, RebuildSource,
    RebuildSourceError, RebuildThresholds, SnapshotFuture,
};
pub use workers::hype::{HypeGenOutcome, HypeGenerator};
pub use workers::idempotency_cleanup::{IdempotencyCleanupWorker, DEFAULT_IDEMPOTENCY_TTL};
pub use workers::llm_cache_sweeper::LlmCacheSweeper;
pub use workers::slot_reclaim::{SlotReclamationWorker, DEFAULT_FORGET_GRACE};
pub use workers::snapshot::{
    decide_retention, DisabledSnapshotSource, RetentionPolicy, SnapshotDesc, SnapshotId,
    SnapshotSource, SnapshotSourceError, SnapshotWorker,
};
pub use workers::statement_embed::{StatementEmbedKnobs, StatementEmbedWorker};
pub use workers::statistics::{StatisticsUpdateWorker, Stats};
pub use workers::temporal_edge::{
    resolved_topical_threshold, TemporalEdgeKnobs, TemporalEdgeWorker, DEFAULT_CROSS_CONTEXT,
    DEFAULT_TEMPORAL_EDGE_TOPICAL_THRESHOLD, DEFAULT_WEIGHT_MIN, DEFAULT_WINDOW_SECONDS,
    TEMPORAL_EDGE_TOPICAL_THRESHOLD_ENV,
};
pub use workers::wal_retention::{
    decide_deletions, CheckpointDesc, CheckpointFuture, DeleteFuture, DisabledWalRetentionSource,
    SegmentDesc, SegmentListFuture, WalRetentionSource, WalRetentionSourceError,
    WalRetentionWorker,
};
