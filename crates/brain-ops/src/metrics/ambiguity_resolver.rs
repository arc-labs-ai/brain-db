//! `AmbiguityResolverWorker` metric family.
//!
//! Lives in `brain-ops` (not `brain-workers`) so `/metrics` exposition
//! in `brain-server` can read snapshots without forming a
//! `brain-server -> brain-workers` dependency edge — mirrors the other
//! background-worker families (`auto_edge`, `extractor`, `confidence_sweep`).

use std::sync::atomic::{AtomicU64, Ordering};

use super::histograms::{WorkerHistogram, WorkerHistogramSnapshot, DEFAULT_CYCLE_BUCKETS_SECONDS};

/// Counters + histogram for the ambiguity-resolver / merge-review-queue
/// worker. One per shard; shared by `Arc` between the worker (the
/// producer) and the metrics exposition path (the consumer).
#[derive(Debug)]
pub struct AmbiguityResolverMetrics {
    /// One per `tick()` call, regardless of work done.
    sweeps_total: AtomicU64,
    /// Pending proposals the worker promoted to an actual `merge_entity`
    /// call because their recomputed cosine cleared the auto-apply
    /// threshold.
    proposals_promoted_to_merge_total: AtomicU64,
    /// Proposals the worker marked Rejected because their recomputed
    /// cosine fell below the partial-match floor (no longer plausible).
    proposals_rejected_total: AtomicU64,
    /// Proposals the worker marked Expired because they sat in Pending
    /// past the `expire_after_secs` window.
    proposals_expired_total: AtomicU64,
    /// Errors the worker encountered re-running the embedder or
    /// invoking `merge_entity`. Bumped per failure; the worker logs a
    /// warn and leaves the proposal Pending for the next tick.
    errors_total: AtomicU64,
    /// Gauge: pending proposals observed at the start of the most
    /// recent sweep. Updated once per cycle.
    pending_queue_depth: AtomicU64,
    /// Wall-clock per sweep (one observe per cycle).
    sweep_duration_seconds: WorkerHistogram,
}

impl AmbiguityResolverMetrics {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sweeps_total: AtomicU64::new(0),
            proposals_promoted_to_merge_total: AtomicU64::new(0),
            proposals_rejected_total: AtomicU64::new(0),
            proposals_expired_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            pending_queue_depth: AtomicU64::new(0),
            sweep_duration_seconds: WorkerHistogram::new(DEFAULT_CYCLE_BUCKETS_SECONDS),
        }
    }

    pub fn inc_sweeps(&self) {
        self.sweeps_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_promoted(&self, n: u64) {
        self.proposals_promoted_to_merge_total
            .fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_rejected(&self, n: u64) {
        self.proposals_rejected_total
            .fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_expired(&self, n: u64) {
        self.proposals_expired_total.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_errors(&self) {
        self.errors_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_pending_queue_depth(&self, n: u64) {
        self.pending_queue_depth.store(n, Ordering::Relaxed);
    }

    pub fn observe_sweep_duration(&self, seconds: f64) {
        self.sweep_duration_seconds.observe(seconds);
    }

    #[must_use]
    pub fn snapshot(&self) -> AmbiguityResolverMetricsSnapshot {
        AmbiguityResolverMetricsSnapshot {
            sweeps_total: self.sweeps_total.load(Ordering::Relaxed),
            proposals_promoted_to_merge_total: self
                .proposals_promoted_to_merge_total
                .load(Ordering::Relaxed),
            proposals_rejected_total: self.proposals_rejected_total.load(Ordering::Relaxed),
            proposals_expired_total: self.proposals_expired_total.load(Ordering::Relaxed),
            errors_total: self.errors_total.load(Ordering::Relaxed),
            pending_queue_depth: self.pending_queue_depth.load(Ordering::Relaxed),
            sweep_duration_seconds: self.sweep_duration_seconds.snapshot(),
        }
    }
}

impl Default for AmbiguityResolverMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-data snapshot of [`AmbiguityResolverMetrics`]. Crosses the
/// shard boundary via `flume` like the other worker snapshots.
#[derive(Debug, Clone)]
pub struct AmbiguityResolverMetricsSnapshot {
    pub sweeps_total: u64,
    pub proposals_promoted_to_merge_total: u64,
    pub proposals_rejected_total: u64,
    pub proposals_expired_total: u64,
    pub errors_total: u64,
    pub pending_queue_depth: u64,
    pub sweep_duration_seconds: WorkerHistogramSnapshot,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_depth_is_a_gauge_not_a_counter() {
        let m = AmbiguityResolverMetrics::new();
        m.set_pending_queue_depth(10);
        assert_eq!(m.snapshot().pending_queue_depth, 10);
        m.set_pending_queue_depth(3);
        assert_eq!(
            m.snapshot().pending_queue_depth,
            3,
            "gauge writes overwrite, not accumulate",
        );
    }
}
