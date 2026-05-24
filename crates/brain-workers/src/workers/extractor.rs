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

use brain_core::{StatementKind, StatementObject, StatementValue};
use brain_core::{
    AgentId, ContextId, EntityId, ExtractorId, Memory as CoreMemory, MemoryId, MemoryKind, Salience,
};
use brain_extractors::{
    classify_statement_kind_pattern,
    resolver::{
        resolve_or_create_with_deps, EmbeddingDeps, EntityDisambiguator, ResolutionTier,
        ResolverError,
    },
    EntityMention, ExtractedItem, ExtractionContext, ExtractionResult, ExtractionStatus, Extractor,
    ExtractorContext, ExtractorRegistry, StatementMention, STATEMENT_KIND_PATTERN_THRESHOLD,
};
use brain_metadata::pipeline_has_extracted;
use brain_metadata::relation::types::relation_type_intern_or_get;
use brain_metadata::schema::predicate::predicate_intern_or_get;
use brain_metadata::tables::edge::{
    self, derived_by, origin, zero_disambiguator, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE,
};
use brain_metadata::tables::extractor_audit::{
    pipeline_status, record_extracted, tier_status, ExtractorItemCounts,
    ExtractorPipelineAuditEntry,
};
use brain_metadata::tables::predicate::{PredicateDefinition, SchemaOrigin, PREDICATES_TABLE};
use brain_metadata::tables::relation_type::{
    RelationTypeDefinition, RelationTypeOrigin, RELATION_TYPES_TABLE,
};
use brain_metadata::tables::schema_version::SCHEMA_ACTIVE_VERSIONS_TABLE;
use brain_ops::apply::encode_helpers::{
    fetch_extractor_context, ExtractorContextFetchConfig, DEFAULT_EXTRACTOR_CONTEXT_TOP_M,
};
use brain_ops::writer::extractor_writes::{
    relation_create_internal, statement_create_internal, RelationCreatePayload,
    StatementCreatePayload,
};
use brain_ops::{
    CausalEdgeEnqueue, CausalEdgeMetrics, EventEnvelope, ExtractorEnqueue, ExtractorItemKind,
    ExtractorMetrics, ResolverOutcome, TierKind as MetricTierKind, TierStatus as MetricTierStatus,
};
use brain_protocol::shared::enums::{
    EventType, StageAuditStatus, StageExtractorPayload, StageKind, StageOutcome, StagePayload,
};
use futures_lite::FutureExt;
use glommio::timer::sleep;
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
    /// Number of memories the pipeline batches into one classifier
    /// forward pass. Single-input GLiNER inference is ~4s on CPU
    /// (DeBERTa-v3-small + BiLSTM + span head); a batched backbone
    /// pass over 8 rows completes in ~1-2x the single-input cost
    /// because the GEMMs saturate the CPU's vector units. Tunable via
    /// `BRAIN_EXTRACTOR_BATCH_SIZE` for ops who need to balance
    /// throughput against per-encode tail latency.
    pub batch_size: usize,
}

pub const DEFAULT_EXTRACTOR_DRAIN_PER_CYCLE: usize = 32;
pub const DEFAULT_EXTRACTOR_LLM_BUDGET_MICRO_USD: u64 = 50_000;
pub const DEFAULT_EXTRACTOR_SKIP_AUDITED: bool = true;
/// Memories per classifier forward pass. 8 is the sweet spot on the
/// dev container's CPU: the backbone GEMM saturates well before then,
/// and going higher adds latency without throughput gains. Bigger
/// hosts can lift this via `BRAIN_EXTRACTOR_BATCH_SIZE`.
pub const DEFAULT_EXTRACTOR_BATCH_SIZE: usize = 8;
/// Env-var override for [`ExtractorKnobs::batch_size`]. Parsed as a
/// `usize`; invalid / zero values fall back to the default with a
/// `tracing::warn!`.
pub const EXTRACTOR_BATCH_SIZE_ENV: &str = "BRAIN_EXTRACTOR_BATCH_SIZE";

impl Default for ExtractorKnobs {
    fn default() -> Self {
        Self {
            drain_per_cycle: DEFAULT_EXTRACTOR_DRAIN_PER_CYCLE,
            llm_budget_per_cycle_micro_usd: DEFAULT_EXTRACTOR_LLM_BUDGET_MICRO_USD,
            skip_already_extracted: DEFAULT_EXTRACTOR_SKIP_AUDITED,
            batch_size: resolve_batch_size_from_env(),
        }
    }
}

fn resolve_batch_size_from_env() -> usize {
    parse_batch_size_raw(std::env::var(EXTRACTOR_BATCH_SIZE_ENV).ok().as_deref())
}

/// Pure-logic parser for the `BRAIN_EXTRACTOR_BATCH_SIZE` env var.
/// Returns the parsed value when it's a positive `usize`, otherwise
/// the default. Pulled out so tests can exercise the rejection
/// branches without mutating the process-wide env (which is racy
/// across test threads and `unsafe` under Rust 2024).
fn parse_batch_size_raw(raw: Option<&str>) -> usize {
    let Some(raw) = raw else {
        return DEFAULT_EXTRACTOR_BATCH_SIZE;
    };
    if raw.is_empty() {
        return DEFAULT_EXTRACTOR_BATCH_SIZE;
    }
    match raw.parse::<usize>() {
        Ok(v) if v > 0 => v,
        Ok(_) => {
            tracing::warn!(
                target: "brain_workers::extractor",
                env_var = EXTRACTOR_BATCH_SIZE_ENV,
                value = %raw,
                "batch_size env var must be > 0; using default"
            );
            DEFAULT_EXTRACTOR_BATCH_SIZE
        }
        Err(e) => {
            tracing::warn!(
                target: "brain_workers::extractor",
                env_var = EXTRACTOR_BATCH_SIZE_ENV,
                value = %raw,
                error = %e,
                "batch_size env var is not a valid usize; using default"
            );
            DEFAULT_EXTRACTOR_BATCH_SIZE
        }
    }
}

/// Wiring bundle for the CausalEdgeWorker fan-out. When the
/// ExtractorWorker writes a statement whose predicate qname matches
/// `whitelist_qnames`, it pushes the new `StatementId` onto `sender`
/// so the CausalEdgeWorker can walk the cause/effect graph. The
/// extractor never blocks: on a full channel it bumps `metrics.drops`
/// and moves on (the statement is still committed; the auto-edge
/// derivation is just deferred until the next live causal statement
/// or a future re-extraction).
///
/// The qname-vs-id check happens here (not via `PredicateId`) because
/// the ExtractorWorker already has the parsed `(namespace, name)`
/// from `sm.predicate_qname` and the CausalEdgeWorker independently
/// validates predicate ids on its side. Matching by qname avoids a
/// circular dependency on the CausalEdgeWorker's resolved set.
#[derive(Clone)]
pub struct CausalEdgeFeed {
    pub sender: flume::Sender<CausalEdgeEnqueue>,
    pub metrics: Arc<CausalEdgeMetrics>,
    /// `(namespace, name)` pairs whose presence triggers an enqueue.
    /// Substrate-only deployments leave this empty by construction
    /// (no `[workers.causal_edge]` wiring) and the filter never fires.
    pub whitelist_qnames: std::collections::HashSet<(String, String)>,
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
    /// Optional fan-out to the CausalEdgeWorker. `None` when causal
    /// derivation is disabled at the shard (or when no causal predicate
    /// names are configured). When `Some`, the worker checks each
    /// newly-written statement's qname against `whitelist_qnames` and
    /// `try_send`s matching `StatementId`s.
    causal_edge: Option<CausalEdgeFeed>,
    /// Optional entity-HNSW + embedder bundle passed through to the
    /// resolver as tier-3b. `None` means the resolver runs in legacy
    /// 4-tier mode (no embedding probe, no synchronous HNSW population
    /// on entity create). Substrate-only deployments and tests that
    /// don't care about entity-paraphrase resolution leave this unset;
    /// production shards stamp the per-shard `EntityHnswIndex` +
    /// shared embedder dispatcher.
    embed_deps: Option<EmbeddingDeps>,
    /// Optional disambiguator consulted when the embedding probe lands
    /// in the ambiguous band. `None` means the resolver keeps its
    /// existing behaviour (Create + enqueue merge proposal) for every
    /// partial match. Production shards stamp this with the
    /// `EntityDisambiguator` built from the shared LLM client when
    /// `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` are present at startup.
    entity_disambiguator: Option<Arc<EntityDisambiguator>>,
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
            causal_edge: None,
            embed_deps: None,
            entity_disambiguator: None,
        }
    }

    /// Wire the resolver's tier-3b embedding path. Without this call
    /// the resolver runs without HNSW + embedder and never returns
    /// `ResolutionTier::Embedding`. Production shards always set this
    /// from their per-shard `EntityHnswIndex` + the shared BGE
    /// dispatcher; tests opt out by leaving the bundle unset.
    #[must_use]
    pub fn with_embed_deps(mut self, deps: EmbeddingDeps) -> Self {
        self.embed_deps = Some(deps);
        self
    }

    /// Wire the partial-match disambiguator. Without this call the
    /// resolver keeps its existing behaviour (mint + enqueue merge
    /// proposal) for every ambiguous-band candidate. Production shards
    /// set this when the LLM tier is provisioned via env; tests that
    /// don't care about disambiguation leave it unset.
    #[must_use]
    pub fn with_entity_disambiguator(mut self, disambiguator: Arc<EntityDisambiguator>) -> Self {
        self.entity_disambiguator = Some(disambiguator);
        self
    }

    /// Wire the CausalEdgeWorker fan-out. Without this call the
    /// extractor never enqueues onto the causal channel — useful for
    /// tests that don't care about edge derivation and for substrate-
    /// only deployments where no causal predicates are declared.
    #[must_use]
    pub fn with_causal_edge_feed(mut self, feed: CausalEdgeFeed) -> Self {
        self.causal_edge = Some(feed);
        self
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

    // Log entry only when the queue has something in it. The cycle
    // fires every interval_ms regardless; logging the empty case
    // every tick would drown the log. `flume::Receiver::len()` is
    // an O(1) atomic read.
    let initial_queue_len = worker.queue.len();
    if initial_queue_len > 0 {
        tracing::info!(
            target: "brain_debug::extractor",
            queue_len = initial_queue_len,
            sender_count = worker.queue.sender_count(),
            registry_size = ctx.ops.extractor_registry.read().iter_enabled().count(),
            "do_extractor_cycle: entering with non-empty queue",
        );
    }

    let cycle_cap = worker.knobs.drain_per_cycle.min(cfg.batch_size);
    let micro_batch = worker.knobs.batch_size.max(1);
    while processed < cycle_cap {
        if started.elapsed() >= cfg.max_runtime {
            break;
        }
        if ctx.is_shutdown() {
            break;
        }

        // Drain up to `micro_batch` items, blocking only on the first
        // item of the first iteration so the cycle wakes promptly when
        // a single encode arrives. Subsequent items in this micro-batch
        // are pulled non-blockingly — when the queue dries up we run
        // the partial batch through the pipeline rather than wait.
        let mut micro: Vec<ExtractorEnqueue> = Vec::with_capacity(micro_batch);
        if processed == 0 {
            let recv = async { worker.queue.recv_async().await.ok() };
            let tick = async {
                sleep(cfg.interval).await;
                None
            };
            match recv.or(tick).await {
                Some(item) => micro.push(item),
                None => break,
            }
        }
        while micro.len() < micro_batch && processed + micro.len() < cycle_cap {
            match worker.queue.try_recv() {
                Ok(item) => micro.push(item),
                Err(_) => break,
            }
        }
        if micro.is_empty() {
            break;
        }

        // Run the whole micro-batch through one pipeline invocation.
        // `drain_batch` returns one StageDecision per memory in input
        // order so we can publish per-memory StageCompleted exactly
        // once even when the batched classifier pass amortises across
        // multiple memories.
        let batch_decisions = drain_batch(worker, ctx, &micro).await;
        for ((memory_id, _), decision) in micro.iter().zip(batch_decisions) {
            let (counts, audit_status) = match decision {
                StageDecision::Applied {
                    counts,
                    status_byte,
                } => (counts, audit_status_from_byte(status_byte)),
                StageDecision::AppliedFailed | StageDecision::GateFailed => {
                    (ExtractorItemCounts::zero(), StageAuditStatus::Failed)
                }
                StageDecision::AlreadyExtracted => {
                    (ExtractorItemCounts::zero(), StageAuditStatus::Skipped)
                }
            };
            publish_extracted_knowledge(ctx, *memory_id, counts, audit_status);
        }
        processed += micro.len();

        // Cooperative yield after every micro-batch so the scheduler
        // stays responsive even when a batch lands in one tick.
        glommio::executor().yield_if_needed().await;
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

/// Per-memory drain outcome. `do_extractor_cycle` lifts this into a
/// `StageCompleted` publish for the memory. There is no Err variant
/// by design — the contract is "every drained memory_id publishes
/// exactly once," so internal failures get folded back as decisions
/// rather than `?`-escaping past the publish path.
enum StageDecision {
    /// Pipeline ran and `apply_outcome` committed. `counts` and
    /// `status_byte` come from the apply commit; the publish maps
    /// `status_byte` through `audit_status_from_byte`.
    Applied {
        counts: ExtractorItemCounts,
        status_byte: u8,
    },
    /// `apply_outcome` returned an error. Best-effort `audit_failure`
    /// has already been attempted; any error from it was logged, not
    /// propagated. The publish records `Failed` with zero counts so
    /// subscribers unblock.
    AppliedFailed,
    /// A pre-pipeline gate (read_txn open, audit probe) errored
    /// before any work was attempted. The publish still records
    /// `Failed` with zero counts so subscribers unblock.
    GateFailed,
    /// `skip_already_extracted` saw an existing audit row for this
    /// memory. No work attempted; publish records `Skipped`.
    AlreadyExtracted,
}

/// Drain a micro-batch of `(memory_id, text)` pairs through the
/// idempotency gate, pipeline (batched at the classifier tier),
/// apply, and failure-audit paths. Returns one `StageDecision` per
/// input in input order; the caller publishes one `StageCompleted`
/// per memory regardless of outcome.
///
/// The classifier tier sees ALL non-skipped memories in one
/// `run_batch` call (amortising the GLiNER forward pass). Pattern +
/// LLM tiers run per-memory because pattern is fast and LLM
/// per-memory accounting drives the budget gate.
async fn drain_batch(
    worker: &ExtractorWorker,
    ctx: &WorkerContext,
    items: &[ExtractorEnqueue],
) -> Vec<StageDecision> {
    // Pre-allocate the output with a placeholder; we overwrite as
    // each row resolves.
    let mut decisions: Vec<StageDecision> = (0..items.len())
        .map(|_| StageDecision::GateFailed)
        .collect();

    // Idempotency probe each row. Failures and AlreadyExtracted slots
    // are written into `decisions` immediately; `live` collects the
    // (input_index, memory_id, text) we still want to process.
    type LiveRow<'a> = (usize, MemoryId, Arc<str>);
    let mut live: Vec<LiveRow<'_>> = Vec::with_capacity(items.len());
    for (idx, (memory_id, text)) in items.iter().enumerate() {
        if worker.knobs.skip_already_extracted {
            let probe = {
                let db_guard = ctx.ops.executor.metadata.lock();
                match db_guard.read_txn() {
                    Ok(rtxn) => pipeline_has_extracted(&rtxn, *memory_id)
                        .map_err(|e| format!("pipeline_has_extracted: {e}")),
                    Err(e) => Err(format!("extractor read_txn: {e}")),
                }
            };
            match probe {
                Ok(true) => {
                    decisions[idx] = StageDecision::AlreadyExtracted;
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    warn!(
                        memory_id = ?memory_id,
                        error = %e,
                        "extractor gate probe failed; publishing Failed so wait-for-extraction unblocks",
                    );
                    decisions[idx] = StageDecision::GateFailed;
                    continue;
                }
            }
        }
        live.push((idx, *memory_id, text.clone()));
    }

    if live.is_empty() {
        return decisions;
    }

    // Snapshot the registry under a read lock — tier execution
    // doesn't need the lock held.
    let extractors: Vec<Arc<dyn Extractor>> = {
        let reg = ctx.ops.extractor_registry.read();
        reg.iter_enabled().cloned().collect()
    };

    let cycle_budget = worker.knobs.llm_budget_per_cycle_micro_usd;
    let spent_so_far = { *worker.llm_spend.lock() };
    let skip_llm_budget_exhausted = cycle_budget > 0 && spent_so_far >= cycle_budget;

    let live_mems: Vec<CoreMemory> = live
        .iter()
        .map(|(_, mid, text)| CoreMemory {
            id: *mid,
            agent: AgentId::new(),
            context: ContextId(0),
            kind: MemoryKind::Episodic,
            salience: Salience::default(),
            text: Some(text.to_string()),
            created_at_unix_ms: 0,
            last_accessed_at_unix_ms: 0,
        })
        .collect();

    // Fetch bounded LLM context (top-m similar memories + optional
    // rolling summary) per memory before running tiers. The fetch
    // happens once for the whole micro-batch so the per-memory cost
    // amortises against the LLM call that follows. Skipped when the
    // cycle LLM budget is exhausted or no LLM-tier extractors are
    // registered — there's nothing to feed.
    let extractor_context_map = if skip_llm_budget_exhausted || llm_exts_count(&extractors) == 0 {
        None
    } else {
        Some(fetch_extractor_context_for_batch(ctx, &live_mems, &worker.metrics).await)
    };

    // Per-memory neighbor-count + approximate token observation. We
    // observe pre-LLM-call so the histograms cover every dispatch
    // (success or failure). Token estimate uses the same `chars / 4`
    // heuristic LlmRequest::approx_input_tokens applies, computed
    // from the cue text + sum of neighbor texts + a small overhead
    // for the section scaffolding.
    if let Some(map) = extractor_context_map.as_ref() {
        for m in &live_mems {
            let Some(ec) = map.get(&m.id) else {
                continue;
            };
            worker
                .metrics
                .observe_llm_neighbors_included(ec.neighbors.len());
            let cue_chars = m.text.as_deref().map(|t| t.chars().count()).unwrap_or(0);
            let neighbor_chars: usize = ec.neighbors.iter().map(|n| n.text.chars().count()).sum();
            let summary_chars = ec
                .summary
                .as_deref()
                .map(|s| s.chars().count())
                .unwrap_or(0);
            // Add a 300-char overhead for the prompt scaffolding
            // (section headers, instruction blurbs, role text). It's
            // a coarse upper bound but tracks the real cost closely.
            let total_chars = cue_chars + neighbor_chars + summary_chars + 300;
            worker
                .metrics
                .observe_llm_tokens_per_query((total_chars / 4) as u64);
        }
    }

    let outcomes = run_pipeline_batch(
        extractors,
        &live_mems,
        skip_llm_budget_exhausted,
        extractor_context_map,
    )
    .await;

    // Cycle-LLM-budget bookkeeping + per-tier-run metrics.
    let mut total_llm_micro: u64 = 0;
    for outcome in &outcomes {
        total_llm_micro = total_llm_micro.saturating_add(outcome.llm_cost_micro_usd);
        publish_tier_run_metrics(&worker.metrics, outcome);
    }
    if total_llm_micro > 0 {
        let mut spend = worker.llm_spend.lock();
        *spend = spend.saturating_add(total_llm_micro);
        worker.metrics.add_llm_micro_usd(total_llm_micro);
    }

    // Apply each outcome and fold into the per-memory decision slot.
    for ((idx, memory_id, _), outcome) in live.into_iter().zip(outcomes) {
        let decision = match apply_outcome(worker, ctx, memory_id, &outcome).await {
            Ok(applied) => StageDecision::Applied {
                counts: applied.counts,
                status_byte: applied.status_byte,
            },
            Err(e) => {
                warn!(
                    memory_id = ?memory_id,
                    error = %e,
                    "extractor apply failed; auditing as FAILURE so it isn't retried",
                );
                if let Err(audit_err) = audit_failure(ctx, memory_id, e.to_string()) {
                    warn!(
                        memory_id = ?memory_id,
                        error = %audit_err,
                        "extractor audit_failure also errored; StageCompleted Failed still publishes",
                    );
                }
                StageDecision::AppliedFailed
            }
        };
        decisions[idx] = decision;
    }
    decisions
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

/// True iff `extractors` contains at least one LLM tier — used to
/// skip the bounded-context fetch when only pattern/classifier tiers
/// are registered (the fetch result has no consumer).
fn llm_exts_count(extractors: &[Arc<dyn Extractor>]) -> usize {
    extractors
        .iter()
        .filter(|e| matches!(e.kind(), brain_core::ExtractorKind::Llm))
        .count()
}

/// Per-batch bounded-context fetch. Calls
/// [`fetch_extractor_context`] for each memory using its own text as
/// the cue. Failures degrade silently to an empty entry so the LLM
/// tier still runs (logged + counted via metrics). Returns one entry
/// per input memory, keyed by `MemoryId`.
async fn fetch_extractor_context_for_batch(
    ctx: &WorkerContext,
    mems: &[CoreMemory],
    metrics: &ExtractorMetrics,
) -> HashMap<MemoryId, ExtractorContext> {
    let cfg = ExtractorContextFetchConfig {
        top_m: DEFAULT_EXTRACTOR_CONTEXT_TOP_M,
        same_context_only: true,
    };
    let mut out = HashMap::with_capacity(mems.len());
    for m in mems {
        let started = Instant::now();
        let cue_text = m.text.as_deref().unwrap_or("");
        let entry = match fetch_extractor_context(&ctx.ops, m.id, cue_text, cfg).await {
            Ok(ec) => ec,
            Err(e) => {
                warn!(
                    target: "brain_workers::extractor",
                    memory_id = ?m.id,
                    error = %e,
                    "context fetch failed; falling back to context-free extraction",
                );
                ExtractorContext::empty()
            }
        };
        metrics.observe_context_fetch_duration(started.elapsed().as_secs_f64());
        out.insert(m.id, entry);
    }
    out
}

/// Aggregate of one pipeline run across all enabled extractors.
struct PipelineOutcome {
    items: Vec<ExtractedItem>,
    pattern: u8,
    classifier: u8,
    llm: u8,
    failure_reason: Option<String>,
    /// Actual LLM cost in dollar-micro-units for this pipeline run.
    /// Non-LLM tiers always contribute zero.
    llm_cost_micro_usd: u64,
}

/// Run every enabled extractor over a batch of memories, returning
/// one `PipelineOutcome` per memory in input order. Extractors are
/// invoked in a fixed tier order — Pattern -> Classifier -> LLM —
/// regardless of registration order, so the LLM tier sees the cheap
/// tiers' entity mentions through `ExtractionContext::prior_tier_items`
/// and can reuse canonical names instead of re-extracting them. The
/// classifier tier uses `run_batch` so the GLiNER forward pass
/// amortises across the whole batch in one GEMM; pattern + LLM tiers
/// fall through to the default per-row impl because they don't benefit
/// from batching.
async fn run_pipeline_batch(
    extractors: Vec<Arc<dyn Extractor>>,
    mems: &[CoreMemory],
    skip_llm_budget_exhausted: bool,
    extractor_context_map: Option<HashMap<MemoryId, ExtractorContext>>,
) -> Vec<PipelineOutcome> {
    use brain_core::ExtractorKind;

    let empty_reg = ExtractorRegistry::new();
    let now = now_unix_nanos();

    let mut outcomes: Vec<PipelineOutcome> = (0..mems.len())
        .map(|_| PipelineOutcome {
            items: Vec::new(),
            pattern: tier_status::ABSENT,
            classifier: tier_status::ABSENT,
            llm: tier_status::ABSENT,
            failure_reason: None,
            llm_cost_micro_usd: 0,
        })
        .collect();

    // Bucket extractors by tier so we can run in pipeline order and
    // populate `prior_tier_items` between tiers.
    let mut pattern_exts: Vec<Arc<dyn Extractor>> = Vec::new();
    let mut classifier_exts: Vec<Arc<dyn Extractor>> = Vec::new();
    let mut llm_exts: Vec<Arc<dyn Extractor>> = Vec::new();
    for ext in extractors {
        match ext.kind() {
            ExtractorKind::Pattern => pattern_exts.push(ext),
            ExtractorKind::Classifier => classifier_exts.push(ext),
            ExtractorKind::Llm => llm_exts.push(ext),
        }
    }

    // Accumulates `EntityMention`s (and any other prior-tier items) per
    // memory id across pattern + classifier. The LLM tier reads from
    // this map so its prompt can anchor on canonical names instead of
    // re-extracting the same surface forms.
    let mut prior_items: HashMap<MemoryId, Vec<ExtractedItem>> = HashMap::new();

    // ----- Tier 1: pattern -------------------------------------------
    {
        let ctx = ExtractionContext {
            schema_version: 1,
            now_unix_nanos: now,
            registry: &empty_reg,
            prior_tier_items: None,
            extractor_context: None,
        };
        run_tier_into(
            &pattern_exts,
            &ctx,
            mems,
            &mut outcomes,
            ExtractorKind::Pattern,
        )
        .await;
    }
    accumulate_into_prior(&outcomes, mems, &mut prior_items);

    // ----- Tier 2: classifier ----------------------------------------
    {
        let ctx = ExtractionContext {
            schema_version: 1,
            now_unix_nanos: now,
            registry: &empty_reg,
            prior_tier_items: Some(&prior_items),
            extractor_context: None,
        };
        run_tier_into(
            &classifier_exts,
            &ctx,
            mems,
            &mut outcomes,
            ExtractorKind::Classifier,
        )
        .await;
    }
    accumulate_into_prior(&outcomes, mems, &mut prior_items);

    // ----- Tier 3: llm -----------------------------------------------
    if skip_llm_budget_exhausted {
        // Cycle-budget gate: skip the LLM tier across the whole batch
        // when prior cycles have eaten the budget. Pattern + classifier
        // already ran so cheap-tier output still lands under load.
        for o in &mut outcomes {
            o.llm = tier_status::SKIPPED;
        }
    } else {
        // Bounded inferential context per memory: top-10 similar
        // priors + (when wired) a rolling summary. Without this the
        // LLM can only see the memory it's extracting and cannot
        // anchor predicates like "Alice mentioned earlier"; with it
        // the prompt grows by at most a few thousand tokens.
        //
        // Fetch failures degrade gracefully — the LLM still runs,
        // just without the neighbor section.
        let extractor_context_map = extractor_context_map.as_ref();
        let ctx = ExtractionContext {
            schema_version: 1,
            now_unix_nanos: now,
            registry: &empty_reg,
            prior_tier_items: Some(&prior_items),
            extractor_context: extractor_context_map,
        };
        run_tier_into(&llm_exts, &ctx, mems, &mut outcomes, ExtractorKind::Llm).await;
    }

    // Post-tier statement-kind refinement. The LLM tier's open
    // statement-projection path defaults `kind` to Fact (1) for any row
    // it doesn't explicitly type — and the open path is the common one
    // because today's prompt asks the model for predicates, not kinds.
    // The deterministic pattern classifier catches the high-signal
    // Preference / Event cases (first-person preference verbs, dated
    // event nouns) without an extra LLM call. We only override when the
    // statement is currently Fact and the pattern fires above the
    // threshold — anything else means the LLM made an explicit choice
    // and we don't second-guess it.
    refine_statement_kinds(mems, &mut outcomes);

    outcomes
}

/// Override Fact-defaulted statement kinds with pattern-classifier
/// decisions when the classifier is confident. Keeps the cheap tier off
/// the LLM cost path for the common Preference / Event cues.
fn refine_statement_kinds(mems: &[CoreMemory], outcomes: &mut [PipelineOutcome]) {
    debug_assert_eq!(mems.len(), outcomes.len());
    for (mem, outcome) in mems.iter().zip(outcomes.iter_mut()) {
        let Some(text) = mem.text.as_deref() else {
            continue;
        };
        let Some((kind, confidence)) = classify_statement_kind_pattern(text) else {
            continue;
        };
        if confidence < STATEMENT_KIND_PATTERN_THRESHOLD {
            continue;
        }
        let wire_byte = statement_kind_to_byte(kind);
        for item in &mut outcome.items {
            if let ExtractedItem::StatementMention(sm) = item {
                // Only rewrite the Fact default (wire byte 1). When the
                // LLM emitted an explicit Preference (2) or Event (3),
                // the model had context the pattern doesn't — trust it.
                if sm.kind == 1 && wire_byte != 1 {
                    sm.kind = wire_byte;
                }
            }
        }
    }
}

/// Convert a `StatementKind` into the wire byte the pattern uses for
/// `StatementMention.kind`. The wire convention is `1/2/3` (matches
/// `statement_kind_from_byte` and the LLM's `kind_to_byte`).
fn statement_kind_to_byte(k: StatementKind) -> u8 {
    match k {
        StatementKind::Fact => 1,
        StatementKind::Preference => 2,
        StatementKind::Event => 3,
    }
}

/// Execute every extractor in `tier_exts` against the batch, folding
/// each result into the corresponding `outcomes[i]` slot. Captures the
/// per-tier RAN/SKIPPED/FAILED byte on the slot, and appends Success
/// items to `outcomes[i].items` so the next tier sees them via
/// `accumulate_into_prior`.
async fn run_tier_into(
    tier_exts: &[Arc<dyn Extractor>],
    ctx: &ExtractionContext<'_>,
    mems: &[CoreMemory],
    outcomes: &mut [PipelineOutcome],
    tier_kind: brain_core::ExtractorKind,
) {
    use brain_core::ExtractorKind;
    for extractor in tier_exts {
        let results = extractor.run_batch(ctx, mems).await;
        debug_assert_eq!(results.len(), mems.len());
        for (i, result) in results.into_iter().enumerate() {
            let outcome_byte = tier_outcome_for(&result);
            let slot = &mut outcomes[i];
            match tier_kind {
                ExtractorKind::Pattern => slot.pattern = outcome_byte,
                ExtractorKind::Classifier => slot.classifier = outcome_byte,
                ExtractorKind::Llm => slot.llm = outcome_byte,
            }
            if matches!(result.status, ExtractionStatus::Success) {
                slot.items.extend(result.items);
            } else if slot.failure_reason.is_none()
                && !matches!(result.status, ExtractionStatus::SkippedDisabled)
            {
                slot.failure_reason =
                    Some(format!("{:?}: {}", result.status, result.status_reason));
            }
        }
    }
}

/// Snapshot every memory's currently-accumulated items into
/// `prior_items` so the next tier's `ExtractionContext` can borrow
/// them. Replaces the prior snapshot rather than appending — each tier
/// sees the cumulative output of all prior tiers in source-order
/// stable form.
fn accumulate_into_prior(
    outcomes: &[PipelineOutcome],
    mems: &[CoreMemory],
    prior_items: &mut HashMap<MemoryId, Vec<ExtractedItem>>,
) {
    debug_assert_eq!(outcomes.len(), mems.len());
    for (i, mem) in mems.iter().enumerate() {
        prior_items.insert(mem.id, outcomes[i].items.clone());
    }
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

/// Summary of a successful `apply_outcome` commit. The cycle uses it
/// to populate the `ExtractedKnowledge` SUBSCRIBE event so clients
/// know exactly how many entities / statements / relations landed.
struct ApplyOutcome {
    counts: ExtractorItemCounts,
    status_byte: u8,
}

async fn apply_outcome(
    worker: &ExtractorWorker,
    ctx: &WorkerContext,
    memory_id: MemoryId,
    outcome: &PipelineOutcome,
) -> Result<ApplyOutcome, ApplyError> {
    let now = now_unix_nanos();
    let mut counts = ExtractorItemCounts::zero();
    let mut entity_map: HashMap<String, EntityId> = HashMap::new();
    // Collected during pass 2 when a freshly-written statement matches
    // the causal whitelist. Drained after `wtxn.commit()` to fan out
    // onto the CausalEdgeWorker channel — never before commit, so a
    // rolled-back txn never produces phantom enqueues.
    let mut causal_enqueues: Vec<brain_core::StatementId> = Vec::new();

    let mut db_guard = ctx.ops.executor.metadata.lock();
    let wtxn = db_guard
        .write_txn()
        .map_err(|e| ApplyError::Storage(format!("write_txn: {e:?}")))?;

    // Pass 1 — entity mentions, in source order. Resolving early gives
    // statements + relations a populated `entity_map` to look up
    // surface forms against.
    let embed_deps = worker.embed_deps.as_ref();
    let entity_disambiguator = worker.entity_disambiguator.as_deref();
    for item in &outcome.items {
        if let ExtractedItem::EntityMention(em) = item {
            let (entity_id, tier) =
                resolve_entity_mention(&wtxn, em, now, embed_deps, entity_disambiguator)?;
            worker
                .metrics
                .inc_resolver_outcome(resolution_tier_to_metric(tier));
            entity_map.insert(em.text.clone(), entity_id);
            write_mention_edge(&wtxn, memory_id, entity_id, em, now)?;
            // Bump the audit + metric counters for every successful
            // resolve (Exact / Alias / Fuzzy / Create). Splitting this
            // into "resolved (all)" vs "created (tier-4 only)" lives
            // behind brook plan §2.4 — it needs an
            // ExtractorItemCounts schema bump (the type is rkyv-archived
            // so adding fields is a stored-format change). Tracked as a
            // follow-up; today the single counter mirrors the rest of
            // the items_written metric path.
            counts.entities = counts.entities.saturating_add(1);
            if matches!(tier, ResolutionTier::Created) {
                worker
                    .metrics
                    .add_items_written(ExtractorItemKind::Entity, 1);
            }
            counts.mention_edges = counts.mention_edges.saturating_add(1);
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

                    // Resolve the LLM's proposed predicate against the
                    // schema. Three outcomes:
                    //   (a) qname is admissible (declared in active
                    //       schema, or schemaless namespace) → intern
                    //       the real predicate id; statement carries
                    //       the predicate's own qname (no override).
                    //   (b) qname is unknown → route to the
                    //       `brain:fact` wildcard sink. The original
                    //       qname is preserved on the row so a later
                    //       SCHEMA_UPLOAD can promote these to typed
                    //       predicates without re-extraction.
                    let (pid, original_predicate_qname, used_qname) =
                        if predicate_allowed_by_schema(&wtxn, ns, name)? {
                            let pid = predicate_intern_or_get(&wtxn, ns, name, 0, now)
                                .map_err(|e| ApplyError::Predicate(format!("{e}")))?;
                            (pid, None, (ns.to_string(), name.to_string()))
                        } else {
                            let original = format!("{ns}:{name}");
                            worker.metrics.inc_schema_filtered(&sm.predicate_qname);
                            tracing::info!(
                                target: "brain_workers::extractor",
                                memory_id = ?memory_id,
                                predicate = %original,
                                "predicate outside active schema; routing to brain:fact wildcard sink",
                            );
                            let pid =
                                predicate_lookup_by_qname_in_write_txn(&wtxn, "brain", "fact")?
                                    .ok_or_else(|| {
                                        ApplyError::Predicate(
                                            "system schema must declare brain:fact".into(),
                                        )
                                    })?;
                            (
                                pid,
                                Some(original),
                                ("brain".to_string(), "fact".to_string()),
                            )
                        };

                    // Per-statement statefulness: for the wildcard sink the
                    // LLM's per-mention signal is authoritative (brain:fact's
                    // registry row is the cumulative default by design). For
                    // declared predicates the schema-registered flag wins —
                    // it's what the operator committed to when uploading the
                    // schema and what supersession logic keys off.
                    let is_stateful = if original_predicate_qname.is_some() {
                        sm.is_stateful
                    } else {
                        predicate_is_stateful_in_write_txn(&wtxn, pid)?.unwrap_or(sm.is_stateful)
                    };

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
                        original_predicate_qname,
                        is_stateful,
                    };
                    match statement_create_internal(&wtxn, &payload) {
                        Ok(sid) => {
                            counts.statements = counts.statements.saturating_add(1);
                            worker
                                .metrics
                                .add_items_written(ExtractorItemKind::Statement, 1);
                            if let Some(feed) = worker.causal_edge.as_ref() {
                                if feed.whitelist_qnames.contains(&used_qname) {
                                    causal_enqueues.push(sid);
                                }
                            }
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
                        // Unknown relation types are dropped — relation
                        // graph semantics require a typed relation row,
                        // and statement-level promotion via brain:fact
                        // doesn't apply to two-endpoint relations.
                        // Surface at warn so an operator notices.
                        worker.metrics.inc_schema_filtered(&rm.relation_type_qname);
                        warn!(
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
    //
    // The audit row records this pipeline run's LLM cost (so per-row
    // forensics show "this memory cost N µ$"); the worker's per-cycle
    // accumulator drives the cross-memory budget gate separately.
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
        outcome.llm_cost_micro_usd,
    );
    record_extracted(&wtxn, &audit)
        .map_err(|e| ApplyError::Audit(format!("record_extracted: {e}")))?;

    wtxn.commit()
        .map_err(|e| ApplyError::Storage(format!("commit: {e:?}")))?;
    drop(db_guard);

    // Fan out to the CausalEdgeWorker only after the commit succeeds —
    // a rolled-back txn never produces phantom enqueues. The channel
    // is bounded; on `Full` we bump the drop counter and move on
    // (the statement is durable; only its derived edges are deferred).
    if let Some(feed) = worker.causal_edge.as_ref() {
        for sid in causal_enqueues {
            if let Err(err) = feed.sender.try_send(sid) {
                match err {
                    flume::TrySendError::Full(_) => {
                        feed.metrics.inc_drop();
                        warn!(
                            target: "brain_workers::extractor",
                            statement_id = ?sid,
                            "causal_edge channel full; dropping enqueue (statement still durable)",
                        );
                    }
                    flume::TrySendError::Disconnected(_) => {
                        // Worker shut down. Quiet — fires every drain
                        // during graceful shutdown.
                        trace!(
                            target: "brain_workers::extractor",
                            statement_id = ?sid,
                            "causal_edge receiver dropped",
                        );
                    }
                }
            }
        }
    }

    Ok(ApplyOutcome {
        counts,
        status_byte,
    })
}

/// Translate the worker's `pipeline_status` byte into the wire
/// [`StageAuditStatus`] carried on the `StageCompleted` event.
/// Unknown bytes round to `Failed`; the worker's `decide_status`
/// only ever emits one of the four documented constants, so the
/// fallback is strictly defensive.
fn audit_status_from_byte(byte: u8) -> StageAuditStatus {
    match byte {
        pipeline_status::SUCCESS => StageAuditStatus::Succeeded,
        pipeline_status::PARTIAL_FAILURE => StageAuditStatus::PartiallyApplied,
        pipeline_status::SKIPPED => StageAuditStatus::Skipped,
        _ => StageAuditStatus::Failed,
    }
}

/// Publish the `ExtractedKnowledge` SUBSCRIBE event onto the per-shard
/// bus. A no-op when no subscriber is listening — `events.publish`
/// still mints an LSN for the bus's own bookkeeping.
/// Publish a `StageCompleted{Extractor}` event so subscribers waiting
/// via `--wait` can decrement their pending-stage checklist for this
/// memory.
fn publish_extracted_knowledge(
    ctx: &WorkerContext,
    memory_id: MemoryId,
    counts: ExtractorItemCounts,
    audit_status: StageAuditStatus,
) {
    let outcome = match audit_status {
        StageAuditStatus::Succeeded | StageAuditStatus::PartiallyApplied => StageOutcome::Ok,
        StageAuditStatus::Skipped => StageOutcome::Empty,
        StageAuditStatus::Failed => StageOutcome::Failed,
    };
    tracing::info!(
        target: "brain_debug::extractor",
        memory_id = ?memory_id,
        audit_status = ?audit_status,
        outcome = ?outcome,
        entities = counts.entities,
        statements = counts.statements,
        relations = counts.relations,
        bus_subscriber_count = ctx.ops.events.subscriber_count(),
        "publish_extracted_knowledge: emitting StageCompleted",
    );
    let payload = StagePayload::Extractor(StageExtractorPayload {
        entity_count: counts.entities,
        statement_count: counts.statements,
        relation_count: counts.relations,
        audit_status,
        error_message: String::new(),
    });
    let envelope = EventEnvelope {
        lsn: 0,
        event_type: EventType::StageCompleted,
        memory_id,
        context_id: ContextId::default(),
        kind: MemoryKind::Episodic,
        salience: 0.0,
        timestamp_unix_nanos: now_unix_nanos(),
        text: None,
        knowledge_payload: None,
        edge_payload: None,
        stage_kind: Some(StageKind::Extractor),
        stage_outcome: Some(outcome),
        stage_payload: Some(payload),
        agent_id: AgentId::default(),
    };
    let _ = ctx.ops.events.publish(envelope);
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
    embed_deps: Option<&EmbeddingDeps>,
    entity_disambiguator: Option<&EntityDisambiguator>,
) -> Result<(EntityId, ResolutionTier), ApplyError> {
    let res = resolve_or_create_with_deps(
        wtxn,
        &em.text,
        &em.entity_type_qname,
        em.confidence,
        now,
        embed_deps,
        entity_disambiguator,
    )
    .map_err(ApplyError::from)?;
    Ok((res.entity_id, res.tier))
}

fn resolution_tier_to_metric(tier: ResolutionTier) -> ResolverOutcome {
    match tier {
        ResolutionTier::Exact => ResolverOutcome::Exact,
        ResolutionTier::Alias => ResolverOutcome::Alias,
        ResolutionTier::Fuzzy => ResolverOutcome::Fuzzy,
        ResolutionTier::Embedding => ResolverOutcome::Embedding,
        ResolutionTier::Disambiguated => ResolverOutcome::Disambiguated,
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

/// Look up a predicate by qname inside an active write transaction.
/// `predicate_lookup_by_qname` takes a `&ReadTransaction`; the
/// extractor pipeline holds a `&WriteTransaction` and needs to see
/// its own pending writes (e.g. when `brain:fact` was just interned
/// in the same txn during system-schema seeding). Iterates the
/// primary table — predicate count is small (< 100s typical).
fn predicate_lookup_by_qname_in_write_txn(
    wtxn: &redb::WriteTransaction,
    namespace: &str,
    name: &str,
) -> Result<Option<brain_core::PredicateId>, ApplyError> {
    let t = wtxn
        .open_table(PREDICATES_TABLE)
        .map_err(|e| ApplyError::Storage(format!("predicates open: {e}")))?;
    for entry in t
        .iter()
        .map_err(|e| ApplyError::Storage(format!("predicates iter: {e}")))?
    {
        let (_k, v) = entry.map_err(|e| ApplyError::Storage(format!("predicates entry: {e}")))?;
        let row: PredicateDefinition = v.value();
        if row.namespace == namespace && row.name == name {
            return Ok(Some(brain_core::PredicateId::from(row.predicate_id)));
        }
    }
    Ok(None)
}

/// Read the `is_stateful` flag from the predicate registry for an interned
/// predicate id, inside a live write transaction. Returns `None` if the row
/// has been deleted (shouldn't happen in normal use; callers fall back to the
/// extractor's per-mention signal).
fn predicate_is_stateful_in_write_txn(
    wtxn: &redb::WriteTransaction,
    pid: brain_core::PredicateId,
) -> Result<Option<bool>, ApplyError> {
    let t = wtxn
        .open_table(PREDICATES_TABLE)
        .map_err(|e| ApplyError::Storage(format!("predicates open: {e}")))?;
    let row = t
        .get(&pid.raw())
        .map_err(|e| ApplyError::Storage(format!("predicates get: {e}")))?;
    Ok(row.map(|g| g.value().is_stateful))
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

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(pattern: u8, classifier: u8, llm: u8) -> PipelineOutcome {
        PipelineOutcome {
            items: Vec::new(),
            pattern,
            classifier,
            llm,
            failure_reason: None,
            llm_cost_micro_usd: 0,
        }
    }

    /// Reproduces the user-visible regression: pattern produces 1
    /// entity, classifier is unconfigured (now SKIPPED, not FAILED),
    /// llm absent. The whole memory must classify as SUCCESS — a
    /// "partially applied" badge here would be a lie because nothing
    /// was dropped.
    #[test]
    fn pattern_succeeds_plus_classifier_skipped_classifies_as_success() {
        let o = outcome(tier_status::RAN, tier_status::SKIPPED, tier_status::ABSENT);
        let counts = ExtractorItemCounts {
            entities: 1,
            statements: 0,
            relations: 0,
            mention_edges: 1,
        };
        let (status, reason) = decide_status(&o, counts);
        assert_eq!(
            status,
            pipeline_status::SUCCESS,
            "skipped tiers must not turn a clean run into PARTIAL_FAILURE",
        );
        assert!(reason.is_empty());
    }

    /// A real tier-level error (not "not configured") still classifies
    /// as PARTIAL_FAILURE — admission semantics are unchanged for that
    /// case.
    #[test]
    fn pattern_succeeds_plus_classifier_errors_still_partial_failure() {
        let mut o = outcome(tier_status::RAN, tier_status::FAILED, tier_status::ABSENT);
        o.failure_reason = Some("Failure: classifier inference crashed".into());
        let counts = ExtractorItemCounts {
            entities: 1,
            statements: 0,
            relations: 0,
            mention_edges: 1,
        };
        let (status, reason) = decide_status(&o, counts);
        assert_eq!(status, pipeline_status::PARTIAL_FAILURE);
        assert!(reason.contains("classifier inference crashed"));
    }

    /// All tiers either absent or skipped, nothing produced → the
    /// memory's audit row reads as SKIPPED. Prevents a misleading
    /// SUCCESS audit when no work actually happened.
    #[test]
    fn all_tiers_skipped_or_absent_classifies_as_skipped() {
        let o = outcome(
            tier_status::SKIPPED,
            tier_status::SKIPPED,
            tier_status::ABSENT,
        );
        let (status, _) = decide_status(&o, ExtractorItemCounts::zero());
        assert_eq!(status, pipeline_status::SKIPPED);
    }

    /// The reported bug: classifier produces N entities cleanly,
    /// LLM tier is unconfigured (registered as `SkippedDisabled`,
    /// surfacing as `tier_status::SKIPPED`). The whole memory must
    /// classify as SUCCESS — the unconfigured tier never ran, so
    /// nothing was partially applied.
    #[test]
    fn classifier_succeeds_and_llm_unconfigured_audits_as_succeeded() {
        let o = outcome(tier_status::ABSENT, tier_status::RAN, tier_status::SKIPPED);
        let counts = ExtractorItemCounts {
            entities: 5,
            statements: 0,
            relations: 0,
            mention_edges: 5,
        };
        let (status, reason) = decide_status(&o, counts);
        assert_eq!(
            status,
            pipeline_status::SUCCESS,
            "unconfigured LLM tier must not flip a clean classifier run to PARTIAL_FAILURE",
        );
        assert!(reason.is_empty());
    }

    /// Classifier runs cleanly, LLM tier genuinely errored (network
    /// blew up, schema validation failed twice, …). That IS partial
    /// application — entities landed but the LLM-derived statements
    /// did not. PARTIAL_FAILURE is correct here.
    #[test]
    fn classifier_succeeds_and_llm_errored_audits_as_partially_applied() {
        let mut o = outcome(tier_status::ABSENT, tier_status::RAN, tier_status::FAILED);
        o.failure_reason = Some("Failure: llm rate-limited".into());
        let counts = ExtractorItemCounts {
            entities: 5,
            statements: 0,
            relations: 0,
            mention_edges: 5,
        };
        let (status, reason) = decide_status(&o, counts);
        assert_eq!(status, pipeline_status::PARTIAL_FAILURE);
        assert!(reason.contains("llm rate-limited"));
    }

    // ---------------------------------------------------------------
    // Batched classifier wiring: `run_pipeline_batch` must call the
    // classifier extractor's batched path exactly once per cycle,
    // regardless of how many memories were drained — that's the whole
    // point of restructuring the cycle to drain a micro-batch first.
    // ---------------------------------------------------------------

    use brain_core::{
        AgentId as TestAgentId, ContextId as TestContextId, MemoryId as TestMemoryId, MemoryKind,
        Salience,
    };
    use brain_extractors::{
        ClassifiedSpan, ClassifierExtractor, ClassifierModel, ExtractorError as TestExtractorError,
    };
    use brain_protocol::schema::ExtractorTarget;
    use std::sync::Arc;

    /// Test double that records every `predict` / `predict_batch` call
    /// so the assertion can prove the worker batched a multi-memory
    /// drain into one classifier forward pass.
    #[derive(Default)]
    struct BatchRecordingModel {
        per_row_calls: parking_lot::Mutex<usize>,
        batch_calls: parking_lot::Mutex<Vec<usize>>,
    }

    impl ClassifierModel for BatchRecordingModel {
        fn predict(
            &self,
            _text: &str,
            _labels: &[&str],
        ) -> Result<Vec<ClassifiedSpan>, TestExtractorError> {
            *self.per_row_calls.lock() += 1;
            Ok(Vec::new())
        }
        fn predict_batch(
            &self,
            inputs: &[(&str, &[&str])],
        ) -> Result<Vec<Vec<ClassifiedSpan>>, TestExtractorError> {
            self.batch_calls.lock().push(inputs.len());
            Ok(vec![Vec::new(); inputs.len()])
        }
        fn version(&self) -> &str {
            "batch-recording"
        }
    }

    fn make_mem(id_seq: u64, text: &str) -> CoreMemory {
        CoreMemory {
            id: TestMemoryId::pack(0, id_seq, 0),
            agent: TestAgentId::new(),
            context: TestContextId(0),
            kind: MemoryKind::Episodic,
            salience: Salience::default(),
            text: Some(text.into()),
            created_at_unix_ms: 0,
            last_accessed_at_unix_ms: 0,
        }
    }

    #[test]
    fn run_pipeline_batch_calls_predict_batch_once_for_classifier_across_multiple_memories() {
        let model = Arc::new(BatchRecordingModel::default());
        let classifier = Arc::new(ClassifierExtractor::new(
            brain_core::ExtractorId::from(7),
            "brain:gliner".into(),
            ExtractorTarget::Entity {
                entity_type: "brain:Person".into(),
            },
            1,
            0.5,
            model.clone(),
            Arc::new(vec!["brain:Person".into()]),
        ));

        let mems = vec![
            make_mem(1, "Alice met Bob"),
            make_mem(2, "Carol joined Acme"),
            make_mem(3, "Dave moved to Tokyo"),
            make_mem(4, "Eve started a project"),
            make_mem(5, "Frank wrote code"),
        ];

        let outcomes = futures_lite::future::block_on(run_pipeline_batch(
            vec![classifier as Arc<dyn Extractor>],
            &mems,
            false,
            None,
        ));
        assert_eq!(outcomes.len(), mems.len());

        let batch_calls = model.batch_calls.lock();
        let per_row_calls = *model.per_row_calls.lock();
        assert_eq!(
            batch_calls.len(),
            1,
            "classifier model must be called exactly once for the whole micro-batch; got {batch_calls:?}",
        );
        assert_eq!(
            batch_calls[0],
            mems.len(),
            "batched call must carry every memory in the micro-batch in one shot",
        );
        assert_eq!(
            per_row_calls, 0,
            "per-row predict must NOT fire when the worker drives run_batch on a ClassifierExtractor",
        );

        // Every outcome must register the classifier tier as RAN
        // (predict_batch returned Ok), with zero items.
        for o in &outcomes {
            assert_eq!(o.classifier, tier_status::RAN);
            assert!(o.items.is_empty());
        }
    }

    /// `run_pipeline_batch` must invoke the LLM tier with a populated
    /// `prior_tier_items` map after the classifier tier has produced
    /// entity mentions. This is the user-visible contract that lets
    /// the LLM anchor its prompt on canonical names — if the map is
    /// empty here, the LLM will re-extract or hallucinate.
    #[test]
    fn cycle_passes_classifier_entities_to_llm_extractor() {
        use brain_core::ExtractorKind;
        use brain_core::{ExtractorId as TestExtractorId, MemoryId};
        use brain_extractors::{
            EntityMention as TestEntityMention, ExtractedItem as TestExtractedItem,
            ExtractionFuture, ExtractionResult, Extractor as TestExtractor,
        };
        use std::collections::HashMap as TestHashMap;
        use std::sync::Mutex as StdMutex;

        // Classifier double that returns a fixed pair of entity mentions
        // so the LLM tier sees a known input.
        struct StubClassifier {
            id: TestExtractorId,
            name: String,
        }
        impl TestExtractor for StubClassifier {
            fn id(&self) -> TestExtractorId {
                self.id
            }
            fn kind(&self) -> ExtractorKind {
                ExtractorKind::Classifier
            }
            fn name(&self) -> &str {
                &self.name
            }
            fn extractor_version(&self) -> u32 {
                1
            }
            fn run<'a>(
                &'a self,
                _ctx: &'a ExtractionContext<'a>,
                _mem: &'a CoreMemory,
            ) -> ExtractionFuture<'a> {
                Box::pin(async move {
                    let items = vec![
                        TestExtractedItem::EntityMention(TestEntityMention {
                            entity_type_qname: "brain:Person".into(),
                            text: "Alice Wong".into(),
                            start: 0,
                            end: 10,
                            confidence: 0.95,
                            extractor_id: 7,
                            extractor_version: 1,
                        }),
                        TestExtractedItem::EntityMention(TestEntityMention {
                            entity_type_qname: "brain:Organization".into(),
                            text: "Acme Corp".into(),
                            start: 20,
                            end: 29,
                            confidence: 0.93,
                            extractor_id: 7,
                            extractor_version: 1,
                        }),
                    ];
                    ExtractionResult::success(items, 0, 0)
                })
            }
        }

        // LLM double that snapshots `ctx.prior_tier_items` so the test
        // can assert exactly what the LLM tier observed.
        type SeenPriors = Arc<StdMutex<Option<TestHashMap<MemoryId, Vec<TestExtractedItem>>>>>;
        struct RecordingLlm {
            id: TestExtractorId,
            name: String,
            seen_priors: SeenPriors,
        }
        impl TestExtractor for RecordingLlm {
            fn id(&self) -> TestExtractorId {
                self.id
            }
            fn kind(&self) -> ExtractorKind {
                ExtractorKind::Llm
            }
            fn name(&self) -> &str {
                &self.name
            }
            fn extractor_version(&self) -> u32 {
                1
            }
            fn run<'a>(
                &'a self,
                ctx: &'a ExtractionContext<'a>,
                _mem: &'a CoreMemory,
            ) -> ExtractionFuture<'a> {
                let snap = ctx
                    .prior_tier_items
                    .map(|m| m.iter().map(|(k, v)| (*k, v.clone())).collect());
                let store = self.seen_priors.clone();
                Box::pin(async move {
                    *store.lock().unwrap() = snap;
                    ExtractionResult::success(Vec::new(), 0, 0)
                })
            }
        }

        let seen = Arc::new(StdMutex::new(None));
        let classifier: Arc<dyn TestExtractor> = Arc::new(StubClassifier {
            id: TestExtractorId::from(101),
            name: "stub:classifier".into(),
        });
        let llm: Arc<dyn TestExtractor> = Arc::new(RecordingLlm {
            id: TestExtractorId::from(102),
            name: "stub:llm".into(),
            seen_priors: seen.clone(),
        });

        let mems = vec![make_mem(1, "Alice Wong works at Acme Corp.")];

        let _ = futures_lite::future::block_on(run_pipeline_batch(
            vec![llm, classifier], // intentional reverse order — pipeline must reorder.
            &mems,
            false,
            None,
        ));

        let observed = seen.lock().unwrap().clone().expect(
            "LLM tier must have seen `prior_tier_items = Some(_)` after the classifier tier ran",
        );
        let mid = mems[0].id;
        let items = observed
            .get(&mid)
            .expect("classifier output for this memory must be in the prior-items map");
        assert_eq!(
            items.len(),
            2,
            "LLM tier must see exactly the two entities the classifier produced",
        );
        let surfaces: Vec<&str> = items
            .iter()
            .filter_map(|i| match i {
                TestExtractedItem::EntityMention(em) => Some(em.text.as_str()),
                _ => None,
            })
            .collect();
        assert!(surfaces.contains(&"Alice Wong"));
        assert!(surfaces.contains(&"Acme Corp"));
    }

    #[test]
    fn batch_size_env_parses_valid_positive_usize() {
        assert_eq!(parse_batch_size_raw(Some("16")), 16);
        assert_eq!(parse_batch_size_raw(Some("1")), 1);
    }

    #[test]
    fn batch_size_env_unset_or_empty_falls_back_to_default() {
        assert_eq!(parse_batch_size_raw(None), DEFAULT_EXTRACTOR_BATCH_SIZE);
        assert_eq!(parse_batch_size_raw(Some("")), DEFAULT_EXTRACTOR_BATCH_SIZE);
    }

    #[test]
    fn batch_size_env_zero_or_garbage_falls_back_to_default_with_warn() {
        assert_eq!(
            parse_batch_size_raw(Some("0")),
            DEFAULT_EXTRACTOR_BATCH_SIZE
        );
        assert_eq!(
            parse_batch_size_raw(Some("not-a-number")),
            DEFAULT_EXTRACTOR_BATCH_SIZE
        );
        assert_eq!(
            parse_batch_size_raw(Some("-3")),
            DEFAULT_EXTRACTOR_BATCH_SIZE
        );
    }
}
