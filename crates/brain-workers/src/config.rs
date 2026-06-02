//! Worker configuration.
//!
//! `WorkerKind` enumerates the 12 workers. `WorkerConfig` is the shared
//! bag of knobs every worker shares; per-worker configs add their own
//! fields on top.

use std::time::Duration;

/// — one variant per shipped worker.
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
    // Typed-graph workers.
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
    /// memory→memory edges from cause to effect. Typed-graph only:
    /// no-schema deployments resolve an empty whitelist and the
    /// worker no-ops.
    CausalEdge,
    /// Drains the per-shard `STATEMENT_EMBED_QUEUE` redb table,
    /// embeds each pending statement's `subject + predicate + object`
    /// text via the shared BGE dispatcher, and inserts the resulting
    /// 384-d vector into the per-shard `StatementHnswIndex`. Without
    /// this worker the statement HNSW stays empty forever and the
    /// hybrid query path's statement-corpus semantic retriever returns
    /// zero hits — hybrid recall over statements degenerates to
    /// BM25 + graph only.
    StatementEmbed,
    /// Walks active Statement rows and re-aggregates their stored
    /// confidence via noisy-OR with kind-specific decay. Without this
    /// worker long-running deployments accumulate overly-high
    /// confidence on Facts/Preferences whose evidence has aged out,
    /// because confidence is snapshotted at write/touch time and never
    /// lazily recomputed at read.
    ConfidenceSweep,
    /// Sweeps the `merge_review_queue` re-evaluating Pending entity-
    /// merge proposals that the resolver filed in the
    /// `[0.7, 0.78)` partial-match band. As the entity HNSW absorbs
    /// new aliases and paraphrases, the cosine between a queued
    /// `source` and its `candidate` shifts; this worker promotes
    /// proposals whose recomputed cosine clears the auto-apply
    /// threshold (default 0.95), rejects ones that have dropped below
    /// the partial-match floor (default 0.7), and expires proposals
    /// older than `expire_after_secs` (default 30 days).
    AmbiguityResolver,
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
            Self::StatementEmbed => "statement_embed",
            Self::ConfidenceSweep => "confidence_sweep",
            Self::AmbiguityResolver => "ambiguity_resolver",
        }
    }
}

/// — knobs every worker shares.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// Disabled workers stay registered (for introspection) but their
    /// loop never calls `run_cycle`: operator command
    /// `ADMIN_WORKER_STOP` flips this to `false`.
    pub enabled: bool,
    /// Sleep between cycles.
    pub interval: Duration,
    /// Soft cap on units of work per cycle.
    pub batch_size: usize,
    /// Soft cap on wall-clock time per cycle.
    pub max_runtime: Duration,
}

impl WorkerConfig {
    /// Default cadence table. Per-worker configs may tune (e.g., HNSW
    /// maintenance bumps `max_runtime` for the rebuild).
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
            // On by default: an hourly snapshot bounds the next restart's
            // WAL replay. `do_snapshot_cycle` skips empty-HNSW shards to
            // avoid writing redundant CHECKPOINT records.
            WorkerKind::Snapshot => (true, Duration::from_secs(3600), 1, 300_000),
            // Typed-graph workers.
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
            // Statement-embed drains a redb queue, embeds in batches,
            // and inserts into HNSW. 1s tick is more than enough to
            // keep up with extractor throughput (statements/sec is
            // bounded by LLM cost); batch_size=256 caps the per-cycle
            // queue scan; max_runtime=5s caps a pathological embed
            // batch that exceeds the BGE-small forward-pass budget.
            WorkerKind::StatementEmbed => (true, Duration::from_secs(1), 256, 5_000),
            // Confidence sweep is a heavy-ish scan (each row touches
            // STATEMENTS_TABLE) but the per-row cost is small. 1 h tick
            // matches the decay worker; batch_size=256 paces the read
            // phase so a single cycle never holds the metadata lock
            // long enough to block a writer; max_runtime=10s caps the
            // pathological "huge predicate bucket" case.
            WorkerKind::ConfidenceSweep => (true, Duration::from_secs(3600), 256, 10_000),
            // Ambiguity resolver re-checks merge-review-queue rows.
            // 1 h tick: a proposal's confidence
            // shifts only as the HNSW absorbs new aliases (hours, not
            // seconds). batch_size=64 caps per-cycle wall-clock at
            // 64 * (1 embed + 1 HNSW knn + 1 possible merge_entity) ≈
            // ~ low seconds; max_runtime=10s defends against a stuck
            // embedder.
            WorkerKind::AmbiguityResolver => (true, Duration::from_secs(3600), 64, 10_000),
        };
        Self {
            enabled,
            interval,
            batch_size,
            max_runtime: Duration::from_millis(max_runtime_ms),
        }
    }
}
