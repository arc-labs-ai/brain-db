//! Per-retriever read-path metric family.
//!
//! Records what each retriever lane (semantic / lexical / graph)
//! contributed on a served RECALL: how many times it was invoked, how
//! many candidates it returned, and how long it took. The data is
//! MEASURED by the planner executor (per-lane elapsed + candidate
//! counts already ride in `QueryMetadata`) and RECORDED here from the
//! RECALL handler after `execute` returns — the hot fan-out loop is
//! untouched. Cost per recall is a handful of atomic adds plus one
//! histogram observe per lane.
//!
//! Same shared-by-`Arc` pattern as the worker families: the handler
//! side bumps the atomics, `brain-server`'s `/metrics` exposition
//! reads through [`RetrieverMetrics::snapshot`] on scrape.

use std::sync::atomic::{AtomicU64, Ordering};

use super::histograms::{WorkerHistogram, WorkerHistogramSnapshot};

/// Bucket bounds (milliseconds) for the per-retriever latency
/// histogram. Covers the sub-millisecond HNSW fast path through the
/// ~1 s soft-timeout ceiling; `+Inf` catches a pathological lane.
pub const RETRIEVER_LATENCY_MS_BUCKETS: &[f64] = &[
    0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0,
];

/// Retriever lane label. Mirrors `brain_planner`'s `Retriever`
/// discriminant, but kept local so the metric family doesn't couple
/// its counter layout to a planner enum. Discriminants are append-only
/// — they index counter slots observability reads by position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrieverKind {
    Semantic = 0,
    Lexical = 1,
    Graph = 2,
}

/// Label values published by the retriever counter families, indexed
/// by [`RetrieverKind`].
pub const RETRIEVER_LABELS: &[&str] = &["semantic", "lexical", "graph"];

impl RetrieverKind {
    fn idx(self) -> usize {
        self as usize
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        RETRIEVER_LABELS[self.idx()]
    }
}

/// Metric family for the read-path retriever lanes. One `Arc` is
/// shared between the RECALL handler (which records) and the
/// `/metrics` exposition (which snapshots).
#[derive(Debug)]
pub struct RetrieverMetrics {
    /// Indexed by [`RetrieverKind`].
    invocations_total: Vec<AtomicU64>,
    /// Indexed by [`RetrieverKind`].
    candidates_total: Vec<AtomicU64>,
    /// One latency histogram per [`RetrieverKind`].
    latency_ms: Vec<WorkerHistogram>,
}

impl RetrieverMetrics {
    /// Construct a zeroed instance.
    #[must_use]
    pub fn new() -> Self {
        let invocations_total = (0..RETRIEVER_LABELS.len())
            .map(|_| AtomicU64::new(0))
            .collect();
        let candidates_total = (0..RETRIEVER_LABELS.len())
            .map(|_| AtomicU64::new(0))
            .collect();
        let latency_ms = (0..RETRIEVER_LABELS.len())
            .map(|_| WorkerHistogram::new(RETRIEVER_LATENCY_MS_BUCKETS))
            .collect();
        Self {
            invocations_total,
            candidates_total,
            latency_ms,
        }
    }

    /// Record one invoked retriever lane: bump its invocation counter,
    /// add its returned candidate count, and observe its wall-clock
    /// (milliseconds). The RECALL handler calls this once per lane that
    /// actually ran (skipped lanes are not recorded — they weren't
    /// invoked).
    pub fn record(&self, retriever: RetrieverKind, elapsed_ms: f64, candidates: u64) {
        let idx = retriever.idx();
        self.invocations_total[idx].fetch_add(1, Ordering::Relaxed);
        self.candidates_total[idx].fetch_add(candidates, Ordering::Relaxed);
        self.latency_ms[idx].observe(elapsed_ms);
    }

    /// Read-only snapshot for `/metrics`.
    #[must_use]
    pub fn snapshot(&self) -> RetrieverMetricsSnapshot {
        let invocations_total = self
            .invocations_total
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();
        let candidates_total = self
            .candidates_total
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();
        let latency_ms = self
            .latency_ms
            .iter()
            .map(WorkerHistogram::snapshot)
            .collect();
        RetrieverMetricsSnapshot {
            invocations_total,
            candidates_total,
            latency_ms,
        }
    }
}

impl Default for RetrieverMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-data snapshot of [`RetrieverMetrics`]. All `Vec`s are indexed
/// in [`RETRIEVER_LABELS`] order.
#[derive(Debug, Clone)]
pub struct RetrieverMetricsSnapshot {
    pub invocations_total: Vec<u64>,
    pub candidates_total: Vec<u64>,
    pub latency_ms: Vec<WorkerHistogramSnapshot>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retriever_counters_round_trip() {
        let m = RetrieverMetrics::new();
        m.record(RetrieverKind::Semantic, 3.0, 40);
        m.record(RetrieverKind::Semantic, 5.0, 10);
        m.record(RetrieverKind::Lexical, 1.0, 7);
        // Graph never invoked this run.
        let s = m.snapshot();
        assert_eq!(s.invocations_total[RetrieverKind::Semantic as usize], 2);
        assert_eq!(s.candidates_total[RetrieverKind::Semantic as usize], 50);
        assert_eq!(s.invocations_total[RetrieverKind::Lexical as usize], 1);
        assert_eq!(s.candidates_total[RetrieverKind::Lexical as usize], 7);
        assert_eq!(s.invocations_total[RetrieverKind::Graph as usize], 0);
        assert_eq!(s.candidates_total[RetrieverKind::Graph as usize], 0);
        assert_eq!(s.latency_ms[RetrieverKind::Semantic as usize].count, 2);
        assert_eq!(s.latency_ms[RetrieverKind::Graph as usize].count, 0);
    }

    #[test]
    fn retriever_labels_match_discriminants() {
        assert_eq!(RetrieverKind::Semantic.label(), "semantic");
        assert_eq!(RetrieverKind::Lexical.label(), "lexical");
        assert_eq!(RetrieverKind::Graph.label(), "graph");
    }
}
