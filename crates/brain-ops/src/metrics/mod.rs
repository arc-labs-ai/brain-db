//! Shared metric state for the writer-fed background workers.
//!
//! Both the writer (which performs the post-ENCODE enqueue) and the
//! worker (which drains the queue and runs the cycle) need to publish
//! into the same counter family. They sit on opposite sides of the
//! `brain-ops` → `brain-workers` dependency, so the atomics live here
//! and both layers hold an `Arc` to the same struct.
//!
//! The structs are deliberately allocation-light at construction:
//! every counter is an `AtomicU64`; every histogram is a fixed-size
//! `Vec<AtomicU64>` sized once. After construction the hot path is
//! lock-free `fetch_add`.
//!
//! `brain-server`'s `/metrics` exposition reads through
//! `snapshot()` on every scrape; production latency is the cost of
//! loading a small number of atomics.
//!
//! Worker families live one-per-file under this module; shared
//! histogram machinery + label arrays live in [`histograms`]. The
//! `crate::worker_metrics` alias (in `lib.rs`) preserves the
//! pre-split external import path.

pub mod ambiguity_resolver;
pub mod auto_edge;
pub mod causal_edge;
pub mod confidence_sweep;
pub mod extractor;
pub mod forget_cascade;
pub mod histograms;
pub mod llm;
pub mod llm_cache;
pub mod schema_migration;
pub mod statement_embed;
pub mod temporal_edge;
pub mod writer;

pub use ambiguity_resolver::{AmbiguityResolverMetrics, AmbiguityResolverMetricsSnapshot};
pub use auto_edge::{AutoEdgeMetrics, AutoEdgeMetricsSnapshot};
pub use causal_edge::{CausalEdgeMetrics, CausalEdgeMetricsSnapshot, CausalSkipReason};
pub use confidence_sweep::{ConfidenceSweepMetrics, ConfidenceSweepMetricsSnapshot};
pub use extractor::{
    ExtractorItemKind, ExtractorMetrics, ExtractorMetricsSnapshot, ResolverOutcome, TierKind,
    TierStatus,
};
pub use forget_cascade::{ForgetCascadeMetrics, ForgetCascadeMetricsSnapshot};
pub use histograms::{
    WorkerBucketSnapshot, WorkerHistogram, WorkerHistogramSnapshot, DEFAULT_CYCLE_BUCKETS_SECONDS,
    DEFAULT_NEIGHBOURS_BUCKETS, ITEM_KIND_LABELS, RESOLVER_OUTCOME_LABELS, TIER_LABELS,
    TIER_STATUS_LABELS,
};
pub use llm::{LlmCacheMetrics, LlmCacheMetricsSnapshot, LlmCacheModelCounts};
pub use llm_cache::{LlmCacheSweepMetrics, LlmCacheSweepMetricsSnapshot};
pub use schema_migration::{SchemaMigrationMetrics, SchemaMigrationMetricsSnapshot};
pub use statement_embed::{StatementEmbedMetrics, StatementEmbedMetricsSnapshot};
pub use temporal_edge::{TemporalEdgeMetrics, TemporalEdgeMetricsSnapshot, TemporalSkipReason};
pub use writer::{
    ApplyErrorSnapshot, IdempotencyOutcome, PerPhaseSnapshot, SubmitOutcome, WriterMetrics,
    WriterMetricsSnapshot,
};
