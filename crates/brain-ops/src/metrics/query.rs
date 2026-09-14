//! End-to-end RECALL (query) metric family.
//!
//! One recall = one observation here: total served, end-to-end wall
//! time, the effective fusion `k` the engine actually fused at, whether
//! the cross-encoder rerank stage reordered the list, and the answer
//! shape returned (Single / Many / None). Recorded from the RECALL
//! handler after the answer is shaped, from the executor's returned
//! metadata plus the final outcome — the hot path pays a handful of
//! atomic adds and two histogram observes.
//!
//! Same shared-by-`Arc` pattern as the worker families: the handler
//! side bumps the atomics, `brain-server`'s `/metrics` exposition reads
//! through [`QueryMetrics::snapshot`] on scrape.

use std::sync::atomic::{AtomicU64, Ordering};

use super::histograms::{WorkerHistogram, WorkerHistogramSnapshot};

/// Bucket bounds (milliseconds) for the end-to-end recall latency
/// histogram. Covers the ~1 ms in-memory hit through a multi-second
/// worst case (rerank model + deep graph walk); `+Inf` catches
/// anything past 5 s.
pub const QUERY_LATENCY_MS_BUCKETS: &[f64] = &[
    1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0,
];

/// Bucket bounds (unitless) for the effective-fusion-`k` histogram.
/// RRF `adaptive_k` scales with the candidate-pool size; these buckets
/// span the small-store default through a large-pool deepening pass.
pub const QUERY_FUSION_K_BUCKETS: &[f64] = &[
    10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 80.0, 100.0, 150.0, 200.0,
];

/// Answer shape a served recall returned. Discriminants are
/// append-only — they index counter slots observability reads by
/// position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryOutcome {
    Single = 0,
    Many = 1,
    None = 2,
}

/// Label values published by the `outcome_total` counter family,
/// indexed by [`QueryOutcome`].
pub const QUERY_OUTCOME_LABELS: &[&str] = &["single", "many", "none"];

impl QueryOutcome {
    fn idx(self) -> usize {
        self as usize
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        QUERY_OUTCOME_LABELS[self.idx()]
    }
}

/// Metric family for end-to-end RECALL. One `Arc` is shared between the
/// RECALL handler (which records) and the `/metrics` exposition (which
/// snapshots).
#[derive(Debug)]
pub struct QueryMetrics {
    total: AtomicU64,
    latency_ms: WorkerHistogram,
    fusion_k: WorkerHistogram,
    rerank_invoked_total: AtomicU64,
    /// Indexed by [`QueryOutcome`].
    outcome_total: Vec<AtomicU64>,
}

impl QueryMetrics {
    /// Construct a zeroed instance.
    #[must_use]
    pub fn new() -> Self {
        let outcome_total = (0..QUERY_OUTCOME_LABELS.len())
            .map(|_| AtomicU64::new(0))
            .collect();
        Self {
            total: AtomicU64::new(0),
            latency_ms: WorkerHistogram::new(QUERY_LATENCY_MS_BUCKETS),
            fusion_k: WorkerHistogram::new(QUERY_FUSION_K_BUCKETS),
            rerank_invoked_total: AtomicU64::new(0),
            outcome_total,
        }
    }

    /// Record one served recall. `latency_ms` is the handler-measured
    /// end-to-end wall time; `fusion_k` is the executor's effective
    /// fusion constant; `rerank_invoked` is true only when the
    /// cross-encoder actually reordered the fused list; `outcome` is the
    /// final answer shape.
    pub fn record(
        &self,
        latency_ms: f64,
        fusion_k: u32,
        rerank_invoked: bool,
        outcome: QueryOutcome,
    ) {
        self.total.fetch_add(1, Ordering::Relaxed);
        self.latency_ms.observe(latency_ms);
        self.fusion_k.observe(f64::from(fusion_k));
        if rerank_invoked {
            self.rerank_invoked_total.fetch_add(1, Ordering::Relaxed);
        }
        self.outcome_total[outcome.idx()].fetch_add(1, Ordering::Relaxed);
    }

    /// Read-only snapshot for `/metrics`.
    #[must_use]
    pub fn snapshot(&self) -> QueryMetricsSnapshot {
        let outcome_total = self
            .outcome_total
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();
        QueryMetricsSnapshot {
            total: self.total.load(Ordering::Relaxed),
            latency_ms: self.latency_ms.snapshot(),
            fusion_k: self.fusion_k.snapshot(),
            rerank_invoked_total: self.rerank_invoked_total.load(Ordering::Relaxed),
            outcome_total,
        }
    }
}

impl Default for QueryMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-data snapshot of [`QueryMetrics`]. `outcome_total` is indexed
/// in [`QUERY_OUTCOME_LABELS`] order.
#[derive(Debug, Clone)]
pub struct QueryMetricsSnapshot {
    pub total: u64,
    pub latency_ms: WorkerHistogramSnapshot,
    pub fusion_k: WorkerHistogramSnapshot,
    pub rerank_invoked_total: u64,
    pub outcome_total: Vec<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_counters_round_trip() {
        let m = QueryMetrics::new();
        m.record(12.0, 60, true, QueryOutcome::Single);
        m.record(30.0, 80, false, QueryOutcome::Many);
        m.record(5.0, 40, true, QueryOutcome::None);
        m.record(6.0, 40, false, QueryOutcome::None);
        let s = m.snapshot();
        assert_eq!(s.total, 4);
        assert_eq!(s.rerank_invoked_total, 2);
        assert_eq!(s.outcome_total[QueryOutcome::Single as usize], 1);
        assert_eq!(s.outcome_total[QueryOutcome::Many as usize], 1);
        assert_eq!(s.outcome_total[QueryOutcome::None as usize], 2);
        assert_eq!(s.latency_ms.count, 4);
        assert_eq!(s.fusion_k.count, 4);
    }

    #[test]
    fn query_outcome_labels_match_discriminants() {
        assert_eq!(QueryOutcome::Single.label(), "single");
        assert_eq!(QueryOutcome::Many.label(), "many");
        assert_eq!(QueryOutcome::None.label(), "none");
    }
}
