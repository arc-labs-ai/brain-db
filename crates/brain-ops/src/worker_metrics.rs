//! Shared metric state for the writer-fed background workers.
//!
//! Both the writer (which performs the post-ENCODE enqueue) and the
//! worker (which drains the queue and runs the cycle) need to publish
//! into the same counter family. They sit on opposite sides of the
//! `brain-ops` → `brain-workers` dependency, so the atomics live here
//! and both layers hold an `Arc` to the same struct.
//!
//! The structs are deliberately allocation-light at construction:
//! every counter is an `AtomicU64`; every histogram is a fixed-size
//! `Vec<AtomicU64>` sized once. After construction the hot path is
//! lock-free `fetch_add`.
//!
//! `brain-server`'s `/metrics` exposition reads through
//! [`snapshot`](AutoEdgeMetrics::snapshot) /
//! [`snapshot`](ExtractorMetrics::snapshot) on every scrape; production
//! latency is the cost of loading a small number of atomics.

use std::sync::atomic::{AtomicU64, Ordering};

/// Bucket bounds (seconds, cumulative) for the worker cycle-duration
/// histograms. Range covers the 1 ms fast path (queue empty, immediate
/// exit) through 30 s safety ceiling (well past the worker's 5 s
/// `max_runtime` budget) so an over-budget cycle still lands in a
/// bounded bucket rather than `+Inf`.
pub const DEFAULT_CYCLE_BUCKETS_SECONDS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// Bucket bounds (counts) for the AutoEdge "neighbours found per
/// cycle" histogram. Caps out around the worker's `batch_size` (256)
/// times an aggressive `top_k` (5) → 1280; `+Inf` catches anything
/// extreme.
pub const DEFAULT_NEIGHBOURS_BUCKETS: &[f64] =
    &[1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0];

/// Tier label values published by the extractor `tier_runs_total` and
/// `resolver_outcome_total` counter families on [`ExtractorMetrics`].
/// The resolver labels are a superset because the resolver outcome
/// family carries `exact / alias / fuzzy / create` rather than tier kinds.
pub const TIER_LABELS: &[&str] = &["pattern", "classifier", "llm"];
pub const TIER_STATUS_LABELS: &[&str] = &["ran", "skipped", "failed"];
pub const RESOLVER_OUTCOME_LABELS: &[&str] = &["exact", "alias", "fuzzy", "create"];

/// Item kinds published by the `items_written_total` counter family
/// on [`ExtractorMetrics`].
pub const ITEM_KIND_LABELS: &[&str] = &["entity", "statement", "relation", "mention"];

// ---------------------------------------------------------------------
// Fixed-bucket histogram (worker-local, allocation-free per observe).
// ---------------------------------------------------------------------

/// Fixed-bucket histogram with cumulative semantics. Mirrors the
/// shape of `brain-server`'s `Histogram`, but kept here to avoid a
/// `brain-ops -> brain-server` dependency edge. Observations are
/// stored unscaled (`f64` sum exposed at snapshot time).
#[derive(Debug)]
pub struct WorkerHistogram {
    bounds: &'static [f64],
    /// `counts.len() == bounds.len() + 1` — trailing entry is `+Inf`.
    counts: Vec<AtomicU64>,
    /// Sum × 1_000_000 (six decimal places of precision for seconds).
    sum_micros: AtomicU64,
    count: AtomicU64,
}

impl WorkerHistogram {
    /// Construct an empty histogram with the supplied bucket bounds.
    /// Bounds must be sorted ascending; the constructor doesn't sort
    /// — callers pass the static slices above.
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

    /// Record one observation. Negative values are clamped to zero
    /// so the histogram sum stays meaningful.
    pub fn observe(&self, value: f64) {
        let v = value.max(0.0);
        let scaled = (v * 1_000_000.0) as u64;
        self.sum_micros.fetch_add(scaled, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        for (i, &bound) in self.bounds.iter().enumerate() {
            if v <= bound {
                self.counts[i].fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        let last = self.counts.len() - 1;
        self.counts[last].fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot bucket counts cumulatively. Used by `/metrics`
    /// exposition.
    #[must_use]
    pub fn snapshot(&self) -> WorkerHistogramSnapshot {
        let mut buckets = Vec::with_capacity(self.counts.len());
        let mut running = 0u64;
        for (i, c) in self.counts.iter().enumerate() {
            running += c.load(Ordering::Relaxed);
            let upper = if i < self.bounds.len() {
                Some(self.bounds[i])
            } else {
                None
            };
            buckets.push(WorkerBucketSnapshot {
                le: upper,
                cumulative_count: running,
            });
        }
        WorkerHistogramSnapshot {
            buckets,
            sum: self.sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            count: self.count.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorkerHistogramSnapshot {
    pub buckets: Vec<WorkerBucketSnapshot>,
    pub sum: f64,
    pub count: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct WorkerBucketSnapshot {
    /// Upper bound (`<=`) or `None` for the `+Inf` overflow bucket.
    pub le: Option<f64>,
    pub cumulative_count: u64,
}

// ---------------------------------------------------------------------
// AutoEdgeMetrics
// ---------------------------------------------------------------------

/// Metric family for `AutoEdgeWorker`. Shared between the writer
/// (drops counter on `try_send` overflow) and the worker (everything
/// else).
#[derive(Debug)]
pub struct AutoEdgeMetrics {
    drops_total: AtomicU64,
    edges_written_total: AtomicU64,
    cycle_duration_seconds: WorkerHistogram,
    neighbours_found_per_cycle: WorkerHistogram,
}

impl AutoEdgeMetrics {
    /// Construct a zeroed instance. One per shard at startup, shared
    /// by `Arc` between the writer's enqueue path and the worker's
    /// cycle loop.
    #[must_use]
    pub fn new() -> Self {
        Self {
            drops_total: AtomicU64::new(0),
            edges_written_total: AtomicU64::new(0),
            cycle_duration_seconds: WorkerHistogram::new(DEFAULT_CYCLE_BUCKETS_SECONDS),
            neighbours_found_per_cycle: WorkerHistogram::new(DEFAULT_NEIGHBOURS_BUCKETS),
        }
    }

    /// Bumped by the writer's `try_send` path when the bounded channel
    /// is full (encode succeeds; the enqueue is dropped).
    pub fn inc_drop(&self) {
        self.drops_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Bumped by the worker once per logical edge persisted in the
    /// cycle's wtxn.
    pub fn add_edges_written(&self, n: u64) {
        self.edges_written_total.fetch_add(n, Ordering::Relaxed);
    }

    /// Observed by the worker at the end of every cycle (wall-clock).
    pub fn observe_cycle_duration(&self, seconds: f64) {
        self.cycle_duration_seconds.observe(seconds);
    }

    /// Observed by the worker once per cycle: the total number of
    /// post-threshold neighbours collected across the drained
    /// memories. Zero is recorded on empty cycles so PromQL `_count`
    /// matches `brain_worker_cycles_total` for this worker.
    pub fn observe_neighbours_found(&self, n: u64) {
        self.neighbours_found_per_cycle.observe(n as f64);
    }

    /// Read-only snapshot for `/metrics`.
    #[must_use]
    pub fn snapshot(&self) -> AutoEdgeMetricsSnapshot {
        AutoEdgeMetricsSnapshot {
            drops_total: self.drops_total.load(Ordering::Relaxed),
            edges_written_total: self.edges_written_total.load(Ordering::Relaxed),
            cycle_duration_seconds: self.cycle_duration_seconds.snapshot(),
            neighbours_found_per_cycle: self.neighbours_found_per_cycle.snapshot(),
        }
    }
}

impl Default for AutoEdgeMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-data snapshot of [`AutoEdgeMetrics`]. Crosses the shard
/// boundary via `flume` like the existing worker `Snapshot`.
#[derive(Debug, Clone)]
pub struct AutoEdgeMetricsSnapshot {
    pub drops_total: u64,
    pub edges_written_total: u64,
    pub cycle_duration_seconds: WorkerHistogramSnapshot,
    pub neighbours_found_per_cycle: WorkerHistogramSnapshot,
}

// ---------------------------------------------------------------------
// TemporalEdgeMetrics
// ---------------------------------------------------------------------

/// Bucket boundaries (seconds) for the temporal gap histogram. Tuned
/// for the 0–5 minute default window with logarithmic spacing past
/// 60 s so operators can see both "tight agent loops" and "near the
/// window edge."
const DEFAULT_TEMPORAL_GAP_BUCKETS_SECONDS: &[f64] = &[
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];

/// Why the TemporalEdgeWorker skipped writing a `FollowedBy` for an
/// enqueued memory. Each variant tracks a counter so operators can
/// answer "why no temporal edges?" without trawling logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalSkipReason {
    /// No predecessor found in the agent-timeline index.
    NoPrev = 0,
    /// Candidate predecessor's `created_at` is ≥ this memory's
    /// (clock-skew / replay).
    OutOfOrder = 1,
    /// Candidate predecessor is tombstoned.
    Tombstoned = 2,
    /// Candidate predecessor is in a different context (and
    /// `cross_context = false`).
    CrossContext = 3,
    /// Gap exceeded the temporal window.
    WindowExceeded = 4,
}

/// Metric family for `TemporalEdgeWorker`. Shared between the writer
/// (drops counter on `try_send` overflow) and the worker (everything
/// else).
#[derive(Debug)]
pub struct TemporalEdgeMetrics {
    drops_total: AtomicU64,
    edges_written_total: AtomicU64,
    skipped_no_prev: AtomicU64,
    skipped_out_of_order: AtomicU64,
    skipped_tombstoned: AtomicU64,
    skipped_cross_context: AtomicU64,
    skipped_window_exceeded: AtomicU64,
    cycle_duration_seconds: WorkerHistogram,
    gap_seconds: WorkerHistogram,
}

impl TemporalEdgeMetrics {
    #[must_use]
    pub fn new() -> Self {
        Self {
            drops_total: AtomicU64::new(0),
            edges_written_total: AtomicU64::new(0),
            skipped_no_prev: AtomicU64::new(0),
            skipped_out_of_order: AtomicU64::new(0),
            skipped_tombstoned: AtomicU64::new(0),
            skipped_cross_context: AtomicU64::new(0),
            skipped_window_exceeded: AtomicU64::new(0),
            cycle_duration_seconds: WorkerHistogram::new(DEFAULT_CYCLE_BUCKETS_SECONDS),
            gap_seconds: WorkerHistogram::new(DEFAULT_TEMPORAL_GAP_BUCKETS_SECONDS),
        }
    }

    pub fn inc_drop(&self) {
        self.drops_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_edges_written(&self, n: u64) {
        self.edges_written_total.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_skip(&self, reason: TemporalSkipReason) {
        let c = match reason {
            TemporalSkipReason::NoPrev => &self.skipped_no_prev,
            TemporalSkipReason::OutOfOrder => &self.skipped_out_of_order,
            TemporalSkipReason::Tombstoned => &self.skipped_tombstoned,
            TemporalSkipReason::CrossContext => &self.skipped_cross_context,
            TemporalSkipReason::WindowExceeded => &self.skipped_window_exceeded,
        };
        c.fetch_add(1, Ordering::Relaxed);
    }

    pub fn observe_cycle_duration(&self, seconds: f64) {
        self.cycle_duration_seconds.observe(seconds);
    }

    pub fn observe_gap_seconds(&self, seconds: f64) {
        self.gap_seconds.observe(seconds);
    }

    #[must_use]
    pub fn snapshot(&self) -> TemporalEdgeMetricsSnapshot {
        TemporalEdgeMetricsSnapshot {
            drops_total: self.drops_total.load(Ordering::Relaxed),
            edges_written_total: self.edges_written_total.load(Ordering::Relaxed),
            skipped_no_prev: self.skipped_no_prev.load(Ordering::Relaxed),
            skipped_out_of_order: self.skipped_out_of_order.load(Ordering::Relaxed),
            skipped_tombstoned: self.skipped_tombstoned.load(Ordering::Relaxed),
            skipped_cross_context: self.skipped_cross_context.load(Ordering::Relaxed),
            skipped_window_exceeded: self.skipped_window_exceeded.load(Ordering::Relaxed),
            cycle_duration_seconds: self.cycle_duration_seconds.snapshot(),
            gap_seconds: self.gap_seconds.snapshot(),
        }
    }
}

impl Default for TemporalEdgeMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct TemporalEdgeMetricsSnapshot {
    pub drops_total: u64,
    pub edges_written_total: u64,
    pub skipped_no_prev: u64,
    pub skipped_out_of_order: u64,
    pub skipped_tombstoned: u64,
    pub skipped_cross_context: u64,
    pub skipped_window_exceeded: u64,
    pub cycle_duration_seconds: WorkerHistogramSnapshot,
    pub gap_seconds: WorkerHistogramSnapshot,
}

// ---------------------------------------------------------------------
// CausalEdgeMetrics
// ---------------------------------------------------------------------

/// Why the CausalEdgeWorker skipped writing a `Caused` edge for an
/// enqueued statement. Operators read these to triage "why are no
/// causal edges landing?" without trawling logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CausalSkipReason {
    /// The statement's predicate isn't in the resolved causal whitelist.
    /// This is the dominant outcome on a healthy substrate-only or
    /// schema-without-causal-predicates deployment.
    NonCausalPredicate = 0,
    /// Statement confidence is below the worker's floor.
    LowConfidence = 1,
    /// Statement has no evidence memories to anchor the effect side.
    NoEvidence = 2,
    /// Statement object isn't an entity or memory ref — `Value(_)` and
    /// `Statement(_)` variants don't produce a memory→memory edge in v1.
    ObjectNotEntity = 3,
    /// No related statement on the cause-side entity, so there's nothing
    /// to walk back from `object` to a cause-anchoring memory.
    NoRelatedStatement = 4,
    /// Statement row vanished (race with FORGET cascade).
    StatementMissing = 5,
}

#[derive(Debug)]
pub struct CausalEdgeMetrics {
    drops_total: AtomicU64,
    edges_written_total: AtomicU64,
    skipped_non_causal_predicate: AtomicU64,
    skipped_low_confidence: AtomicU64,
    skipped_no_evidence: AtomicU64,
    skipped_object_not_entity: AtomicU64,
    skipped_no_related_statement: AtomicU64,
    skipped_statement_missing: AtomicU64,
    /// Gauge — how many causal predicates the worker resolved on this
    /// shard. Bumped on first successful resolve; 0 means the worker
    /// has no causal vocabulary and every drained enqueue no-ops.
    predicate_whitelist_resolved: AtomicU64,
    cycle_duration_seconds: WorkerHistogram,
}

impl CausalEdgeMetrics {
    #[must_use]
    pub fn new() -> Self {
        Self {
            drops_total: AtomicU64::new(0),
            edges_written_total: AtomicU64::new(0),
            skipped_non_causal_predicate: AtomicU64::new(0),
            skipped_low_confidence: AtomicU64::new(0),
            skipped_no_evidence: AtomicU64::new(0),
            skipped_object_not_entity: AtomicU64::new(0),
            skipped_no_related_statement: AtomicU64::new(0),
            skipped_statement_missing: AtomicU64::new(0),
            predicate_whitelist_resolved: AtomicU64::new(0),
            cycle_duration_seconds: WorkerHistogram::new(DEFAULT_CYCLE_BUCKETS_SECONDS),
        }
    }

    pub fn inc_drop(&self) {
        self.drops_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_edges_written(&self, n: u64) {
        self.edges_written_total.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_skip(&self, reason: CausalSkipReason) {
        let c = match reason {
            CausalSkipReason::NonCausalPredicate => &self.skipped_non_causal_predicate,
            CausalSkipReason::LowConfidence => &self.skipped_low_confidence,
            CausalSkipReason::NoEvidence => &self.skipped_no_evidence,
            CausalSkipReason::ObjectNotEntity => &self.skipped_object_not_entity,
            CausalSkipReason::NoRelatedStatement => &self.skipped_no_related_statement,
            CausalSkipReason::StatementMissing => &self.skipped_statement_missing,
        };
        c.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_whitelist_resolved(&self, n: u64) {
        self.predicate_whitelist_resolved
            .store(n, Ordering::Relaxed);
    }

    pub fn observe_cycle_duration(&self, seconds: f64) {
        self.cycle_duration_seconds.observe(seconds);
    }

    #[must_use]
    pub fn snapshot(&self) -> CausalEdgeMetricsSnapshot {
        CausalEdgeMetricsSnapshot {
            drops_total: self.drops_total.load(Ordering::Relaxed),
            edges_written_total: self.edges_written_total.load(Ordering::Relaxed),
            skipped_non_causal_predicate: self.skipped_non_causal_predicate.load(Ordering::Relaxed),
            skipped_low_confidence: self.skipped_low_confidence.load(Ordering::Relaxed),
            skipped_no_evidence: self.skipped_no_evidence.load(Ordering::Relaxed),
            skipped_object_not_entity: self.skipped_object_not_entity.load(Ordering::Relaxed),
            skipped_no_related_statement: self.skipped_no_related_statement.load(Ordering::Relaxed),
            skipped_statement_missing: self.skipped_statement_missing.load(Ordering::Relaxed),
            predicate_whitelist_resolved: self.predicate_whitelist_resolved.load(Ordering::Relaxed),
            cycle_duration_seconds: self.cycle_duration_seconds.snapshot(),
        }
    }
}

impl Default for CausalEdgeMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct CausalEdgeMetricsSnapshot {
    pub drops_total: u64,
    pub edges_written_total: u64,
    pub skipped_non_causal_predicate: u64,
    pub skipped_low_confidence: u64,
    pub skipped_no_evidence: u64,
    pub skipped_object_not_entity: u64,
    pub skipped_no_related_statement: u64,
    pub skipped_statement_missing: u64,
    pub predicate_whitelist_resolved: u64,
    pub cycle_duration_seconds: WorkerHistogramSnapshot,
}

// ---------------------------------------------------------------------
// ExtractorMetrics
// ---------------------------------------------------------------------

/// Resolver outcome the worker reports per resolved `EntityMention`.
/// `exact` / `alias` / `fuzzy` correspond to the three lookup tiers in
/// `brain-extractors::resolver`; `create` is the tier-4 fall-through
/// that minted a fresh entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolverOutcome {
    Exact = 0,
    Alias = 1,
    Fuzzy = 2,
    Create = 3,
}

impl ResolverOutcome {
    fn idx(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        RESOLVER_OUTCOME_LABELS[self.idx()]
    }
}

/// Item kind for [`ExtractorMetrics::add_items_written`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractorItemKind {
    Entity = 0,
    Statement = 1,
    Relation = 2,
    Mention = 3,
}

impl ExtractorItemKind {
    fn idx(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        ITEM_KIND_LABELS[self.idx()]
    }
}

/// Tier-status pair for [`ExtractorMetrics::inc_tier_run`]. The byte
/// values match `brain_metadata::tables::extractor_audit::tier_status`
/// so the worker can pass through the same enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierKind {
    Pattern = 0,
    Classifier = 1,
    Llm = 2,
}

impl TierKind {
    fn idx(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        TIER_LABELS[self.idx()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierStatus {
    Ran = 0,
    Skipped = 1,
    Failed = 2,
}

impl TierStatus {
    fn idx(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        TIER_STATUS_LABELS[self.idx()]
    }
}

/// Metric family for `ExtractorWorker`. Same shared-by-Arc pattern as
/// [`AutoEdgeMetrics`].
///
/// `schema_filtered_total` tracks per-predicate label cardinality via
/// a `Mutex<HashMap>` because predicate qnames are deployment-shaped
/// (low cardinality in practice but unbounded in theory). The
/// exposition layer reads the snapshot under a short-lived lock.
#[derive(Debug)]
pub struct ExtractorMetrics {
    drops_total: AtomicU64,
    schema_filtered_total: parking_lot::Mutex<std::collections::HashMap<String, u64>>,
    /// Indexed by [`ExtractorItemKind`].
    items_written_total: Vec<AtomicU64>,
    llm_micro_usd_spent_total: AtomicU64,
    cycle_duration_seconds: WorkerHistogram,
    /// `tier_idx * 3 + status_idx`.
    tier_runs_total: Vec<AtomicU64>,
    /// Indexed by [`ResolverOutcome`].
    resolver_outcome_total: Vec<AtomicU64>,
}

impl ExtractorMetrics {
    /// Construct a zeroed instance.
    #[must_use]
    pub fn new() -> Self {
        let items_written_total = (0..ITEM_KIND_LABELS.len())
            .map(|_| AtomicU64::new(0))
            .collect();
        let tier_runs_total = (0..TIER_LABELS.len() * TIER_STATUS_LABELS.len())
            .map(|_| AtomicU64::new(0))
            .collect();
        let resolver_outcome_total = (0..RESOLVER_OUTCOME_LABELS.len())
            .map(|_| AtomicU64::new(0))
            .collect();
        Self {
            drops_total: AtomicU64::new(0),
            schema_filtered_total: parking_lot::Mutex::new(std::collections::HashMap::new()),
            items_written_total,
            llm_micro_usd_spent_total: AtomicU64::new(0),
            cycle_duration_seconds: WorkerHistogram::new(DEFAULT_CYCLE_BUCKETS_SECONDS),
            tier_runs_total,
            resolver_outcome_total,
        }
    }

    /// Bumped by the writer when the bounded extractor channel is
    /// full and the encode-side enqueue is dropped.
    pub fn inc_drop(&self) {
        self.drops_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Bumped by the worker when a predicate or relation-type qname
    /// fails the active-schema admission check.
    pub fn inc_schema_filtered(&self, predicate_qname: &str) {
        let mut guard = self.schema_filtered_total.lock();
        *guard.entry(predicate_qname.to_string()).or_insert(0) += 1;
    }

    /// Bumped by the worker per successfully-written item, by kind.
    pub fn add_items_written(&self, kind: ExtractorItemKind, n: u64) {
        self.items_written_total[kind.idx()].fetch_add(n, Ordering::Relaxed);
    }

    /// Bumped by the worker when the LLM extractor reports cost (in
    /// dollar-micro-units, 1e-6 USD).
    pub fn add_llm_micro_usd(&self, n: u64) {
        self.llm_micro_usd_spent_total
            .fetch_add(n, Ordering::Relaxed);
    }

    /// Observed once per cycle (wall-clock).
    pub fn observe_cycle_duration(&self, seconds: f64) {
        self.cycle_duration_seconds.observe(seconds);
    }

    /// Bumped once per tier per processed memory with the tier's
    /// outcome status.
    pub fn inc_tier_run(&self, tier: TierKind, status: TierStatus) {
        let idx = tier.idx() * TIER_STATUS_LABELS.len() + status.idx();
        self.tier_runs_total[idx].fetch_add(1, Ordering::Relaxed);
    }

    /// Bumped once per resolved entity mention with the resolver
    /// outcome.
    pub fn inc_resolver_outcome(&self, outcome: ResolverOutcome) {
        self.resolver_outcome_total[outcome.idx()].fetch_add(1, Ordering::Relaxed);
    }

    /// Read-only snapshot for `/metrics`.
    #[must_use]
    pub fn snapshot(&self) -> ExtractorMetricsSnapshot {
        let schema_filtered_total = self.schema_filtered_total.lock().clone();
        let items_written_total = self
            .items_written_total
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();
        let tier_runs_total = self
            .tier_runs_total
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();
        let resolver_outcome_total = self
            .resolver_outcome_total
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();
        ExtractorMetricsSnapshot {
            drops_total: self.drops_total.load(Ordering::Relaxed),
            schema_filtered_total,
            items_written_total,
            llm_micro_usd_spent_total: self.llm_micro_usd_spent_total.load(Ordering::Relaxed),
            cycle_duration_seconds: self.cycle_duration_seconds.snapshot(),
            tier_runs_total,
            resolver_outcome_total,
        }
    }
}

impl Default for ExtractorMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-data snapshot of [`ExtractorMetrics`].
#[derive(Debug, Clone)]
pub struct ExtractorMetricsSnapshot {
    pub drops_total: u64,
    pub schema_filtered_total: std::collections::HashMap<String, u64>,
    /// Indexed in the same order as [`ITEM_KIND_LABELS`].
    pub items_written_total: Vec<u64>,
    pub llm_micro_usd_spent_total: u64,
    pub cycle_duration_seconds: WorkerHistogramSnapshot,
    /// `tier_idx * 3 + status_idx`. Iterate via [`TIER_LABELS`] and
    /// [`TIER_STATUS_LABELS`] for label ordering.
    pub tier_runs_total: Vec<u64>,
    /// Indexed in the same order as [`RESOLVER_OUTCOME_LABELS`].
    pub resolver_outcome_total: Vec<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_edge_counters_start_at_zero() {
        let m = AutoEdgeMetrics::new();
        let s = m.snapshot();
        assert_eq!(s.drops_total, 0);
        assert_eq!(s.edges_written_total, 0);
        assert_eq!(s.cycle_duration_seconds.count, 0);
        assert_eq!(s.neighbours_found_per_cycle.count, 0);
    }

    #[test]
    fn auto_edge_increments_round_trip() {
        let m = AutoEdgeMetrics::new();
        m.inc_drop();
        m.inc_drop();
        m.add_edges_written(5);
        m.observe_cycle_duration(0.003);
        m.observe_neighbours_found(7);
        let s = m.snapshot();
        assert_eq!(s.drops_total, 2);
        assert_eq!(s.edges_written_total, 5);
        assert_eq!(s.cycle_duration_seconds.count, 1);
        assert!((s.cycle_duration_seconds.sum - 0.003).abs() < 1e-6);
        assert_eq!(s.neighbours_found_per_cycle.count, 1);
        assert!((s.neighbours_found_per_cycle.sum - 7.0).abs() < 1e-6);
    }

    #[test]
    fn extractor_counters_round_trip() {
        let m = ExtractorMetrics::new();
        m.inc_drop();
        m.inc_schema_filtered("acme:knows");
        m.inc_schema_filtered("acme:knows");
        m.inc_schema_filtered("acme:works_at");
        m.add_items_written(ExtractorItemKind::Entity, 3);
        m.add_items_written(ExtractorItemKind::Mention, 3);
        m.add_items_written(ExtractorItemKind::Statement, 2);
        m.add_llm_micro_usd(12_000);
        m.observe_cycle_duration(0.21);
        m.inc_tier_run(TierKind::Pattern, TierStatus::Ran);
        m.inc_tier_run(TierKind::Llm, TierStatus::Skipped);
        m.inc_resolver_outcome(ResolverOutcome::Exact);
        m.inc_resolver_outcome(ResolverOutcome::Create);
        let s = m.snapshot();
        assert_eq!(s.drops_total, 1);
        assert_eq!(s.schema_filtered_total.get("acme:knows"), Some(&2));
        assert_eq!(s.schema_filtered_total.get("acme:works_at"), Some(&1));
        assert_eq!(s.items_written_total[ExtractorItemKind::Entity as usize], 3);
        assert_eq!(
            s.items_written_total[ExtractorItemKind::Mention as usize],
            3
        );
        assert_eq!(
            s.items_written_total[ExtractorItemKind::Statement as usize],
            2
        );
        assert_eq!(
            s.items_written_total[ExtractorItemKind::Relation as usize],
            0
        );
        assert_eq!(s.llm_micro_usd_spent_total, 12_000);
        assert_eq!(s.cycle_duration_seconds.count, 1);
        let pattern_ran_idx =
            TierKind::Pattern as usize * TIER_STATUS_LABELS.len() + TierStatus::Ran as usize;
        let llm_skipped_idx =
            TierKind::Llm as usize * TIER_STATUS_LABELS.len() + TierStatus::Skipped as usize;
        assert_eq!(s.tier_runs_total[pattern_ran_idx], 1);
        assert_eq!(s.tier_runs_total[llm_skipped_idx], 1);
        assert_eq!(s.resolver_outcome_total[ResolverOutcome::Exact as usize], 1);
        assert_eq!(
            s.resolver_outcome_total[ResolverOutcome::Create as usize],
            1
        );
    }

    #[test]
    fn histogram_overflow_lands_in_inf_bucket() {
        let h = WorkerHistogram::new(DEFAULT_CYCLE_BUCKETS_SECONDS);
        h.observe(100.0);
        let s = h.snapshot();
        assert_eq!(s.count, 1);
        assert_eq!(s.buckets.last().unwrap().cumulative_count, 1);
        assert!(s.buckets.last().unwrap().le.is_none());
    }
}
