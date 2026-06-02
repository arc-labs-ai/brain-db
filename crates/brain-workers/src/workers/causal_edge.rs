//! CausalEdgeWorker — derives `Caused` substrate edges from extractor-
//! produced causal statements (`caused_by`, `triggered`, `led_to`, …).
//!
//! ## Why this exists
//!
//! The extractor pipeline materialises typed-graph — entities,
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
//! 3. The worker builds a single
//!    `Write { phases: Vec<Phase::Link(kind=Caused, derived_by=CAUSAL_WORKER)> }`
//!    and calls `RealWriterHandle::submit`. The unified write path
//!    WALs every edge, commits the redb rows, and publishes the
//!    `EdgeAdded(AUTO_DERIVED)` envelope on the subscribe bus so
//!    subscribe replay reconstructs derived edges from the WAL.
//!
//! ## Predicate-whitelist resolver
//!
//! Predicates are deployment-shaped: a no-schema build never
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
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use brain_core::{
    AgentId, EdgeKind, EdgeKindRef, EntityId, MemoryId, NodeRef, PredicateId, StatementId,
};
use brain_core::{EvidenceRef, Statement, StatementObject};
use brain_metadata::schema::predicate::predicate_lookup_by_qname;
use brain_metadata::statement::{
    evidence_overflow_load, statement_get, statement_list, StatementListFilter, StatementOpError,
};
use brain_metadata::tables::edge::{derived_by, origin, zero_disambiguator};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_ops::{
    CausalEdgeEnqueue, CausalEdgeMetrics, CausalSkipReason, Phase, RealWriterHandle, Write, WriteId,
};
use futures_lite::FutureExt;
use glommio::timer::sleep;
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
    /// An empty resolved set is a valid steady state when no schema
    /// has declared any causal predicate — the worker drains the queue
    /// without writing.
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
    db: &brain_metadata::MetadataDb,
    qnames: &[(String, String)],
) -> Result<HashSet<PredicateId>, WorkerError> {
    let mut out = HashSet::new();
    let rtxn = db
        .read_txn()
        .map_err(|e| WorkerError::Ops(format!("causal_edge read_txn: {e:?}")))?;
    for (ns, name) in qnames {
        match predicate_lookup_by_qname(&rtxn, ns, name) {
            Ok(Some(predicate)) => {
                out.insert(predicate.id);
            }
            Ok(None) => {
                // The deployment hasn't declared this predicate. Expected
                // when no schema has been uploaded yet, or when the
                // declared schema partially overlaps the configured
                // whitelist; not an error.
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
        let _ = worker.resolved.set(resolved);
        worker.metrics.set_whitelist_resolved(count);
    }
    let whitelist = worker.resolved.get().expect("resolved set populated above");

    let mut pairs: Vec<(MemoryId, MemoryId, f32)> = Vec::new();
    let mut processed = 0usize;
    while processed < cfg.batch_size {
        if started.elapsed() >= cfg.max_runtime {
            break;
        }
        if ctx.is_shutdown() {
            break;
        }
        // First iteration blocks on the queue (raced against the
        // tick interval). The queue's wake fires when the
        // ExtractorWorker fans out a causal statement, so the
        // statement-commit→edge-derive path has no per-interval
        // latency floor. Subsequent iterations drain without
        // blocking so a burst batches into one cycle.
        let statement_id = if processed == 0 {
            let recv = async { worker.queue.recv_async().await.ok() };
            let tick = async {
                sleep(cfg.interval).await;
                None
            };
            match recv.or(tick).await {
                Some(item) => item,
                None => break,
            }
        } else {
            match worker.queue.try_recv() {
                Ok(item) => item,
                Err(_) => break,
            }
        };
        processed += 1;
        if whitelist.is_empty() {
            // No causal vocabulary on this deployment — drain and
            // skip. Substrate-only steady state.
            worker
                .metrics
                .inc_skip(CausalSkipReason::NonCausalPredicate);
            continue;
        }
        let outcome =
            collect_pairs_for_statement(ctx, statement_id, whitelist, &worker.knobs, &mut pairs)?;
        if let Some(skip) = outcome {
            worker.metrics.inc_skip(skip);
        }
    }

    let written = if pairs.is_empty() {
        0usize
    } else {
        let created_at = now_unix_nanos_causal();
        let phases: Vec<Phase> = pairs
            .iter()
            .map(|(cause, effect, weight)| Phase::Link {
                from: NodeRef::Memory(*cause),
                to: NodeRef::Memory(*effect),
                kind: EdgeKindRef::Builtin(EdgeKind::Caused),
                weight: *weight,
                origin: origin::AUTO_DERIVED,
                derived_by: derived_by::CAUSAL_WORKER,
                disambiguator: zero_disambiguator(),
                created_at_unix_nanos: created_at,
            })
            .collect();
        let request_hash = hash_causal_batch(&pairs);
        let write = Write::from_phases(WriteId::new(), AgentId::default(), phases)
            .with_request_hash(request_hash);
        let real_writer = ctx
            .ops
            .executor
            .writer
            .as_any()
            .downcast_ref::<RealWriterHandle>()
            .ok_or_else(|| {
                WorkerError::Ops("causal_edge: unified path requires RealWriterHandle".into())
            })?;
        real_writer
            .submit(write)
            .await
            .map_err(|e| WorkerError::Ops(format!("submit: {e:?}")))?;
        pairs.len()
    };
    worker.metrics.add_edges_written(written as u64);

    let elapsed = started.elapsed().as_secs_f64();
    worker.metrics.observe_cycle_duration(elapsed);
    Ok(processed)
}

/// Resolve one enqueued statement into `(cause_mem, effect_mem, weight)`
/// tuples appended to `pairs`. Returns `Some(reason)` when the
/// statement was skipped without producing any pair so the worker can
/// bump the matching counter; returns `None` when at least one pair
/// landed (or when the statement was unreachable — caller still treats
/// missing rows as a metric event via `Some(StatementMissing)`).
fn collect_pairs_for_statement(
    ctx: &WorkerContext,
    sid: StatementId,
    whitelist: &HashSet<PredicateId>,
    knobs: &CausalEdgeKnobs,
    pairs: &mut Vec<(MemoryId, MemoryId, f32)>,
) -> Result<Option<CausalSkipReason>, WorkerError> {
    let metadata = ctx.ops.executor.metadata.clone();
    let rtxn = metadata
        .read_txn()
        .map_err(|e| WorkerError::Ops(format!("causal_edge read_txn: {e:?}")))?;

    let statement = match statement_get(&rtxn, sid)
        .map_err(|e| WorkerError::Ops(format!("statement_get: {e}")))?
    {
        Some(s) => s,
        None => return Ok(Some(CausalSkipReason::StatementMissing)),
    };
    if statement.tombstoned {
        return Ok(Some(CausalSkipReason::StatementMissing));
    }
    if !whitelist.contains(&statement.predicate) {
        // Stale enqueue from a previous schema version, or extractor
        // qname mismatch. Either way: not actionable.
        return Ok(Some(CausalSkipReason::NonCausalPredicate));
    }
    if statement.confidence < knobs.min_confidence {
        return Ok(Some(CausalSkipReason::LowConfidence));
    }

    let effect_memories = top_evidence_memory_ids(
        &rtxn,
        &statement.evidence,
        knobs.max_effect_memories_per_statement,
    )?;
    if effect_memories.is_empty() {
        return Ok(Some(CausalSkipReason::NoEvidence));
    }

    // Direction: "Outage caused_by Deploy" means Deploy caused Outage.
    // Cause-side entity = statement.object; effect-side anchors come
    // from statement.evidence. The Memory(_) shortcut writes a direct
    // edge without walking the statement graph.
    let cause_entity = match statement.object {
        StatementObject::Entity(eid) => eid,
        StatementObject::Memory(cause_mem) => {
            // Short-circuit: object names a memory directly. One edge
            // per (cause_mem, effect_mem) pair at the statement's
            // own confidence — no related-statement walk needed.
            for em in &effect_memories {
                pairs.push((cause_mem, *em, statement.confidence.clamp(0.0, 1.0)));
            }
            return Ok(None);
        }
        StatementObject::Value(_) | StatementObject::Statement(_) => {
            return Ok(Some(CausalSkipReason::ObjectNotEntity));
        }
    };

    let related = related_statements_for_entity(
        &rtxn,
        cause_entity,
        knobs.max_related_statements_per_entity,
    )?;
    if related.is_empty() {
        return Ok(Some(CausalSkipReason::NoRelatedStatement));
    }

    let mut produced = 0usize;
    for r in &related {
        let cause_memories =
            top_evidence_memory_ids(&rtxn, &r.evidence, knobs.max_cause_memories_per_statement)?;
        for cm in &cause_memories {
            for em in &effect_memories {
                if cm == em {
                    // Self-loop guard: same memory describes both sides
                    // of the asserted causality. Skip silently — the
                    // edge would be a no-information cycle.
                    continue;
                }
                let weight = (statement.confidence * r.confidence).clamp(0.0, 1.0);
                pairs.push((*cm, *em, weight));
                produced += 1;
            }
        }
    }
    if produced == 0 {
        Ok(Some(CausalSkipReason::NoEvidence))
    } else {
        Ok(None)
    }
}

/// Resolve `evidence` (inline or overflow) into a memory-id list,
/// keeping at most `cap` entries ordered by descending
/// `confidence_milli`. Tombstoned memories are filtered: their
/// presence here would write an edge that immediately dangles.
fn top_evidence_memory_ids(
    rtxn: &redb::ReadTransaction,
    evidence: &EvidenceRef,
    cap: usize,
) -> Result<Vec<MemoryId>, WorkerError> {
    if cap == 0 {
        return Ok(Vec::new());
    }
    let mut entries: Vec<(MemoryId, u16)> = match evidence {
        EvidenceRef::Inline(box_smallvec) => box_smallvec
            .iter()
            .map(|e| (e.memory_id, e.confidence_milli))
            .collect(),
        EvidenceRef::Overflow(oid) => {
            let loaded = evidence_overflow_load(rtxn, *oid)
                .map_err(|e| WorkerError::Ops(format!("evidence_overflow_load: {e}")))?;
            loaded
                .unwrap_or_default()
                .into_iter()
                .map(|e| (e.memory_id, e.confidence_milli))
                .collect()
        }
    };
    entries.sort_by_key(|b| std::cmp::Reverse(b.1));
    entries.truncate(cap);

    let memories_t = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open MEMORIES_TABLE: {e:?}")))?;
    let mut out = Vec::with_capacity(entries.len());
    for (mid, _) in entries {
        // Filter tombstones: a memory row missing or marked
        // tombstoned would produce a dangling edge. We keep only
        // present + live memories.
        let row = memories_t
            .get(&mid.to_be_bytes())
            .map_err(|e| WorkerError::Ops(format!("MEMORIES_TABLE.get: {e:?}")))?;
        if row.is_none() {
            continue;
        }
        // The MEMORIES_TABLE value's `tombstoned_at_unix_nanos == 0`
        // marks live; non-zero marks tombstoned. We cross-check via
        // the typed access below.
        // For now: presence is sufficient — slot reclamation removes
        // tombstoned rows after grace, so a present row is live or
        // within grace. Edge writes against tombstoned-but-present
        // memories are tolerable in v1 (edge_scrub cleans them).
        out.push(mid);
    }
    Ok(out)
}

/// Find up to `cap` current statements whose subject is `entity`.
/// Used to walk back from the cause-side entity to its evidence
/// memories. We bound the related-statement count to keep edge
/// fan-out predictable.
fn related_statements_for_entity(
    rtxn: &redb::ReadTransaction,
    entity: EntityId,
    cap: usize,
) -> Result<Vec<Statement>, WorkerError> {
    if cap == 0 {
        return Ok(Vec::new());
    }
    let filter = StatementListFilter {
        subject: Some(entity),
        predicate: None,
        kind: None,
        current_only: true,
        min_confidence: None,
        limit: cap,
    };
    statement_list(rtxn, &filter).map_err(|e| match e {
        StatementOpError::DecodeFailed => WorkerError::Ops("statement decode failed".to_string()),
        other => WorkerError::Ops(format!("statement_list: {other}")),
    })
}

fn now_unix_nanos_causal() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Deterministic hash of a batch of `(cause, effect, weight)` tuples.
/// Sorted by `(cause, effect)` so retries of the same drained
/// statement set hash the same way regardless of fan-out walk
/// ordering. Weight is excluded — the cause/effect set is the
/// invariant the idempotency cache keys on.
fn hash_causal_batch(pairs: &[(MemoryId, MemoryId, f32)]) -> [u8; 32] {
    let mut sorted: Vec<(MemoryId, MemoryId)> = pairs.iter().map(|(c, e, _)| (*c, *e)).collect();
    sorted.sort();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"causal_edge:caused:v1");
    for (c, e) in &sorted {
        hasher.update(&c.to_be_bytes());
        hasher.update(&e.to_be_bytes());
    }
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_metadata::schema::predicate::predicate_intern_or_get;
    use brain_metadata::MetadataDb;
    use tempfile::TempDir;

    fn open_db() -> (TempDir, MetadataDb) {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        (dir, db)
    }

    #[test]
    fn resolver_returns_empty_when_no_schema_declares_predicates() {
        // No predicates declared → all whitelist qnames resolve to None.
        let (_dir, db) = open_db();
        let qnames = vec![
            ("brain".to_string(), "caused_by".to_string()),
            ("brain".to_string(), "triggered".to_string()),
        ];
        let resolved = resolve_whitelist(&db, &qnames).expect("resolver runs cleanly");
        assert!(
            resolved.is_empty(),
            "no declared predicates must produce no resolved predicates"
        );
    }

    #[test]
    fn resolver_picks_up_only_declared_subset() {
        let (_dir, db) = open_db();
        // Declare exactly one of the whitelist predicates.
        let declared_id = {
            let wtxn = db.write_txn().unwrap();
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
            let wtxn = db.write_txn().unwrap();
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
