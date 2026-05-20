//! Worker configuration. Spec §11/01 §3 + §11/01 §11.
//!
//! `WorkerKind` enumerates the 12 workers shipped by sub-tasks
//! 8.2 – 8.13. `WorkerConfig` is the shared bag of knobs every worker
//! shares; per-worker configs add their own fields on top.

use std::time::Duration;

/// Spec §11/00 §14 — one variant per shipped worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum WorkerKind {
    Decay,
    AccessBoost,
    Consolidation,
    HnswMaintenance,
    IdempotencyCleanup,
    SlotReclamation,
    WalRetention,
    EdgeScrub,
    CounterReconcile,
    Statistics,
    EmbedderCacheEvict,
    Snapshot,
    // Phase 24 — knowledge-layer workers.
    Backfill,
    ForgetCascade,
    SchemaMigration,
    SupersessionSweeper,
    AuditLogSweeper,
    LlmCacheSweeper,
    StaleExtractionDetector,
    EntityGc,
    /// Derives `SimilarTo` edges from HNSW knn after each successful
    /// ENCODE. Turns the substrate's static vector store into a graph
    /// the planner can traverse without forcing clients to LINK manually.
    AutoEdge,
    /// Runs the three-tier extractor pipeline (pattern + classifier +
    /// LLM) after each ENCODE, then writes the resolved entities /
    /// statements / relations / mention edges back through brain-metadata.
    Extractor,
    /// Derives `FollowedBy` edges by walking the per-agent timeline
    /// index after each ENCODE. Connects each new memory to the
    /// agent's previous memory in the same context, weighted by
    /// elapsed time. The substrate's narrative spine.
    TemporalEdge,
    /// Derives `Caused` edges from extractor-produced causal
    /// statements (predicates `caused_by`, `triggered`, `led_to`, …).
    /// Walks the statement-by-subject index to find the cause-side
    /// memories anchoring the statement's object entity, and writes
    /// memory→memory edges from cause to effect. Knowledge-layer only:
    /// substrate-only deployments resolve an empty whitelist and the
    /// worker no-ops.
    CausalEdge,
}

impl WorkerKind {
    /// Stable name used as the scheduler registry key and in metrics.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Decay => "decay",
            Self::AccessBoost => "access_boost",
            Self::Consolidation => "consolidation",
            Self::HnswMaintenance => "hnsw_maintenance",
            Self::IdempotencyCleanup => "idempotency_cleanup",
            Self::SlotReclamation => "slot_reclamation",
            Self::WalRetention => "wal_retention",
            Self::EdgeScrub => "edge_scrub",
            Self::CounterReconcile => "counter_reconcile",
            Self::Statistics => "statistics",
            Self::EmbedderCacheEvict => "embedder_cache_evict",
            Self::Snapshot => "snapshot",
            Self::Backfill => "backfill",
            Self::ForgetCascade => "forget_cascade",
            Self::SchemaMigration => "schema_migration",
            Self::SupersessionSweeper => "supersession_sweeper",
            Self::AuditLogSweeper => "audit_log_sweeper",
            Self::LlmCacheSweeper => "llm_cache_sweeper",
            Self::StaleExtractionDetector => "stale_extraction_detector",
            Self::EntityGc => "entity_gc",
            Self::AutoEdge => "auto_edge",
            Self::Extractor => "extractor",
            Self::TemporalEdge => "temporal_edge",
            Self::CausalEdge => "causal_edge",
        }
    }
}

/// Spec §11/01 §3 — knobs every worker shares.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// Disabled workers stay registered (for introspection) but their
    /// loop never calls `run_cycle`. Spec §11/01 §13: operator command
    /// `ADMIN_WORKER_STOP` flips this to `false`.
    pub enabled: bool,
    /// Sleep between cycles. Spec §11/01 §11.
    pub interval: Duration,
    /// Soft cap on units of work per cycle. Spec §11/01 §5.
    pub batch_size: usize,
    /// Soft cap on wall-clock time per cycle. Spec §11/01 §5.
    pub max_runtime: Duration,
}

impl WorkerConfig {
    /// Spec §11/01 §11 default cadence table. Per-worker sub-tasks
    /// may tune (e.g., HNSW maintenance bumps `max_runtime` for the
    /// rebuild). Snapshot defaults disabled — operators opt in via
    /// `ADMIN_*_SNAPSHOT` (Phase 9).
    #[must_use]
    pub fn defaults_for(kind: WorkerKind) -> Self {
        let (enabled, interval, batch_size, max_runtime_ms) = match kind {
            WorkerKind::Decay => (true, Duration::from_secs(3600), 10_000, 5_000),
            WorkerKind::AccessBoost => (true, Duration::from_secs(10), 1_000, 500),
            WorkerKind::Consolidation => (true, Duration::from_secs(300), 100, 10_000),
            WorkerKind::HnswMaintenance => (true, Duration::from_secs(300), 1, 60_000),
            WorkerKind::IdempotencyCleanup => (true, Duration::from_secs(3600), 10_000, 5_000),
            WorkerKind::SlotReclamation => (true, Duration::from_secs(600), 1_000, 5_000),
            WorkerKind::WalRetention => (true, Duration::from_secs(60), 100, 2_000),
            WorkerKind::EdgeScrub => (true, Duration::from_secs(1800), 5_000, 5_000),
            WorkerKind::CounterReconcile => (true, Duration::from_secs(3600), 1, 30_000),
            WorkerKind::Statistics => (true, Duration::from_secs(300), 1, 5_000),
            WorkerKind::EmbedderCacheEvict => (true, Duration::from_secs(60), 5_000, 2_000),
            WorkerKind::Snapshot => (false, Duration::from_secs(3600), 1, 300_000),
            // Phase 24 — knowledge workers.
            // Backfill is admin-triggered; the loop ticks fast when work is pending.
            WorkerKind::Backfill => (true, Duration::from_secs(1), 256, 20_000),
            WorkerKind::ForgetCascade => (true, Duration::from_secs(1), 256, 10_000),
            WorkerKind::SchemaMigration => (true, Duration::from_secs(1), 128, 30_000),
            WorkerKind::SupersessionSweeper => (true, Duration::from_secs(86400), 256, 30_000),
            WorkerKind::AuditLogSweeper => (true, Duration::from_secs(86400), 1024, 30_000),
            WorkerKind::LlmCacheSweeper => (true, Duration::from_secs(3600), 1024, 10_000),
            WorkerKind::StaleExtractionDetector => (true, Duration::from_secs(3600), 512, 10_000),
            WorkerKind::EntityGc => (false, Duration::from_secs(86400), 256, 30_000),
            // 100ms tick keeps encode→edge latency tight; batch=256 caps
            // how much HNSW + redb work one cycle can do.
            WorkerKind::AutoEdge => (true, Duration::from_millis(100), 256, 5_000),
            // Extraction is heavier than HNSW knn (pattern + classifier
            // inference + LLM round-trip). 1s tick + 32-memory batch
            // gives the pipeline room to amortise LLM latency without
            // blocking the scheduler. max_runtime=5s caps a stuck LLM
            // call from monopolising the lane.
            WorkerKind::Extractor => (true, Duration::from_secs(1), 32, 5_000),
            // Temporal-edge derivation is one redb point-lookup per
            // enqueue. Cheap; tick at 100ms to keep encode→edge
            // latency tight, same shape as AutoEdge.
            WorkerKind::TemporalEdge => (true, Duration::from_millis(100), 256, 5_000),
            // Causal-edge derivation runs after statement-create, which
            // is already latency-tolerant (LLM in the loop). 200ms tick
            // + 64-statement batch trades a small extra latency for
            // less wakeup churn. max_runtime=5s caps any pathological
            // statement-by-subject scan.
            WorkerKind::CausalEdge => (true, Duration::from_millis(200), 64, 5_000),
        };
        Self {
            enabled,
            interval,
            batch_size,
            max_runtime: Duration::from_millis(max_runtime_ms),
        }
    }
}
