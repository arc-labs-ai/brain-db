//! Per-stage wall-clock accounting for one derivation cycle.
//!
//! The extractor cycle is a chain of stages — gate probe, text load,
//! preparation, three extractor tiers, apply, HyPE — several of which run
//! concurrently with one another. A throughput number for the cycle as a
//! whole says nothing about which stage holds the clock, so each stage
//! reports the wall time between its entry and its exit into a process-wide
//! accumulator keyed by stage.
//!
//! Wall time, not poll time: a stage parked on a semaphore or a network
//! round-trip is still holding the cycle, and that is exactly what we are
//! trying to see. The consequence is that concurrently-running stages each
//! report the full overlapping span, so sibling stages can sum to more than
//! their parent. Read a parent's own figure as the truth and the children as
//! a split that may double-count overlap.
//!
//! Cost per stage is one `Instant::now()` pair and one relaxed atomic add.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// One accounted stage of the derivation cycle.
///
/// Ordered outermost-first so a printed report reads as a nesting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Whole `do_extractor_cycle`, wakeup included.
    CycleTotal,
    /// Blocking on the wakeup channel or the interval timer.
    CycleWait,
    /// Reading the durable extraction queue for the cycle's work list.
    LoadPending,
    /// Every micro-batch chunk, from first launch to last completion.
    Chunks,
    /// The post-extraction graph-aware HyPE refresh sweep.
    HypeRefresh,
    /// Resolving queued ids to their stored text.
    TextLoad,
    /// One micro-batch end to end.
    DrainBatch,
    /// The per-memory already-extracted idempotency probe.
    GateProbe,
    /// Waiting for the batch's preparation byte reservation.
    PrepAdmit,
    /// Reading each memory's row facts and building its `CoreMemory`.
    RowFacts,
    /// Resolving owning namespace ids to names.
    NamespaceResolve,
    /// Fetching the per-memory neighbour context fed to the LLM prompt.
    ContextFetch,
    /// Rendering per-namespace entity vocabulary and the kind taxonomy.
    Vocabulary,
    /// Embedding each memory and ranking candidate predicates for its prompt.
    CandidatePredicates,
    /// The concurrent extraction-plus-HyPE pair.
    ExtractAndHype,
    /// The tiered extraction pipeline alone.
    Pipeline,
    /// Writing every memory's outcome into the graph.
    Apply,
    /// Write-time HyPE generation for the batch.
    HypePass,
    /// Tier 1.
    PatternTier,
    /// Tier 2.
    ClassifierTier,
    /// Tier 3.
    LlmTier,
}

impl Stage {
    const ALL: [Stage; 21] = [
        Stage::CycleTotal,
        Stage::CycleWait,
        Stage::LoadPending,
        Stage::Chunks,
        Stage::HypeRefresh,
        Stage::TextLoad,
        Stage::DrainBatch,
        Stage::GateProbe,
        Stage::PrepAdmit,
        Stage::RowFacts,
        Stage::NamespaceResolve,
        Stage::ContextFetch,
        Stage::Vocabulary,
        Stage::CandidatePredicates,
        Stage::ExtractAndHype,
        Stage::Pipeline,
        Stage::Apply,
        Stage::HypePass,
        Stage::PatternTier,
        Stage::ClassifierTier,
        Stage::LlmTier,
    ];

    const fn index(self) -> usize {
        self as usize
    }

    /// Short label for a printed report.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Stage::CycleTotal => "cycle_total",
            Stage::CycleWait => "cycle_wait",
            Stage::LoadPending => "load_pending",
            Stage::Chunks => "chunks",
            Stage::HypeRefresh => "hype_refresh_sweep",
            Stage::TextLoad => "text_load",
            Stage::DrainBatch => "drain_batch",
            Stage::GateProbe => "gate_probe",
            Stage::PrepAdmit => "prep_admit",
            Stage::RowFacts => "row_facts",
            Stage::NamespaceResolve => "namespace_resolve",
            Stage::ContextFetch => "context_fetch",
            Stage::Vocabulary => "vocabulary",
            Stage::CandidatePredicates => "candidate_predicates",
            Stage::ExtractAndHype => "extract_and_hype",
            Stage::Pipeline => "pipeline",
            Stage::Apply => "apply",
            Stage::HypePass => "hype_pass",
            Stage::PatternTier => "tier_pattern",
            Stage::ClassifierTier => "tier_classifier",
            Stage::LlmTier => "tier_llm",
        }
    }
}

const STAGES: usize = Stage::ALL.len();

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);
static NANOS: [AtomicU64; STAGES] = [ZERO; STAGES];
static HITS: [AtomicU64; STAGES] = [ZERO; STAGES];

/// Account `nanos` against `stage`.
pub fn record(stage: Stage, nanos: u64) {
    NANOS[stage.index()].fetch_add(nanos, Ordering::Relaxed);
    HITS[stage.index()].fetch_add(1, Ordering::Relaxed);
}

/// Time a synchronous block against `stage`.
pub fn timed_sync<T>(stage: Stage, f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let out = f();
    record(stage, started.elapsed().as_nanos() as u64);
    out
}

/// Time a future from first poll's caller to completion against `stage`.
pub async fn timed<T>(stage: Stage, fut: impl Future<Output = T>) -> T {
    let started = Instant::now();
    let out = fut.await;
    record(stage, started.elapsed().as_nanos() as u64);
    out
}

/// Clear every accumulator. A measurement run calls this between samples.
pub fn reset() {
    for i in 0..STAGES {
        NANOS[i].store(0, Ordering::Relaxed);
        HITS[i].store(0, Ordering::Relaxed);
    }
}

/// Accumulated nanoseconds and entry count for `stage`.
#[must_use]
pub fn read(stage: Stage) -> (u64, u64) {
    (
        NANOS[stage.index()].load(Ordering::Relaxed),
        HITS[stage.index()].load(Ordering::Relaxed),
    )
}

/// Every stage's accumulated time, in declaration order.
#[must_use]
pub fn snapshot() -> Vec<(Stage, u64, u64)> {
    Stage::ALL
        .iter()
        .map(|&s| {
            let (nanos, hits) = read(s);
            (s, nanos, hits)
        })
        .collect()
}

/// A printed report of where the cycle's wall clock went, each stage as a
/// share of `Stage::CycleTotal`.
#[must_use]
pub fn report() -> String {
    let (total, _) = read(Stage::CycleTotal);
    let total_f = if total == 0 { 1.0 } else { total as f64 };
    let mut out = String::new();
    out.push_str("stage                    ms        share   entries\n");
    for (stage, nanos, hits) in snapshot() {
        out.push_str(&format!(
            "{:<22} {:>9.2}  {:>6.2}%  {:>8}\n",
            stage.label(),
            nanos as f64 / 1e6,
            100.0 * nanos as f64 / total_f,
            hits,
        ));
    }
    out
}
