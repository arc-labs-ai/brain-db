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
        };
        Self {
            enabled,
            interval,
            batch_size,
            max_runtime: Duration::from_millis(max_runtime_ms),
        }
    }
}
