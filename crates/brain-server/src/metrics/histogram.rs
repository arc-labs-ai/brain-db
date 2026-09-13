//! Fixed-bucket histogram.
//!
//! Buckets are cumulative ("less-than-or-equal" semantics) — the
//! standard Prometheus convention. The implicit `+Inf` bucket is
//! always the last entry of the internal `counts` vector, so a
//! fully-stored histogram has `buckets.len() + 1` counts.
//!
//! ## Why an integer sum
//!
//! Prometheus accepts a floating-point `_sum`, but `AtomicF64` isn't
//! stable in libcore. We track `sum_scaled: AtomicU64` and divide by
//! a per-histogram `scale` at exposition time.
//!
//! - **ms histograms** use `scale = 1000`. Observations multiply by
//!   1000 before `fetch_add`, giving one decimal place of precision
//!   on the rendered `_sum`.
//! - **raw histograms** (byte counts, item counts, etc.) use
//!   `scale = 1`. The sum is the true integer total.
//!
//! Constructed via [`Histogram::new`] (raw) or
//! [`Histogram::new_default_ms`] (ms-scaled with buckets).
//!
//! ## Allocation
//!
//! Each histogram owns a `Vec<AtomicU64>` sized once at construction.
//! Allocate at startup; never resize.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// Default histogram buckets (ms boundaries).
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
    /// Sum × `scale`. Stored as `u64` so `fetch_add` works without
    /// an atomic float. Divide by `scale` at exposition.
    sum_scaled: AtomicU64,
    count: AtomicU64,
    /// Multiplier applied to observations before integer-summing.
    /// 1 = raw integer sum; 1000 = ms-decimal with one digit of
    /// precision.
    scale: u64,
}

impl Histogram {
    /// Construct a histogram with raw integer-sum semantics. Use
    /// this for byte counts, item counts, frame sizes — anything
    /// where the observation is an integer and you want
    /// `_sum` to be the exact total.
    #[must_use]
    pub fn new(bounds: &'static [f64]) -> Self {
        Self::with_scale(bounds, 1)
    }

    /// Construct an ms-scaled histogram with the
    /// default buckets. Observations are stored as `value × 1000`
    /// internally; the rendered `_sum` is `sum_scaled / 1000` (one
    /// decimal place).
    #[must_use]
    pub fn new_default_ms() -> Self {
        Self::with_scale(DEFAULT_BUCKETS_MS, 1000)
    }

    fn with_scale(bounds: &'static [f64], scale: u64) -> Self {
        let mut counts = Vec::with_capacity(bounds.len() + 1);
        for _ in 0..=bounds.len() {
            counts.push(AtomicU64::new(0));
        }
        Self {
            bounds,
            counts,
            sum_scaled: AtomicU64::new(0),
            count: AtomicU64::new(0),
            scale,
        }
    }

    /// Observe a value. Negative values are clamped to zero — they
    /// would otherwise pollute the sum and aren't meaningful for
    /// latency or size histograms.
    pub fn observe(&self, value: f64) {
        let v = value.max(0.0);
        // Multiply by scale (lossy if scale > 1; that's the
        // intentional precision trade-off documented in the module
        // docstring).
        let scaled = (v * self.scale as f64) as u64;
        self.sum_scaled.fetch_add(scaled, Ordering::Relaxed);
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

    /// Convenience alias for [`Self::observe`] on ms-scaled
    /// histograms. Kept for call-site readability where the unit
    /// matters (`request.observe_ms(elapsed.as_secs_f64() * 1000.0)`).
    pub fn observe_ms(&self, value_ms: f64) {
        self.observe(value_ms);
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
            sum: self.sum_scaled.load(Ordering::Relaxed) as f64 / self.scale as f64,
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
        let _ = writeln!(out, "{name}_sum{bare_label} {sum}", sum = snap.sum);
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
///
/// `sum` is the unscaled, true total — divided by the histogram's
/// internal `scale` at snapshot time. For ms histograms `sum` is in
/// milliseconds; for raw histograms it's the integer total cast to
/// `f64`.
#[derive(Debug, Clone)]
pub struct HistogramSnapshot {
    pub buckets: Vec<BucketSnapshot>,
    pub sum: f64,
    pub count: u64,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    #[test]
    fn default_buckets_match_spec() {
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
        assert!((snap.sum - 15_010.5).abs() < 0.001, "sum = {}", snap.sum);
    }

    #[test]
    fn negative_values_are_clamped() {
        let h = Histogram::new_default_ms();
        h.observe_ms(-1.0);
        let snap = h.snapshot();
        assert_eq!(snap.count, 1);
        assert!(snap.sum < 0.001, "sum = {}", snap.sum);
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
        assert!((snap.sum - 36_000.0).abs() < 1.0, "sum = {}", snap.sum);
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

    /// Raw-mode histograms emit the true integer sum, not
    /// scaled. Used by frame-size histograms where the observation
    /// is bytes.
    #[test]
    fn raw_histogram_sum_is_unscaled() {
        const BYTE_BUCKETS: &[f64] = &[64.0, 256.0, 1024.0, 4096.0, 16_384.0];
        let h = Histogram::new(BYTE_BUCKETS);
        h.observe(128.0); // ≤ 256
        h.observe(2048.0); // ≤ 4096
        h.observe(10_000.0); // ≤ 16384
        let snap = h.snapshot();
        // True byte total: 128 + 2048 + 10000 = 12176.
        assert!(
            (snap.sum - 12_176.0).abs() < 0.001,
            "raw sum = {}",
            snap.sum
        );
        assert_eq!(snap.count, 3);
        assert_eq!(
            snap.buckets[1].cumulative_count, 1,
            "le=256 has the 128 obs"
        );
        assert_eq!(snap.buckets[3].cumulative_count, 2, "le=4096 has 128+2048");
    }
}
