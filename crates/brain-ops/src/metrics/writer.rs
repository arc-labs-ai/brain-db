//! Writer-level metric family for the unified `submit(Write)` path.
//!
//! Counts every phase submitted, every idempotency-cache outcome, every
//! WAL-skip, and every apply error — keyed by [`crate::write::Phase::tag`]
//! so the existing tag strings become metric labels without a second
//! enum. Latency is recorded once per phase using the same bucket set
//! the worker cycle-duration histograms use.
//!
//! Hot path is lock-free `fetch_add` against per-tag `AtomicU64`
//! counters. Tag-keyed maps are guarded by a single `Mutex` that's only
//! locked when the tag is seen for the first time; subsequent observations
//! resolve through a snapshot read and then index the existing atomic.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

use super::histograms::{WorkerHistogram, WorkerHistogramSnapshot, DEFAULT_CYCLE_BUCKETS_SECONDS};
use super::write_stage::{WriteStageMetrics, WriteStageMetricsSnapshot};

/// Outcome label for `brain_writer_submit_total`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitOutcome {
    Ok,
    Err,
    Conflict,
}

impl SubmitOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Err => "err",
            Self::Conflict => "conflict",
        }
    }
}

/// Outcome label for `brain_writer_idempotency_total`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdempotencyOutcome {
    Hit,
    Miss,
    Conflict,
}

impl IdempotencyOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Conflict => "conflict",
        }
    }
}

/// Per-phase counters keyed by phase tag.
///
/// Each entry holds three submit counters (ok / err / conflict), one
/// wal-skip counter, and one latency histogram. Apply-error counters
/// live alongside but are keyed by `(phase_tag, error_tag)` so the
/// label cardinality is bounded by `|Phase| * |ApplyError|`.
#[derive(Debug)]
struct PerPhase {
    submit_ok: AtomicU64,
    submit_err: AtomicU64,
    submit_conflict: AtomicU64,
    wal_skip: AtomicU64,
    submit_duration_seconds: WorkerHistogram,
}

impl PerPhase {
    fn new() -> Self {
        Self {
            submit_ok: AtomicU64::new(0),
            submit_err: AtomicU64::new(0),
            submit_conflict: AtomicU64::new(0),
            wal_skip: AtomicU64::new(0),
            submit_duration_seconds: WorkerHistogram::new(DEFAULT_CYCLE_BUCKETS_SECONDS),
        }
    }
}

/// Writer-level metric family. One instance per shard; shared between
/// the writer's submit path and `/metrics` exposition via `Arc`.
#[derive(Debug)]
pub struct WriterMetrics {
    /// Per-phase counters and histogram. Mutex guards the map shape;
    /// the atomics inside each `PerPhase` are read/written lock-free.
    by_phase: Mutex<HashMap<&'static str, PerPhase>>,
    /// `(phase_tag, error_tag) → count`.
    apply_errors: Mutex<HashMap<(&'static str, &'static str), AtomicU64>>,
    /// Idempotency outcomes — bounded label set (3 values), so a flat
    /// triple of atomics avoids the map cost.
    idempotency_hit: AtomicU64,
    idempotency_miss: AtomicU64,
    idempotency_conflict: AtomicU64,
    /// Per-stage latency for the synchronous ack path. Lives here so
    /// the write path threads one `Arc` and one snapshot carries the
    /// whole write-side picture; the stage axis is independent of the
    /// phase axis above, hence its own type.
    stages: WriteStageMetrics,
}

impl WriterMetrics {
    /// Construct a zeroed instance.
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_phase: Mutex::new(HashMap::new()),
            apply_errors: Mutex::new(HashMap::new()),
            idempotency_hit: AtomicU64::new(0),
            idempotency_miss: AtomicU64::new(0),
            idempotency_conflict: AtomicU64::new(0),
            stages: WriteStageMetrics::new(),
        }
    }

    /// Per-stage latency family for the ack path. The write path
    /// records each stage through this handle.
    #[must_use]
    pub fn stages(&self) -> &WriteStageMetrics {
        &self.stages
    }

    /// Record one phase's submit outcome + observed duration. Called
    /// once per phase in the parent [`crate::write::Write`].
    pub fn record_submit(
        &self,
        phase_tag: &'static str,
        outcome: SubmitOutcome,
        duration: Duration,
    ) {
        let secs = duration.as_secs_f64();
        let mut map = self.by_phase.lock();
        let entry = map.entry(phase_tag).or_insert_with(PerPhase::new);
        match outcome {
            SubmitOutcome::Ok => entry.submit_ok.fetch_add(1, Ordering::Relaxed),
            SubmitOutcome::Err => entry.submit_err.fetch_add(1, Ordering::Relaxed),
            SubmitOutcome::Conflict => entry.submit_conflict.fetch_add(1, Ordering::Relaxed),
        };
        entry.submit_duration_seconds.observe(secs);
    }

    /// Record one idempotency-cache lookup outcome.
    pub fn record_idempotency(&self, outcome: IdempotencyOutcome) {
        let target = match outcome {
            IdempotencyOutcome::Hit => &self.idempotency_hit,
            IdempotencyOutcome::Miss => &self.idempotency_miss,
            IdempotencyOutcome::Conflict => &self.idempotency_conflict,
        };
        target.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one phase that was elided from the WAL append (typically
    /// because [`crate::writer::wal_map::phase_to_wal_payload`] returned
    /// `None` inside a multi-phase write).
    pub fn record_wal_skip(&self, phase_tag: &'static str) {
        let mut map = self.by_phase.lock();
        let entry = map.entry(phase_tag).or_insert_with(PerPhase::new);
        entry.wal_skip.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one apply error keyed by `(phase_tag, error_tag)` —
    /// where `error_tag` is [`crate::apply::ApplyError::tag`].
    pub fn record_apply_error(&self, phase_tag: &'static str, error_tag: &'static str) {
        let mut map = self.apply_errors.lock();
        map.entry((phase_tag, error_tag))
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Read-only snapshot for `/metrics`.
    #[must_use]
    pub fn snapshot(&self) -> WriterMetricsSnapshot {
        let by_phase = {
            let map = self.by_phase.lock();
            let mut out = Vec::with_capacity(map.len());
            for (tag, entry) in map.iter() {
                out.push(PerPhaseSnapshot {
                    phase: tag,
                    submit_ok: entry.submit_ok.load(Ordering::Relaxed),
                    submit_err: entry.submit_err.load(Ordering::Relaxed),
                    submit_conflict: entry.submit_conflict.load(Ordering::Relaxed),
                    wal_skip: entry.wal_skip.load(Ordering::Relaxed),
                    submit_duration_seconds: entry.submit_duration_seconds.snapshot(),
                });
            }
            out.sort_by_key(|p| p.phase);
            out
        };
        let apply_errors = {
            let map = self.apply_errors.lock();
            let mut out = Vec::with_capacity(map.len());
            for ((phase, error), c) in map.iter() {
                out.push(ApplyErrorSnapshot {
                    phase,
                    error,
                    count: c.load(Ordering::Relaxed),
                });
            }
            out.sort_by_key(|e| (e.phase, e.error));
            out
        };
        WriterMetricsSnapshot {
            by_phase,
            apply_errors,
            idempotency_hit: self.idempotency_hit.load(Ordering::Relaxed),
            idempotency_miss: self.idempotency_miss.load(Ordering::Relaxed),
            idempotency_conflict: self.idempotency_conflict.load(Ordering::Relaxed),
            stages: self.stages.snapshot(),
        }
    }
}

impl Default for WriterMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-data snapshot of [`WriterMetrics`]. Crosses async boundaries
/// freely; `/metrics` exposition reads from this.
#[derive(Clone, Debug)]
pub struct WriterMetricsSnapshot {
    pub by_phase: Vec<PerPhaseSnapshot>,
    pub apply_errors: Vec<ApplyErrorSnapshot>,
    pub idempotency_hit: u64,
    pub idempotency_miss: u64,
    pub idempotency_conflict: u64,
    pub stages: WriteStageMetricsSnapshot,
}

impl WriterMetricsSnapshot {
    /// Convenience accessor — sum of submit-ok counts across every
    /// phase tag. Useful for higher-level assertions in tests and
    /// dashboard "total writes" tiles.
    #[must_use]
    pub fn submit_ok_total(&self) -> u64 {
        self.by_phase.iter().map(|p| p.submit_ok).sum()
    }
}

#[derive(Clone, Debug)]
pub struct PerPhaseSnapshot {
    pub phase: &'static str,
    pub submit_ok: u64,
    pub submit_err: u64,
    pub submit_conflict: u64,
    pub wal_skip: u64,
    pub submit_duration_seconds: WorkerHistogramSnapshot,
}

#[derive(Clone, Debug)]
pub struct ApplyErrorSnapshot {
    pub phase: &'static str,
    pub error: &'static str,
    pub count: u64,
}

// Prometheus-style label getters — kept here so exposition code doesn't
// need to repeat the str conversions.
impl SubmitOutcome {
    #[must_use]
    pub fn label(self) -> &'static str {
        self.as_str()
    }
}

impl IdempotencyOutcome {
    #[must_use]
    pub fn label(self) -> &'static str {
        self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_metrics_start_at_zero() {
        let m = WriterMetrics::new();
        let s = m.snapshot();
        assert!(s.by_phase.is_empty());
        assert!(s.apply_errors.is_empty());
        assert_eq!(s.idempotency_hit, 0);
        assert_eq!(s.idempotency_miss, 0);
        assert_eq!(s.idempotency_conflict, 0);
    }

    #[test]
    fn counters_increment_independently_per_phase_tag() {
        let m = WriterMetrics::new();
        m.record_submit(
            "upsert_memory",
            SubmitOutcome::Ok,
            Duration::from_micros(100),
        );
        m.record_submit(
            "upsert_memory",
            SubmitOutcome::Ok,
            Duration::from_micros(150),
        );
        m.record_submit("link", SubmitOutcome::Err, Duration::from_micros(80));

        let snap = m.snapshot();
        // Two distinct tags observed.
        assert_eq!(snap.by_phase.len(), 2);

        let upsert = snap
            .by_phase
            .iter()
            .find(|p| p.phase == "upsert_memory")
            .expect("upsert_memory phase entry");
        assert_eq!(upsert.submit_ok, 2);
        assert_eq!(upsert.submit_err, 0);
        assert_eq!(upsert.submit_conflict, 0);
        // Two observations → histogram count == 2; sum ≈ 250µs = 0.00025s.
        assert_eq!(upsert.submit_duration_seconds.count, 2);
        assert!((upsert.submit_duration_seconds.sum - 0.000_250).abs() < 1e-6);

        let link = snap
            .by_phase
            .iter()
            .find(|p| p.phase == "link")
            .expect("link phase entry");
        assert_eq!(link.submit_ok, 0);
        assert_eq!(link.submit_err, 1);
        assert_eq!(link.submit_duration_seconds.count, 1);
    }

    #[test]
    fn idempotency_outcomes_count_separately() {
        let m = WriterMetrics::new();
        m.record_idempotency(IdempotencyOutcome::Hit);
        m.record_idempotency(IdempotencyOutcome::Hit);
        m.record_idempotency(IdempotencyOutcome::Miss);
        m.record_idempotency(IdempotencyOutcome::Conflict);
        let s = m.snapshot();
        assert_eq!(s.idempotency_hit, 2);
        assert_eq!(s.idempotency_miss, 1);
        assert_eq!(s.idempotency_conflict, 1);
    }

    #[test]
    fn wal_skip_counts_per_tag() {
        let m = WriterMetrics::new();
        m.record_wal_skip("reclaim_slots");
        m.record_wal_skip("reclaim_slots");
        m.record_wal_skip("update_embedding");
        let s = m.snapshot();
        let reclaim = s
            .by_phase
            .iter()
            .find(|p| p.phase == "reclaim_slots")
            .unwrap();
        let upd = s
            .by_phase
            .iter()
            .find(|p| p.phase == "update_embedding")
            .unwrap();
        assert_eq!(reclaim.wal_skip, 2);
        assert_eq!(upd.wal_skip, 1);
    }

    #[test]
    fn apply_errors_keyed_by_phase_and_error_tag() {
        let m = WriterMetrics::new();
        m.record_apply_error("upsert_memory", "storage");
        m.record_apply_error("upsert_memory", "storage");
        m.record_apply_error("link", "not_found");
        let s = m.snapshot();
        assert_eq!(s.apply_errors.len(), 2);
        let storage = s
            .apply_errors
            .iter()
            .find(|e| e.phase == "upsert_memory" && e.error == "storage")
            .unwrap();
        assert_eq!(storage.count, 2);
        let nf = s
            .apply_errors
            .iter()
            .find(|e| e.phase == "link" && e.error == "not_found")
            .unwrap();
        assert_eq!(nf.count, 1);
    }

    #[test]
    fn submit_ok_total_sums_across_tags() {
        let m = WriterMetrics::new();
        m.record_submit("upsert_memory", SubmitOutcome::Ok, Duration::from_micros(1));
        m.record_submit("link", SubmitOutcome::Ok, Duration::from_micros(1));
        m.record_submit("link", SubmitOutcome::Ok, Duration::from_micros(1));
        m.record_submit("link", SubmitOutcome::Err, Duration::from_micros(1));
        let s = m.snapshot();
        assert_eq!(s.submit_ok_total(), 3);
    }
}
