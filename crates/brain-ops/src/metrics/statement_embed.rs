//! `StatementEmbedWorker` metric family.
//!
//! Lives in `brain-ops` (not `brain-workers`) so `/metrics` exposition
//! in `brain-server` can read snapshots without forming a
//! `brain-server -> brain-workers` dependency edge — mirrors the other
//! background-worker families (`auto_edge`, `extractor`, `llm_cache`).

use std::sync::atomic::{AtomicU64, Ordering};

use super::histograms::{WorkerHistogram, WorkerHistogramSnapshot, DEFAULT_CYCLE_BUCKETS_SECONDS};

/// Counters + histogram for the Statement-HNSW embedding worker.
/// One per shard; shared by `Arc` between the worker (the producer)
/// and the metrics exposition path (the consumer).
#[derive(Debug)]
pub struct StatementEmbedMetrics {
    /// One per `tick()` call, regardless of work done.
    cycles_total: AtomicU64,
    /// Statements actually inserted into the Statement HNSW.
    rows_embedded_total: AtomicU64,
    /// Statements peeked from the queue but skipped — tombstoned,
    /// superseded, decoded-bad, or already present in the HNSW.
    rows_skipped_total: AtomicU64,
    /// Embedder failures (per-text or per-batch). Bumped once per
    /// failed batch; the worker logs a warn and leaves the row in the
    /// queue for the next tick.
    embed_errors_total: AtomicU64,
    /// Wall-clock per batch (one observe per `cycle`).
    batch_duration_seconds: WorkerHistogram,
}

impl StatementEmbedMetrics {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cycles_total: AtomicU64::new(0),
            rows_embedded_total: AtomicU64::new(0),
            rows_skipped_total: AtomicU64::new(0),
            embed_errors_total: AtomicU64::new(0),
            batch_duration_seconds: WorkerHistogram::new(DEFAULT_CYCLE_BUCKETS_SECONDS),
        }
    }

    pub fn inc_cycles(&self) {
        self.cycles_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_rows_embedded(&self, n: u64) {
        self.rows_embedded_total.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_rows_skipped(&self, n: u64) {
        self.rows_skipped_total.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_embed_errors(&self) {
        self.embed_errors_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn observe_batch_duration(&self, seconds: f64) {
        self.batch_duration_seconds.observe(seconds);
    }

    #[must_use]
    pub fn snapshot(&self) -> StatementEmbedMetricsSnapshot {
        StatementEmbedMetricsSnapshot {
            cycles_total: self.cycles_total.load(Ordering::Relaxed),
            rows_embedded_total: self.rows_embedded_total.load(Ordering::Relaxed),
            rows_skipped_total: self.rows_skipped_total.load(Ordering::Relaxed),
            embed_errors_total: self.embed_errors_total.load(Ordering::Relaxed),
            batch_duration_seconds: self.batch_duration_seconds.snapshot(),
        }
    }
}

impl Default for StatementEmbedMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-data snapshot of [`StatementEmbedMetrics`]. Crosses the shard
/// boundary via `flume` like the other worker snapshots.
#[derive(Debug, Clone)]
pub struct StatementEmbedMetricsSnapshot {
    pub cycles_total: u64,
    pub rows_embedded_total: u64,
    pub rows_skipped_total: u64,
    pub embed_errors_total: u64,
    pub batch_duration_seconds: WorkerHistogramSnapshot,
}
