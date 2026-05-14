//! Fixed-bucket histogram per spec §14/01 §12.
//!
//! Buckets are cumulative ("less-than-or-equal" semantics) — the
//! standard Prometheus convention. The implicit `+Inf` bucket is
//! always the last entry of [`Self::counts`], so a fully-stored
//! histogram has `buckets.len() + 1` counts.
//!
//! ## Why an integer sum
//!
//! Prometheus accepts a floating-point `_sum`, but `AtomicF64` isn't
//! stable in libcore. We track `sum_micros: AtomicU64` (sum × 1000) so
//! `fetch_add` works; exposition divides by 1000 to emit a decimal
//! millisecond value with one digit of precision.
//!
//! ## Allocation
//!
//! Each histogram owns a `Vec<AtomicU64>` sized once at construction.
//! Allocate at startup; never resize.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// Default histogram buckets per spec §14/01 §12 (ms boundaries).
pub const DEFAULT_BUCKETS_MS: &[f64] = &[
    1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0,
];

/// Histogram with fixed bucket bounds.
///
/// `counts.len() == bounds.len() + 1` — the trailing entry is the
/// `+Inf` overflow bucket.
#[derive(Debug)]
pub struct Histogram {
    bounds: &'static [f64],
    counts: Vec<AtomicU64>,
    sum_micros: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    /// Construct a histogram with the supplied bucket boundaries
    /// (cumulative `le` semantics). Boundaries must be sorted
    /// ascending; the constructor does not validate this — callers
    /// pass static slices.
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

    /// Construct a histogram with the spec §14/01 §12 default buckets.
    #[must_use]
    pub fn new_default_ms() -> Self {
        Self::new(DEFAULT_BUCKETS_MS)
    }

    /// Observe a value (ms). Negative values are clamped to zero —
    /// they would otherwise pollute the sum and aren't meaningful for
    /// latency histograms.
    pub fn observe_ms(&self, value_ms: f64) {
        let v = value_ms.max(0.0);
        let micros = (v * 1000.0) as u64;
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        for (i, &bound) in self.bounds.iter().enumerate() {
            if v <= bound {
                self.counts[i].fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        // +Inf bucket.
        let last = self.counts.len() - 1;
        self.counts[last].fetch_add(1, Ordering::Relaxed);
    }

    /// Bucket bounds (does not include +Inf).
    #[must_use]
    pub fn bounds(&self) -> &'static [f64] {
        self.bounds
    }

    /// Snapshot the counts. Each value is monotonic non-decreasing
    /// (cumulative). Used by exposition; not part of the hot path.
    #[must_use]
    pub fn snapshot(&self) -> HistogramSnapshot {
        let mut buckets = Vec::with_capacity(self.counts.len());
        let mut running = 0u64;
        for (i, c) in self.counts.iter().enumerate() {
            // Cumulative semantics: each bucket includes all earlier
            // observations. Sum as we walk.
            running += c.load(Ordering::Relaxed);
            let bound = if i < self.bounds.len() {
                Bound::Le(self.bounds[i])
            } else {
                Bound::Inf
            };
            buckets.push(BucketSnapshot {
                le: bound,
                cumulative_count: running,
            });
        }
        HistogramSnapshot {
            buckets,
            sum_ms: self.sum_micros.load(Ordering::Relaxed) as f64 / 1000.0,
            count: self.count.load(Ordering::Relaxed),
        }
    }

    /// Render the histogram in Prometheus text-format exposition into
    /// `out`. `name` is the metric base (e.g. `brain_request_duration_ms`);
    /// `label_prefix` is the `{...}` substring without the leading `{`
    /// or trailing `}` — empty for label-less metrics, otherwise
    /// something like `op="encode",shard="0"`.
    pub fn expose(&self, name: &str, label_prefix: &str, out: &mut String) {
        let snap = self.snapshot();
        for b in &snap.buckets {
            let label = match label_prefix.is_empty() {
                true => format!("{{le=\"{}\"}}", b.le),
                false => format!("{{{label_prefix},le=\"{}\"}}", b.le),
            };
            let _ = writeln!(
                out,
                "{name}_bucket{label} {count}",
                count = b.cumulative_count
            );
        }
        let bare_label = match label_prefix.is_empty() {
            true => String::new(),
            false => format!("{{{label_prefix}}}"),
        };
        let _ = writeln!(out, "{name}_sum{bare_label} {sum}", sum = snap.sum_ms);
        let _ = writeln!(out, "{name}_count{bare_label} {count}", count = snap.count);
    }
}

/// Snapshot of one histogram bucket.
#[derive(Debug, Clone, Copy)]
pub struct BucketSnapshot {
    pub le: Bound,
    pub cumulative_count: u64,
}

/// Bucket upper bound — either a finite ms value or `+Inf`.
#[derive(Debug, Clone, Copy)]
pub enum Bound {
    Le(f64),
    Inf,
}

impl std::fmt::Display for Bound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Bound::Le(v) => write!(f, "{v}"),
            Bound::Inf => write!(f, "+Inf"),
        }
    }
}

/// Snapshot of a histogram. Cheap to produce; computed on `/metrics`
/// scrape only.
#[derive(Debug, Clone)]
pub struct HistogramSnapshot {
    pub buckets: Vec<BucketSnapshot>,
    pub sum_ms: f64,
    pub count: u64,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    #[test]
    fn default_buckets_match_spec() {
        // spec §14/01 §12.
        assert_eq!(
            DEFAULT_BUCKETS_MS,
            &[
                1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0,
                10000.0,
            ]
        );
    }

    #[test]
    fn observe_lands_in_correct_bucket() {
        let h = Histogram::new_default_ms();
        h.observe_ms(0.5); // ≤ 1
        h.observe_ms(3.0); // ≤ 5
        h.observe_ms(7.0); // ≤ 10
        h.observe_ms(15_000.0); // overflow

        let snap = h.snapshot();
        // Cumulative semantics:
        //   le=1   : 1 (0.5)
        //   le=2.5 : 1
        //   le=5   : 2 (+3.0)
        //   le=10  : 3 (+7.0)
        //   le=25  : 3
        //   ...
        //   le=+Inf: 4 (+15000)
        assert_eq!(snap.buckets[0].cumulative_count, 1, "le=1 cumulative");
        assert_eq!(snap.buckets[2].cumulative_count, 2, "le=5 cumulative");
        assert_eq!(snap.buckets[3].cumulative_count, 3, "le=10 cumulative");
        assert_eq!(
            snap.buckets.last().unwrap().cumulative_count,
            4,
            "+Inf cumulative"
        );
        assert_eq!(snap.count, 4);
        // Sum: 0.5 + 3.0 + 7.0 + 15000.0 = 15010.5
        assert!(
            (snap.sum_ms - 15_010.5).abs() < 0.001,
            "sum_ms = {}",
            snap.sum_ms
        );
    }

    #[test]
    fn negative_values_are_clamped() {
        let h = Histogram::new_default_ms();
        h.observe_ms(-1.0);
        let snap = h.snapshot();
        assert_eq!(snap.count, 1);
        assert!(snap.sum_ms < 0.001, "sum_ms = {}", snap.sum_ms);
        assert_eq!(snap.buckets[0].cumulative_count, 1, "le=1 catches 0");
    }

    #[test]
    fn race_free_under_contention() {
        let h = Arc::new(Histogram::new_default_ms());
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let h = h.clone();
                thread::spawn(move || {
                    let v = (i + 1) as f64; // 1, 2, …, 8 ms
                    for _ in 0..1_000 {
                        h.observe_ms(v);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let snap = h.snapshot();
        assert_eq!(snap.count, 8 * 1_000);
        // Sum: (1+2+3+4+5+6+7+8) × 1000 = 36000.0
        assert!(
            (snap.sum_ms - 36_000.0).abs() < 1.0,
            "sum_ms = {}",
            snap.sum_ms
        );
    }

    #[test]
    fn expose_emits_prometheus_text() {
        let h = Histogram::new_default_ms();
        h.observe_ms(2.0);
        let mut out = String::new();
        h.expose("brain_request_duration_ms", "op=\"encode\"", &mut out);
        assert!(out.contains("brain_request_duration_ms_bucket{op=\"encode\",le=\"1\"} 0"));
        assert!(out.contains("brain_request_duration_ms_bucket{op=\"encode\",le=\"2.5\"} 1"));
        assert!(out.contains("brain_request_duration_ms_bucket{op=\"encode\",le=\"+Inf\"} 1"));
        assert!(out.contains("brain_request_duration_ms_sum{op=\"encode\"} 2"));
        assert!(out.contains("brain_request_duration_ms_count{op=\"encode\"} 1"));
    }

    #[test]
    fn expose_handles_label_free_metrics() {
        let h = Histogram::new_default_ms();
        h.observe_ms(0.5);
        let mut out = String::new();
        h.expose("brain_test_duration_ms", "", &mut out);
        assert!(out.contains("brain_test_duration_ms_bucket{le=\"1\"} 1"));
        assert!(out.contains("brain_test_duration_ms_count 1"));
    }
}
