//! `SchemaMigrationWorker` metric family.
//!
//! Mirrors `ForgetCascadeMetrics`: the writer bumps `drops_total` on a
//! full channel; the worker bumps everything else from inside its cycle
//! loop. Both ends hold the same `Arc<SchemaMigrationMetrics>` so a
//! `/metrics` snapshot covers them in one read.
//!
//! The "sweep" here is the post-`SCHEMA_UPLOAD` flagging pass that walks
//! `STATEMENTS_TABLE` and re-aligns the `OUTSIDE_ACTIVE_SCHEMA` flag bit
//! against the just-committed schema vocabulary. Moving the sweep out
//! of the upload transaction keeps upload-commit latency bounded —
//! observability of the deferred work lives in this family.

use std::sync::atomic::{AtomicU64, Ordering};

use super::histograms::{WorkerHistogram, WorkerHistogramSnapshot, DEFAULT_CYCLE_BUCKETS_SECONDS};

/// Per-shard counters surfacing the schema flag-sweep's behaviour. All
/// fields are monotonic — `snapshot()` is a point-in-time read suitable
/// for Prometheus exposition.
#[derive(Debug)]
pub struct SchemaMigrationMetrics {
    drops_total: AtomicU64,
    sweeps_completed_total: AtomicU64,
    rows_flagged_total: AtomicU64,
    rows_cleared_total: AtomicU64,
    errors_total: AtomicU64,
    sweep_duration_seconds: WorkerHistogram,
}

impl SchemaMigrationMetrics {
    /// Construct a zeroed instance. One per shard at startup, shared
    /// by `Arc` between the writer's enqueue path and the worker's
    /// cycle loop.
    #[must_use]
    pub fn new() -> Self {
        Self {
            drops_total: AtomicU64::new(0),
            sweeps_completed_total: AtomicU64::new(0),
            rows_flagged_total: AtomicU64::new(0),
            rows_cleared_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            sweep_duration_seconds: WorkerHistogram::new(DEFAULT_CYCLE_BUCKETS_SECONDS),
        }
    }

    /// Bumped by the writer's `try_send` path when the bounded
    /// sweep channel is full. The SCHEMA_UPLOAD itself still succeeded
    /// — the deferred sweep is just missed for this commit. The worker
    /// will catch up on a later trigger (or via the periodic
    /// reconciliation tick).
    pub fn inc_drop(&self) {
        self.drops_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Bumped once per successful sweep after the commit.
    pub fn add_sweep_completed(&self) {
        self.sweeps_completed_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_rows_flagged(&self, n: u64) {
        self.rows_flagged_total.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_rows_cleared(&self, n: u64) {
        self.rows_cleared_total.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_error(&self) {
        self.errors_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn observe_sweep_duration_seconds(&self, seconds: f64) {
        self.sweep_duration_seconds.observe(seconds);
    }

    #[must_use]
    pub fn snapshot(&self) -> SchemaMigrationMetricsSnapshot {
        SchemaMigrationMetricsSnapshot {
            drops_total: self.drops_total.load(Ordering::Relaxed),
            sweeps_completed_total: self.sweeps_completed_total.load(Ordering::Relaxed),
            rows_flagged_total: self.rows_flagged_total.load(Ordering::Relaxed),
            rows_cleared_total: self.rows_cleared_total.load(Ordering::Relaxed),
            errors_total: self.errors_total.load(Ordering::Relaxed),
            sweep_duration_seconds: self.sweep_duration_seconds.snapshot(),
        }
    }
}

impl Default for SchemaMigrationMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-data snapshot of [`SchemaMigrationMetrics`].
#[derive(Debug, Clone)]
pub struct SchemaMigrationMetricsSnapshot {
    pub drops_total: u64,
    pub sweeps_completed_total: u64,
    pub rows_flagged_total: u64,
    pub rows_cleared_total: u64,
    pub errors_total: u64,
    pub sweep_duration_seconds: WorkerHistogramSnapshot,
}
