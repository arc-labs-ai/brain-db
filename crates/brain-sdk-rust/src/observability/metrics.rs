//! Internal counter state + `MetricsSnapshot`.
//!
//! Atomic counters mutated from anywhere in the SDK; cloned
//! `Client`s share one `Arc<MetricsState>` so a snapshot from
//! any clone reflects every op anywhere in the process.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

/// Per-operation counters. Updated through
/// [`MetricsState::record_*`].
#[derive(Debug, Default)]
pub struct OpCounters {
    pub requests_total: AtomicU64,
    pub errors_total: AtomicU64,
    pub retries_total: AtomicU64,
}

impl OpCounters {
    fn snapshot(&self) -> OpMetrics {
        OpMetrics {
            requests_total: self.requests_total.load(Ordering::Relaxed),
            errors_total: self.errors_total.load(Ordering::Relaxed),
            retries_total: self.retries_total.load(Ordering::Relaxed),
        }
    }
}

/// Shared mutable state. Held inside the `Client` as
/// `Arc<MetricsState>`; cloned `Client`s share it.
#[derive(Default)]
pub struct MetricsState {
    requests_total: AtomicU64,
    errors_total: AtomicU64,
    retries_total: AtomicU64,
    in_flight: AtomicU64,
    connections_opened_total: AtomicU64,
    /// Per-op breakdown. Lazy-init on first record; subsequent
    /// recordings hit the AtomicU64s lock-free.
    by_op: Mutex<BTreeMap<&'static str, OpCounters>>,
}

impl std::fmt::Debug for MetricsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsState")
            .field(
                "requests_total",
                &self.requests_total.load(Ordering::Relaxed),
            )
            .field("errors_total", &self.errors_total.load(Ordering::Relaxed))
            .field("retries_total", &self.retries_total.load(Ordering::Relaxed))
            .field("in_flight", &self.in_flight.load(Ordering::Relaxed))
            .finish()
    }
}

impl MetricsState {
    /// Increment `requests_total` (global + per-op) and the
    /// `in_flight` gauge. Returns an
    /// [`InFlightGuard`] that decrements `in_flight` on drop.
    pub fn begin_request(&self, op: &'static str) -> InFlightGuard<'_> {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        self.with_op(op, |c| {
            c.requests_total.fetch_add(1, Ordering::Relaxed);
        });
        InFlightGuard { state: self }
    }

    /// Record one retry attempt (i.e. a previous attempt failed
    /// and we're about to try again).
    pub fn record_retry(&self, op: &'static str) {
        self.retries_total.fetch_add(1, Ordering::Relaxed);
        self.with_op(op, |c| {
            c.retries_total.fetch_add(1, Ordering::Relaxed);
        });
    }

    /// Record a terminal failure (after retries are exhausted
    /// or the error was non-retryable).
    pub fn record_error(&self, op: &'static str) {
        self.errors_total.fetch_add(1, Ordering::Relaxed);
        self.with_op(op, |c| {
            c.errors_total.fetch_add(1, Ordering::Relaxed);
        });
    }

    /// Record that a fresh connection was opened (handshake
    /// completed). Spec §13/07 §12.
    pub fn record_connection_opened(&self) {
        self.connections_opened_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Capture a point-in-time view of the counters.
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        let by_op = {
            let guard = self.by_op.lock();
            guard.iter().map(|(k, v)| (*k, v.snapshot())).collect()
        };
        MetricsSnapshot {
            requests_total: self.requests_total.load(Ordering::Relaxed),
            errors_total: self.errors_total.load(Ordering::Relaxed),
            retries_total: self.retries_total.load(Ordering::Relaxed),
            in_flight_gauge: self.in_flight.load(Ordering::Relaxed),
            connections_opened_total: self.connections_opened_total.load(Ordering::Relaxed),
            by_op,
        }
    }

    fn with_op<F: FnOnce(&OpCounters)>(&self, op: &'static str, f: F) {
        // Fast path: entry already present.
        let mut guard = self.by_op.lock();
        let entry = guard.entry(op).or_default();
        f(entry);
    }
}

/// RAII helper that decrements `in_flight` on drop. The
/// caller's op `send()` holds it for the duration of the
/// request → retry → response cycle.
pub struct InFlightGuard<'a> {
    state: &'a MetricsState,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.state.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Point-in-time copy of the counters. Returned by
/// [`crate::Client::metrics_snapshot`]. All counters are
/// monotonically increasing across the process's lifetime;
/// callers compute deltas.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub requests_total: u64,
    pub errors_total: u64,
    pub retries_total: u64,
    pub in_flight_gauge: u64,
    pub connections_opened_total: u64,
    pub by_op: BTreeMap<&'static str, OpMetrics>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpMetrics {
    pub requests_total: u64,
    pub errors_total: u64,
    pub retries_total: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn default_state_snapshot_is_zero() {
        let s = MetricsState::default();
        let snap = s.snapshot();
        assert_eq!(snap.requests_total, 0);
        assert_eq!(snap.errors_total, 0);
        assert_eq!(snap.in_flight_gauge, 0);
        assert!(snap.by_op.is_empty());
    }

    #[test]
    fn begin_request_increments_global_and_op() {
        let s = MetricsState::default();
        {
            let _g = s.begin_request("encode");
            let snap = s.snapshot();
            assert_eq!(snap.requests_total, 1);
            assert_eq!(snap.in_flight_gauge, 1);
            assert_eq!(snap.by_op["encode"].requests_total, 1);
        }
        // Guard dropped → in_flight goes back to 0.
        assert_eq!(s.snapshot().in_flight_gauge, 0);
    }

    #[test]
    fn record_retry_and_error() {
        let s = MetricsState::default();
        s.record_retry("encode");
        s.record_retry("encode");
        s.record_error("encode");
        let snap = s.snapshot();
        assert_eq!(snap.retries_total, 2);
        assert_eq!(snap.errors_total, 1);
        assert_eq!(snap.by_op["encode"].retries_total, 2);
        assert_eq!(snap.by_op["encode"].errors_total, 1);
    }

    #[test]
    fn arc_clone_shares_state() {
        let s1 = Arc::new(MetricsState::default());
        let s2 = s1.clone();
        let _g = s1.begin_request("recall");
        assert_eq!(s2.snapshot().requests_total, 1);
    }
}
