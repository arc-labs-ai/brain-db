//! Per-stage latency family for the synchronous write (ack) path.
//!
//! `submit(Write)` acks the client only after four distinct pieces of
//! work have run in sequence: the WAL append, the redb commit, the
//! HNSW insert, and the hand-off to the lexical indexer. A single
//! end-to-end timer cannot say which of those owns a latency
//! regression, so each one gets its own histogram and the four share
//! one `stage` label — the same shape
//! [`super::retriever::RetrieverKind`] uses for the read path, so a
//! fifth stage is a label value rather than a fifth struct field.
//!
//! The stages are observed only when they actually run. A write with
//! no HNSW-bearing phase records nothing under `hnsw_insert`; a writer
//! with no lexical dispatcher records nothing under `tantivy_index`.
//! That keeps each histogram a measurement of the stage itself rather
//! than a bimodal mix of real work and no-op early returns, and makes
//! `_count` a usable "how often did this stage run per write" signal.
//!
//! ## Units and bucket bounds
//!
//! Observations are MILLISECONDS — these are sub-millisecond to
//! tens-of-milliseconds operations, so seconds-valued buckets (what
//! the worker cycle histograms use) would collapse every one of them
//! into the first bucket. Each stage carries its own bounds because
//! their healthy ranges differ by three orders of magnitude:
//!
//! - **`wal_append`** — a channel hop to the WAL drain plus a
//!   group-commit `fdatasync`. Healthy is tens of microseconds when
//!   the batch folds into someone else's fsync, ~0.5–2 ms when it
//!   pays for its own on NVMe. Bounds start at 0.05 ms to resolve the
//!   folded case and run to 250 ms so a stalled device lands in a
//!   bounded bucket instead of `+Inf`.
//! - **`redb_commit`** — B-tree page writes plus redb's own durability
//!   fsync, and it carries every phase's mutations. Same fsync floor
//!   as the WAL but a fatter body, so the bounds start coarser
//!   (0.1 ms) and reach further (500 ms) for a large multi-phase
//!   transaction on a loaded disk.
//! - **`hnsw_insert`** — pure in-memory graph work under the writer's
//!   HNSW mutex, no IO. Healthy is 50–500 µs; the tail is lock
//!   contention, not the insert. Finest bounds of the four (0.025 ms)
//!   to resolve the common case, ceiling 100 ms.
//! - **`tantivy_index`** — the ack path does NOT write tantivy; it
//!   enqueues onto the lexical indexer's bounded queue, which the
//!   indexer drains off the ack path. A send into a queue with room
//!   is sub-microsecond, so the bounds start at 0.005 ms. The reason
//!   this stage is on the ack path at all is that the dispatch
//!   backpressures rather than dropping, so a saturated indexer shows
//!   up here as a multi-hundred-millisecond wait — hence a 1000 ms
//!   ceiling, the widest of the four.

use std::time::{Duration, Instant};

use super::histograms::{WorkerHistogram, WorkerHistogramSnapshot};

/// Bucket bounds (milliseconds) for the WAL append stage.
pub const WAL_APPEND_MS_BUCKETS: &[f64] = &[
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0,
];

/// Bucket bounds (milliseconds) for the redb commit stage.
pub const REDB_COMMIT_MS_BUCKETS: &[f64] = &[
    0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0,
];

/// Bucket bounds (milliseconds) for the redb apply stage.
///
/// Apply mutates B-tree pages in memory and never fsyncs — the
/// durability cost belongs to the commit that follows — so a healthy
/// apply sits in the tens of microseconds. The bounds start finer than
/// the commit's and still reach far, because apply scales with the
/// number of phases in a multi-phase write while commit does not.
pub const REDB_APPLY_MS_BUCKETS: &[f64] = &[
    0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 25.0, 100.0, 250.0,
];

/// Bucket bounds (milliseconds) for the HNSW insert stage.
pub const HNSW_INSERT_MS_BUCKETS: &[f64] = &[
    0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0,
];

/// Bucket bounds (milliseconds) for the lexical-index hand-off stage.
pub const TANTIVY_INDEX_MS_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 50.0, 100.0, 500.0, 1000.0,
];

/// A stage of the synchronous write path. Discriminants are
/// append-only — they index histogram slots that exposition reads by
/// position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStage {
    /// Durable WAL append (`append_many` through the shard's sink).
    WalAppend = 0,
    /// `WriteTransaction::commit` on the per-shard metadata DB.
    RedbCommit = 1,
    /// Vector insert into the shard's HNSW graph.
    HnswInsert = 2,
    /// Hand-off of the memory text to the lexical indexer queue.
    TantivyIndex = 3,
    /// Applying every phase into the open write transaction, plus the
    /// idempotency stamp — all the redb work that precedes the commit.
    ///
    /// Separate from [`Self::RedbCommit`] because the two answer
    /// different questions: apply is how much work the write asks redb
    /// to do, commit is what durability costs. A single `redb` timer
    /// conflates a wide multi-phase write with a slow disk.
    RedbApply = 4,
}

/// Label values published by the write-stage histogram family,
/// indexed by [`WriteStage`].
pub const WRITE_STAGE_LABELS: &[&str] = &[
    "wal_append",
    "redb_commit",
    "hnsw_insert",
    "tantivy_index",
    "redb_apply",
];

impl WriteStage {
    fn idx(self) -> usize {
        self as usize
    }

    /// Bucket bounds this stage's histogram is built with. Each stage
    /// has its own range — see the module doc for the reasoning.
    #[must_use]
    pub fn buckets(self) -> &'static [f64] {
        match self {
            Self::WalAppend => WAL_APPEND_MS_BUCKETS,
            Self::RedbCommit => REDB_COMMIT_MS_BUCKETS,
            Self::HnswInsert => HNSW_INSERT_MS_BUCKETS,
            Self::TantivyIndex => TANTIVY_INDEX_MS_BUCKETS,
            Self::RedbApply => REDB_APPLY_MS_BUCKETS,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        WRITE_STAGE_LABELS[self.idx()]
    }
}

/// Latency histograms for the ack path's stages, one per
/// [`WriteStage`]. Lives inside [`super::writer::WriterMetrics`] so
/// the write path threads a single `Arc` and one snapshot covers the
/// whole family; the type stays separate because the stage labels and
/// the phase tags are independent axes.
#[derive(Debug)]
pub struct WriteStageMetrics {
    /// Indexed by [`WriteStage`].
    latency_ms: Vec<WorkerHistogram>,
}

impl WriteStageMetrics {
    /// Construct a zeroed instance with each stage's own bounds.
    #[must_use]
    pub fn new() -> Self {
        let latency_ms = [
            WriteStage::WalAppend,
            WriteStage::RedbCommit,
            WriteStage::HnswInsert,
            WriteStage::TantivyIndex,
            WriteStage::RedbApply,
        ]
        .into_iter()
        .map(|stage| WorkerHistogram::new(stage.buckets()))
        .collect();
        Self { latency_ms }
    }

    /// Observe one run of `stage`. Called from the write path with a
    /// wall-clock `Instant::elapsed`; the cost is the conversion plus
    /// one lock-free histogram observe.
    pub fn record(&self, stage: WriteStage, elapsed: Duration) {
        self.latency_ms[stage.idx()].observe(elapsed.as_secs_f64() * 1_000.0);
    }

    /// Begin timing `stage`, observing it when the returned guard is
    /// dropped.
    ///
    /// For a stage whose region has more than one exit — an early
    /// `return` on an error, or a `?` — the guard is what keeps the
    /// histogram honest. Recording at the end of the region instead
    /// means a failed run contributes nothing, so the stage that is
    /// slow *because* it is failing is exactly the one the data hides.
    /// A guard observes on every exit path, and a future early return
    /// cannot silently opt out of it.
    ///
    /// Stages whose region is a single expression record directly
    /// through [`Self::record`]; they have no second exit to miss.
    pub fn start(&self, stage: WriteStage) -> StageTimer<'_> {
        StageTimer {
            metrics: self,
            stage,
            started: Instant::now(),
        }
    }

    /// Read-only snapshot for `/metrics`.
    #[must_use]
    pub fn snapshot(&self) -> WriteStageMetricsSnapshot {
        WriteStageMetricsSnapshot {
            latency_ms: self
                .latency_ms
                .iter()
                .map(WorkerHistogram::snapshot)
                .collect(),
        }
    }
}

impl Default for WriteStageMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Observes its stage's elapsed time when dropped.
///
/// Returned by [`WriteStageMetrics::start`]. Dropping it explicitly is
/// how a caller ends one stage before the next begins, so two adjacent
/// stages partition the work rather than overlap it.
pub struct StageTimer<'a> {
    metrics: &'a WriteStageMetrics,
    stage: WriteStage,
    started: Instant,
}

impl Drop for StageTimer<'_> {
    fn drop(&mut self) {
        self.metrics.record(self.stage, self.started.elapsed());
    }
}

/// Plain-data snapshot of [`WriteStageMetrics`]. `latency_ms` is
/// indexed in [`WRITE_STAGE_LABELS`] order.
#[derive(Debug, Clone)]
pub struct WriteStageMetricsSnapshot {
    pub latency_ms: Vec<WorkerHistogramSnapshot>,
}

impl WriteStageMetricsSnapshot {
    /// Observation count for one stage.
    #[must_use]
    pub fn count(&self, stage: WriteStage) -> u64 {
        self.latency_ms[stage.idx()].count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timer_observes_its_stage_even_when_its_scope_returns_early() {
        // The reason the apply stage uses a guard: its region returns
        // early on a phase failure, and a run that was slow because it
        // was failing is the one an operator most needs to see. A
        // record-at-the-end timer contributes nothing on that path.
        let m = WriteStageMetrics::new();

        // `fail` makes the return genuinely early — there is work
        // after it that the error path skips, which is the shape the
        // real apply region has.
        fn apply_phases(m: &WriteStageMetrics, fail: bool) -> Result<u32, String> {
            let _timer = m.start(WriteStage::RedbApply);
            if fail {
                return Err("phase apply failed".to_string());
            }
            Ok(1)
        }

        assert!(apply_phases(&m, true).is_err());
        assert_eq!(
            m.snapshot().count(WriteStage::RedbApply),
            1,
            "an early return must still observe the stage",
        );
    }

    #[test]
    fn dropping_a_timer_early_ends_its_stage_before_the_next_begins() {
        // Adjacent stages must partition the work, not overlap it:
        // apply ends exactly where commit starts.
        let m = WriteStageMetrics::new();
        let apply = m.start(WriteStage::RedbApply);
        std::thread::sleep(Duration::from_millis(2));
        drop(apply);
        let commit = m.start(WriteStage::RedbCommit);
        drop(commit);

        let snap = m.snapshot();
        let apply_ms = snap.latency_ms[WriteStage::RedbApply as usize].sum;
        let commit_ms = snap.latency_ms[WriteStage::RedbCommit as usize].sum;
        assert!(
            apply_ms >= 2.0,
            "apply observed {apply_ms} ms, expected >= 2"
        );
        assert!(
            commit_ms < apply_ms,
            "commit ({commit_ms} ms) must not include apply's time ({apply_ms} ms)",
        );
    }

    #[test]
    fn stage_labels_match_discriminants() {
        assert_eq!(WriteStage::WalAppend.label(), "wal_append");
        assert_eq!(WriteStage::RedbCommit.label(), "redb_commit");
        assert_eq!(WriteStage::HnswInsert.label(), "hnsw_insert");
        assert_eq!(WriteStage::TantivyIndex.label(), "tantivy_index");
        assert_eq!(WriteStage::RedbApply.label(), "redb_apply");
        assert_eq!(WRITE_STAGE_LABELS.len(), 5);
    }

    #[test]
    fn each_stage_records_into_its_own_histogram() {
        let m = WriteStageMetrics::new();
        m.record(WriteStage::WalAppend, Duration::from_micros(800));
        m.record(WriteStage::WalAppend, Duration::from_micros(200));
        m.record(WriteStage::HnswInsert, Duration::from_micros(300));
        let s = m.snapshot();
        assert_eq!(s.count(WriteStage::WalAppend), 2);
        assert_eq!(s.count(WriteStage::HnswInsert), 1);
        assert_eq!(s.count(WriteStage::RedbCommit), 0);
        assert_eq!(s.count(WriteStage::TantivyIndex), 0);
        assert_eq!(s.count(WriteStage::RedbApply), 0);
        // 800 µs + 200 µs == 1 ms, in the family's millisecond unit.
        let sum = s.latency_ms[WriteStage::WalAppend as usize].sum;
        assert!((sum - 1.0).abs() < 1e-6, "sum was {sum} ms");
    }

    #[test]
    fn stage_bucket_bounds_are_ascending_and_distinct_per_stage() {
        for stage in [
            WriteStage::WalAppend,
            WriteStage::RedbCommit,
            WriteStage::HnswInsert,
            WriteStage::TantivyIndex,
            WriteStage::RedbApply,
        ] {
            let b = stage.buckets();
            assert!(b.windows(2).all(|w| w[0] < w[1]), "{stage:?} not ascending");
        }
        assert_ne!(
            WriteStage::HnswInsert.buckets()[0],
            WriteStage::TantivyIndex.buckets()[0],
            "stages must carry bounds tuned to their own range",
        );
    }
}
