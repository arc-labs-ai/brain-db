//! Per-worker metrics. Spec §11/01 §15 + §11/00 §7.
//!
//! v1 publishes through atomics; Phase 9's tracing/OpenTelemetry
//! plumbing reads them out. Snapshot getters return a plain `Snapshot`
//! struct so callers (tests, admin handlers) don't have to chase
//! atomics by hand.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default, Debug)]
pub struct WorkerMetrics {
    /// Spec §11/01 §15 — `brain_worker_cycles_total`.
    pub cycles_total: AtomicU64,
    /// Spec §11/01 §15 — `brain_worker_processed_total`.
    pub processed_total: AtomicU64,
    /// Spec §11/01 §15 — `brain_worker_errors_total`.
    pub errors_total: AtomicU64,
    /// Spec §11/01 §15 — `brain_worker_cycle_duration_ms` (last).
    pub last_cycle_duration_ms: AtomicU64,
    /// Spec §11/01 §15 — `brain_worker_last_run_unixtime`.
    pub last_run_unix_secs: AtomicU64,
    /// Spec §11/01 §15 — `brain_worker_pending_work`. Workers update
    /// this as they discover the size of their queue.
    pub pending_work_estimate: AtomicU64,
}

impl WorkerMetrics {
    /// Read the metrics into a plain struct. Useful in tests / admin
    /// handlers where caller wants a consistent point-in-time view
    /// (each field still loads independently, so the snapshot isn't
    /// a full atomic across fields).
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            cycles_total: self.cycles_total.load(Ordering::Relaxed),
            processed_total: self.processed_total.load(Ordering::Relaxed),
            errors_total: self.errors_total.load(Ordering::Relaxed),
            last_cycle_duration_ms: self.last_cycle_duration_ms.load(Ordering::Relaxed),
            last_run_unix_secs: self.last_run_unix_secs.load(Ordering::Relaxed),
            pending_work_estimate: self.pending_work_estimate.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub cycles_total: u64,
    pub processed_total: u64,
    pub errors_total: u64,
    pub last_cycle_duration_ms: u64,
    pub last_run_unix_secs: u64,
    pub pending_work_estimate: u64,
}
