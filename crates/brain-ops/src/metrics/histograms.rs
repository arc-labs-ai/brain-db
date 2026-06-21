//! Worker-local histogram + shared label arrays.
//!
//! Both the writer (which performs the post-ENCODE enqueue) and the
//! worker (which drains the queue and runs the cycle) need to publish
//! into the same counter family. They sit on opposite sides of the
//! `brain-ops` → `brain-workers` dependency, so the atomics live here
//! and both layers hold an `Arc` to the same struct.
//!
//! [`WorkerHistogram`] mirrors the shape of `brain-server`'s
//! `Histogram` but is kept here to avoid a `brain-ops -> brain-server`
//! dependency edge. The hot path is lock-free `fetch_add`; snapshot
//! returns plain data for `/metrics` exposition.

use std::sync::atomic::{AtomicU64, Ordering};

/// Bucket bounds (seconds, cumulative) for the worker cycle-duration
/// histograms. Range covers the 1 ms fast path (queue empty, immediate
/// exit) through 30 s safety ceiling (well past the worker's 5 s
/// `max_runtime` budget) so an over-budget cycle still lands in a
/// bounded bucket rather than `+Inf`.
pub const DEFAULT_CYCLE_BUCKETS_SECONDS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// Bucket bounds (counts) for the AutoEdge "neighbours found per
/// cycle" histogram. Caps out around the worker's `batch_size` (256)
/// times an aggressive `top_k` (5) → 1280; `+Inf` catches anything
/// extreme.
pub const DEFAULT_NEIGHBOURS_BUCKETS: &[f64] =
    &[1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0];

/// Tier label values published by the extractor `tier_runs_total` and
/// `resolver_outcome_total` counter families on
/// [`super::extractor::ExtractorMetrics`]. The resolver labels are a
/// superset because the resolver outcome family carries
/// `exact / alias / fuzzy / create` rather than tier kinds.
pub const TIER_LABELS: &[&str] = &["pattern", "classifier", "llm"];
pub const TIER_STATUS_LABELS: &[&str] = &["ran", "skipped", "failed"];
pub const RESOLVER_OUTCOME_LABELS: &[&str] = &[
    "exact",
    "alias",
    "fuzzy",
    "embedding",
    "disambiguated",
    "create",
];

/// Item kinds published by the `items_written_total` counter family
/// on [`super::extractor::ExtractorMetrics`].
pub const ITEM_KIND_LABELS: &[&str] = &["entity", "statement", "relation", "mention", "hype"];

// ---------------------------------------------------------------------
// Fixed-bucket histogram (worker-local, allocation-free per observe).
// ---------------------------------------------------------------------

/// Fixed-bucket histogram with cumulative semantics. Mirrors the
/// shape of `brain-server`'s `Histogram`, but kept here to avoid a
/// `brain-ops -> brain-server` dependency edge. Observations are
/// stored unscaled (`f64` sum exposed at snapshot time).
#[derive(Debug)]
pub struct WorkerHistogram {
    bounds: &'static [f64],
    /// `counts.len() == bounds.len() + 1` — trailing entry is `+Inf`.
    counts: Vec<AtomicU64>,
    /// Sum × 1_000_000 (six decimal places of precision for seconds).
    sum_micros: AtomicU64,
    count: AtomicU64,
}

impl WorkerHistogram {
    /// Construct an empty histogram with the supplied bucket bounds.
    /// Bounds must be sorted ascending; the constructor doesn't sort
    /// — callers pass the static slices above.
    #[must_use]
    pub fn new(bounds: &'static [f64]) -> Self {
        let mut counts = Vec::with_capacity(bounds.len() + 1);
        for _ in 0..=bounds.len() {
            counts.push(AtomicU64::new(0));
        }
        Self {
            bounds,
            counts,
            sum_micros: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// Record one observation. Negative values are clamped to zero
    /// so the histogram sum stays meaningful.
    pub fn observe(&self, value: f64) {
        let v = value.max(0.0);
        let scaled = (v * 1_000_000.0) as u64;
        self.sum_micros.fetch_add(scaled, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        for (i, &bound) in self.bounds.iter().enumerate() {
            if v <= bound {
                self.counts[i].fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        let last = self.counts.len() - 1;
        self.counts[last].fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot bucket counts cumulatively. Used by `/metrics`
    /// exposition.
    #[must_use]
    pub fn snapshot(&self) -> WorkerHistogramSnapshot {
        let mut buckets = Vec::with_capacity(self.counts.len());
        let mut running = 0u64;
        for (i, c) in self.counts.iter().enumerate() {
            running += c.load(Ordering::Relaxed);
            let upper = if i < self.bounds.len() {
                Some(self.bounds[i])
            } else {
                None
            };
            buckets.push(WorkerBucketSnapshot {
                le: upper,
                cumulative_count: running,
            });
        }
        WorkerHistogramSnapshot {
            buckets,
            sum: self.sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            count: self.count.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorkerHistogramSnapshot {
    pub buckets: Vec<WorkerBucketSnapshot>,
    pub sum: f64,
    pub count: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct WorkerBucketSnapshot {
    /// Upper bound (`<=`) or `None` for the `+Inf` overflow bucket.
    pub le: Option<f64>,
    pub cumulative_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_overflow_lands_in_inf_bucket() {
        let h = WorkerHistogram::new(DEFAULT_CYCLE_BUCKETS_SECONDS);
        h.observe(100.0);
        let s = h.snapshot();
        assert_eq!(s.count, 1);
        assert_eq!(s.buckets.last().unwrap().cumulative_count, 1);
        assert!(s.buckets.last().unwrap().le.is_none());
    }
}
