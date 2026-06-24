//! `AutoEdgeWorker` metric family.

use std::sync::atomic::{AtomicU64, Ordering};

use super::histograms::{
    WorkerHistogram, WorkerHistogramSnapshot, DEFAULT_CYCLE_BUCKETS_SECONDS,
    DEFAULT_NEIGHBOURS_BUCKETS,
};

/// Metric family for `AutoEdgeWorker`. Shared between the writer
/// (drops counter on `try_send` overflow) and the worker (everything
/// else).
#[derive(Debug)]
pub struct AutoEdgeMetrics {
    drops_total: AtomicU64,
    edges_written_total: AtomicU64,
    cycle_duration_seconds: WorkerHistogram,
    neighbours_found_per_cycle: WorkerHistogram,
}

impl AutoEdgeMetrics {
    /// Construct a zeroed instance. One per shard at startup, shared
    /// by `Arc` between the writer's enqueue path and the worker's
    /// cycle loop.
    #[must_use]
    pub fn new() -> Self {
        Self {
            drops_total: AtomicU64::new(0),
            edges_written_total: AtomicU64::new(0),
            cycle_duration_seconds: WorkerHistogram::new(DEFAULT_CYCLE_BUCKETS_SECONDS),
            neighbours_found_per_cycle: WorkerHistogram::new(DEFAULT_NEIGHBOURS_BUCKETS),
        }
    }

    /// Bumped by the writer's `try_send` path when the bounded channel
    /// is full (encode succeeds; the enqueue is dropped).
    pub fn inc_drop(&self) {
        self.drops_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Bumped by the worker once per logical edge persisted in the
    /// cycle's wtxn.
    pub fn add_edges_written(&self, n: u64) {
        self.edges_written_total.fetch_add(n, Ordering::Relaxed);
    }

    /// Observed by the worker at the end of every cycle (wall-clock).
    pub fn observe_cycle_duration(&self, seconds: f64) {
        self.cycle_duration_seconds.observe(seconds);
    }

    /// Observed by the worker once per cycle: the total number of
    /// post-threshold neighbours collected across the drained
    /// memories. Zero is recorded on empty cycles so PromQL `_count`
    /// matches `brain_worker_cycles_total` for this worker.
    pub fn observe_neighbours_found(&self, n: u64) {
        self.neighbours_found_per_cycle.observe(n as f64);
    }

    /// Read-only snapshot for `/metrics`.
    #[must_use]
    pub fn snapshot(&self) -> AutoEdgeMetricsSnapshot {
        AutoEdgeMetricsSnapshot {
            drops_total: self.drops_total.load(Ordering::Relaxed),
            edges_written_total: self.edges_written_total.load(Ordering::Relaxed),
            cycle_duration_seconds: self.cycle_duration_seconds.snapshot(),
            neighbours_found_per_cycle: self.neighbours_found_per_cycle.snapshot(),
        }
    }
}

impl Default for AutoEdgeMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-data snapshot of [`AutoEdgeMetrics`]. Crosses the shard
/// boundary via `flume` like the existing worker `Snapshot`.
#[derive(Debug, Clone)]
pub struct AutoEdgeMetricsSnapshot {
    pub drops_total: u64,
    pub edges_written_total: u64,
    pub cycle_duration_seconds: WorkerHistogramSnapshot,
    pub neighbours_found_per_cycle: WorkerHistogramSnapshot,
}
