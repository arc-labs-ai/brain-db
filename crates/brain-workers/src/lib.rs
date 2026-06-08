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
pub use metrics::{Snapshot as MetricsSnapshot, WorkerMetrics};
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

pub use workers::ambiguity_resolver::{
    AmbiguityResolverConfig, AmbiguityResolverWorker,
    DEFAULT_AUTO_APPLY_THRESHOLD as DEFAULT_AMBIGUITY_RESOLVER_AUTO_APPLY_THRESHOLD,
    DEFAULT_EXPIRE_AFTER_SECS as DEFAULT_AMBIGUITY_RESOLVER_EXPIRE_AFTER_SECS,
    DEFAULT_INTERVAL_SECS as DEFAULT_AMBIGUITY_RESOLVER_INTERVAL_SECS,
    DEFAULT_MAX_PER_TICK as DEFAULT_AMBIGUITY_RESOLVER_MAX_PER_TICK,
    DEFAULT_REJECT_FLOOR as DEFAULT_AMBIGUITY_RESOLVER_REJECT_FLOOR,
    SWEEP_INTERVAL_ENV as AMBIGUITY_RESOLVER_INTERVAL_ENV,
};

pub use workers::access_boost::{
    boosted_salience, AccessBoostWorker, DEFAULT_BOOST_FACTOR, MAX_SALIENCE,
};
pub use workers::auto_edge::{
    resolved_threshold as resolved_auto_edge_threshold, AutoEdgeKnobs, AutoEdgeWorker,
    AUTO_EDGE_THRESHOLD_ENV, DEFAULT_AUTO_EDGE_SIMILARITY_THRESHOLD, DEFAULT_EF_SEARCH,
    DEFAULT_TOP_K,
};
pub use workers::cache_evict::{
    CacheEvictionError, CacheEvictionSource, CacheEvictionWorker, DisabledCacheEvictionSource,
    PruneFuture, DEFAULT_CACHE_MAX_AGE,
};
pub use workers::causal_edge::{
    resolve_whitelist as resolve_causal_whitelist, CausalEdgeKnobs, CausalEdgeWorker,
    DEFAULT_MAX_CAUSE_MEMORIES, DEFAULT_MAX_EFFECT_MEMORIES, DEFAULT_MAX_RELATED_STATEMENTS,
    DEFAULT_MIN_CONFIDENCE as DEFAULT_CAUSAL_MIN_CONFIDENCE, DEFAULT_WHITELIST_QNAMES,
};
pub use workers::confidence_sweep::{
    decay as confidence_decay, ConfidenceSweepKnobs, ConfidenceSweepWorker,
    DEFAULT_BATCH_SIZE as CONFIDENCE_SWEEP_BATCH_SIZE,
    DEFAULT_INTERVAL_SECS as DEFAULT_CONFIDENCE_SWEEP_INTERVAL_SECS,
    DEFAULT_MAX_CHANGE_PER_TICK as CONFIDENCE_SWEEP_MAX_CHANGE_PER_TICK,
    DEFAULT_MAX_PER_TICK as CONFIDENCE_SWEEP_MAX_PER_TICK,
    DEFAULT_MIN_AGE_SECONDS as CONFIDENCE_SWEEP_MIN_AGE_SECONDS,
    DEFAULT_MIN_DRIFT_FOR_WRITE as CONFIDENCE_SWEEP_MIN_DRIFT_FOR_WRITE,
    SWEEP_INTERVAL_ENV as CONFIDENCE_SWEEP_INTERVAL_ENV,
};
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
    RebuildSourceError, RebuildThresholds, SnapshotFuture as RebuildSnapshotFuture,
};
pub use workers::idempotency_cleanup::{IdempotencyCleanupWorker, DEFAULT_IDEMPOTENCY_TTL};
pub use workers::llm_cache_sweeper::{
    LlmCacheSweeper, DEFAULT_INTERVAL_SECS as DEFAULT_LLM_CACHE_SWEEP_INTERVAL_SECS,
    SWEEP_INTERVAL_ENV as LLM_CACHE_SWEEP_INTERVAL_ENV,
};
pub use workers::slot_reclaim::{SlotReclamationWorker, DEFAULT_FORGET_GRACE};
pub use workers::snapshot::{
    decide_retention, DeleteFuture as SnapshotDeleteFuture, DisabledSnapshotSource,
    ListFuture as SnapshotListFuture, RetentionPolicy, SnapshotDesc, SnapshotId, SnapshotSource,
    SnapshotSourceError, SnapshotWorker, TakeFuture as SnapshotTakeFuture,
};
pub use workers::statement_embed::{
    StatementEmbedKnobs, StatementEmbedWorker, DEFAULT_BATCH_SIZE as STATEMENT_EMBED_BATCH_SIZE,
    DEFAULT_MAX_PER_TICK as STATEMENT_EMBED_MAX_PER_TICK,
};
pub use workers::statistics::{StatisticsUpdateWorker, Stats};
pub use workers::temporal_edge::{
    resolved_topical_threshold, TemporalEdgeKnobs, TemporalEdgeWorker, DEFAULT_CROSS_CONTEXT,
    DEFAULT_TEMPORAL_EDGE_TOPICAL_THRESHOLD, DEFAULT_WEIGHT_MIN, DEFAULT_WINDOW_SECONDS,
    TEMPORAL_EDGE_TOPICAL_THRESHOLD_ENV,
};
pub use workers::wal_retention::{
    decide_deletions, CheckpointDesc, CheckpointFuture, DeleteFuture as WalDeleteFuture,
    DisabledWalRetentionSource, SegmentDesc, SegmentListFuture, WalRetentionSource,
    WalRetentionSourceError, WalRetentionWorker,
};
