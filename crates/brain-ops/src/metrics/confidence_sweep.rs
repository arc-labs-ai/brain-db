//! `ConfidenceSweepWorker` metric family.
//!
//! Lives in `brain-ops` (not `brain-workers`) so `/metrics` exposition
//! in `brain-server` can read snapshots without forming a
//! `brain-server -> brain-workers` dependency edge — mirrors the other
//! background-worker families (`statement_embed`, `llm_cache`, ...).

use std::sync::atomic::{AtomicU64, Ordering};

use super::histograms::{WorkerHistogram, WorkerHistogramSnapshot, DEFAULT_CYCLE_BUCKETS_SECONDS};

/// Counters, gauge, and histogram for the periodic Statement confidence
/// re-aggregation worker. One per shard; shared by `Arc` between the
/// worker (producer) and the `/metrics` exposition path (consumer).
///
/// The confidence sweep walks active statements, recomputes their
/// confidence via noisy-OR with kind-specific decay, and writes back
/// the new value when it moved beyond a small floor. The metrics
/// surface lets operators see how much drift the system is absorbing
/// per cycle and how long the sweep itself takes.
#[derive(Debug)]
pub struct ConfidenceSweepMetrics {
    /// One per tick, regardless of work done.
    cycles_total: AtomicU64,
    /// Statements visited during the read scan.
    rows_swept_total: AtomicU64,
    /// Statements whose confidence was written back to redb.
    rows_updated_total: AtomicU64,
    /// Last cycle's average absolute drift × 1e6 (six decimal places of
    /// `f32` precision). Exposed as an `f32` gauge in
    /// [`ConfidenceSweepMetricsSnapshot::last_avg_drift`].
    last_avg_drift_micro: AtomicU64,
    /// Wall-clock per cycle (one observe per tick).
    duration_seconds: WorkerHistogram,
}

impl ConfidenceSweepMetrics {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cycles_total: AtomicU64::new(0),
            rows_swept_total: AtomicU64::new(0),
            rows_updated_total: AtomicU64::new(0),
            last_avg_drift_micro: AtomicU64::new(0),
            duration_seconds: WorkerHistogram::new(DEFAULT_CYCLE_BUCKETS_SECONDS),
        }
    }

    pub fn inc_cycles(&self) {
        self.cycles_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_rows_swept(&self, n: u64) {
        self.rows_swept_total.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_rows_updated(&self, n: u64) {
        self.rows_updated_total.fetch_add(n, Ordering::Relaxed);
    }

    /// Set the gauge that records the average absolute drift on the
    /// most recent cycle. `0.0` when no rows were updated.
    pub fn set_last_avg_drift(&self, drift: f32) {
        let clamped = drift.max(0.0);
        let scaled = (clamped * 1_000_000.0) as u64;
        self.last_avg_drift_micro.store(scaled, Ordering::Relaxed);
    }

    pub fn observe_duration(&self, seconds: f64) {
        self.duration_seconds.observe(seconds);
    }

    #[must_use]
    pub fn snapshot(&self) -> ConfidenceSweepMetricsSnapshot {
        let drift_micro = self.last_avg_drift_micro.load(Ordering::Relaxed);
        ConfidenceSweepMetricsSnapshot {
            cycles_total: self.cycles_total.load(Ordering::Relaxed),
            rows_swept_total: self.rows_swept_total.load(Ordering::Relaxed),
            rows_updated_total: self.rows_updated_total.load(Ordering::Relaxed),
            last_avg_drift: (drift_micro as f32) / 1_000_000.0,
            duration_seconds: self.duration_seconds.snapshot(),
        }
    }
}

impl Default for ConfidenceSweepMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-data snapshot. Crosses the shard boundary via `flume` like the
/// other worker snapshots.
#[derive(Debug, Clone)]
pub struct ConfidenceSweepMetricsSnapshot {
    pub cycles_total: u64,
    pub rows_swept_total: u64,
    pub rows_updated_total: u64,
    pub last_avg_drift: f32,
    pub duration_seconds: WorkerHistogramSnapshot,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_gauge_clamps_negative_to_zero() {
        let m = ConfidenceSweepMetrics::new();
        m.set_last_avg_drift(-1.0);
        assert_eq!(m.snapshot().last_avg_drift, 0.0);
    }
}
