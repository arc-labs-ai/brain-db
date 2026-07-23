//! `TemporalEdgeWorker` metric family.

use std::sync::atomic::{AtomicU64, Ordering};

use super::histograms::{WorkerHistogram, WorkerHistogramSnapshot, DEFAULT_CYCLE_BUCKETS_SECONDS};

/// Bucket boundaries (seconds) for the temporal gap histogram. Tuned
/// for the 0–5 minute default window with logarithmic spacing past
/// 60 s so operators can see both "tight space loops" and "near the
/// window edge."
const DEFAULT_TEMPORAL_GAP_BUCKETS_SECONDS: &[f64] = &[
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];

/// Why the TemporalEdgeWorker skipped writing a `FollowedBy` for an
/// enqueued memory. Each variant tracks a counter so operators can
/// answer "why no temporal edges?" without trawling logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalSkipReason {
    /// No predecessor found in the space-timeline index.
    NoPrev = 0,
    /// Candidate predecessor's `created_at` is ≥ this memory's
    /// (clock-skew / replay).
    OutOfOrder = 1,
    /// Candidate predecessor is tombstoned.
    Tombstoned = 2,
    /// Candidate predecessor is in a different context (and
    /// `cross_session = false`).
    CrossContext = 3,
    /// Gap exceeded the temporal window.
    WindowExceeded = 4,
    /// Cosine similarity between the new memory and its candidate
    /// predecessor was below the topical floor. Without this gate
    /// every successor in the same context window gets a `FollowedBy`
    /// edge regardless of content — "I had lunch" → "deployed to prod"
    /// would link. The cosine check preserves narrative threads while
    /// suppressing accidental adjacencies.
    BelowTopical = 5,
}

/// Metric family for `TemporalEdgeWorker`. Shared between the writer
/// (drops counter on `try_send` overflow) and the worker (everything
/// else).
#[derive(Debug)]
pub struct TemporalEdgeMetrics {
    drops_total: AtomicU64,
    edges_written_total: AtomicU64,
    skipped_no_prev: AtomicU64,
    skipped_out_of_order: AtomicU64,
    skipped_tombstoned: AtomicU64,
    skipped_cross_session: AtomicU64,
    skipped_window_exceeded: AtomicU64,
    skipped_below_topical: AtomicU64,
    cycle_duration_seconds: WorkerHistogram,
    gap_seconds: WorkerHistogram,
}

impl TemporalEdgeMetrics {
    #[must_use]
    pub fn new() -> Self {
        Self {
            drops_total: AtomicU64::new(0),
            edges_written_total: AtomicU64::new(0),
            skipped_no_prev: AtomicU64::new(0),
            skipped_out_of_order: AtomicU64::new(0),
            skipped_tombstoned: AtomicU64::new(0),
            skipped_cross_session: AtomicU64::new(0),
            skipped_window_exceeded: AtomicU64::new(0),
            skipped_below_topical: AtomicU64::new(0),
            cycle_duration_seconds: WorkerHistogram::new(DEFAULT_CYCLE_BUCKETS_SECONDS),
            gap_seconds: WorkerHistogram::new(DEFAULT_TEMPORAL_GAP_BUCKETS_SECONDS),
        }
    }

    pub fn inc_drop(&self) {
        self.drops_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_edges_written(&self, n: u64) {
        self.edges_written_total.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_skip(&self, reason: TemporalSkipReason) {
        let c = match reason {
            TemporalSkipReason::NoPrev => &self.skipped_no_prev,
            TemporalSkipReason::OutOfOrder => &self.skipped_out_of_order,
            TemporalSkipReason::Tombstoned => &self.skipped_tombstoned,
            TemporalSkipReason::CrossContext => &self.skipped_cross_session,
            TemporalSkipReason::WindowExceeded => &self.skipped_window_exceeded,
            TemporalSkipReason::BelowTopical => &self.skipped_below_topical,
        };
        c.fetch_add(1, Ordering::Relaxed);
    }

    pub fn observe_cycle_duration(&self, seconds: f64) {
        self.cycle_duration_seconds.observe(seconds);
    }

    pub fn observe_gap_seconds(&self, seconds: f64) {
        self.gap_seconds.observe(seconds);
    }

    #[must_use]
    pub fn snapshot(&self) -> TemporalEdgeMetricsSnapshot {
        TemporalEdgeMetricsSnapshot {
            drops_total: self.drops_total.load(Ordering::Relaxed),
            edges_written_total: self.edges_written_total.load(Ordering::Relaxed),
            skipped_no_prev: self.skipped_no_prev.load(Ordering::Relaxed),
            skipped_out_of_order: self.skipped_out_of_order.load(Ordering::Relaxed),
            skipped_tombstoned: self.skipped_tombstoned.load(Ordering::Relaxed),
            skipped_cross_session: self.skipped_cross_session.load(Ordering::Relaxed),
            skipped_window_exceeded: self.skipped_window_exceeded.load(Ordering::Relaxed),
            skipped_below_topical: self.skipped_below_topical.load(Ordering::Relaxed),
            cycle_duration_seconds: self.cycle_duration_seconds.snapshot(),
            gap_seconds: self.gap_seconds.snapshot(),
        }
    }
}

impl Default for TemporalEdgeMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct TemporalEdgeMetricsSnapshot {
    pub drops_total: u64,
    pub edges_written_total: u64,
    pub skipped_no_prev: u64,
    pub skipped_out_of_order: u64,
    pub skipped_tombstoned: u64,
    pub skipped_cross_session: u64,
    pub skipped_window_exceeded: u64,
    pub skipped_below_topical: u64,
    pub cycle_duration_seconds: WorkerHistogramSnapshot,
    pub gap_seconds: WorkerHistogramSnapshot,
}
