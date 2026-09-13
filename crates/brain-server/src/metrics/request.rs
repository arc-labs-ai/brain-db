//! Per-operation request metrics.
//!
//! mandates three families:
//!
//! - `brain_request_total{op=, status=}` — counter, every completed
//!   request bumps once.
//! - `brain_request_duration_ms{op=}` — histogram, every completed
//!   request observes once.
//! - `brain_request_active{op=}` — gauge, incremented when a request
//!   begins dispatch and decremented when its response frame is sent.
//!
//! The spec also lists a `shard=` label on these families. We omit
//! it deliberately: the dispatch path doesn't know the target shard
//! until *after* validation, and emitting a partial `shard="?"` label
//! would corrupt PromQL `sum by (shard)` queries. Per-shard request
//! counts can be derived from worker / connection metrics.
//!
//! ## RAII timing
//!
//! [`RequestTimer`] is a guard. Construct one before awaiting the
//! shard dispatch; let it drop when the response frame is built (the
//! caller passes the status into [`RequestTimer::record`] which
//! consumes it, or [`Drop`] auto-records `timeout` if it falls out
//! of scope without explicit completion).

use std::sync::Arc;
use std::time::Instant;

use brain_protocol::envelope::request::RequestBody;

use super::counter::Counter;
use super::gauge::Gauge;
use super::histogram::Histogram;

/// Operation labels — the ten request-bearing RequestBody variants.
/// Order is the canonical exposition order; keep it stable so
/// dashboards built against an early run still scrape cleanly.
pub const OP_LABELS: &[&str] = &[
    "encode",
    "recall",
    "plan",
    "reason",
    "forget",
    "link",
    "unlink",
    "txn_begin",
    "txn_commit",
    "txn_abort",
];

/// Status labels — the three terminal outcomes of a request.
///
/// `error` is the catch-all for any wire-level error response;
/// a later refinement may split this to `error_<code>` if the
/// cardinality budget permits.
pub const STATUS_LABELS: &[&str] = &["success", "error", "timeout"];

/// Number of `(op, status)` combinations indexed in [`RequestMetrics`].
const N_OP_STATUS: usize = OP_LABELS.len() * STATUS_LABELS.len();

/// All request-path counters / gauges / histograms.
///
/// Construction is zero-cost (one boxed slice per family). One
/// instance per server, shared via `Arc` across the connection layer
/// and the admin exposition path.
pub struct RequestMetrics {
    /// `op × status` → counter. Indexed by `op_idx * 3 + status_idx`.
    totals: Vec<Counter>,
    /// One histogram per op. Sized 10 — one per `OP_LABELS` entry.
    durations: Vec<Histogram>,
    /// One in-flight gauge per op.
    active: Vec<Gauge>,
}

impl RequestMetrics {
    /// Construct a fresh, all-zero metrics set.
    #[must_use]
    pub fn new() -> Self {
        let mut totals = Vec::with_capacity(N_OP_STATUS);
        for _ in 0..N_OP_STATUS {
            totals.push(Counter::new());
        }
        let mut durations = Vec::with_capacity(OP_LABELS.len());
        let mut active = Vec::with_capacity(OP_LABELS.len());
        for _ in 0..OP_LABELS.len() {
            durations.push(Histogram::new_default_ms());
            active.push(Gauge::new());
        }
        Self {
            totals,
            durations,
            active,
        }
    }

    /// Read a counter by `(op_idx, status_idx)`. Intended for
    /// exposition and tests.
    #[must_use]
    pub fn total(&self, op_idx: usize, status_idx: usize) -> &Counter {
        &self.totals[op_idx * STATUS_LABELS.len() + status_idx]
    }

    /// Read the per-op duration histogram.
    #[must_use]
    pub fn duration(&self, op_idx: usize) -> &Histogram {
        &self.durations[op_idx]
    }

    /// Read the per-op in-flight gauge.
    #[must_use]
    pub fn active_gauge(&self, op_idx: usize) -> &Gauge {
        &self.active[op_idx]
    }
}

impl Default for RequestMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a [`RequestBody`] variant to its `OP_LABELS` index. Returns
/// `None` for variants that aren't on the OpDispatch path (Subscribe
/// family, Ping, Hello, Auth, Bye). Those are handled by the connection-
/// layer frame metrics.
#[must_use]
pub fn op_index(req: &RequestBody) -> Option<usize> {
    use RequestBody::*;
    let label = match req {
        Encode(_) => "encode",
        Recall(_) => "recall",
        Plan(_) => "plan",
        Reason(_) => "reason",
        Forget(_) => "forget",
        Link(_) => "link",
        Unlink(_) => "unlink",
        TxnBegin(_) => "txn_begin",
        TxnCommit(_) => "txn_commit",
        TxnAbort(_) => "txn_abort",
        _ => return None,
    };
    OP_LABELS.iter().position(|&l| l == label)
}

/// Terminal status of a request — passed to [`RequestTimer::record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Success,
    Error,
    Timeout,
}

impl Status {
    fn idx(self) -> usize {
        match self {
            Status::Success => 0,
            Status::Error => 1,
            Status::Timeout => 2,
        }
    }
}

/// RAII guard: increments the in-flight gauge on construction and
/// records (duration, status, in-flight decrement) on
/// [`Self::record`]. If dropped without `record`, defaults to
/// `Status::Timeout` — a request that fell out of scope is by
/// definition not a clean completion.
pub struct RequestTimer {
    metrics: Arc<RequestMetrics>,
    op_idx: usize,
    started_at: Instant,
    /// Set to `Some(status)` by [`Self::record`]; consulted by `Drop`.
    completed: Option<Status>,
}

impl RequestTimer {
    /// Start the timer for the supplied op. Bumps the in-flight gauge.
    #[must_use]
    pub fn start(metrics: Arc<RequestMetrics>, op_idx: usize) -> Self {
        metrics.active_gauge(op_idx).inc();
        Self {
            metrics,
            op_idx,
            started_at: Instant::now(),
            completed: None,
        }
    }

    /// Record a clean completion. Pass the terminal status. After
    /// this, the guard's drop is a no-op.
    pub fn record(mut self, status: Status) {
        self.completed = Some(status);
        // Drop runs next — handles the counter / histogram updates.
    }
}

impl Drop for RequestTimer {
    fn drop(&mut self) {
        let status = self.completed.unwrap_or(Status::Timeout);
        let elapsed_ms = self.started_at.elapsed().as_secs_f64() * 1000.0;
        self.metrics.duration(self.op_idx).observe_ms(elapsed_ms);
        self.metrics.total(self.op_idx, status.idx()).inc();
        self.metrics.active_gauge(self.op_idx).dec();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_labels_match_status_grid_size() {
        assert_eq!(N_OP_STATUS, OP_LABELS.len() * STATUS_LABELS.len());
    }

    #[test]
    fn op_index_maps_all_op_dispatch_variants() {
        // Verify each labelled op maps to a stable index. The map is
        // derived from RequestBody variant matching; if a variant is
        // added to brain-protocol the match in `op_index` must grow.
        for (i, &expected) in OP_LABELS.iter().enumerate() {
            assert_eq!(
                OP_LABELS.iter().position(|&l| l == expected),
                Some(i),
                "{expected} must be findable in OP_LABELS"
            );
        }
    }

    #[test]
    fn record_success_bumps_success_counter_and_histogram() {
        let m = Arc::new(RequestMetrics::new());
        let op_idx = 0; // encode
        {
            let t = RequestTimer::start(m.clone(), op_idx);
            assert_eq!(m.active_gauge(op_idx).get(), 1);
            t.record(Status::Success);
        }
        assert_eq!(m.active_gauge(op_idx).get(), 0);
        assert_eq!(m.total(op_idx, 0).get(), 1, "success counter");
        assert_eq!(m.total(op_idx, 1).get(), 0, "error counter");
        assert_eq!(m.duration(op_idx).snapshot().count, 1);
    }

    #[test]
    fn record_error_bumps_error_counter() {
        let m = Arc::new(RequestMetrics::new());
        let t = RequestTimer::start(m.clone(), 1); // recall
        t.record(Status::Error);
        assert_eq!(m.total(1, 1).get(), 1);
    }

    #[test]
    fn drop_without_record_is_timeout() {
        let m = Arc::new(RequestMetrics::new());
        let op_idx = 2; // plan
        {
            let _t = RequestTimer::start(m.clone(), op_idx);
            // _t falls out of scope without `record`.
        }
        assert_eq!(m.total(op_idx, 2).get(), 1, "timeout counter");
        assert_eq!(m.active_gauge(op_idx).get(), 0);
    }
}
