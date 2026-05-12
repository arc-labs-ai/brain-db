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

pub mod access_boost;
pub mod config;
pub mod consolidation;
pub mod context;
pub mod decay;
pub mod error;
pub mod hnsw_maint;
pub mod metrics;
pub mod scheduler;
pub mod summarizer;
pub mod worker;

pub use access_boost::{boosted_salience, AccessBoostWorker, DEFAULT_BOOST_FACTOR, MAX_SALIENCE};
pub use config::{WorkerConfig, WorkerKind};
pub use consolidation::{
    cluster_by_similarity, cosine, deterministic_request_id, ClusterCandidate, ConsolidationWorker,
    DEFAULT_INITIAL_SALIENCE, DEFAULT_MIN_CLUSTER_SIZE, DEFAULT_RECENCY_WINDOW,
    DEFAULT_SIMILARITY_THRESHOLD,
};
pub use context::WorkerContext;
pub use decay::{
    decayed_salience, half_life_days, DecayWorker, CONSOLIDATED_HALF_LIFE_DAYS,
    EPISODIC_HALF_LIFE_DAYS, MIN_DELTA_FOR_WRITE, SEMANTIC_HALF_LIFE_DAYS,
};
pub use error::WorkerError;
pub use hnsw_maint::{
    decide_action, Action, DisabledRebuildSource, HnswMaintenanceWorker, IndexStats, RebuildSource,
    RebuildSourceError, RebuildThresholds,
};
pub use metrics::{Snapshot as MetricsSnapshot, WorkerMetrics};
pub use scheduler::{WorkerHandle, WorkerScheduler};
pub use summarizer::{DisabledSummarizer, Summarizer, SummarizerError};
pub use worker::{drive_batch, Worker};
