//! `LlmCacheSweeper` metric family.
//!
//! Counts every sweep tick + the rows the sweeper deleted, plus a
//! histogram of per-tick wall-clock duration. Lives here rather than
//! inside `brain-workers` so `/metrics` exposition (in `brain-server`)
//! can read the snapshot without forming a `brain-server →
//! brain-workers` dependency edge.

use std::sync::atomic::{AtomicU64, Ordering};

use super::histograms::{WorkerHistogram, WorkerHistogramSnapshot, DEFAULT_CYCLE_BUCKETS_SECONDS};

/// Metric family for the LLM-cache sweeper worker. One per shard,
/// shared by `Arc` between the worker (the producer) and the metrics
/// exposition path (the consumer).
#[derive(Debug)]
pub struct LlmCacheSweepMetrics {
    sweeps_total: AtomicU64,
    rows_removed_total: AtomicU64,
    sweep_duration_seconds: WorkerHistogram,
}

impl LlmCacheSweepMetrics {
    /// Construct a zeroed instance.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sweeps_total: AtomicU64::new(0),
            rows_removed_total: AtomicU64::new(0),
            sweep_duration_seconds: WorkerHistogram::new(DEFAULT_CYCLE_BUCKETS_SECONDS),
        }
    }

    /// Bumped once per sweep tick, regardless of whether any rows
    /// were removed. Pairs with `sweep_duration_seconds.count` for
    /// sanity checking under PromQL.
    pub fn inc_sweeps(&self) {
        self.sweeps_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Bumped by the worker once per tick with the count of rows the
    /// underlying `sweep_expired` call removed. Passing zero is fine
    /// — the counter advances by zero and `sweeps_total` still ticks
    /// so empty cycles are visible.
    pub fn add_rows_removed(&self, n: u64) {
        self.rows_removed_total.fetch_add(n, Ordering::Relaxed);
    }

    /// Observed by the worker at the end of every tick (wall-clock).
    pub fn observe_sweep_duration(&self, seconds: f64) {
        self.sweep_duration_seconds.observe(seconds);
    }

    /// Read-only snapshot for `/metrics`.
    #[must_use]
    pub fn snapshot(&self) -> LlmCacheSweepMetricsSnapshot {
        LlmCacheSweepMetricsSnapshot {
            sweeps_total: self.sweeps_total.load(Ordering::Relaxed),
            rows_removed_total: self.rows_removed_total.load(Ordering::Relaxed),
            sweep_duration_seconds: self.sweep_duration_seconds.snapshot(),
        }
    }
}

impl Default for LlmCacheSweepMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-data snapshot of [`LlmCacheSweepMetrics`]. Crosses the shard
/// boundary via `flume` like the other worker snapshots.
#[derive(Debug, Clone)]
pub struct LlmCacheSweepMetricsSnapshot {
    pub sweeps_total: u64,
    pub rows_removed_total: u64,
    pub sweep_duration_seconds: WorkerHistogramSnapshot,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_row_observe_is_idempotent_per_tick() {
        let m = LlmCacheSweepMetrics::new();
        m.inc_sweeps();
        m.add_rows_removed(0);
        m.observe_sweep_duration(0.0001);
        let s = m.snapshot();
        assert_eq!(s.sweeps_total, 1);
        assert_eq!(s.rows_removed_total, 0);
        assert_eq!(s.sweep_duration_seconds.count, 1);
    }
}
