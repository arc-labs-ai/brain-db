//! `ExtractorWorker` — drains the post-ENCODE extractor queue and
//! materialises entity / statement / relation / mention-edge rows
//! from the three-tier extractor framework's output.
//!
//! ## Why this exists
//!
//! Before this worker landed, ENCODE wrote a memory row and a vector
//! into HNSW and stopped there. The knowledge layer's entities,
//! statements, and relations only appeared when an operator hand-wrote
//! them through `ENTITY_CREATE` / `STATEMENT_CREATE` / `RELATION_CREATE`
//! wire ops. ExtractorWorker turns ENCODE into a knowledge-rich
//! operation: every encoded memory runs through pattern + classifier +
//! LLM tiers, items are resolved against the entity registry, and the
//! resulting graph rows are written transactionally.
//!
//! ## Flow per cycle
//!
//! 1. Drain up to `drain_per_cycle` `(memory_id, text)` pairs from the
//!    writer-fed channel.
//! 2. For each pair: probe the per-memory audit table; skip if already
//!    processed (queue-replay idempotency).
//! 3. Build a `brain_extractors::Memory` and an `ExtractionContext`.
//! 4. Run every enabled extractor in the registry.
//! 5. Merge the per-extractor outputs into one `Vec<ExtractedItem>`.
//! 6. Apply the merged result inside one redb write txn:
//!    - Resolve each `EntityMention` via the resolver gauntlet
//!      (exact / alias / trigram-fuzzy / create).
//!    - Write one `Mentions` edge per resolved entity
//!      (memory → entity, asymmetric).
//!    - Resolve each `StatementMention` / `RelationMention` against
//!      the in-cycle `surface → EntityId` map, intern the predicate /
//!      relation_type, and call the internal write helpers.
//! 7. Record an `ExtractorPipelineAuditEntry` and commit.
//!
//! ## Backpressure and failure
//!
//! - Full channel: writer drops the enqueue with a warn (encode never
//!   fails). Backfill is the recovery path (post-v1 admin op).
//! - Per-memory apply error: the worker logs at warn level and audits
//!   the memory as `PARTIAL_FAILURE` / `FAILURE` so a re-drain doesn't
//!   loop on the same memory.
//! - LLM tier unavailable: the registered LLM extractor returns
//!   `Failure(reason)` deterministically; pattern + classifier still run.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use brain_core::knowledge::{StatementKind, StatementObject, StatementValue};
use brain_core::{
    AgentId, ContextId, EntityId, ExtractorId, Memory as CoreMemory, MemoryId, MemoryKind, Salience,
};
use brain_extractors::{
    resolver::{resolve_or_create, ResolutionTier, ResolverError},
    EntityMention, ExtractedItem, ExtractionContext, ExtractionResult, ExtractionStatus, Extractor,
    ExtractorRegistry, StatementMention,
};
use brain_metadata::pipeline_has_extracted;
use brain_metadata::predicate_ops::predicate_intern_or_get;
use brain_metadata::relation_type_ops::relation_type_intern_or_get;
use brain_metadata::tables::edge::{
    self, derived_by, origin, zero_disambiguator, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE,
};
use brain_metadata::tables::extractor_audit::{
    pipeline_status, record_extracted, tier_status, ExtractorItemCounts,
    ExtractorPipelineAuditEntry,
};
use brain_metadata::tables::knowledge::predicate::{
    PredicateDefinition, SchemaOrigin, PREDICATES_TABLE,
};
use brain_metadata::tables::knowledge::relation_type::{
    RelationTypeDefinition, RelationTypeOrigin, RELATION_TYPES_TABLE,
};
use brain_metadata::tables::knowledge::schema_version::SCHEMA_ACTIVE_VERSIONS_TABLE;
use brain_ops::extractor_writes::{
    relation_create_internal, statement_create_internal, RelationCreatePayload,
    StatementCreatePayload,
};
use brain_ops::{
    ExtractorEnqueue, ExtractorItemKind, ExtractorMetrics, ResolverOutcome,
    TierKind as MetricTierKind, TierStatus as MetricTierStatus,
};
use parking_lot::Mutex;
use redb::ReadableTable;
use tracing::{trace, warn};

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Worker-specific knobs that don't fit `WorkerConfig`. Defaults
/// match the plan's "200ms–2s per memory with LLM, 6ms without"
/// latency budget — 32 memories per cycle absorbs an LLM-on burst
/// inside the 5s `max_runtime`.
#[derive(Clone, Copy, Debug)]
pub struct ExtractorKnobs {
    /// Hard cap on memories drained per cycle. Lower than AutoEdge's
    /// 256 because extraction is heavier (pattern + classifier inference
    /// + LLM round-trip).
    pub drain_per_cycle: usize,
    /// Cycle-wide LLM cost budget in dollar-micro-units (1e-6 USD).
    /// When the per-cycle sum exceeds this, the worker still runs
    /// pattern + classifier on remaining memories but stops invoking
    /// the LLM tier until the next cycle. Phase E ships an
    /// observability stub for now — the framework's per-call budget
    /// (`CostBudget::per_call_micro_usd`) is the active enforcement
    /// surface. Per-cycle accounting wires through here in a later
    /// iteration; for now this field tracks the configured ceiling.
    pub llm_budget_per_cycle_micro_usd: u64,
    /// When `true`, the worker probes the per-memory audit table and
    /// skips memories that have already been processed. The plan's
    /// "queue-replay idempotency" guard. Set to `false` for tests
    /// that want to drive multiple extraction passes against the same
    /// memory (e.g., re-extraction backfill).
    pub skip_already_extracted: bool,
}

pub const DEFAULT_EXTRACTOR_DRAIN_PER_CYCLE: usize = 32;
pub const DEFAULT_EXTRACTOR_LLM_BUDGET_MICRO_USD: u64 = 50_000;
pub const DEFAULT_EXTRACTOR_SKIP_AUDITED: bool = true;

impl Default for ExtractorKnobs {
    fn default() -> Self {
        Self {
            drain_per_cycle: DEFAULT_EXTRACTOR_DRAIN_PER_CYCLE,
            llm_budget_per_cycle_micro_usd: DEFAULT_EXTRACTOR_LLM_BUDGET_MICRO_USD,
            skip_already_extracted: DEFAULT_EXTRACTOR_SKIP_AUDITED,
        }
    }
}

/// Per-shard ExtractorWorker. Owns the receiver end of the writer's
/// extractor channel.
pub struct ExtractorWorker {
    config: WorkerConfig,
    knobs: ExtractorKnobs,
    queue: flume::Receiver<ExtractorEnqueue>,
    /// Per-cycle LLM cost accumulator. `Mutex` so the worker's
    /// `&self` cycle can still mutate it; lock contention is nil
    /// because a single shard drains its own queue.
    llm_spend: Mutex<u64>,
    /// Shared with the writer's enqueue path; both sides bump the
    /// same atomics. Defaults to a fresh local instance when the
    /// scheduler doesn't wire one.
    metrics: Arc<ExtractorMetrics>,
}

impl ExtractorWorker {
    /// Wire up the worker. The matching `flume::Sender` must be
    /// installed on the writer via `RealWriterHandle::set_extractor_sender`
    /// before any ENCODE runs; otherwise the queue stays empty.
    #[must_use]
    pub fn new(queue: flume::Receiver<ExtractorEnqueue>) -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::Extractor),
            knobs: ExtractorKnobs::default(),
            queue,
            llm_spend: Mutex::new(0),
            metrics: Arc::new(ExtractorMetrics::new()),
        }
    }

    /// Install the shared metric handle. Production wires this with
    /// the same `Arc<ExtractorMetrics>` it handed to
    /// `RealWriterHandle::set_extractor_metrics`.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<ExtractorMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Read accessor — tests assert on counter state through this.
    #[must_use]
    pub fn metrics(&self) -> Arc<ExtractorMetrics> {
        self.metrics.clone()
    }

    /// Override the scheduler config (interval / batch_size /
    /// max_runtime / enabled). Tests use this to shorten the cycle;
    /// operators wire it from `[workers.extractor]` TOML.
    #[must_use]
    pub fn with_config(mut self, config: WorkerConfig) -> Self {
        self.config = config;
        self
    }

    /// Override the worker-specific knobs.
    #[must_use]
    pub fn with_knobs(mut self, knobs: ExtractorKnobs) -> Self {
        self.knobs = knobs;
        self
    }

    /// Read accessor for tests.
    #[must_use]
    pub fn knobs(&self) -> ExtractorKnobs {
        self.knobs
    }
}

impl Worker for ExtractorWorker {
    fn name(&self) -> &'static str {
        WorkerKind::Extractor.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::Extractor
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_extractor_cycle(self, ctx))
    }
}

async fn do_extractor_cycle(
    worker: &ExtractorWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    let cfg = worker.config.clone();
    if cfg.batch_size == 0 {
        return Ok(0);
    }
    // Reset per-cycle LLM spend. Re-entering the cycle restarts the
    // budget window.
    {
        *worker.llm_spend.lock() = 0;
    }

    let started = Instant::now();
    let mut processed = 0usize;

    while processed < worker.knobs.drain_per_cycle.min(cfg.batch_size) {
        if started.elapsed() >= cfg.max_runtime {
            break;
        }
        if ctx.is_shutdown() {
            break;
        }
        let Ok((memory_id, text)) = worker.queue.try_recv() else {
            break;
        };
        processed += 1;

        // Idempotency probe — drop fast if we've already processed
        // this memory.
        if worker.knobs.skip_already_extracted {
            let db_guard = ctx.ops.executor.metadata.lock();
            let rtxn = db_guard
                .read_txn()
                .map_err(|e| WorkerError::Ops(format!("extractor read_txn: {e}")))?;
            let already = pipeline_has_extracted(&rtxn, memory_id)
                .map_err(|e| WorkerError::Ops(format!("pipeline_has_extracted: {e}")))?;
            drop(rtxn);
            drop(db_guard);
            if already {
                continue;
            }
        }

        // Snapshot the registry under a read lock — tier execution
        // doesn't need the lock held.
        let extractors: Vec<Arc<dyn Extractor>> = {
            let reg = ctx.ops.extractor_registry.read();
            reg.iter_enabled().cloned().collect()
        };

        let result = run_pipeline(extractors, memory_id, text.clone()).await;
        publish_tier_run_metrics(&worker.metrics, &result);
        if let Err(e) = apply_outcome(worker, ctx, memory_id, &result).await {
            warn!(
                memory_id = ?memory_id,
                error = %e,
                "extractor apply failed; auditing as FAILURE so it isn't retried",
            );
            audit_failure(ctx, memory_id, e.to_string())?;
        }

        // Cooperative yield every few drains so the scheduler stays
        // responsive.
        if processed.is_multiple_of(4) {
            glommio::executor().yield_if_needed().await;
        }
    }

    let elapsed = started.elapsed();
    worker.metrics.observe_cycle_duration(elapsed.as_secs_f64());
    trace!(
        drained = processed,
        cycle_ms = elapsed.as_millis() as u64,
        "extractor cycle",
    );
    Ok(processed)
}

/// Map the pipeline's per-tier outcome bytes onto the metric atomics.
/// Called once per processed memory, before `apply_outcome`.
fn publish_tier_run_metrics(metrics: &ExtractorMetrics, outcome: &PipelineOutcome) {
    let pairs = [
        (MetricTierKind::Pattern, outcome.pattern),
        (MetricTierKind::Classifier, outcome.classifier),
        (MetricTierKind::Llm, outcome.llm),
    ];
    for (tier, raw) in pairs {
        let status = match raw {
            tier_status::RAN => Some(MetricTierStatus::Ran),
            tier_status::SKIPPED => Some(MetricTierStatus::Skipped),
            tier_status::FAILED => Some(MetricTierStatus::Failed),
            // tier_status::ABSENT — tier wasn't registered; not a run.
            _ => None,
        };
        if let Some(status) = status {
            metrics.inc_tier_run(tier, status);
        }
    }
}

/// Aggregate of one pipeline run across all enabled extractors.
struct PipelineOutcome {
    items: Vec<ExtractedItem>,
    pattern: u8,
    classifier: u8,
    llm: u8,
    failure_reason: Option<String>,
}

async fn run_pipeline(
    extractors: Vec<Arc<dyn Extractor>>,
    memory_id: MemoryId,
    text: Arc<str>,
) -> PipelineOutcome {
    use brain_core::knowledge::ExtractorKind;

    // Build the Memory value the extractors consume. `created_at` /
    // `last_accessed_at` aren't relevant for extraction; pass zeros.
    let mem = CoreMemory {
        id: memory_id,
        agent: AgentId::new(),
        context: ContextId(0),
        kind: MemoryKind::Episodic,
        salience: Salience::default(),
        text: Some(text.to_string()),
        created_at_unix_ms: 0,
        last_accessed_at_unix_ms: 0,
    };

    // Empty registry for the ExtractionContext: tiers don't currently
    // cross-reference each other through it (same as the pre-existing
    // extractor_pipeline.rs in brain-ops).
    let empty_reg = ExtractorRegistry::new();
    let ext_ctx = ExtractionContext {
        schema_version: 1,
        now_unix_nanos: now_unix_nanos(),
        registry: &empty_reg,
    };

    let mut out = PipelineOutcome {
        items: Vec::new(),
        pattern: tier_status::ABSENT,
        classifier: tier_status::ABSENT,
        llm: tier_status::ABSENT,
        failure_reason: None,
    };

    for extractor in extractors {
        let kind = extractor.kind();
        let result = extractor.run(&ext_ctx, &mem).await;
        let outcome = tier_outcome_for(&result);
        match kind {
            ExtractorKind::Pattern => out.pattern = outcome,
            ExtractorKind::Classifier => out.classifier = outcome,
            ExtractorKind::Llm => out.llm = outcome,
        }
        if matches!(result.status, ExtractionStatus::Success) {
            out.items.extend(result.items);
        } else if out.failure_reason.is_none()
            && !matches!(result.status, ExtractionStatus::SkippedDisabled)
        {
            out.failure_reason = Some(format!("{:?}: {}", result.status, result.status_reason));
        }
    }
    out
}

fn tier_outcome_for(result: &ExtractionResult) -> u8 {
    match result.status {
        ExtractionStatus::Success => tier_status::RAN,
        ExtractionStatus::Failure => tier_status::FAILED,
        ExtractionStatus::SkippedBudget
        | ExtractionStatus::SkippedFilter
        | ExtractionStatus::SkippedDuplicate
        | ExtractionStatus::SkippedDisabled => tier_status::SKIPPED,
    }
}

async fn apply_outcome(
    worker: &ExtractorWorker,
    ctx: &WorkerContext,
    memory_id: MemoryId,
    outcome: &PipelineOutcome,
) -> Result<(), ApplyError> {
    let now = now_unix_nanos();
    let mut counts = ExtractorItemCounts::zero();
    let mut entity_map: HashMap<String, EntityId> = HashMap::new();

    let mut db_guard = ctx.ops.executor.metadata.lock();
    let wtxn = db_guard
        .write_txn()
        .map_err(|e| ApplyError::Storage(format!("write_txn: {e:?}")))?;

    // Pass 1 — entity mentions, in source order. Resolving early gives
    // statements + relations a populated `entity_map` to look up
    // surface forms against.
    for item in &outcome.items {
        if let ExtractedItem::EntityMention(em) = item {
            let (entity_id, tier) = resolve_entity_mention(&wtxn, em, now)?;
            worker
                .metrics
                .inc_resolver_outcome(resolution_tier_to_metric(tier));
            entity_map.insert(em.text.clone(), entity_id);
            write_mention_edge(&wtxn, memory_id, entity_id, em, now)?;
            // Count one fresh entity row only on a tier-4 create — the
            // resolver currently doesn't surface that distinction
            // through the worker's API, so we track every successful
            // resolve here. A future tweak can count Created vs.
            // Existing separately.
            counts.entities = counts.entities.saturating_add(1);
            counts.mention_edges = counts.mention_edges.saturating_add(1);
            worker
                .metrics
                .add_items_written(ExtractorItemKind::Entity, 1);
            worker
                .metrics
                .add_items_written(ExtractorItemKind::Mention, 1);
        }
    }

    // Pass 2 — statements + relations. These reference entities by
    // surface form; we look them up in `entity_map`. Items whose
    // referenced surface form wasn't in the entity-mention pass are
    // dropped with a trace (the LLM tier occasionally emits implicit
    // entities; auto-creating them here would produce ghost entities
    // without a mention edge).
    for item in &outcome.items {
        match item {
            ExtractedItem::EntityMention(_) => {}
            ExtractedItem::StatementMention(sm) => {
                if let Some(subject) = sm
                    .subject_text
                    .as_deref()
                    .and_then(|t| entity_map.get(t).copied())
                {
                    let object = statement_object_for(sm, &entity_map);
                    let (ns, name) =
                        split_qname(&sm.predicate_qname).map_err(ApplyError::InvalidQname)?;
                    if !predicate_allowed_by_schema(&wtxn, ns, name)? {
                        worker.metrics.inc_schema_filtered(&sm.predicate_qname);
                        tracing::info!(
                            target: "brain_workers::extractor",
                            memory_id = ?memory_id,
                            predicate = %sm.predicate_qname,
                            "predicate outside active schema; dropping",
                        );
                        continue;
                    }
                    let pid = predicate_intern_or_get(&wtxn, ns, name, 0, now)
                        .map_err(|e| ApplyError::Predicate(format!("{e}")))?;
                    let kind = statement_kind_from_byte(sm.kind);
                    let payload = StatementCreatePayload {
                        kind,
                        subject,
                        predicate: pid,
                        object,
                        confidence: sm.confidence.clamp(0.0, 1.0),
                        evidence_memory_ids: vec![memory_id],
                        extractor_id: ExtractorId::from(sm.extractor_id),
                        schema_version: 0,
                        extracted_at_unix_nanos: now,
                    };
                    match statement_create_internal(&wtxn, &payload) {
                        Ok(_) => {
                            counts.statements = counts.statements.saturating_add(1);
                            worker
                                .metrics
                                .add_items_written(ExtractorItemKind::Statement, 1);
                        }
                        Err(e) => trace!(
                            memory_id = ?memory_id,
                            error = %e,
                            "statement_create dropped",
                        ),
                    }
                } else {
                    trace!(
                        memory_id = ?memory_id,
                        subject = ?sm.subject_text,
                        "statement subject not in entity_map; dropping",
                    );
                }
            }
            ExtractedItem::RelationMention(rm) => {
                let from = entity_map.get(&rm.subject_text).copied();
                let to = entity_map.get(&rm.object_text).copied();
                if let (Some(from), Some(to)) = (from, to) {
                    let (ns, name) =
                        split_qname(&rm.relation_type_qname).map_err(ApplyError::InvalidQname)?;
                    if !relation_type_allowed_by_schema(&wtxn, ns, name)? {
                        worker.metrics.inc_schema_filtered(&rm.relation_type_qname);
                        tracing::info!(
                            target: "brain_workers::extractor",
                            memory_id = ?memory_id,
                            relation_type = %rm.relation_type_qname,
                            "relation_type outside active schema; dropping",
                        );
                        continue;
                    }
                    let rt = relation_type_intern_or_get(&wtxn, ns, name, 0, now)
                        .map_err(|e| ApplyError::RelationType(format!("{e}")))?;
                    let payload = RelationCreatePayload {
                        relation_type: rt,
                        from_entity: from,
                        to_entity: to,
                        confidence: rm.confidence.clamp(0.0, 1.0),
                        evidence_memory_ids: vec![memory_id],
                        extractor_id: ExtractorId::from(rm.extractor_id),
                        is_symmetric: false,
                        extracted_at_unix_nanos: now,
                    };
                    match relation_create_internal(&wtxn, &payload) {
                        Ok(_) => {
                            counts.relations = counts.relations.saturating_add(1);
                            worker
                                .metrics
                                .add_items_written(ExtractorItemKind::Relation, 1);
                        }
                        Err(e) => trace!(
                            memory_id = ?memory_id,
                            error = %e,
                            "relation_create dropped",
                        ),
                    }
                } else {
                    trace!(
                        memory_id = ?memory_id,
                        from = ?rm.subject_text,
                        to = ?rm.object_text,
                        "relation endpoint not in entity_map; dropping",
                    );
                }
            }
        }
    }

    // Audit the pipeline outcome inside the same txn so the audit
    // row + the writes commit atomically. A crash between commit and
    // audit insert would re-extract on next drain, which the resolver
    // would deduplicate but at the cost of an extra LLM call.
    let llm_spent = *worker.llm_spend.lock();
    let (status_byte, reason) = decide_status(outcome, counts);
    let audit = ExtractorPipelineAuditEntry::new(
        memory_id,
        now,
        status_byte,
        reason,
        outcome.pattern,
        outcome.classifier,
        outcome.llm,
        counts,
        llm_spent,
    );
    record_extracted(&wtxn, &audit)
        .map_err(|e| ApplyError::Audit(format!("record_extracted: {e}")))?;

    wtxn.commit()
        .map_err(|e| ApplyError::Storage(format!("commit: {e:?}")))?;
    Ok(())
}

fn decide_status(outcome: &PipelineOutcome, counts: ExtractorItemCounts) -> (u8, String) {
    let any_failed = outcome.pattern == tier_status::FAILED
        || outcome.classifier == tier_status::FAILED
        || outcome.llm == tier_status::FAILED;
    let any_ran = outcome.pattern == tier_status::RAN
        || outcome.classifier == tier_status::RAN
        || outcome.llm == tier_status::RAN;
    if !any_ran && any_failed {
        return (
            pipeline_status::FAILURE,
            outcome
                .failure_reason
                .clone()
                .unwrap_or_else(|| "all tiers failed".into()),
        );
    }
    if any_failed {
        return (
            pipeline_status::PARTIAL_FAILURE,
            outcome
                .failure_reason
                .clone()
                .unwrap_or_else(|| "one or more tiers failed".into()),
        );
    }
    if !any_ran && counts.is_empty() {
        return (pipeline_status::SKIPPED, String::new());
    }
    (pipeline_status::SUCCESS, String::new())
}

fn audit_failure(
    ctx: &WorkerContext,
    memory_id: MemoryId,
    reason: String,
) -> Result<(), WorkerError> {
    let mut db_guard = ctx.ops.executor.metadata.lock();
    let wtxn = db_guard
        .write_txn()
        .map_err(|e| WorkerError::Ops(format!("audit_failure write_txn: {e:?}")))?;
    let entry = ExtractorPipelineAuditEntry::new(
        memory_id,
        now_unix_nanos(),
        pipeline_status::FAILURE,
        reason,
        tier_status::ABSENT,
        tier_status::ABSENT,
        tier_status::ABSENT,
        ExtractorItemCounts::zero(),
        0,
    );
    record_extracted(&wtxn, &entry)
        .map_err(|e| WorkerError::Ops(format!("audit_failure record: {e}")))?;
    wtxn.commit()
        .map_err(|e| WorkerError::Ops(format!("audit_failure commit: {e:?}")))?;
    Ok(())
}

fn resolve_entity_mention(
    wtxn: &redb::WriteTransaction,
    em: &EntityMention,
    now: u64,
) -> Result<(EntityId, ResolutionTier), ApplyError> {
    let res = resolve_or_create(wtxn, &em.text, &em.entity_type_qname, em.confidence, now)
        .map_err(ApplyError::from)?;
    Ok((res.entity_id, res.tier))
}

fn resolution_tier_to_metric(tier: ResolutionTier) -> ResolverOutcome {
    match tier {
        ResolutionTier::Exact => ResolverOutcome::Exact,
        ResolutionTier::Alias => ResolverOutcome::Alias,
        ResolutionTier::Fuzzy => ResolverOutcome::Fuzzy,
        ResolutionTier::Created => ResolverOutcome::Create,
    }
}

fn write_mention_edge(
    wtxn: &redb::WriteTransaction,
    memory_id: MemoryId,
    entity_id: EntityId,
    em: &EntityMention,
    now: u64,
) -> Result<(), ApplyError> {
    use brain_core::{EdgeKindRef, NodeRef};
    let mut edges_t = wtxn
        .open_table(EDGES_TABLE)
        .map_err(|e| ApplyError::Edge(format!("open EDGES: {e:?}")))?;
    let mut edges_rev_t = wtxn
        .open_table(EDGES_REVERSE_TABLE)
        .map_err(|e| ApplyError::Edge(format!("open EDGES_REVERSE: {e:?}")))?;
    let data = EdgeData {
        weight: em.confidence.clamp(0.0, 1.0),
        origin: origin::AUTO_DERIVED,
        derived_by: derived_by::SIMILARITY_WORKER,
        created_at_unix_nanos: now,
        annotation: Some(em.text.clone()),
    };
    edge::link(
        &mut edges_t,
        &mut edges_rev_t,
        NodeRef::Memory(memory_id),
        EdgeKindRef::Mentions,
        NodeRef::Entity(entity_id),
        zero_disambiguator(),
        &data,
    )
    .map_err(|e| ApplyError::Edge(format!("link: {e:?}")))?;
    Ok(())
}

/// Look up the active schema version for `namespace` inside an
/// existing write txn. `None` = schemaless (open vocabulary).
fn schema_active_for_namespace(
    wtxn: &redb::WriteTransaction,
    namespace: &str,
) -> Result<Option<u32>, ApplyError> {
    let table = match wtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(ApplyError::Storage(format!("schema_active open: {e}"))),
    };
    let value: Option<u32> = table
        .get(&namespace)
        .map_err(|e| ApplyError::Storage(format!("schema_active get: {e}")))?
        .map(|g| g.value());
    Ok(value)
}

/// Returns `true` when the extractor's predicate qname is admissible
/// for the current schema posture of its namespace. Schemaless
/// (no active version) → always admissible. Schema-strict → predicate
/// must already exist AND its origin row must be a SchemaDeclared row
/// for the active version.
fn predicate_allowed_by_schema(
    wtxn: &redb::WriteTransaction,
    namespace: &str,
    name: &str,
) -> Result<bool, ApplyError> {
    let Some(active_version) = schema_active_for_namespace(wtxn, namespace)? else {
        return Ok(true);
    };
    let t = wtxn
        .open_table(PREDICATES_TABLE)
        .map_err(|e| ApplyError::Storage(format!("predicates open: {e}")))?;
    for entry in t
        .iter()
        .map_err(|e| ApplyError::Storage(format!("predicates iter: {e}")))?
    {
        let (_k, v) = entry.map_err(|e| ApplyError::Storage(format!("predicates entry: {e}")))?;
        let row: PredicateDefinition = v.value();
        if row.namespace != namespace || row.name != name {
            continue;
        }
        if let SchemaOrigin::SchemaDeclared { version } = row.origin() {
            if version == active_version {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn relation_type_allowed_by_schema(
    wtxn: &redb::WriteTransaction,
    namespace: &str,
    name: &str,
) -> Result<bool, ApplyError> {
    let Some(active_version) = schema_active_for_namespace(wtxn, namespace)? else {
        return Ok(true);
    };
    let t = wtxn
        .open_table(RELATION_TYPES_TABLE)
        .map_err(|e| ApplyError::Storage(format!("relation_types open: {e}")))?;
    for entry in t
        .iter()
        .map_err(|e| ApplyError::Storage(format!("relation_types iter: {e}")))?
    {
        let (_k, v) =
            entry.map_err(|e| ApplyError::Storage(format!("relation_types entry: {e}")))?;
        let row: RelationTypeDefinition = v.value();
        if row.namespace != namespace || row.name != name {
            continue;
        }
        if let RelationTypeOrigin::SchemaDeclared { version } = row.origin() {
            if version == active_version {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn statement_object_for(
    sm: &StatementMention,
    entity_map: &HashMap<String, EntityId>,
) -> StatementObject {
    if let Some(text) = sm.object_text.as_deref() {
        if let Some(id) = entity_map.get(text).copied() {
            return StatementObject::Entity(id);
        }
        return StatementObject::Value(StatementValue::Text(text.to_string()));
    }
    StatementObject::Value(StatementValue::Text(String::new()))
}

fn split_qname(q: &str) -> Result<(&str, &str), String> {
    q.split_once(':')
        .ok_or_else(|| format!("qname missing ':' separator: {q}"))
}

fn statement_kind_from_byte(b: u8) -> StatementKind {
    match b {
        2 => StatementKind::Preference,
        3 => StatementKind::Event,
        _ => StatementKind::Fact,
    }
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Internal error type. The cycle converts these into either an audit
// row (on per-memory failures) or a `WorkerError` (on infra failures).
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
enum ApplyError {
    #[error("resolver: {0}")]
    Resolver(#[from] ResolverError),
    #[error("invalid qname: {0}")]
    InvalidQname(String),
    #[error("predicate op: {0}")]
    Predicate(String),
    #[error("relation_type op: {0}")]
    RelationType(String),
    #[error("mention edge: {0}")]
    Edge(String),
    #[error("audit: {0}")]
    Audit(String),
    #[error("storage: {0}")]
    Storage(String),
}
