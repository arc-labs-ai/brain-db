//! `ExtractorWorker` metric family.

use std::sync::atomic::{AtomicU64, Ordering};

use super::histograms::{
    WorkerHistogram, WorkerHistogramSnapshot, DEFAULT_CYCLE_BUCKETS_SECONDS, ITEM_KIND_LABELS,
    RESOLVER_OUTCOME_LABELS, TIER_LABELS, TIER_STATUS_LABELS,
};

/// Bucket bounds for the LLM-tier per-call input-token histogram.
/// Covers the no-context fast path (~200 tokens) through the
/// truncate-to-budget ceiling (~4000 tokens) plus a 10k bucket for
/// pre-truncation prompts that operators may want to flag.
pub const LLM_TOKENS_PER_QUERY_BUCKETS: &[f64] = &[
    50.0, 100.0, 250.0, 500.0, 1000.0, 2000.0, 3000.0, 4000.0, 6000.0, 10000.0,
];

/// Bucket bounds for the bounded-context neighbor-count histogram.
/// Bounded by the W2.3 default top_m (10); a 20-bucket caps off any
/// operator override.
pub const LLM_NEIGHBORS_INCLUDED_BUCKETS: &[f64] =
    &[0.0, 1.0, 2.0, 3.0, 5.0, 7.0, 10.0, 15.0, 20.0];

/// Bucket bounds for the context-fetch wall-clock histogram. Targets
/// the 1-50 ms operating range (HNSW search + a few redb point
/// lookups); the 1-second cap catches pathological cases.
pub const LLM_CONTEXT_FETCH_BUCKETS_SECONDS: &[f64] =
    &[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0];

/// Resolver outcome the worker reports per resolved `EntityMention`.
///
/// - `exact` / `alias` / `fuzzy` / `embedding` correspond to the four
///   lookup tiers in `brain-extractors::resolver` (Exact-canonical,
///   Alias, Trigram-fuzzy, Embedding-HNSW).
/// - `disambiguated` is the post-embedding step where an
///   [`EntityDisambiguator`](brain_extractors::resolver::EntityDisambiguator)
///   confirmed an embedding partial-match as the same entity. Split
///   from `embedding` so dashboards can show how often the
///   disambiguator earns its cost.
/// - `create` is the tier-5 fall-through that minted a fresh entity.
///
/// Discriminants are append-only — they index counter slots that
/// observability systems read by position. Reordering is a breaking
/// metric change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolverOutcome {
    Exact = 0,
    Alias = 1,
    Fuzzy = 2,
    Embedding = 3,
    Disambiguated = 4,
    Create = 5,
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
    /// Hypothetical-question vectors written by the HyPE generator. Folded
    /// into `items_written_total` so the eval's extraction-drain barrier
    /// (which waits for that counter to plateau) also waits for HyPE
    /// generation to finish before querying.
    HyPe = 4,
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
/// [`super::auto_edge::AutoEdgeMetrics`].
///
/// `schema_filtered_total` tracks per-predicate label cardinality via
/// a `Mutex<HashMap>` because predicate qnames are deployment-shaped
/// (low cardinality in practice but unbounded in theory). The
/// exposition layer reads the snapshot under a short-lived lock.
#[derive(Debug)]
pub struct ExtractorMetrics {
    drops_total: AtomicU64,
    schema_filtered_total: parking_lot::Mutex<std::collections::HashMap<String, u64>>,
    /// Real extracted items the apply pass could not persist, keyed by a
    /// low-cardinality `reason` (e.g. `subject_unresolved`,
    /// `predicate_invalid`, `create_rejected`). A memory database must never
    /// lose signal silently — any nonzero value here is a fact that did not
    /// land, surfaced for alerting rather than buried in a trace log.
    apply_dropped_total: parking_lot::Mutex<std::collections::HashMap<String, u64>>,
    /// Indexed by [`ExtractorItemKind`].
    items_written_total: Vec<AtomicU64>,
    llm_micro_usd_spent_total: AtomicU64,
    cycle_duration_seconds: WorkerHistogram,
    /// `tier_idx * 3 + status_idx`.
    tier_runs_total: Vec<AtomicU64>,
    /// Indexed by [`ResolverOutcome`].
    resolver_outcome_total: Vec<AtomicU64>,
    /// Approximate input tokens per LLM call (post-truncation).
    /// Operators tune the neighbor budget by watching this histogram
    /// — a p99 pushed against the 4000-token cap means the prompt
    /// builder is actively trimming neighbors on most calls.
    llm_tokens_per_query: WorkerHistogram,
    /// Neighbor entries that survived budget truncation. A p99 well
    /// below the configured `top_m` means the budget is firing
    /// often; raising the budget or dropping `top_m` is in order.
    llm_neighbors_included: WorkerHistogram,
    /// Wall-clock for `fetch_extractor_context`. Doesn't include the
    /// LLM call itself — strictly the HNSW search + redb reads that
    /// build the prompt context bundle.
    llm_context_fetch_duration_seconds: WorkerHistogram,
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
            apply_dropped_total: parking_lot::Mutex::new(std::collections::HashMap::new()),
            items_written_total,
            llm_micro_usd_spent_total: AtomicU64::new(0),
            cycle_duration_seconds: WorkerHistogram::new(DEFAULT_CYCLE_BUCKETS_SECONDS),
            tier_runs_total,
            resolver_outcome_total,
            llm_tokens_per_query: WorkerHistogram::new(LLM_TOKENS_PER_QUERY_BUCKETS),
            llm_neighbors_included: WorkerHistogram::new(LLM_NEIGHBORS_INCLUDED_BUCKETS),
            llm_context_fetch_duration_seconds: WorkerHistogram::new(
                LLM_CONTEXT_FETCH_BUCKETS_SECONDS,
            ),
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

    /// Bumped by the apply pass when a genuine extracted item (entity,
    /// statement, or relation) could not be persisted, keyed by `reason`.
    /// Distinct from `schema_filtered` (a deliberate admission decision) —
    /// this is unintended signal loss the operator should see.
    pub fn inc_apply_dropped(&self, reason: &str) {
        let mut guard = self.apply_dropped_total.lock();
        *guard.entry(reason.to_string()).or_insert(0) += 1;
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

    /// Observed once per LLM-tier dispatch — the approximate input
    /// tokens after prompt-budget truncation. Lets operators see the
    /// distribution of prompt sizes the bounded-context path
    /// actually delivers.
    pub fn observe_llm_tokens_per_query(&self, tokens: u64) {
        self.llm_tokens_per_query.observe(tokens as f64);
    }

    /// Observed once per LLM-tier dispatch — the number of neighbor
    /// entries that survived budget truncation. Distinct from "how
    /// many neighbors did we fetch" because budget enforcement can
    /// drop the lowest-similarity ones to fit the token cap.
    pub fn observe_llm_neighbors_included(&self, count: usize) {
        self.llm_neighbors_included.observe(count as f64);
    }

    /// Observed once per `fetch_extractor_context` call (success or
    /// failure). Excludes the LLM call itself — this is just the
    /// HNSW + redb work that assembles the prompt context.
    pub fn observe_context_fetch_duration(&self, seconds: f64) {
        self.llm_context_fetch_duration_seconds.observe(seconds);
    }

    /// Read-only snapshot for `/metrics`.
    #[must_use]
    pub fn snapshot(&self) -> ExtractorMetricsSnapshot {
        let schema_filtered_total = self.schema_filtered_total.lock().clone();
        let apply_dropped_total = self.apply_dropped_total.lock().clone();
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
            apply_dropped_total,
            items_written_total,
            llm_micro_usd_spent_total: self.llm_micro_usd_spent_total.load(Ordering::Relaxed),
            cycle_duration_seconds: self.cycle_duration_seconds.snapshot(),
            tier_runs_total,
            resolver_outcome_total,
            llm_tokens_per_query: self.llm_tokens_per_query.snapshot(),
            llm_neighbors_included: self.llm_neighbors_included.snapshot(),
            llm_context_fetch_duration_seconds: self.llm_context_fetch_duration_seconds.snapshot(),
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
    /// Apply-pass signal loss keyed by `reason`. Any nonzero entry is a real
    /// extracted item that did not persist.
    pub apply_dropped_total: std::collections::HashMap<String, u64>,
    /// Indexed in the same order as [`ITEM_KIND_LABELS`].
    pub items_written_total: Vec<u64>,
    pub llm_micro_usd_spent_total: u64,
    pub cycle_duration_seconds: WorkerHistogramSnapshot,
    /// `tier_idx * 3 + status_idx`. Iterate via [`TIER_LABELS`] and
    /// [`TIER_STATUS_LABELS`] for label ordering.
    pub tier_runs_total: Vec<u64>,
    /// Indexed in the same order as [`RESOLVER_OUTCOME_LABELS`].
    pub resolver_outcome_total: Vec<u64>,
    pub llm_tokens_per_query: WorkerHistogramSnapshot,
    pub llm_neighbors_included: WorkerHistogramSnapshot,
    pub llm_context_fetch_duration_seconds: WorkerHistogramSnapshot,
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
