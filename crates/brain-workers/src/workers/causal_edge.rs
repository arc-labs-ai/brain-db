//! CausalEdgeWorker — derives `Caused` substrate edges from extractor-
//! produced causal statements (`caused_by`, `triggered`, `led_to`, …).
//!
//! ## Why this exists
//!
//! The extractor pipeline materialises typed knowledge — entities,
//! statements, relations — but the substrate's planner walks edges
//! between *memories*, not statements. Without a projection step, the
//! cognitive surface ("recall everything caused by deploy X") can't
//! traverse causal chains directly. This worker is that projection:
//! every new causal statement produces one or more memory→memory
//! `Caused` edges so RECALL --include-edges / --include-graph surface
//! the structure the extractor uncovered.
//!
//! ## Flow
//!
//! 1. The ExtractorWorker, after committing a causal statement
//!    (predicate in the configured whitelist, confidence ≥ floor),
//!    pushes the `StatementId` onto a per-shard `flume::Sender`.
//!    Non-blocking; full channel drops with a counter bump. The
//!    extractor's own commit never depends on the worker.
//! 2. This worker drains the receiver each `interval_ms`. Per
//!    statement: fetch the row, walk the evidence (effect side),
//!    walk `STATEMENTS_BY_SUBJECT` keyed on the object entity to find
//!    cause-side statements, intersect their evidence (cause side),
//!    cap fan-out, and build (cause_mem, effect_mem, weight) tuples.
//! 3. `OpsContext::write_causal_edges` persists the edges +
//!    publishes `EdgeAdded(AUTO_DERIVED, kind=Caused)` events.
//!
//! ## Predicate-whitelist resolver
//!
//! Predicates are deployment-shaped: a substrate-only build never
//! declares `brain:caused_by`, so the worker must gracefully no-op on
//! deployments without a causal vocabulary. On first cycle the worker
//! opens a read txn and runs `predicate_lookup_by_qname` for each
//! configured qname; the resolved set is cached in a `OnceLock<…>` so
//! subsequent cycles skip the lookup. An empty resolved set means the
//! worker drains the queue and skips every entry with
//! `CausalSkipReason::NonCausalPredicate`.
//!
//! ## What's *not* in scope
//!
//! - LLM-judge causal inference. The worker only fires on
//!   extractor-asserted causal statements; the LLM-judge path is a v2
//!   item.
//! - Multi-hop causal closure. If A caused B and B caused C, this
//!   worker does not auto-derive A→C. That's a REASON-verb concern,
//!   not a write-time materialisation.
//! - Supersession-driven retraction. When a causal statement is
//!   superseded the original edge persists. Tracked as a known v1
//!   limitation; edge_scrub can be extended later.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

use brain_core::PredicateId;
use brain_metadata::predicate_ops::predicate_lookup_by_qname;
use brain_ops::{CausalEdgeEnqueue, CausalEdgeMetrics};
use tracing::{trace, warn};

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Knobs that don't fit `WorkerConfig`'s generic shape. Defaults match
/// `plans/causal-edge-worker-impl.md`.
#[derive(Clone, Debug)]
pub struct CausalEdgeKnobs {
    /// Predicate qnames whose presence triggers causal-edge derivation.
    /// Each entry is a `(namespace, name)` pair. Substrate-only
    /// deployments leave this empty and the worker no-ops by design.
    pub whitelist_qnames: Vec<(String, String)>,
    /// Minimum statement confidence. Below this, no edge — causal
    /// inference at low confidence produces more noise than signal.
    pub min_confidence: f32,
    /// Per-statement cap on effect-side memories. Effect memories come
    /// from the statement's own evidence; tighter caps keep the edge
    /// table bounded when an extractor over-cites.
    pub max_effect_memories_per_statement: usize,
    /// Per-related-statement cap on cause-side memories.
    pub max_cause_memories_per_statement: usize,
    /// Cap on related statements walked back from the object entity.
    /// Net per causal statement: max_effect × max_cause × max_related
    /// edges. Default 3 × 3 × 5 = 45.
    pub max_related_statements_per_entity: usize,
}

pub const DEFAULT_MIN_CONFIDENCE: f32 = 0.6;
pub const DEFAULT_MAX_EFFECT_MEMORIES: usize = 3;
pub const DEFAULT_MAX_CAUSE_MEMORIES: usize = 3;
pub const DEFAULT_MAX_RELATED_STATEMENTS: usize = 5;

/// The starter whitelist. Operators who declare these predicates in
/// their schema get causal-edge inference for free. Brain ships these
/// as defaults because they're the predicate names extractors most
/// commonly emit for English causal phrasing.
pub const DEFAULT_WHITELIST_QNAMES: &[(&str, &str)] = &[
    ("brain", "caused_by"),
    ("brain", "triggered"),
    ("brain", "led_to"),
    ("brain", "resulted_in"),
    ("brain", "because_of"),
];

impl Default for CausalEdgeKnobs {
    fn default() -> Self {
        Self {
            whitelist_qnames: DEFAULT_WHITELIST_QNAMES
                .iter()
                .map(|(ns, name)| ((*ns).to_owned(), (*name).to_owned()))
                .collect(),
            min_confidence: DEFAULT_MIN_CONFIDENCE,
            max_effect_memories_per_statement: DEFAULT_MAX_EFFECT_MEMORIES,
            max_cause_memories_per_statement: DEFAULT_MAX_CAUSE_MEMORIES,
            max_related_statements_per_entity: DEFAULT_MAX_RELATED_STATEMENTS,
        }
    }
}

pub struct CausalEdgeWorker {
    config: WorkerConfig,
    knobs: CausalEdgeKnobs,
    queue: flume::Receiver<CausalEdgeEnqueue>,
    metrics: Arc<CausalEdgeMetrics>,
    /// Cached predicate-id set, resolved lazily on the first cycle.
    /// An empty resolved set is a valid steady state on substrate-only
    /// deployments — the worker drains the queue without writing.
    resolved: OnceLock<HashSet<PredicateId>>,
}

impl CausalEdgeWorker {
    #[must_use]
    pub fn new(queue: flume::Receiver<CausalEdgeEnqueue>) -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::CausalEdge),
            knobs: CausalEdgeKnobs::default(),
            queue,
            metrics: Arc::new(CausalEdgeMetrics::new()),
            resolved: OnceLock::new(),
        }
    }

    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<CausalEdgeMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    #[must_use]
    pub fn metrics(&self) -> Arc<CausalEdgeMetrics> {
        self.metrics.clone()
    }

    #[must_use]
    pub fn with_config(mut self, config: WorkerConfig) -> Self {
        self.config = config;
        self
    }

    #[must_use]
    pub fn with_knobs(mut self, knobs: CausalEdgeKnobs) -> Self {
        self.knobs = knobs;
        self
    }

    #[must_use]
    pub fn knobs(&self) -> CausalEdgeKnobs {
        self.knobs.clone()
    }
}

/// Resolve every configured qname against the active schema. Missing
/// qnames don't error — they're expected on deployments that didn't
/// declare a particular causal predicate. The function returns the set
/// of `PredicateId`s that actually resolved.
///
/// Kept separate from the worker struct so the resolver can be unit-
/// tested directly against a `MetadataDb` without spinning a worker
/// context.
pub fn resolve_whitelist(
    db: &parking_lot::Mutex<brain_metadata::MetadataDb>,
    qnames: &[(String, String)],
) -> Result<HashSet<PredicateId>, WorkerError> {
    let mut out = HashSet::new();
    let guard = db.lock();
    let rtxn = guard
        .read_txn()
        .map_err(|e| WorkerError::Ops(format!("causal_edge read_txn: {e:?}")))?;
    for (ns, name) in qnames {
        match predicate_lookup_by_qname(&rtxn, ns, name) {
            Ok(Some(predicate)) => {
                out.insert(predicate.id);
            }
            Ok(None) => {
                // The deployment hasn't declared this predicate. Expected
                // on substrate-only or partially-overlapping schemas;
                // not an error.
                trace!(
                    target: "brain_workers::causal_edge",
                    namespace = %ns,
                    name = %name,
                    "causal whitelist predicate not declared in this deployment; skipping",
                );
            }
            Err(e) => {
                // Malformed qname in config. Warn loudly so operators see
                // the typo on the first cycle; don't abort the worker
                // (other entries may resolve fine).
                warn!(
                    target: "brain_workers::causal_edge",
                    namespace = %ns,
                    name = %name,
                    error = %e,
                    "causal whitelist qname rejected by validator; skipping",
                );
            }
        }
    }
    Ok(out)
}

impl Worker for CausalEdgeWorker {
    fn name(&self) -> &'static str {
        WorkerKind::CausalEdge.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::CausalEdge
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_causal_edge_cycle(self, ctx))
    }
}

async fn do_causal_edge_cycle(
    worker: &CausalEdgeWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    let cfg = worker.config.clone();
    if cfg.batch_size == 0 {
        return Ok(0);
    }
    let started = Instant::now();

    // Lazy whitelist resolution. We can't run it at construction —
    // the worker is built before the metadata DB is wired up with the
    // schema's predicates. First cycle resolves once and caches.
    if worker.resolved.get().is_none() {
        let metadata = ctx.ops.executor.metadata.clone();
        let resolved = resolve_whitelist(&metadata, &worker.knobs.whitelist_qnames)?;
        let count = resolved.len() as u64;
        // `set` may lose a race; either copy is correct so we don't care.
        let _ = worker.resolved.set(resolved);
        worker.metrics.set_whitelist_resolved(count);
    }
    let whitelist = worker.resolved.get().expect("resolved set populated above");

    // Drain the queue but no-op until C3 lands the cause/effect walk.
    // The fast-exit on empty resolved set keeps substrate-only
    // deployments cheap: one queue.try_recv() per drained entry, no
    // metadata I/O.
    let mut processed = 0usize;
    while processed < cfg.batch_size {
        if started.elapsed() >= cfg.max_runtime {
            break;
        }
        if ctx.is_shutdown() {
            break;
        }
        let Ok(_statement_id) = worker.queue.try_recv() else {
            break;
        };
        processed += 1;
        if whitelist.is_empty() {
            // No causal vocabulary on this deployment — every entry is
            // a no-op skip. Tracked under NonCausalPredicate because
            // the writer enqueue filter wouldn't have fired on a real
            // build; in tests it's the easiest counter to assert.
            worker
                .metrics
                .inc_skip(brain_ops::CausalSkipReason::NonCausalPredicate);
            continue;
        }
        // C3 fills in the cause/effect walk + write_causal_edges call.
        // For C1 the worker is a structural no-op: it drains the queue
        // and reports skip telemetry, but doesn't write edges yet.
    }

    let elapsed = started.elapsed().as_secs_f64();
    worker.metrics.observe_cycle_duration(elapsed);
    Ok(processed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_metadata::predicate_ops::predicate_intern_or_get;
    use brain_metadata::MetadataDb;
    use tempfile::TempDir;

    fn open_db() -> (TempDir, parking_lot::Mutex<MetadataDb>) {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        (dir, parking_lot::Mutex::new(db))
    }

    #[test]
    fn resolver_returns_empty_for_substrate_only_deployment() {
        // No predicates declared → all whitelist qnames resolve to None.
        let (_dir, db) = open_db();
        let qnames = vec![
            ("brain".to_string(), "caused_by".to_string()),
            ("brain".to_string(), "triggered".to_string()),
        ];
        let resolved = resolve_whitelist(&db, &qnames).expect("resolver runs cleanly");
        assert!(
            resolved.is_empty(),
            "substrate-only must produce no resolved predicates"
        );
    }

    #[test]
    fn resolver_picks_up_only_declared_subset() {
        let (_dir, db) = open_db();
        // Declare exactly one of the whitelist predicates.
        let declared_id = {
            let mut guard = db.lock();
            let wtxn = guard.write_txn().unwrap();
            let id =
                predicate_intern_or_get(&wtxn, "brain", "caused_by", 1, 1_700_000_000_000).unwrap();
            wtxn.commit().unwrap();
            id
        };
        let qnames = vec![
            ("brain".to_string(), "caused_by".to_string()),
            ("brain".to_string(), "triggered".to_string()),
            ("brain".to_string(), "led_to".to_string()),
        ];
        let resolved = resolve_whitelist(&db, &qnames).expect("resolver runs cleanly");
        assert_eq!(
            resolved.len(),
            1,
            "exactly one declared predicate must resolve"
        );
        assert!(resolved.contains(&declared_id));
    }

    #[test]
    fn resolver_ignores_malformed_qnames_without_aborting() {
        let (_dir, db) = open_db();
        // The validator rejects empty names; we mix it with a valid
        // one and assert the valid one still resolves.
        let _good_id = {
            let mut guard = db.lock();
            let wtxn = guard.write_txn().unwrap();
            let id =
                predicate_intern_or_get(&wtxn, "brain", "led_to", 1, 1_700_000_000_000).unwrap();
            wtxn.commit().unwrap();
            id
        };
        let qnames = vec![
            ("brain".to_string(), "".to_string()), // malformed
            ("brain".to_string(), "led_to".to_string()),
        ];
        let resolved = resolve_whitelist(&db, &qnames).expect("resolver tolerates malformed");
        assert_eq!(resolved.len(), 1, "valid predicate still resolves");
    }

    #[test]
    fn default_whitelist_starts_at_five_brain_predicates() {
        let knobs = CausalEdgeKnobs::default();
        assert_eq!(
            knobs.whitelist_qnames.len(),
            DEFAULT_WHITELIST_QNAMES.len(),
            "default whitelist matches DEFAULT_WHITELIST_QNAMES"
        );
        for (ns, name) in &knobs.whitelist_qnames {
            assert_eq!(ns, "brain");
            assert!(!name.is_empty());
        }
    }
}
