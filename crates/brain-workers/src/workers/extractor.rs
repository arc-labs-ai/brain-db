//! `ExtractorWorker` — drains the post-ENCODE extractor queue and
//! materialises entity / statement / relation / mention-edge rows
//! from the three-tier extractor framework's output.
//!
//! ## Why this exists
//!
//! Before this worker landed, ENCODE wrote a memory row and a vector
//! into HNSW and stopped there. The phase bodies's entities,
//! statements, and relations only appeared when an operator hand-wrote
//! them through `ENTITY_CREATE` / `STATEMENT_CREATE` / `RELATION_CREATE`
//! wire ops. ExtractorWorker turns ENCODE into a typed-graph-rich
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

use crate::workers::hype::HypeGenerator;
use brain_core::{
    AgentId, ContextId, EntityId, ExtractorId, Memory as CoreMemory, MemoryId, MemoryKind, Salience,
};
use brain_core::{StatementKind, StatementObject, StatementValue, SubjectRef};
use brain_extractors::{
    resolver::{
        resolve_or_create_with_deps, EmbeddingDeps, EntityDisambiguator, ResolutionTier,
        ResolverError,
    },
    EntityMention, ExtractedItem, ExtractionContext, ExtractionFailureClass, ExtractionResult,
    ExtractionStatus, Extractor, ExtractorContext, ExtractorRegistry, StatementMention,
};
use brain_metadata::relation::types::relation_type_intern_or_get;
use brain_metadata::schema::predicate::predicate_intern_or_get;
use brain_metadata::tables::edge::{
    self, derived_by, origin, zero_disambiguator, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE,
};
use brain_metadata::tables::extractor_audit::{
    pipeline_status, record_extracted, tier_status, ExtractorItemCounts,
    ExtractorPipelineAuditEntry,
};
use brain_metadata::tables::predicate::{PREDICATES_TABLE, PREDICATE_EMBEDDINGS_TABLE};
use brain_metadata::tables::relation::RELATION_TYPE_EMBEDDINGS_TABLE;
use brain_metadata::{hype_has_vectors, pipeline_has_extracted};
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
    /// the LLM tier until the next cycle. This is an observability stub
    /// for now — the framework's per-call budget
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
    /// `[workers.extractor] batch_size` in the server config for ops
    /// who need to balance throughput against per-encode tail latency.
    pub batch_size: usize,
    /// Memories examined per cycle by the graph-aware HyPE refresh sweep —
    /// the Phase-3 path that regenerates a memory's hypothetical questions
    /// once its typed-graph neighborhood has grown (so multi-hop bridge
    /// questions become writable after the connecting facts land). Each
    /// examined memory is a cheap neighborhood-hash check; only the ones
    /// whose neighborhood actually changed cost an LLM call, and those are
    /// still bounded by `llm_budget_per_cycle_micro_usd`. `0` disables the
    /// sweep. A round-robin cursor advances across cycles so the whole
    /// corpus is revisited over time.
    pub hype_refresh_per_cycle: usize,
}

pub const DEFAULT_EXTRACTOR_DRAIN_PER_CYCLE: usize = 32;
pub const DEFAULT_EXTRACTOR_LLM_BUDGET_MICRO_USD: u64 = 50_000;
pub const DEFAULT_EXTRACTOR_SKIP_AUDITED: bool = true;
/// Memories examined per cycle by the HyPE refresh sweep. Small: the sweep
/// is a steady background trickle, and each changed neighborhood costs an
/// LLM call against the shared per-cycle budget.
pub const DEFAULT_EXTRACTOR_HYPE_REFRESH_PER_CYCLE: usize = 8;
/// Memories per classifier forward pass. 8 is the sweet spot on the
/// dev container's CPU: the backbone GEMM saturates well before then,
/// and going higher adds latency without throughput gains. Bigger
/// hosts can lift this via `[workers.extractor] batch_size`.
pub const DEFAULT_EXTRACTOR_BATCH_SIZE: usize = 8;

impl Default for ExtractorKnobs {
    fn default() -> Self {
        Self {
            drain_per_cycle: DEFAULT_EXTRACTOR_DRAIN_PER_CYCLE,
            llm_budget_per_cycle_micro_usd: DEFAULT_EXTRACTOR_LLM_BUDGET_MICRO_USD,
            skip_already_extracted: DEFAULT_EXTRACTOR_SKIP_AUDITED,
            batch_size: DEFAULT_EXTRACTOR_BATCH_SIZE,
            hype_refresh_per_cycle: DEFAULT_EXTRACTOR_HYPE_REFRESH_PER_CYCLE,
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
    /// `EntityDisambiguator` built from the shared LLM client when the
    /// single credential (`[llm] api_key` / `BRAIN__LLM__API_KEY`) is
    /// present at startup.
    entity_disambiguator: Option<Arc<EntityDisambiguator>>,
    /// Optional write-time HyPE generator. `None` (the default) skips
    /// hypothetical-question generation entirely — substrate-only
    /// deployments, tests, and any shard without the LLM tier leave it
    /// unset. Production shards stamp it when the LLM tier is provisioned
    /// and `[extractors.hype]` is enabled.
    hype: Option<HypeGenerator>,
    /// Round-robin cursor for the HyPE refresh sweep: the last memory key
    /// examined, so each cycle resumes after it and the whole corpus is
    /// revisited over time. Wraps to the start of `TEXTS_TABLE` at the end.
    /// `Mutex` for the same reason as `llm_spend` — the `&self` cycle mutates
    /// it with no real contention (one shard, one drainer).
    refresh_cursor: Mutex<[u8; 16]>,
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
            hype: None,
            refresh_cursor: Mutex::new([0u8; 16]),
        }
    }

    /// Wire the write-time HyPE generator. Without this call the worker
    /// generates no hypothetical-question embeddings. Production shards
    /// set it when the LLM tier is provisioned and `[extractors.hype]` is
    /// enabled; tests and substrate-only deployments leave it unset.
    #[must_use]
    pub fn with_hype(mut self, hype: HypeGenerator) -> Self {
        self.hype = Some(hype);
        self
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

    // The durable work surface is `EXTRACTION_QUEUE_TABLE`, not the
    // flume channel. The channel is only a low-latency wakeup hint: a
    // single encode signals it so the worker doesn't wait a full
    // interval to pick the new memory up. Block on the channel OR the
    // interval timer for the cycle's wakeup, then read the actual work
    // list from the durable table. On restart there is no channel
    // backlog, but the table still holds every un-extracted memory —
    // draining the table each cycle makes the worker naturally
    // resumable with no special recovery path.
    {
        let recv = async {
            let _ = worker.queue.recv_async().await;
        };
        let tick = async {
            sleep(cfg.interval).await;
        };
        recv.or(tick).await;
    }
    // Drain whatever else is on the channel so it doesn't accumulate;
    // the contents are ignored — text comes from TEXTS_TABLE, the work
    // list from the durable queue.
    while worker.queue.try_recv().is_ok() {}

    while processed < cycle_cap {
        if started.elapsed() >= cfg.max_runtime {
            break;
        }
        if ctx.is_shutdown() {
            break;
        }

        // Pull the next micro-batch of pending memory ids from the
        // durable queue, bounded by what's left of the cycle cap.
        let want = micro_batch.min(cycle_cap - processed);
        let pending = match load_pending_batch(ctx, want) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    target: "brain_workers::extractor",
                    error = %e,
                    "extraction queue drain failed; ending cycle",
                );
                break;
            }
        };
        if pending.is_empty() {
            break;
        }

        // Resolve each pending id to its text from TEXTS_TABLE. A
        // missing text means the memory was forgotten before extraction
        // ran — remove the stale queue row and skip it.
        let mut micro: Vec<ExtractorEnqueue> = Vec::with_capacity(pending.len());
        for memory_id in pending {
            match load_memory_text(ctx, memory_id) {
                Ok(Some(text)) => micro.push((memory_id, text)),
                Ok(None) => {
                    trace!(
                        memory_id = ?memory_id,
                        "queued memory has no text (forgotten?); removing stale queue row",
                    );
                    if let Err(e) = remove_queue_row(ctx, memory_id) {
                        warn!(memory_id = ?memory_id, error = %e, "queue row remove failed");
                    }
                    processed += 1;
                }
                Err(e) => {
                    warn!(memory_id = ?memory_id, error = %e, "text load failed; leaving in queue");
                    processed += 1;
                }
            }
        }
        if micro.is_empty() {
            // Every id in this batch resolved to a stale/erroring row.
            // Loop again to make progress on the rest of the queue.
            continue;
        }

        // Run the whole micro-batch through one pipeline invocation.
        // `drain_batch` returns one StageDecision per memory in input
        // order so we can publish per-memory StageCompleted exactly
        // once even when the batched classifier pass amortises across
        // multiple memories.
        let batch_decisions = drain_batch(worker, ctx, &micro).await;
        for ((memory_id, _), decision) in micro.iter().zip(batch_decisions) {
            // Remove the durable queue row only for outcomes that reached a
            // terminal state. Failure variants stay queued for a retry:
            // GateFailed (read_txn/audit probe errored before any work),
            // AppliedFailed (a transient apply error that rolled back without
            // writing an audit row), and Applied{retry_pending} (a transient
            // LLM-tier timeout under the attempt budget — the audit row is
            // written to advance the attempt count, but the queue row is kept
            // so the next cycle re-runs the LLM tier). A terminal Applied
            // commits an audit row the idempotency gate honors; AlreadyExtracted is a
            // stale row left by a crash between commit and remove — the remove
            // here is the cleanup. A crash between the audit commit and this
            // remove leaves a stale row the next cycle folds into
            // AlreadyExtracted and removes — at-least-once with idempotent
            // extraction.
            let keep_queued = matches!(
                decision,
                StageDecision::GateFailed
                    | StageDecision::AppliedFailed
                    | StageDecision::Applied {
                        retry_pending: true,
                        ..
                    }
            );
            let (counts, audit_status) = match decision {
                StageDecision::Applied {
                    counts,
                    status_byte,
                    ..
                } => (counts, audit_status_from_byte(status_byte)),
                StageDecision::AppliedFailed | StageDecision::GateFailed => {
                    (ExtractorItemCounts::zero(), StageAuditStatus::Failed)
                }
                StageDecision::AlreadyExtracted => {
                    (ExtractorItemCounts::zero(), StageAuditStatus::Skipped)
                }
            };
            if !keep_queued {
                if let Err(e) = remove_queue_row(ctx, *memory_id) {
                    warn!(memory_id = ?memory_id, error = %e, "queue row remove failed");
                }
            }
            publish_extracted_graph(ctx, *memory_id, counts, audit_status);
        }
        processed += micro.len();

        // Cooperative yield after every micro-batch so the scheduler
        // stays responsive even when a batch lands in one tick.
        glommio::executor().yield_if_needed().await;
    }

    // Graph-aware HyPE refresh: with the cycle's extraction committed, spend
    // any remaining LLM budget revisiting already-encoded memories whose
    // typed-graph neighborhood has since grown, regenerating their bridge
    // questions. Bounded per cycle and budget-gated, so it's a steady
    // background trickle that converges as the graph stabilises.
    run_hype_refresh_sweep(worker, ctx).await;

    let elapsed = started.elapsed();
    worker.metrics.observe_cycle_duration(elapsed.as_secs_f64());
    trace!(
        drained = processed,
        cycle_ms = elapsed.as_millis() as u64,
        "extractor cycle",
    );
    Ok(processed)
}

/// Read up to `limit` pending memory ids from the durable extraction
/// queue. The queue is the source of truth for "needs extraction";
/// draining it each cycle is what makes the worker resumable across
/// restarts.
fn load_pending_batch(ctx: &WorkerContext, limit: usize) -> Result<Vec<MemoryId>, String> {
    let rtxn = ctx
        .ops
        .executor
        .metadata
        .read_txn()
        .map_err(|e| format!("extraction queue read_txn: {e}"))?;
    // Over-drain, then drop ids whose transient-failure backoff hasn't
    // elapsed: a memory that just failed transiently stays queued but is
    // skipped until its retry is due, so a provider outage retries on a
    // widening interval instead of every cycle. Terminal (permanent / non-LLM)
    // failures are reported "due" here and handled by the idempotency gate in
    // `drain_batch` (AlreadyExtracted → queue row removed). First-time and
    // succeeded-then-requeued ids are always due.
    let now = now_unix_nanos();
    let pending = brain_metadata::extraction_queue_drain(&rtxn, limit)
        .map_err(|e| format!("extraction_queue_drain: {e}"))?;
    let mut due = Vec::with_capacity(pending.len());
    for (id, _) in pending {
        match brain_metadata::pipeline_extraction_retry_due(&rtxn, id, now) {
            Ok(true) => due.push(id),
            Ok(false) => {} // backing off — leave queued, retry when due
            Err(e) => {
                // Probe error shouldn't strand the memory: treat as due so it
                // gets a chance rather than silently stalling forever.
                warn!(memory_id = ?id, error = %e, "retry-due probe failed; treating as due");
                due.push(id);
            }
        }
    }
    Ok(due)
}

/// Load a memory's text from `TEXTS_TABLE`. `Ok(None)` means the row is
/// absent (the memory was forgotten before extraction ran); the caller
/// removes the stale queue row and skips.
fn load_memory_text(ctx: &WorkerContext, memory_id: MemoryId) -> Result<Option<Arc<str>>, String> {
    use brain_metadata::tables::text::TEXTS_TABLE;
    let rtxn = ctx
        .ops
        .executor
        .metadata
        .read_txn()
        .map_err(|e| format!("text read_txn: {e}"))?;
    let t = rtxn
        .open_table(TEXTS_TABLE)
        .map_err(|e| format!("open TEXTS: {e}"))?;
    let row = t
        .get(&memory_id.to_be_bytes())
        .map_err(|e| format!("TEXTS get: {e}"))?;
    Ok(row.map(|g| {
        let s = String::from_utf8_lossy(g.value());
        Arc::from(s.as_ref())
    }))
}

/// Remove a memory's durable extraction-queue row in its own small
/// write txn. Called after the memory's extraction has committed (or
/// when the row is stale). Idempotent — a missing row is a no-op.
fn remove_queue_row(ctx: &WorkerContext, memory_id: MemoryId) -> Result<(), String> {
    let wtxn = ctx
        .ops
        .executor
        .metadata
        .write_txn()
        .map_err(|e| format!("queue remove write_txn: {e:?}"))?;
    brain_metadata::extraction_queue_remove(&wtxn, memory_id)
        .map_err(|e| format!("extraction_queue_remove: {e}"))?;
    wtxn.commit()
        .map_err(|e| format!("queue remove commit: {e:?}"))?;
    Ok(())
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
        /// A retryable LLM-tier failure under the attempt budget: the
        /// audit row is written (advancing the attempt count) but the
        /// queue row is KEPT so the next cycle re-extracts.
        retry_pending: bool,
    },
    /// `apply_outcome` returned a transient error and rolled back without
    /// persisting anything (no audit row written). The durable queue row is
    /// left in place so the next cycle retries; the publish records `Failed`
    /// with zero counts so subscribers unblock.
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
                let db_guard = ctx.ops.executor.metadata.as_ref();
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

    // HyPE generation is deferred to AFTER this batch's extraction (see the
    // `run_hype_pass` call at the end). It is graph-aware: each memory's
    // hypothetical questions are conditioned on the typed-graph neighborhood of
    // the entities it mentions, so they bridge across connected facts. Running
    // it post-extraction means the batch's own freshly-written statements and
    // relations are already in the graph, so a memory's bridge questions can
    // chain through facts that arrived in the same batch — not just prior ones.
    // It still runs over EVERY item (not just `live`): HyPE is independent of
    // the extraction-audit gate, so an already-extracted memory missing its
    // question vectors (e.g. HyPE added after it was first extracted) still
    // gets them. Idempotent on the memory's own vector presence.
    if live.is_empty() {
        run_hype_pass(worker, ctx, items).await;
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

    // Fetch each live memory's event/write timestamps so the temporal
    // extractor can anchor relative dates ("last week") to the real
    // event time (`occurred_at`, else `created_at`) rather than to zero.
    let ts_by_id: std::collections::HashMap<MemoryId, (u64, Option<u64>)> = {
        use brain_metadata::tables::memory::MEMORIES_TABLE;
        let mut m = std::collections::HashMap::with_capacity(live.len());
        if let Ok(rtxn) = ctx.ops.executor.metadata.read_txn() {
            if let Ok(t) = rtxn.open_table(MEMORIES_TABLE) {
                for (_, mid, _) in &live {
                    if let Ok(Some(g)) = t.get(&mid.to_be_bytes()) {
                        let row = g.value();
                        m.insert(
                            *mid,
                            (row.created_at_unix_nanos, row.occurred_at_unix_nanos),
                        );
                    }
                }
            }
        }
        m
    };

    let live_mems: Vec<CoreMemory> = live
        .iter()
        .map(|(_, mid, text)| {
            let (created_ns, occurred) = ts_by_id.get(mid).copied().unwrap_or((0, None));
            CoreMemory {
                id: *mid,
                agent: AgentId::new(),
                context: ContextId(0),
                kind: MemoryKind::Episodic,
                salience: Salience::default(),
                text: Some(text.to_string()),
                created_at_unix_ms: created_ns / 1_000_000,
                last_accessed_at_unix_ms: 0,
                occurred_at_unix_nanos: occurred,
            }
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

    // Snapshot the active schema's predicates as the closed-vocab prompt
    // block for the LLM tier. Read per cycle so a user's SCHEMA_UPLOAD
    // takes effect on the next batch without a restart. Empty string on
    // any read error degrades to an unconstrained prompt rather than
    // failing extraction.
    //
    // Snapshot the kind taxonomy alongside the predicates from the same
    // read txn so both prompt blocks reflect a single consistent schema
    // view; same per-cycle freshness + degrade-to-empty semantics.
    //
    // The classifier (GLiNER) tier's entity-type label set is read from the
    // same txn so a `SCHEMA_UPLOAD` that adds entity types reaches the
    // classifier on the next batch without a shard restart (the labels baked
    // in at spawn are only the fallback). Empty on read error → the classifier
    // falls back to its construction-time labels.
    let (declared_predicates_block, declared_kinds_block, entity_type_labels): (
        String,
        String,
        Vec<String>,
    ) = match ctx.ops.executor.metadata.read_txn() {
        Ok(rtxn) => (
            brain_metadata::render_declared_predicates_block(&rtxn).unwrap_or_default(),
            brain_metadata::render_declared_kinds_block(&rtxn).unwrap_or_default(),
            brain_metadata::entity_type_label_qnames(&rtxn).unwrap_or_default(),
        ),
        Err(_) => (String::new(), String::new(), Vec::new()),
    };
    let entity_type_labels = if entity_type_labels.is_empty() {
        None
    } else {
        Some(entity_type_labels.as_slice())
    };
    let declared_predicates = if declared_predicates_block.is_empty() {
        None
    } else {
        Some(declared_predicates_block.as_str())
    };
    let declared_kinds = if declared_kinds_block.is_empty() {
        None
    } else {
        Some(declared_kinds_block.as_str())
    };

    let outcomes = run_pipeline_batch(
        extractors,
        &live_mems,
        skip_llm_budget_exhausted,
        extractor_context_map,
        declared_predicates,
        declared_kinds,
        entity_type_labels,
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
                retry_pending: applied.retry_pending,
            },
            Err(e) => {
                // A per-item data-shape problem (bad predicate, rejected
                // create) is handled inside `apply_outcome` (skip + count),
                // so a returned Err is a TRANSIENT infra failure: the apply
                // wtxn rolled back, nothing persisted. Do NOT write a FAILURE
                // audit — that would bar re-extraction via the idempotency
                // gate and permanently abandon a real memory's graph for a
                // momentary hiccup. Leave the durable queue row in place so
                // the next cycle retries.
                warn!(
                    memory_id = ?memory_id,
                    error = %e,
                    "extractor apply failed (transient); leaving queued for retry",
                );
                StageDecision::AppliedFailed
            }
        };
        decisions[idx] = decision;
    }

    // Now that the batch's entities, statements, and relations are written,
    // generate graph-aware HyPE questions over every item — the neighborhood
    // each memory is conditioned on now reflects the just-applied graph.
    run_hype_pass(worker, ctx, items).await;

    decisions
}

/// Write-time HyPE generation over a whole batch. HyPE is a core,
/// always-on recall feature, INDEPENDENT of typed-graph extraction: it
/// needs only the memory text, so it runs for every memory regardless of
/// whether the extraction-audit gate skipped it (a re-ingest of an
/// already-extracted memory still needs its question vectors).
///
/// Idempotent on the memory's OWN question-vector presence — never on the
/// extraction audit row — so a re-run skips memories that already have
/// vectors rather than double-inserting them into the live index. Shares
/// the per-cycle LLM budget with the extractor tiers; once spent, the rest
/// pick up on a later cycle. A no-op when HyPE has no provider (substrate
/// deployment) — `worker.hype` is then `None`.
async fn run_hype_pass(worker: &ExtractorWorker, ctx: &WorkerContext, items: &[ExtractorEnqueue]) {
    let Some(hype) = worker.hype.as_ref() else {
        return;
    };
    let cycle_budget = worker.knobs.llm_budget_per_cycle_micro_usd;
    for (memory_id, text) in items {
        if cycle_budget > 0 && *worker.llm_spend.lock() >= cycle_budget {
            break;
        }
        // Skip memories that already own question vectors — HyPE generates
        // once per memory, idempotent across re-ingest, and a second insert
        // would duplicate vectors in the live HNSW index.
        let already = match ctx.ops.executor.metadata.as_ref().read_txn() {
            Ok(rtxn) => hype_has_vectors(&rtxn, *memory_id).unwrap_or(false),
            Err(_) => false,
        };
        if already {
            continue;
        }
        // Render the typed-graph facts already known about the entities this
        // memory mentions, so HyPE can write multi-hop bridge questions that
        // span more than this one memory (read-side multi-hop then resolves to
        // a single cheap ANN probe — no read LLM). Empty for the first memory
        // about a subject; fills in as the graph grows (and on re-ingest).
        let neighborhood = build_neighborhood(ctx, text.as_ref());
        let outcome = hype
            .generate_for(*memory_id, text.as_ref(), &neighborhood)
            .await;
        if outcome.cost_micro_usd > 0 {
            let mut spend = worker.llm_spend.lock();
            *spend = spend.saturating_add(outcome.cost_micro_usd);
            worker.metrics.add_llm_micro_usd(outcome.cost_micro_usd);
        }
        if outcome.questions_written > 0 {
            // Fold into items_written so the eval drain barrier waits for
            // HyPE, not just entity/statement extraction.
            worker
                .metrics
                .add_items_written(ExtractorItemKind::HyPe, outcome.questions_written as u64);
        }
    }
}

/// Graph-aware HyPE refresh sweep — the Phase-3 path that keeps a memory's
/// hypothetical questions in step with a growing typed graph.
///
/// A memory is first HyPE'd at encode time against whatever neighborhood
/// existed then (often empty — the connecting facts arrive in later memories).
/// This sweep revisits already-encoded memories in round-robin, recomputes each
/// one's current neighborhood, and asks [`HypeGenerator::refresh_for`] to
/// regenerate **only when the neighborhood actually changed** (cheap
/// hash-compare otherwise). That is what lets a multi-hop bridge question —
/// "Where did Niraj's manager work before?" — become writable once the
/// reports-to and prior-employer facts both exist, even though they landed in
/// separate memories at different times.
///
/// Bounded two ways: at most `hype_refresh_per_cycle` memories examined per
/// cycle (a round-robin cursor advances across cycles so the whole corpus is
/// revisited), and every regeneration is charged against the same per-cycle
/// LLM budget the extraction tiers use — once spent, the rest wait for a later
/// cycle. No-op when HyPE has no provider or the knob is `0`.
async fn run_hype_refresh_sweep(worker: &ExtractorWorker, ctx: &WorkerContext) {
    use brain_metadata::tables::text::TEXTS_TABLE;

    let Some(hype) = worker.hype.as_ref() else {
        return;
    };
    let limit = worker.knobs.hype_refresh_per_cycle;
    if limit == 0 {
        return;
    }
    let cycle_budget = worker.knobs.llm_budget_per_cycle_micro_usd;
    if cycle_budget > 0 && *worker.llm_spend.lock() >= cycle_budget {
        return;
    }

    // Scan a bounded window of (memory_id, text) starting strictly after the
    // cursor. Read into an owned Vec and drop the txn before any async work —
    // a read txn must never be held across the refresh `.await`.
    let start = *worker.refresh_cursor.lock();
    let mut batch: Vec<(MemoryId, String)> = Vec::with_capacity(limit);
    let mut wrapped = false;
    {
        let Ok(rtxn) = ctx.ops.executor.metadata.read_txn() else {
            return;
        };
        let Ok(t) = rtxn.open_table(TEXTS_TABLE) else {
            return;
        };
        // Exclusive lower bound: keys strictly greater than the cursor.
        let lo = {
            let mut k = start;
            // Increment the 16-byte key by one to make the bound exclusive;
            // on all-0xFF (vanishingly unlikely) just reuse it — a duplicate
            // examine is harmless.
            for byte in k.iter_mut().rev() {
                if *byte == u8::MAX {
                    *byte = 0;
                } else {
                    *byte += 1;
                    break;
                }
            }
            k
        };
        if let Ok(iter) = t.range(lo..) {
            for entry in iter.flatten() {
                let (k, v) = entry;
                let mut id = [0u8; 16];
                id.copy_from_slice(&k.value());
                batch.push((
                    MemoryId::from_be_bytes(id),
                    String::from_utf8_lossy(v.value()).into_owned(),
                ));
                if batch.len() >= limit {
                    break;
                }
            }
        }
        // If the window didn't fill, wrap: take from the start of the table to
        // complete the round, so a small corpus is fully revisited each cycle.
        if batch.len() < limit {
            wrapped = true;
            if let Ok(iter) = t.range([0u8; 16]..) {
                for entry in iter.flatten() {
                    let (k, v) = entry;
                    let mut id = [0u8; 16];
                    id.copy_from_slice(&k.value());
                    let mid = MemoryId::from_be_bytes(id);
                    if batch.iter().any(|(seen, _)| *seen == mid) {
                        continue;
                    }
                    batch.push((mid, String::from_utf8_lossy(v.value()).into_owned()));
                    if batch.len() >= limit {
                        break;
                    }
                }
            }
        }
    }
    if batch.is_empty() {
        return;
    }

    let mut last_examined = start;
    for (memory_id, text) in &batch {
        if cycle_budget > 0 && *worker.llm_spend.lock() >= cycle_budget {
            break;
        }
        let neighborhood = build_neighborhood(ctx, text.as_str());
        if neighborhood.is_empty() {
            last_examined = memory_id.to_be_bytes();
            continue;
        }
        let outcome = hype
            .refresh_for(*memory_id, text.as_str(), &neighborhood)
            .await;
        if outcome.cost_micro_usd > 0 {
            let mut spend = worker.llm_spend.lock();
            *spend = spend.saturating_add(outcome.cost_micro_usd);
            worker.metrics.add_llm_micro_usd(outcome.cost_micro_usd);
        }
        if outcome.questions_written > 0 {
            worker
                .metrics
                .add_items_written(ExtractorItemKind::HyPe, outcome.questions_written as u64);
        }
        last_examined = memory_id.to_be_bytes();
    }

    // Advance the cursor: to the last memory examined, or reset to the start
    // when this round wrapped past the end (so the next cycle begins a fresh
    // pass rather than re-scanning the tail).
    *worker.refresh_cursor.lock() = if wrapped { [0u8; 16] } else { last_examined };
}

/// Render a terse, bounded view of the typed-graph facts already stored about
/// the entities this memory's text mentions — the "neighborhood" fed to HyPE so
/// it can write multi-hop bridge questions.
///
/// Discovery mirrors the read-side graph anchor: mine candidate surfaces from
/// the text (capitalized runs + individual tokens) and resolve each against the
/// canonical-name index. A surface that names no entity simply fails to resolve
/// and is harmless; there is no hardcoded vocabulary. For every resolved entity
/// we render its current statements (predicate-keyed value/entity facts) and
/// its current relation edges (both directions) as one fact per line.
///
/// Best-effort and strictly bounded: any error yields an empty string (the
/// pre-graph-aware behavior), and the entity / line / character caps keep the
/// HyPE prompt from ballooning on a densely-connected hub.
fn build_neighborhood(ctx: &WorkerContext, text: &str) -> String {
    use brain_metadata::{
        entity_get, entity_resolve_canonical_all_types, predicate_get, relation_list_from,
        relation_list_to, relation_type_get, statement_list, RelationListFilter,
        StatementListFilter,
    };

    const MAX_ENTITIES: usize = 6;
    const MAX_LINES: usize = 12;
    const MAX_CHARS: usize = 800;

    let Ok(rtxn) = ctx.ops.executor.metadata.as_ref().read_txn() else {
        return String::new();
    };

    // Candidate surfaces: capitalized multi-word runs (Latin proper nouns) plus
    // every whitespace token of length >= 2 (catches single-token / lowercase
    // names). Deduped via the resolve loop's `seen` set on the entity side.
    let mut surfaces: Vec<String> = capitalized_runs(text);
    surfaces.extend(
        text.split_whitespace()
            .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()))
            .filter(|t| t.chars().count() >= 2)
            .map(str::to_string),
    );

    let mut entities: Vec<EntityId> = Vec::new();
    let mut seen: std::collections::HashSet<EntityId> = std::collections::HashSet::new();
    for s in surfaces {
        if entities.len() >= MAX_ENTITIES {
            break;
        }
        let Ok(ids) = entity_resolve_canonical_all_types(&rtxn, &s) else {
            continue;
        };
        for id in ids {
            if seen.insert(id) {
                entities.push(id);
                if entities.len() >= MAX_ENTITIES {
                    break;
                }
            }
        }
    }
    if entities.is_empty() {
        return String::new();
    }

    let mut lines: Vec<String> = Vec::new();
    'entities: for eid in entities {
        let subj = entity_get(&rtxn, eid)
            .ok()
            .flatten()
            .map(|e| e.canonical_name)
            .unwrap_or_default();
        if subj.trim().is_empty() {
            continue;
        }

        // Predicate-keyed statements (value or entity objects).
        if let Ok(stmts) = statement_list(
            &rtxn,
            &StatementListFilter {
                subject: Some(eid),
                predicate: None,
                kind: None,
                current_only: true,
                min_confidence: None,
                limit: 0,
            },
        ) {
            for st in stmts {
                if lines.len() >= MAX_LINES {
                    break 'entities;
                }
                let pred = predicate_get(&rtxn, st.predicate)
                    .ok()
                    .flatten()
                    .map(|p| humanize_qname(&p.canonical()))
                    .unwrap_or_default();
                let obj = render_object(&rtxn, &st.object);
                if pred.is_empty() || obj.is_empty() {
                    continue;
                }
                lines.push(format!("{subj} {pred} {obj}"));
            }
        }

        // Relation edges (entity<->entity), both directions: the surfaced fact
        // is always subject -> other for outgoing, other -> subject for
        // incoming, so a bridge question can chain either way.
        let rfilter = RelationListFilter {
            relation_type: None,
            current_only: true,
            limit: 0,
        };
        if let Ok(out) = relation_list_from(&rtxn, eid, &rfilter) {
            for r in out {
                if lines.len() >= MAX_LINES {
                    break 'entities;
                }
                let rt = relation_type_get(&rtxn, r.relation_type)
                    .ok()
                    .flatten()
                    .map(|t| humanize_qname(&t.canonical()))
                    .unwrap_or_default();
                let other = entity_get(&rtxn, r.to_entity)
                    .ok()
                    .flatten()
                    .map(|e| e.canonical_name)
                    .unwrap_or_default();
                if rt.is_empty() || other.trim().is_empty() {
                    continue;
                }
                lines.push(format!("{subj} {rt} {other}"));
            }
        }
        if let Ok(inc) = relation_list_to(&rtxn, eid, &rfilter) {
            for r in inc {
                if lines.len() >= MAX_LINES {
                    break 'entities;
                }
                let rt = relation_type_get(&rtxn, r.relation_type)
                    .ok()
                    .flatten()
                    .map(|t| humanize_qname(&t.canonical()))
                    .unwrap_or_default();
                let other = entity_get(&rtxn, r.from_entity)
                    .ok()
                    .flatten()
                    .map(|e| e.canonical_name)
                    .unwrap_or_default();
                if rt.is_empty() || other.trim().is_empty() {
                    continue;
                }
                lines.push(format!("{other} {rt} {subj}"));
            }
        }
    }

    // Dedup adjacent repeats (e.g. a symmetric edge surfaced from both ends),
    // then assemble under the character cap.
    lines.dedup();
    let mut out = String::new();
    for l in lines {
        if out.len() + l.len() + 1 > MAX_CHARS {
            break;
        }
        out.push_str(&l);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Strip the namespace and turn a `namespace:snake_case` qname into a plain
/// phrase ("brain:works_at" -> "works at") so the HyPE prompt reads naturally.
/// This is prompt rendering only — not a matching heuristic.
fn humanize_qname(qname: &str) -> String {
    let name = qname.split(':').next_back().unwrap_or(qname);
    name.replace('_', " ")
}

/// Render a statement object as a short surface for the neighborhood prompt: an
/// entity object resolves to its canonical name, a text/number/bool value
/// renders directly, and a blank or non-surfaceable object yields the empty
/// string (the caller drops the line).
fn render_object(rtxn: &redb::ReadTransaction, obj: &StatementObject) -> String {
    match obj {
        StatementObject::Entity(id) => brain_metadata::entity_get(rtxn, *id)
            .ok()
            .flatten()
            .map(|e| e.canonical_name)
            .unwrap_or_default(),
        StatementObject::Value(v) => match v {
            StatementValue::Text(t) => t.trim().to_string(),
            StatementValue::Integer(n) => n.to_string(),
            StatementValue::Float(f) => f.to_string(),
            StatementValue::Bool(b) => b.to_string(),
            StatementValue::UnixNanos(n) => n.to_string(),
            StatementValue::Blob(_) => String::new(),
        },
        // Meta-objects (memory / statement refs) carry no readable surface.
        StatementObject::Memory(_) | StatementObject::Statement(_) => String::new(),
    }
}

/// Extract capitalized whitespace-delimited runs from `text` — Latin
/// proper-noun surfaces like "Niraj Georgian" or "Web Summit". A run is a
/// maximal sequence of tokens whose first character is uppercase. Mirrors the
/// read-side anchor's surface mining so write- and read-time entity discovery
/// agree.
fn capitalized_runs(text: &str) -> Vec<String> {
    let mut runs: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for raw in text.split_whitespace() {
        let tok = raw.trim_matches(|c: char| !c.is_alphanumeric());
        let starts_upper = tok.chars().next().is_some_and(char::is_uppercase);
        if starts_upper {
            current.push(tok);
        } else if !current.is_empty() {
            runs.push(current.join(" "));
            current.clear();
        }
    }
    if !current.is_empty() {
        runs.push(current.join(" "));
    }
    runs
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
    /// Set when the LLM tier failed: whether the failure is transient
    /// (retry with backoff) or permanent (terminate). Drives the worker's
    /// retry decision so a passing provider outage never permanently drops
    /// a memory's grounding while a bad key / no balance fails loudly.
    llm_failure_class: ExtractionFailureClass,
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
    declared_predicates: Option<&str>,
    declared_kinds: Option<&str>,
    entity_type_labels: Option<&[String]>,
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
            llm_failure_class: ExtractionFailureClass::Unclassified,
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
            declared_predicates,
            declared_kinds,
            entity_type_labels: None,
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
            declared_predicates,
            declared_kinds,
            entity_type_labels,
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
            declared_predicates,
            declared_kinds,
            entity_type_labels: None,
        };
        run_tier_into(&llm_exts, &ctx, mems, &mut outcomes, ExtractorKind::Llm).await;
    }

    // Statement kind is owned by the tiers that can judge it correctly and
    // language-neutrally: a declared predicate's `kind_constraint` wins at
    // create time, else the LLM tier's per-statement kind, else the safe
    // `Fact` default. There is deliberately NO cheap keyword post-pass — a
    // surface-string classifier was English-only and, applied memory-wide,
    // mis-typed unrelated statements (e.g. retagging a Fact as a superseding
    // Preference because another sentence in the memory said "I like …").
    outcomes
}

/// Convert a `StatementKind` into the wire byte the pattern uses for
/// `StatementMention.kind`. The wire convention is `1/2/3` (matches
/// `statement_kind_from_byte` and the LLM's `kind_to_byte`). Test-only:
/// the production write path decodes the wire byte (`statement_kind_from_byte`)
/// but never re-encodes one — kind is carried as `StatementKind` end-to-end.
#[cfg(test)]
fn statement_kind_to_byte(k: StatementKind) -> u8 {
    // Wire convention is the `brain_core` kind byte + 1 (so `1=Fact …
    // 6=Directive`, `7+ = Custom`); inverse of `statement_kind_from_byte`.
    k.as_u8() + 1
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
            // Real provider cost flows from the LLM extractor's result into
            // the per-memory outcome, which the caller sums into the
            // per-cycle spend gate and the cost metric. Non-LLM tiers report
            // zero, so the unconditional add is correct.
            slot.llm_cost_micro_usd = slot
                .llm_cost_micro_usd
                .saturating_add(result.cost_micro_usd);
            // Per-tier visibility: log exactly what THIS tier's extractor
            // produced for THIS memory, before the items are merged into
            // the cumulative slot. Lets an operator see the pattern →
            // classifier → LLM contribution split for any encode.
            match &result.status {
                ExtractionStatus::Success => tracing::info!(
                    target: "brain_debug::extractor",
                    tier = ?tier_kind,
                    memory_id = ?mems[i].id,
                    extracted = result.items.len(),
                    items = %summarize_extracted_items(&result.items),
                    "extractor tier output",
                ),
                ExtractionStatus::SkippedDisabled => tracing::debug!(
                    target: "brain_debug::extractor",
                    tier = ?tier_kind,
                    memory_id = ?mems[i].id,
                    "extractor tier skipped (disabled)",
                ),
                other => tracing::info!(
                    target: "brain_debug::extractor",
                    tier = ?tier_kind,
                    memory_id = ?mems[i].id,
                    status = ?other,
                    reason = %result.status_reason,
                    "extractor tier produced nothing (non-success)",
                ),
            }
            if matches!(result.status, ExtractionStatus::Success) {
                slot.items.extend(result.items);
            } else if slot.failure_reason.is_none()
                && !matches!(result.status, ExtractionStatus::SkippedDisabled)
            {
                slot.failure_reason =
                    Some(format!("{:?}: {}", result.status, result.status_reason));
            }
            // Capture the LLM tier's transient/permanent verdict so the
            // worker can keep transient failures reprocessable (backoff
            // retry) and terminate permanent ones. Only the LLM tier sets a
            // meaningful class; other tiers leave it Unclassified.
            if matches!(tier_kind, ExtractorKind::Llm)
                && matches!(result.status, ExtractionStatus::Failure)
            {
                slot.llm_failure_class = result.failure_class;
            }
        }
    }
}

/// Compact one-line summary of what a tier extracted, for the
/// per-tier debug log. Capped so a large micro-batch can't flood the
/// log; the elided count is still reported.
fn summarize_extracted_items(items: &[ExtractedItem]) -> String {
    const MAX: usize = 16;
    let mut parts: Vec<String> = Vec::with_capacity(items.len().min(MAX));
    for item in items.iter().take(MAX) {
        let part = match item {
            ExtractedItem::EntityMention(m) => {
                let ty = if m.entity_type_qname.is_empty() {
                    "?"
                } else {
                    m.entity_type_qname.as_str()
                };
                format!("entity[{ty}] {:?}@{:.2}", m.text, m.confidence)
            }
            ExtractedItem::StatementMention(m) => format!(
                "stmt[{}] {:?}->{:?}@{:.2}",
                m.predicate_qname,
                m.subject_text.as_deref().unwrap_or(""),
                m.object_text.as_deref().unwrap_or(""),
                m.confidence,
            ),
            ExtractedItem::RelationMention(m) => format!(
                "rel[{}] {:?}->{:?}@{:.2}",
                m.relation_type_qname, m.subject_text, m.object_text, m.confidence,
            ),
        };
        parts.push(part);
    }
    if items.len() > MAX {
        parts.push(format!("… +{} more", items.len() - MAX));
    }
    parts.join(", ")
}

/// Normalize an entity surface form for in-cycle de-duplication. Delegates to
/// the canonical [`brain_metadata::normalize_name`] so the dedup key matches
/// the key the resolver's canonical index uses (Unicode NFC + casefold +
/// whitespace-collapse + determiner-strip). Using the same normalizer keeps
/// two mentions that would resolve to one entity from each minting a separate
/// one — and folds composed/decomposed Unicode ("São Paulo") together.
fn normalize_surface(text: &str) -> String {
    brain_metadata::normalize_name(text)
}

/// Reject obvious non-entity extractor proposals up front so they
/// never reach resolution. The LLM tier occasionally emits long
/// descriptive phrases ("the sharp spike in complaints about failed
/// payments and duplicate charges") and pure quantity tokens
/// ("180 people", "40 million dollars") as entity surfaces; these
/// are prose / quantities, not entities, and accepting them pollutes
/// the entity table + the entity HNSW. The three guards:
///
/// 1. **Non-empty after trim.** Pure-whitespace surfaces always
///    drop.
/// 2. **At most 6 whitespace-separated words** AND at most 50
///    characters. Real entity names are short.
/// 3. **Not a bare `<number> <word>` shape.** `180 people`,
///    `40 million` etc. are quantities.
fn entity_mention_is_acceptable(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    if t.chars().count() > 50 {
        return false;
    }
    let words: Vec<&str> = t.split_whitespace().collect();
    if words.len() > 6 {
        return false;
    }
    // Pure number-led shapes: first token is all digits (optionally
    // grouped with `,`), and the surface has no later capital letter
    // suggesting it's actually a code like `"R2D2 Robotics"`. The
    // simpler heuristic — first token all-digit AND total ≤ 3 words —
    // captures `"180 people"`, `"40 million dollars"`, `"2019 Boston"`
    // without misfiring on `"Aurora Robotics 2019"` (Title-Case first
    // token survives).
    if let Some(first) = words.first() {
        let only_digits_or_separator = !first.is_empty()
            && first
                .chars()
                .all(|c| c.is_ascii_digit() || c == ',' || c == '.');
        if only_digits_or_separator && words.len() <= 3 {
            return false;
        }
    }
    true
}

/// True if `candidate` is the better mention to keep for a surface
/// than `current`: a typed mention (non-empty `entity_type_qname`)
/// always beats an untyped one; among equally-typed mentions, higher
/// confidence wins. This makes the classifier's typed span supersede
/// the pattern tier's untyped capitalized-phrase guess.
fn mention_is_better(candidate: &EntityMention, current: &EntityMention) -> bool {
    let cand_typed = !candidate.entity_type_qname.is_empty();
    let cur_typed = !current.entity_type_qname.is_empty();
    match (cand_typed, cur_typed) {
        (true, false) => true,
        (false, true) => false,
        _ => candidate.confidence > current.confidence,
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
/// to populate the `StageCompleted` SUBSCRIBE event so clients
/// know exactly how many entities / statements / relations landed.
struct ApplyOutcome {
    counts: ExtractorItemCounts,
    status_byte: u8,
    /// True when this run was a retryable LLM-tier failure under the
    /// attempt budget — the cycle keeps the queue row so the memory is
    /// re-extracted next cycle instead of being abandoned.
    retry_pending: bool,
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
    // Collected during pass 2 alongside `causal_enqueues`; drained after
    // `wtxn.commit()` to feed the statement text indexer. Extractor-created
    // statements would otherwise never reach `statements.tantivy/` — only
    // the wire STATEMENT_CREATE handler dispatched, so the lexical statement
    // index stayed empty for extractor writes. Dispatch only post-commit so
    // a rolled-back txn never indexes a phantom row.
    let mut created_statement_ids: Vec<brain_core::StatementId> = Vec::new();

    let db_guard = ctx.ops.executor.metadata.as_ref();

    // Read this memory's prior attempt count so a retryable failure
    // advances the retry budget. Single-writer-per-shard means no
    // concurrent writer can change it between this read and the audit
    // write below. Absent row → 0.
    let prior_attempts = match db_guard.read_txn() {
        Ok(rtxn) => brain_metadata::pipeline_extraction_attempts(&rtxn, memory_id).unwrap_or(0),
        Err(_) => 0,
    };

    let wtxn = db_guard
        .write_txn()
        .map_err(|e| ApplyError::Storage(format!("write_txn: {e:?}")))?;

    // The writing agent's self-entity. First-person statement subjects
    // ("I prefer dark roast") resolve to `EntityId::from(agent_id)` — the
    // SAME identity `MATERIALIZE_PROCEDURAL` reads — so an agent's facts
    // about itself persist and stay queryable instead of being dropped as
    // non-referential pronouns. Per-agent by construction (the id is the
    // agent's), so multi-agent deployments never collapse onto one node.
    // A missing memory row (shouldn't happen for a queued memory) yields a
    // zero agent; first-person routing simply falls back to the drop path.
    let self_entity_id: Option<EntityId> = {
        use brain_metadata::tables::memory::MEMORIES_TABLE;
        wtxn.open_table(MEMORIES_TABLE)
            .ok()
            // Copy the agent bytes out while the table guard is still alive —
            // returning the guard itself would borrow the dropped table.
            .and_then(|t| {
                t.get(&memory_id.to_be_bytes())
                    .ok()
                    .flatten()
                    .map(|g| g.value().agent_id_bytes)
            })
            .filter(|b| *b != [0u8; 16])
            .map(EntityId::from)
    };

    // Pass 1 — entity mentions, in source order. Resolving early gives
    // statements + relations a populated `entity_map` to look up
    // surface forms against.
    let embed_deps = worker.embed_deps.as_ref();
    let entity_disambiguator = worker.entity_disambiguator.as_deref();
    // De-duplicate entity mentions by normalized surface BEFORE
    // resolving. Multiple tiers routinely emit the same surface — the
    // pattern tier's untyped capitalized-phrase guess and the
    // classifier's typed span both yield "Priya Sharma" — and
    // resolving each independently mints a second entity for the same
    // real-world thing (and double-counts it). Keep the single best
    // mention per surface: a typed mention beats an untyped one, and
    // higher confidence breaks ties, so the classifier's typed span
    // wins over the pattern guess. The resulting `counts.entities`
    // reflects distinct entities, not raw mentions.
    let mut best_by_surface: HashMap<String, &EntityMention> = HashMap::new();
    let mut surface_order: Vec<String> = Vec::new();
    for item in &outcome.items {
        if let ExtractedItem::EntityMention(em) = item {
            if !entity_mention_is_acceptable(&em.text) {
                trace!(
                    memory_id = ?memory_id,
                    text = %em.text,
                    "extractor entity mention rejected by surface guards",
                );
                continue;
            }
            let key = normalize_surface(&em.text);
            match best_by_surface.get(key.as_str()) {
                None => {
                    best_by_surface.insert(key.clone(), em);
                    surface_order.push(key);
                }
                Some(existing) if mention_is_better(em, existing) => {
                    best_by_surface.insert(key, em);
                }
                Some(_) => {}
            }
        }
    }
    for key in &surface_order {
        let em = best_by_surface[key];
        let (entity_id, tier) =
            resolve_entity_mention(&wtxn, em, now, embed_deps, entity_disambiguator)?;
        worker
            .metrics
            .inc_resolver_outcome(resolution_tier_to_metric(tier));
        entity_map.insert(em.text.clone(), entity_id);
        write_mention_edge(&wtxn, memory_id, entity_id, em, now)?;
        // One entity + one mention edge per distinct surface. A
        // `Created` tier means a genuinely new entity row landed; the
        // other tiers matched an existing entity.
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

    // Pass 2 — statements + relations. These reference entities by
    // surface form; we look them up in `entity_map`. Items whose
    // referenced surface form wasn't in the entity-mention pass are
    // dropped with a trace (the LLM tier occasionally emits implicit
    // entities; auto-creating them here would produce ghost entities
    // without a mention edge).
    for item in &outcome.items {
        match item {
            ExtractedItem::EntityMention(_) => {}
            ExtractedItem::StatementMention(sm) if sm.subject_is_memory => {
                // Memory-subject statement (temporal Event): the subject is
                // the source memory itself, not an entity. `object_text`
                // carries the resolved event time as decimal unix-nanos →
                // a typed `UnixNanos` value (the `occurred_at` predicate
                // requires `Value<timestamp>`).
                // Open-vocab: the predicate is interned as-is, never gated.
                // The kind (Event here) carries the semantics. A malformed
                // qname / predicate name skips THIS triple only (counted as
                // signal loss) — it must never abort the whole memory's
                // extraction.
                let Ok((ns, name)) = split_qname(&sm.predicate_qname) else {
                    worker.metrics.inc_apply_dropped("predicate_invalid");
                    warn!(
                        memory_id = ?memory_id,
                        predicate = %sm.predicate_qname,
                        "memory-subject statement: malformed predicate qname; skipping triple",
                    );
                    continue;
                };
                // Event time: the resolved `event_at_unix_nanos` (set by the
                // temporal extractor / LLM) wins; fall back to parsing the
                // legacy unix-nanos `object_text`.
                let Some(ts) = sm.event_at_unix_nanos.or_else(|| {
                    sm.object_text
                        .as_deref()
                        .and_then(|t| t.parse::<u64>().ok())
                }) else {
                    worker.metrics.inc_apply_dropped("unparseable_timestamp");
                    warn!(
                        memory_id = ?memory_id,
                        "memory-subject statement with no resolvable event time; skipping triple",
                    );
                    continue;
                };
                let pid = match predicate_intern_or_get(&wtxn, ns, name, 0, now) {
                    Ok(pid) => pid,
                    Err(e) => {
                        worker.metrics.inc_apply_dropped("predicate_invalid");
                        warn!(
                            memory_id = ?memory_id,
                            predicate = %sm.predicate_qname,
                            error = %e,
                            "memory-subject statement: predicate intern failed; skipping triple",
                        );
                        continue;
                    }
                };
                embed_predicate_if_absent(&wtxn, embed_deps, pid, name);
                // A memory-subject statement with a parsed timestamp object is,
                // by construction, a temporal Event — force the kind and plumb
                // the occurrence time so it persists (an Event is rejected at
                // create without `event_at`).
                let payload = StatementCreatePayload {
                    kind: StatementKind::Event,
                    subject: SubjectRef::Memory(memory_id),
                    predicate: pid,
                    object: StatementObject::Value(StatementValue::UnixNanos(ts)),
                    confidence: sm.confidence.clamp(0.0, 1.0),
                    evidence_memory_ids: vec![memory_id],
                    extractor_id: ExtractorId::from(sm.extractor_id),
                    schema_version: 0,
                    extracted_at_unix_nanos: now,
                    is_stateful: false,
                    event_at_unix_nanos: Some(ts),
                };
                // Memory-subject statements are NOT pushed to
                // `created_statement_ids`: the text indexer only indexes
                // entity-subject statements (it needs a canonical subject
                // name), so dispatching a Memory subject would read it back
                // only to skip it.
                match statement_create_internal(&wtxn, &payload) {
                    Ok(_) => {
                        counts.statements = counts.statements.saturating_add(1);
                        worker
                            .metrics
                            .add_items_written(ExtractorItemKind::Statement, 1);
                    }
                    Err(e) => {
                        worker.metrics.inc_apply_dropped("create_rejected");
                        warn!(
                            memory_id = ?memory_id,
                            error = %e,
                            "memory-subject statement create rejected; skipping triple",
                        );
                    }
                }
                continue;
            }
            ExtractedItem::StatementMention(sm) => {
                // Resolve the subject to an entity: prefer one already
                // extracted from this memory; otherwise mint/resolve a coined
                // subject ("Melanie's kids") so the fact persists as a
                // queryable statement instead of being dropped. Non-referential
                // junk subjects are rejected to keep the entity graph clean.
                if let Some(subject) = resolve_statement_subject(
                    &wtxn,
                    sm,
                    &mut entity_map,
                    self_entity_id,
                    embed_deps,
                    entity_disambiguator,
                    now,
                )? {
                    // Open-vocab: the predicate is always interned, never
                    // gated against a whitelist. A clean graph now rests on
                    // canonical entities + a closed KIND taxonomy + embedded
                    // predicates, not a closed predicate vocabulary — so a
                    // free predicate like `donated_bone_marrow_to` persists
                    // instead of being dropped to a review queue. A malformed
                    // qname / predicate name skips THIS triple only (counted),
                    // never aborts the whole memory.
                    let Ok((ns, name)) = split_qname(&sm.predicate_qname) else {
                        worker.metrics.inc_apply_dropped("predicate_invalid");
                        warn!(
                            memory_id = ?memory_id,
                            predicate = %sm.predicate_qname,
                            "statement: malformed predicate qname; skipping triple",
                        );
                        continue;
                    };
                    let pid = match predicate_intern_or_get(&wtxn, ns, name, 0, now) {
                        Ok(pid) => pid,
                        Err(e) => {
                            worker.metrics.inc_apply_dropped("predicate_invalid");
                            warn!(
                                memory_id = ?memory_id,
                                predicate = %sm.predicate_qname,
                                error = %e,
                                "statement: predicate intern failed; skipping triple",
                            );
                            continue;
                        }
                    };
                    embed_predicate_if_absent(&wtxn, embed_deps, pid, name);
                    let used_qname = (ns.to_string(), name.to_string());

                    // Object axis: an object already surfaced as an entity links;
                    // else the predicate's declared object constraint
                    // (Entity→mint / Value→text) wins; else the LLM's per-object
                    // entity-vs-value flag. A real entity not yet surfaced is
                    // minted best-effort (cross-type reuse); a literal stays text.
                    let object = resolve_statement_object(
                        &wtxn,
                        sm,
                        pid,
                        &mut entity_map,
                        embed_deps,
                        entity_disambiguator,
                        now,
                    )?;

                    // RETRACTION: the source text says this fact no longer holds
                    // ("not at Google anymore"). Retire the matching current
                    // fact(s) instead of creating a new row — covering BOTH stores
                    // a fact can live in: the statements table (value/entity-object
                    // facts) and the relations table (entity↔entity links like a
                    // `works_at` edge). We read a committed snapshot: the prior
                    // facts were written in an earlier cycle, so a read txn sees
                    // them; tombstoning happens in the live `wtxn`. A subject/object
                    // freshly minted this cycle simply has no prior row → no-op.
                    if sm.retract {
                        let mut retired = 0u64;
                        if let Ok(snap) = db_guard.read_txn() {
                            if let Ok(rows) = brain_metadata::statement_list(
                                &snap,
                                &brain_metadata::StatementListFilter {
                                    subject: Some(subject),
                                    predicate: Some(pid),
                                    kind: None,
                                    current_only: true,
                                    min_confidence: None,
                                    limit: 0,
                                },
                            ) {
                                for s in rows.into_iter().filter(|s| s.object == object) {
                                    if brain_metadata::statement_tombstone(
                                        &wtxn,
                                        s.id,
                                        brain_core::TombstoneReason::ExtractorRetraction,
                                        now,
                                    )
                                    .is_ok()
                                    {
                                        retired += 1;
                                    }
                                }
                            }
                            // Entity↔entity links (e.g. `works_at` as a typed edge)
                            // live in the relations table. Only relevant when the
                            // retracted object is an entity. `relation_type_intern_or_get`
                            // may mint the type if absent (harmless throwaway) — then
                            // there's simply nothing to retire.
                            if let StatementObject::Entity(to_id) = &object {
                                if let Ok(rt) = relation_type_intern_or_get(&wtxn, ns, name, 0, now)
                                {
                                    if let Ok(rels) = brain_metadata::relation_list_from(
                                        &snap,
                                        subject,
                                        &brain_metadata::RelationListFilter {
                                            relation_type: Some(rt),
                                            current_only: true,
                                            limit: 0,
                                        },
                                    ) {
                                        for r in rels.into_iter().filter(|r| r.to_entity == *to_id)
                                        {
                                            if brain_metadata::relation_tombstone(&wtxn, r.id, now)
                                                .is_ok()
                                            {
                                                retired += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        tracing::debug!(
                            target: "brain_ops::write_trace",
                            ?memory_id,
                            subject = ?subject,
                            predicate = %sm.predicate_qname,
                            retired,
                            "write: retraction tombstoned prior fact(s)"
                        );
                        continue;
                    }

                    // Self-loop guard: a triple whose object resolves to the
                    // same entity as its subject ("Tokyo is_capital_of Tokyo")
                    // is almost always an extraction error (object mis-resolved
                    // back to the subject). Skip + count rather than persist a
                    // corrupt reflexive edge. Domain-neutral — applies to any
                    // subject/predicate.
                    if matches!(object, StatementObject::Entity(obj) if obj == subject) {
                        worker.metrics.inc_apply_dropped("self_reference");
                        warn!(
                            memory_id = ?memory_id,
                            predicate = %sm.predicate_qname,
                            "statement object resolved to its own subject; skipping self-loop",
                        );
                        continue;
                    }

                    // Kind: a seeded/declared predicate's kind_constraint still
                    // wins (e.g. `brain:prefers` is always Preference); else the
                    // LLM's per-statement kind, defaulting to Fact (the catch-all).
                    let mut kind = predicate_declared_kind_in_write_txn(&wtxn, pid)?
                        .unwrap_or_else(|| statement_kind_from_byte(sm.kind));
                    // An entity-subject Event persists only with a resolved event
                    // time (the LLM emits an ISO date the projection resolved into
                    // `event_at_unix_nanos`). Without one, downgrade to Fact —
                    // cumulative and never wrongly superseding — so the fact still
                    // lands rather than being rejected at create (Event requires
                    // `event_at`). This also absorbs any tier that mis-kinds a
                    // timeless statement as Event.
                    if kind == StatementKind::Event && sm.event_at_unix_nanos.is_none() {
                        kind = StatementKind::Fact;
                    }

                    // Axis-faithful entity-object routing (spec §02 data model).
                    // `StatementObject::Entity` is a first-class statement object:
                    // `manages`, `is_a`, `met_with`, `traveled_to` are entity-object
                    // *statements*, read from the subject. Only a `kind=Relation`
                    // triple is a typed graph EDGE that belongs in the relations
                    // table (queryable from both endpoints, cardinality-enforced).
                    // A Fact / Event / Preference with an entity object stays a
                    // statement — it falls through to `statement_create` below,
                    // persisted with its `StatementObject::Entity`. The emitted
                    // kind, not the object's shape, decides the axis.
                    let entity_link = match object {
                        StatementObject::Entity(to_id) if kind == StatementKind::Relation => {
                            Some(to_id)
                        }
                        _ => None,
                    };
                    if let Some(to_id) = entity_link {
                        let rt = match relation_type_intern_or_get(&wtxn, ns, name, 0, now) {
                            Ok(rt) => rt,
                            Err(e) => {
                                worker.metrics.inc_apply_dropped("create_rejected");
                                warn!(
                                    memory_id = ?memory_id,
                                    predicate = %sm.predicate_qname,
                                    error = %e,
                                    "relation_type intern failed for entity link; skipping triple",
                                );
                                continue;
                            }
                        };
                        embed_relation_type_if_absent(&wtxn, embed_deps, rt, name);
                        let payload = RelationCreatePayload {
                            relation_type: rt,
                            from_entity: subject,
                            to_entity: to_id,
                            confidence: sm.confidence.clamp(0.0, 1.0),
                            evidence_memory_ids: vec![memory_id],
                            extractor_id: ExtractorId::from(sm.extractor_id),
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
                            Err(e) => {
                                worker.metrics.inc_apply_dropped("create_rejected");
                                warn!(
                                    memory_id = ?memory_id,
                                    predicate = %sm.predicate_qname,
                                    error = %e,
                                    "relation_create rejected for entity link; skipping triple",
                                );
                            }
                        }
                        continue;
                    }

                    let event_at = if kind == StatementKind::Event {
                        sm.event_at_unix_nanos
                    } else {
                        None
                    };
                    // Denormalized statefulness cache: single-valued kinds
                    // (Attribute, Directive, custom `cardinality: single`)
                    // supersede. The authoritative supersession decision is
                    // re-derived from the kind in `statement_create`.
                    let is_stateful = kind
                        .builtin_behavior()
                        .map(|b| b.cardinality.is_single())
                        .unwrap_or(false);
                    let payload = StatementCreatePayload {
                        kind,
                        subject: SubjectRef::Entity(subject),
                        predicate: pid,
                        object,
                        confidence: sm.confidence.clamp(0.0, 1.0),
                        evidence_memory_ids: vec![memory_id],
                        extractor_id: ExtractorId::from(sm.extractor_id),
                        schema_version: 0,
                        extracted_at_unix_nanos: now,
                        is_stateful,
                        event_at_unix_nanos: event_at,
                    };
                    match statement_create_internal(&wtxn, &payload) {
                        Ok(sid) => {
                            counts.statements = counts.statements.saturating_add(1);
                            created_statement_ids.push(sid);
                            // Write-path trace: the entity-subject structured
                            // fact this extraction produced — the exact rows the
                            // grounded read matches against. Correlate a read
                            // miss to a missing/odd triple here.
                            tracing::debug!(
                                target: "brain_ops::write_trace",
                                ?sid,
                                ?memory_id,
                                subject = ?subject,
                                predicate = %sm.predicate_qname,
                                "write: entity-subject statement created"
                            );
                            worker
                                .metrics
                                .add_items_written(ExtractorItemKind::Statement, 1);
                            if let Some(feed) = worker.causal_edge.as_ref() {
                                if feed.whitelist_qnames.contains(&used_qname) {
                                    causal_enqueues.push(sid);
                                }
                            }
                        }
                        Err(e) => {
                            worker.metrics.inc_apply_dropped("create_rejected");
                            warn!(
                                memory_id = ?memory_id,
                                error = %e,
                                "statement_create rejected; skipping triple",
                            );
                        }
                    }
                } else {
                    // Subject was absent or non-referential (e.g. a bare
                    // pronoun the LLM never coreferenced). The fact is real but
                    // unanchorable here; coreference re-ingest can recover it.
                    // Surface it as signal loss rather than swallowing silently.
                    worker.metrics.inc_apply_dropped("subject_unresolved");
                    warn!(
                        memory_id = ?memory_id,
                        subject = ?sm.subject_text,
                        "statement subject unresolved; skipping triple (recoverable via coref re-ingest)",
                    );
                }
            }
            ExtractedItem::RelationMention(rm) => {
                // Resolve both endpoints, minting best-effort — symmetric with
                // statement subjects — so a real relation isn't lost merely
                // because an endpoint wasn't independently surfaced as an
                // entity mention ("Priya mentored Sam" where only Priya was
                // tagged). Non-referential endpoints are still rejected.
                let from = resolve_relation_endpoint(
                    &wtxn,
                    &rm.subject_text,
                    rm.confidence,
                    &mut entity_map,
                    embed_deps,
                    entity_disambiguator,
                    now,
                )?;
                let to = resolve_relation_endpoint(
                    &wtxn,
                    &rm.object_text,
                    rm.confidence,
                    &mut entity_map,
                    embed_deps,
                    entity_disambiguator,
                    now,
                )?;
                let (Some(from), Some(to)) = (from, to) else {
                    worker.metrics.inc_apply_dropped("endpoint_unresolved");
                    warn!(
                        memory_id = ?memory_id,
                        from = ?rm.subject_text,
                        to = ?rm.object_text,
                        "relation endpoint unresolved; skipping relation",
                    );
                    continue;
                };
                // Open-vocab: the relation type is always interned, never
                // gated against a whitelist. A malformed qname skips this
                // relation only (counted), never aborts the memory.
                let Ok((ns, name)) = split_qname(&rm.relation_type_qname) else {
                    worker.metrics.inc_apply_dropped("relation_type_invalid");
                    warn!(
                        memory_id = ?memory_id,
                        relation_type = %rm.relation_type_qname,
                        "relation: malformed relation_type qname; skipping relation",
                    );
                    continue;
                };
                let rt = match relation_type_intern_or_get(&wtxn, ns, name, 0, now) {
                    Ok(rt) => rt,
                    Err(e) => {
                        worker.metrics.inc_apply_dropped("relation_type_invalid");
                        warn!(
                            memory_id = ?memory_id,
                            relation_type = %rm.relation_type_qname,
                            error = %e,
                            "relation: relation_type intern failed; skipping relation",
                        );
                        continue;
                    }
                };
                embed_relation_type_if_absent(&wtxn, embed_deps, rt, name);
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
                    Err(e) => {
                        worker.metrics.inc_apply_dropped("create_rejected");
                        warn!(
                            memory_id = ?memory_id,
                            error = %e,
                            "relation_create rejected; skipping relation",
                        );
                    }
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
    let attempts = prior_attempts.saturating_add(1);
    // A retryable failure is specifically the LLM tier failing (statements are
    // LLM-only and a failed call wrote zero, so re-running can't duplicate
    // them) with a transient cause. A TRANSIENT failure (timeout / rate-limit /
    // 5xx) keeps the memory queued and is retried with backoff until it
    // succeeds — a passing provider outage must never permanently strip a
    // memory's grounding. A PERMANENT failure (bad key, no balance, malformed)
    // is terminal at once: retrying can't help and only hides the problem.
    // Pattern/classifier-only failures are never retried (their rows already
    // committed; a re-run would duplicate).
    let llm_failed = outcome.llm == brain_metadata::tier_status::FAILED;
    let failure_class_byte = match outcome.llm_failure_class {
        ExtractionFailureClass::Transient => brain_metadata::failure_class::TRANSIENT,
        ExtractionFailureClass::Permanent => brain_metadata::failure_class::PERMANENT,
        ExtractionFailureClass::Unclassified => brain_metadata::failure_class::UNCLASSIFIED,
    };
    let permanent = outcome.llm_failure_class == ExtractionFailureClass::Permanent;
    let retry_pending = llm_failed && !permanent;
    if llm_failed && permanent {
        // Operator-actionable: a permanent extraction failure means this
        // memory's typed-graph grounding will not be created without operator
        // intervention (fix the key / balance, then backfill). Surfaced loudly
        // rather than buried as a generic tier failure.
        warn!(
            target: "brain_workers::extractor",
            memory_id = ?memory_id,
            reason = %outcome.failure_reason.as_deref().unwrap_or("permanent LLM failure"),
            "extraction permanently failed; memory has no typed-graph grounding until backfilled",
        );
    }
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
    )
    .with_attempts(attempts)
    .with_failure_class(failure_class_byte);
    record_extracted(&wtxn, &audit)
        .map_err(|e| ApplyError::Audit(format!("record_extracted: {e}")))?;

    wtxn.commit()
        .map_err(|e| ApplyError::Storage(format!("commit: {e:?}")))?;

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

    // Feed every freshly-committed entity-subject statement to the
    // statement text indexer — mirroring the wire STATEMENT_CREATE
    // handler — so `statements.tantivy/` stays in sync with redb for
    // extractor-driven writes. Post-commit only: a rolled-back txn must
    // never index a phantom row. Best-effort: a failed dispatch is logged
    // by the helper and never blocks the durable write.
    if let Some(dispatcher) = ctx.ops.statement_text_dispatcher.as_ref() {
        let metadata = ctx.ops.executor.metadata.as_ref();
        for sid in created_statement_ids {
            brain_ops::index::text_indexer::statement::dispatch_statement_text_upsert(
                metadata, dispatcher, sid,
            )
            .await;
        }
    }

    Ok(ApplyOutcome {
        counts,
        status_byte,
        retry_pending,
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

/// Publish the `StageCompleted` SUBSCRIBE event onto the per-shard
/// bus. A no-op when no subscriber is listening — `events.publish`
/// still mints an LSN for the bus's own bookkeeping.
/// Publish a `StageCompleted{Extractor}` event so subscribers waiting
/// via `--wait` can decrement their pending-stage checklist for this
/// memory.
fn publish_extracted_graph(
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
        "publish_extracted_graph: emitting StageCompleted",
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
        graph_payload: None,
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

/// The kind a predicate constrains its statements to (`Fact`/`Preference`/
/// `Event`), or `None` for an unconstrained (open-vocabulary or any-kind)
/// predicate. Lets the worker stamp the schema-declared kind on a
/// statement instead of trusting the extractor's guess.
fn predicate_declared_kind_in_write_txn(
    wtxn: &redb::WriteTransaction,
    pid: brain_core::PredicateId,
) -> Result<Option<StatementKind>, ApplyError> {
    use brain_metadata::tables::predicate::decode_kind_constraint;
    let t = wtxn
        .open_table(PREDICATES_TABLE)
        .map_err(|e| ApplyError::Storage(format!("predicates open: {e}")))?;
    let row = t
        .get(&pid.raw())
        .map_err(|e| ApplyError::Storage(format!("predicates get: {e}")))?;
    Ok(row.and_then(|g| decode_kind_constraint(g.value().kind_constraint)))
}

/// Embed a predicate's human phrase the first time it is interned, so the
/// grounded read can match a paraphrased question against it by cosine (the
/// write half of the two-way relation match). Open-vocab predicates have no
/// declared synonyms, so the embedding is the only way `"where do they
/// work"` finds a stored `works_at`. Idempotent: skips predicates that
/// already carry a vector. No-op when the embedder isn't wired (tests).
fn embed_predicate_if_absent(
    wtxn: &redb::WriteTransaction,
    embed_deps: Option<&EmbeddingDeps>,
    pid: brain_core::PredicateId,
    name: &str,
) {
    let Some(deps) = embed_deps else {
        return;
    };
    let already = wtxn
        .open_table(PREDICATE_EMBEDDINGS_TABLE)
        .ok()
        .and_then(|t| t.get(pid.raw()).ok().flatten().map(|_| ()))
        .is_some();
    if already {
        return;
    }
    // Predicate names are snake_case; the natural phrase ("works at")
    // embeds closer to a question's relation than the raw token.
    let phrase = name.replace('_', " ");
    if let Ok(vec) = deps.embedder.embed(&phrase) {
        if let Err(e) = brain_metadata::predicate_embedding_put(wtxn, pid, &vec) {
            trace!(predicate = %name, error = %e, "predicate embedding store failed");
        }
    }
}

/// Embed a relation type's human phrase the first time it is interned, the
/// edge-graph mirror of `embed_predicate_if_absent`. Open-vocab relation
/// types carry no declared synonyms, so the embedding is the only way a
/// paraphrased question's relation matches a stored edge by cosine.
/// Idempotent: skips relation types that already carry a vector. No-op when
/// the embedder isn't wired (tests).
fn embed_relation_type_if_absent(
    wtxn: &redb::WriteTransaction,
    embed_deps: Option<&EmbeddingDeps>,
    rt: brain_core::RelationTypeId,
    name: &str,
) {
    let Some(deps) = embed_deps else {
        return;
    };
    let already = wtxn
        .open_table(RELATION_TYPE_EMBEDDINGS_TABLE)
        .ok()
        .and_then(|t| t.get(rt.raw()).ok().flatten().map(|_| ()))
        .is_some();
    if already {
        return;
    }
    // Relation-type names are snake_case; the natural phrase ("works at")
    // embeds closer to a question's relation than the raw token.
    let phrase = name.replace('_', " ");
    if let Ok(vec) = deps.embedder.embed(&phrase) {
        if let Err(e) = brain_metadata::relation_type_embedding_put(wtxn, rt, &vec) {
            trace!(relation_type = %name, error = %e, "relation type embedding store failed");
        }
    }
}

/// Raw `object_type_constraint_byte` a predicate declares (`1=Entity`,
/// `2=Value`, …), or `None` for an unconstrained (open / any) predicate.
/// Lets the apply pass honor a schema-declared object axis over the LLM's
/// guess. Mirrors `predicate_declared_kind_in_write_txn`.
fn predicate_declared_object_constraint_in_write_txn(
    wtxn: &redb::WriteTransaction,
    pid: brain_core::PredicateId,
) -> Result<Option<u8>, ApplyError> {
    let t = wtxn
        .open_table(PREDICATES_TABLE)
        .map_err(|e| ApplyError::Storage(format!("predicates open: {e}")))?;
    let row = t
        .get(&pid.raw())
        .map_err(|e| ApplyError::Storage(format!("predicates get: {e}")))?;
    Ok(row.and_then(|g| {
        let b = g.value().object_type_constraint_byte;
        if b == 0 {
            None
        } else {
            Some(b)
        }
    }))
}

/// Resolve a statement's object to an `Entity` ref or a literal `Value`.
/// Precedence: (1) an object already surfaced as an entity this cycle links;
/// (2) the predicate's declared object constraint wins — `Entity` → mint
/// best-effort, `Value` → keep text; (3) otherwise the LLM's per-object
/// `object_is_entity` flag decides. A real entity not yet surfaced is minted
/// best-effort (reusing the relation-endpoint path: cross-type reuse +
/// mintability guard); a literal — or a non-mintable surface — stays text, so
/// values like "blue"/"200" never become junk entities.
fn resolve_statement_object(
    wtxn: &redb::WriteTransaction,
    sm: &StatementMention,
    pid: brain_core::PredicateId,
    entity_map: &mut HashMap<String, EntityId>,
    embed_deps: Option<&EmbeddingDeps>,
    entity_disambiguator: Option<&EntityDisambiguator>,
    now: u64,
) -> Result<StatementObject, ApplyError> {
    let Some(text) = sm.object_text.as_deref() else {
        return Ok(StatementObject::Value(StatementValue::Text(String::new())));
    };
    // A first-person object the subject pass already cached (the agent
    // self-entity) links via this entity_map hit. A standalone first-person
    // object ("report to me") is follow-up — the dominant first-person case is
    // the subject ("I prefer …"), handled in `resolve_statement_subject`.
    if let Some(id) = entity_map.get(text).copied() {
        return Ok(StatementObject::Entity(id));
    }
    // Object constraint byte: 1 = Entity, 2 = Value (see brain-metadata
    // predicate table). Open predicate (None) defers to the LLM's flag.
    let want_entity = match predicate_declared_object_constraint_in_write_txn(wtxn, pid)? {
        Some(1) => true,
        Some(2) => false,
        _ => sm.object_is_entity,
    };
    if want_entity {
        if let Some(id) = resolve_relation_endpoint(
            wtxn,
            text,
            sm.confidence,
            entity_map,
            embed_deps,
            entity_disambiguator,
            now,
        )? {
            return Ok(StatementObject::Entity(id));
        }
    }
    Ok(StatementObject::Value(StatementValue::Text(
        text.to_string(),
    )))
}

/// Default entity type for a coined statement subject the classifier never
/// extracted (e.g. "Melanie's kids"). Generic on purpose — the subject is
/// minted only so the fact persists as a queryable statement; its precise
/// type isn't asserted by the LLM.
const COINED_SUBJECT_ENTITY_TYPE: &str = "brain:Concept";

/// Resolve a statement's subject to an entity. Prefers an entity already
/// extracted from this memory (`entity_map`); otherwise mints/resolves a
/// coined subject so the fact isn't dropped at persist. Returns `None` for
/// an absent or non-referential subject (those statements are skipped).
fn resolve_statement_subject(
    wtxn: &redb::WriteTransaction,
    sm: &StatementMention,
    entity_map: &mut HashMap<String, EntityId>,
    self_entity_id: Option<EntityId>,
    embed_deps: Option<&EmbeddingDeps>,
    entity_disambiguator: Option<&EntityDisambiguator>,
    now: u64,
) -> Result<Option<EntityId>, ApplyError> {
    let Some(text) = sm.subject_text.as_deref() else {
        return Ok(None);
    };
    if let Some(id) = entity_map.get(text).copied() {
        return Ok(Some(id));
    }
    // First person ("I prefer …") refers to the writing agent — route to its
    // self-entity rather than dropping it as a non-referential pronoun. The
    // judgment is the LLM's (`subject_is_self`), so it holds across any
    // language with NO hardcoded pronoun list — "I", "yo", "私", "ich" all
    // flow through the same flag. Must precede the non-referential drop (a
    // bare first-person surface would otherwise be discarded). Cached so a
    // later object/endpoint reuses the same id.
    if let Some(self_id) = self_entity_id {
        if sm.subject_is_self {
            // statement_create requires the subject entity to exist, so
            // materialize the agent's self-entity row on first use (idempotent).
            ensure_agent_self_entity(wtxn, self_id, now)?;
            entity_map.insert(text.to_string(), self_id);
            return Ok(Some(self_id));
        }
    }
    if !statement_subject_mintable(text) {
        return Ok(None);
    }
    // Cross-type reuse before minting a generic node: if exactly one already
    // existing entity (of any type) matches this exact canonical name, it is
    // almost certainly the same referent — reuse it so a coined Concept doesn't
    // permanently split from a correctly-typed entity ("aspirin" the Drug).
    // 0 or >1 matches fall through to the normal type-scoped mint.
    if let Some(id) = reuse_cross_type_exact(wtxn, text)? {
        entity_map.insert(text.to_string(), id);
        return Ok(Some(id));
    }
    let res = resolve_or_create_with_deps(
        wtxn,
        text,
        COINED_SUBJECT_ENTITY_TYPE,
        sm.confidence,
        now,
        embed_deps,
        entity_disambiguator,
    )
    .map_err(ApplyError::from)?;
    entity_map.insert(text.to_string(), res.entity_id);
    Ok(Some(res.entity_id))
}

/// Idempotently materialize the writing agent's self-entity row so that
/// first-person statements (which use `EntityId::from(agent_id)` as their
/// subject) pass `statement_create`'s subject-existence check. The canonical
/// name is the agent's own id in hex (`agent:<32 hex>`) — globally unique per
/// agent, so multi-agent self-entities never collide, and it can never clash
/// with a real extracted person's name. Typed `Person`: the agent/user is a
/// person-like referent. A no-op when the row already exists.
fn ensure_agent_self_entity(
    wtxn: &redb::WriteTransaction,
    self_id: EntityId,
    now: u64,
) -> Result<(), ApplyError> {
    let exists = {
        use brain_metadata::tables::entity::ENTITIES_TABLE;
        let t = wtxn
            .open_table(ENTITIES_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open ENTITIES: {e}")))?;
        let present = t
            .get(&self_id.to_bytes())
            .map_err(|e| ApplyError::Storage(format!("get self entity: {e}")))?
            .is_some();
        present
        // `t` drops here so entity_put can reopen ENTITIES_TABLE mutably below.
    };
    if exists {
        return Ok(());
    }
    let canonical = format!("agent:{:032x}", u128::from_be_bytes(self_id.to_bytes()));
    let entity = brain_core::Entity::new_active(
        self_id,
        brain_core::EntityType::PERSON_ID,
        canonical.clone(),
        brain_metadata::entity::ops::normalize_name(&canonical),
        now,
    );
    brain_metadata::entity::ops::entity_put(wtxn, &entity)
        .map_err(|e| ApplyError::Storage(format!("entity_put(self): {e}")))?;
    Ok(())
}

/// Reuse an existing entity for a coined surface when exactly one entity of
/// any type carries this exact canonical name. A single cross-type hit is a
/// strong same-referent signal; 0 or >1 returns `None` so the caller mints
/// under the generic coined type rather than risk a wrong merge.
fn reuse_cross_type_exact(
    wtxn: &redb::WriteTransaction,
    text: &str,
) -> Result<Option<EntityId>, ApplyError> {
    let hits = brain_metadata::entity_resolve_canonical_all_types_wtxn(wtxn, text)
        .map_err(|e| ApplyError::Storage(format!("cross-type resolve: {e}")))?;
    Ok(if hits.len() == 1 { Some(hits[0]) } else { None })
}

/// Resolve one endpoint of a relation to an entity. Prefers an entity already
/// surfaced this memory (`entity_map`); otherwise mints/resolves it
/// best-effort — symmetric with [`resolve_statement_subject`] — so a real
/// relation isn't dropped just because one endpoint wasn't independently
/// tagged. Returns `None` for an empty or non-referential surface (those
/// endpoints can't anchor a relation). A genuine resolver error propagates so
/// the cycle can retry rather than permanently abandon the memory.
fn resolve_relation_endpoint(
    wtxn: &redb::WriteTransaction,
    text: &str,
    confidence: f32,
    entity_map: &mut HashMap<String, EntityId>,
    embed_deps: Option<&EmbeddingDeps>,
    entity_disambiguator: Option<&EntityDisambiguator>,
    now: u64,
) -> Result<Option<EntityId>, ApplyError> {
    if let Some(id) = entity_map.get(text).copied() {
        return Ok(Some(id));
    }
    if !statement_subject_mintable(text) {
        return Ok(None);
    }
    if let Some(id) = reuse_cross_type_exact(wtxn, text)? {
        entity_map.insert(text.to_string(), id);
        return Ok(Some(id));
    }
    let res = resolve_or_create_with_deps(
        wtxn,
        text,
        COINED_SUBJECT_ENTITY_TYPE,
        confidence,
        now,
        embed_deps,
        entity_disambiguator,
    )
    .map_err(ApplyError::from)?;
    entity_map.insert(text.to_string(), res.entity_id);
    Ok(Some(res.entity_id))
}

/// Whether a coined subject is worth minting as an entity. Reuses the
/// entity-mention surface guards and the shared non-referential backstop
/// (`brain_core::is_non_referential_surface`) so a lone pronoun the LLM emits
/// can't repollute the graph.
fn statement_subject_mintable(text: &str) -> bool {
    if !entity_mention_is_acceptable(text) {
        return false;
    }
    !brain_core::is_non_referential_surface(text)
}

fn split_qname(q: &str) -> Result<(&str, &str), String> {
    q.split_once(':')
        .ok_or_else(|| format!("qname missing ':' separator: {q}"))
}

fn statement_kind_from_byte(b: u8) -> StatementKind {
    // Inverse of `statement_kind_to_byte`: wire byte is `core_byte + 1`,
    // so `1=Fact`. A `0` (shouldn't occur on this path) clamps to Fact.
    StatementKind::from_u8(b.saturating_sub(1))
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

    #[test]
    fn humanize_qname_strips_namespace_and_underscores() {
        assert_eq!(humanize_qname("brain:works_at"), "works at");
        assert_eq!(humanize_qname("brain:reports_to"), "reports to");
        // No namespace prefix: pass through with underscores spaced.
        assert_eq!(humanize_qname("favorite_color"), "favorite color");
        // No underscores, no namespace: unchanged.
        assert_eq!(humanize_qname("knows"), "knows");
    }

    #[test]
    fn capitalized_runs_groups_proper_nouns() {
        // Multi-word run joins; lowercase tokens break the run.
        assert_eq!(
            capitalized_runs("Niraj Georgian works at Infosys today"),
            vec!["Niraj Georgian".to_string(), "Infosys".to_string()]
        );
        // Trailing punctuation is trimmed before grouping.
        assert_eq!(
            capitalized_runs("met Meera, then Priya."),
            vec!["Meera".to_string(), "Priya".to_string()]
        );
        // No capitalized tokens → empty.
        assert!(capitalized_runs("all lowercase here").is_empty());
    }

    fn outcome(pattern: u8, classifier: u8, llm: u8) -> PipelineOutcome {
        PipelineOutcome {
            items: Vec::new(),
            pattern,
            classifier,
            llm,
            failure_reason: None,
            llm_failure_class: ExtractionFailureClass::Unclassified,
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
            occurred_at_unix_nanos: None,
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
            None,
            None,
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
            None,
            None,
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
    fn default_knobs_batch_size_matches_constant() {
        let k = ExtractorKnobs::default();
        assert_eq!(k.batch_size, DEFAULT_EXTRACTOR_BATCH_SIZE);
        assert_eq!(k.drain_per_cycle, DEFAULT_EXTRACTOR_DRAIN_PER_CYCLE);
    }

    /// Open-vocab apply is axis-faithful: an extracted item is written on
    /// the axis the extractor chose, never re-axised. A `StatementMention`
    /// becomes a statement (its predicate coined on the fly — predicates are
    /// an open vocabulary) and a `RelationMention` becomes a relation (its
    /// relation_type coined best-effort). Nothing is dropped to a wildcard
    /// sink and nothing is silently flipped across axes; the grounded read
    /// reconciles related concepts semantically at query time. Verified
    /// end-to-end through `apply_outcome`, reading the rows back.
    #[test]
    #[allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send by design
    fn apply_writes_extracted_items_on_their_emitted_axis() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        use brain_core::{EntityType, MemoryId, StatementKind};
        use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
        use brain_index::{IndexParams, SharedHnsw};
        use brain_metadata::entity::ops::entity_lookup_by_canonical_name;
        use brain_metadata::relation::ops::{relation_list_from, RelationListFilter};
        use brain_metadata::relation::types::relation_type_intern_or_get;
        use brain_metadata::schema::predicate::predicate_intern_or_get;
        use brain_metadata::statement::{statement_list, StatementListFilter};
        use brain_metadata::MetadataDb;
        use brain_ops::RealWriterHandle;
        use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};

        use brain_extractors::{RelationMention, StatementMention};

        use crate::context::WorkerContext;

        struct ZeroDispatcher;
        impl Dispatcher for ZeroDispatcher {
            fn embed(&self, _t: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
                Ok([0.0; VECTOR_DIM])
            }
            fn embed_batch(&self, t: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
                Ok(vec![[0.0; VECTOR_DIM]; t.len()])
            }
            fn fingerprint(&self) -> [u8; 16] {
                [0xCD; 16]
            }
        }

        // Fixture: temp metadata (seeds the brain: system schema). reports_to
        // is a seeded relation_type; member_of and the open predicates here are
        // coined on the fly by the apply pass.
        let tempdir = tempfile::tempdir().unwrap();
        let metadata: SharedMetadataDb =
            Arc::new(MetadataDb::open(tempdir.path().join("md.redb")).unwrap());
        let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
        let executor = ExecutorContext::new(
            Arc::new(ZeroDispatcher) as Arc<dyn Dispatcher>,
            shared,
            metadata.clone(),
            writer.clone() as Arc<dyn WriterHandle>,
        );
        let ops = Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor));
        let ctx = WorkerContext {
            ops,
            shutdown: Arc::new(AtomicBool::new(false)),
        };

        let (_tx, rx) = flume::unbounded();
        let worker = ExtractorWorker::new(rx);

        let mention = |text: &str, qn: &str| {
            ExtractedItem::EntityMention(EntityMention {
                entity_type_qname: qn.into(),
                text: text.into(),
                start: 0,
                end: text.chars().count(),
                confidence: 0.95,
                extractor_id: 2,
                extractor_version: 1,
            })
        };
        let outcome = PipelineOutcome {
            items: vec![
                mention("Priya", "brain:Person"),
                mention("Dana", "brain:Person"),
                mention("Acme", "brain:Organization"),
                // Emitted as a statement with an open predicate. Even though
                // `brain:reports_to` also exists as a seeded relation_type,
                // open-vocab apply is axis-faithful: it writes the statement
                // (coining the reports_to predicate), it does not re-axis it
                // into a relation.
                ExtractedItem::StatementMention(StatementMention {
                    kind: StatementKind::Fact.as_u8(),
                    subject_text: Some("Priya".into()),
                    subject_is_memory: false,
                    predicate_qname: "brain:reports_to".into(),
                    object_text: Some("Dana".into()),
                    confidence: 0.9,
                    extractor_id: 3,
                    extractor_version: 1,
                    is_stateful: false,
                    object_is_entity: false,
                    event_at_unix_nanos: None,
                    subject_is_self: false,
                    retract: false,
                }),
                // Emitted as a `kind=Relation` StatementMention whose object is
                // an entity (Priya -> Acme). An entity↔entity link is a graph
                // edge, so apply routes it into the relations table (coining the
                // `partner_of` relation_type) rather than persisting it as a
                // kind=Relation statement, which would leave the typed-edge
                // traversal + cardinality enforcement dead.
                ExtractedItem::StatementMention(StatementMention {
                    kind: statement_kind_to_byte(StatementKind::Relation),
                    subject_text: Some("Priya".into()),
                    subject_is_memory: false,
                    predicate_qname: "brain:partner_of".into(),
                    object_text: Some("Acme".into()),
                    confidence: 0.9,
                    extractor_id: 3,
                    extractor_version: 1,
                    is_stateful: false,
                    object_is_entity: true,
                    event_at_unix_nanos: None,
                    subject_is_self: false,
                    retract: false,
                }),
                // Emitted as a relation. `brain:member_of` is not a seeded
                // relation_type, but open-vocab apply coins it best-effort and
                // writes the relation Priya -> Acme, rather than dropping the
                // row or flipping it into a statement.
                ExtractedItem::RelationMention(RelationMention {
                    relation_type_qname: "brain:member_of".into(),
                    subject_text: "Priya".into(),
                    object_text: "Acme".into(),
                    confidence: 0.9,
                    extractor_id: 3,
                    extractor_version: 1,
                }),
                // Coined subject not in the entity mentions above — must be
                // minted so the fact persists. The mention kind is Fact (what
                // the LLM projection always emits), but `likes` is a
                // Preference-kind predicate: the worker must stamp the declared
                // kind so the create-time kind_constraint check accepts it.
                ExtractedItem::StatementMention(StatementMention {
                    kind: statement_kind_to_byte(StatementKind::Fact),
                    subject_text: Some("Melanie's kids".into()),
                    subject_is_memory: false,
                    predicate_qname: "brain:likes".into(),
                    object_text: Some("dinosaurs".into()),
                    confidence: 0.9,
                    extractor_id: 3,
                    extractor_version: 1,
                    is_stateful: false,
                    object_is_entity: false,
                    event_at_unix_nanos: None,
                    subject_is_self: false,
                    retract: false,
                }),
                // Pronoun subject — must be rejected (no entity minted).
                ExtractedItem::StatementMention(StatementMention {
                    kind: StatementKind::Fact.as_u8(),
                    subject_text: Some("they".into()),
                    subject_is_memory: false,
                    predicate_qname: "brain:likes".into(),
                    object_text: Some("noise".into()),
                    confidence: 0.9,
                    extractor_id: 3,
                    extractor_version: 1,
                    is_stateful: false,
                    object_is_entity: false,
                    event_at_unix_nanos: None,
                    subject_is_self: false,
                    retract: false,
                }),
            ],
            pattern: tier_status::ABSENT,
            classifier: tier_status::ABSENT,
            llm: tier_status::RAN,
            failure_reason: None,
            llm_failure_class: ExtractionFailureClass::Unclassified,
            llm_cost_micro_usd: 0,
        };

        let memory_id = MemoryId::pack(0, 1, 1);
        let _ = futures_lite::future::block_on(apply_outcome(&worker, &ctx, memory_id, &outcome))
            .expect("apply_outcome");

        // Resolve the ids the apply pass coined. reports_to was written as a
        // predicate (the StatementMention axis); member_of was written as a
        // relation_type (the RelationMention axis). intern_or_get is
        // idempotent by qname, so it returns the rows the apply pass produced.
        let (reports_to_pred, member_of_rt) = {
            let wtxn = metadata.write_txn().unwrap();
            let p = predicate_intern_or_get(&wtxn, "brain", "reports_to", 0, 0).unwrap();
            let r = relation_type_intern_or_get(&wtxn, "brain", "member_of", 0, 0).unwrap();
            wtxn.commit().unwrap();
            (p, r)
        };

        let rtxn = metadata.read_txn().unwrap();
        let priya = entity_lookup_by_canonical_name(&rtxn, EntityType::PERSON_ID, "Priya")
            .unwrap()
            .expect("Priya created during apply pass 1");

        // (a) The reports_to StatementMention is written as a *statement* on the
        // coined reports_to predicate — axis-faithful, not flipped to a relation.
        let stmts = statement_list(
            &rtxn,
            &StatementListFilter {
                subject: Some(priya),
                predicate: Some(reports_to_pred),
                ..StatementListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(
            stmts.len(),
            1,
            "reports_to StatementMention must persist as a statement"
        );
        assert_eq!(stmts[0].predicate, reports_to_pred);

        // (b) The member_of RelationMention is written as a *relation* on the
        // coined member_of relation_type — axis-faithful, not flipped to a
        // statement and not dropped.
        let rels = relation_list_from(
            &rtxn,
            priya,
            &RelationListFilter {
                relation_type: Some(member_of_rt),
                ..RelationListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(
            rels.len(),
            1,
            "member_of RelationMention must persist as a relation"
        );
        assert_eq!(rels[0].relation_type, member_of_rt);

        // (b') A `kind=Relation` StatementMention with an entity object is an
        // entity↔entity link: it must persist as a *relation* on a coined
        // relation_type, never as a kind=Relation statement.
        let (partner_pred, partner_rt) = {
            let wtxn = metadata.write_txn().unwrap();
            let p = predicate_intern_or_get(&wtxn, "brain", "partner_of", 0, 0).unwrap();
            let r = relation_type_intern_or_get(&wtxn, "brain", "partner_of", 0, 0).unwrap();
            wtxn.commit().unwrap();
            (p, r)
        };
        let partner_rels = relation_list_from(
            &rtxn,
            priya,
            &RelationListFilter {
                relation_type: Some(partner_rt),
                ..RelationListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(
            partner_rels.len(),
            1,
            "kind=Relation StatementMention with an entity object must persist as a relation"
        );
        assert_eq!(partner_rels[0].relation_type, partner_rt);
        let partner_stmts = statement_list(
            &rtxn,
            &StatementListFilter {
                subject: Some(priya),
                predicate: Some(partner_pred),
                ..StatementListFilter::default()
            },
        )
        .unwrap();
        assert!(
            partner_stmts.is_empty(),
            "entity↔entity link must not also persist as a statement"
        );

        // (c) A coined subject the classifier never extracted ("Melanie's
        // kids") is minted as an entity so its fact persists; a pronoun
        // subject ("they") is rejected so junk can't pollute the graph.
        let likes_id = {
            let wtxn = metadata.write_txn().unwrap();
            let p = predicate_intern_or_get(&wtxn, "brain", "likes", 0, 0).unwrap();
            wtxn.commit().unwrap();
            p
        };
        let kids =
            brain_metadata::entity_resolve_canonical_all_types(&rtxn, "Melanie's kids").unwrap();
        assert_eq!(
            kids.len(),
            1,
            "coined subject 'Melanie's kids' should be minted"
        );
        let kids_stmts = statement_list(
            &rtxn,
            &StatementListFilter {
                subject: Some(kids[0]),
                predicate: Some(likes_id),
                ..StatementListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(kids_stmts.len(), 1, "the coined-subject fact must persist");
        assert!(
            brain_metadata::entity_resolve_canonical_all_types(&rtxn, "they")
                .unwrap()
                .is_empty(),
            "pronoun subject 'they' must not be minted"
        );
    }

    /// Accuracy + domain/language generality through `apply_outcome`:
    /// - a non-ASCII predicate persists (open-vocab, no per-memory abort),
    /// - one un-anchorable triple (pronoun subject) is skipped + counted while
    ///   every other item in the SAME memory still lands (per-item skip),
    /// - an entity-subject Event with no timestamp is downgraded to Fact and
    ///   persists rather than being dropped (B1 safe-booster),
    /// - a relation whose object endpoint wasn't separately surfaced is minted
    ///   best-effort and persists (A5).
    #[test]
    #[allow(clippy::arc_with_non_send_sync)]
    fn apply_is_domain_agnostic_and_lossless() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        use brain_core::{EntityType, MemoryId, StatementKind};
        use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
        use brain_index::{IndexParams, SharedHnsw};
        use brain_metadata::entity::ops::entity_lookup_by_canonical_name;
        use brain_metadata::relation::ops::{relation_list_from, RelationListFilter};
        use brain_metadata::relation::types::relation_type_intern_or_get;
        use brain_metadata::schema::predicate::predicate_intern_or_get;
        use brain_metadata::statement::{statement_list, StatementListFilter};
        use brain_metadata::MetadataDb;
        use brain_ops::RealWriterHandle;
        use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};

        use brain_extractors::{RelationMention, StatementMention};

        use crate::context::WorkerContext;

        struct ZeroDispatcher;
        impl Dispatcher for ZeroDispatcher {
            fn embed(&self, _t: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
                Ok([0.0; VECTOR_DIM])
            }
            fn embed_batch(&self, t: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
                Ok(vec![[0.0; VECTOR_DIM]; t.len()])
            }
            fn fingerprint(&self) -> [u8; 16] {
                [0xCD; 16]
            }
        }

        let tempdir = tempfile::tempdir().unwrap();
        let metadata: SharedMetadataDb =
            Arc::new(MetadataDb::open(tempdir.path().join("md.redb")).unwrap());
        let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
        let executor = ExecutorContext::new(
            Arc::new(ZeroDispatcher) as Arc<dyn Dispatcher>,
            shared,
            metadata.clone(),
            writer.clone() as Arc<dyn WriterHandle>,
        );
        let ops = Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor));
        let ctx = WorkerContext {
            ops,
            shutdown: Arc::new(AtomicBool::new(false)),
        };
        let (_tx, rx) = flume::unbounded();
        let worker = ExtractorWorker::new(rx);

        let mention = |text: &str, qn: &str| {
            ExtractedItem::EntityMention(EntityMention {
                entity_type_qname: qn.into(),
                text: text.into(),
                start: 0,
                end: text.chars().count(),
                confidence: 0.95,
                extractor_id: 2,
                extractor_version: 1,
            })
        };
        let stmt = |subject: &str, predicate: &str, object: &str, kind: StatementKind| {
            ExtractedItem::StatementMention(StatementMention {
                // Wire convention (core byte + 1) — the same byte the LLM tier
                // emits and `statement_kind_from_byte` decodes.
                kind: statement_kind_to_byte(kind),
                subject_text: Some(subject.into()),
                subject_is_memory: false,
                predicate_qname: predicate.into(),
                object_text: Some(object.into()),
                confidence: 0.9,
                extractor_id: 3,
                extractor_version: 1,
                is_stateful: false,
                object_is_entity: false,
                event_at_unix_nanos: None,
                subject_is_self: false,
                retract: false,
            })
        };
        let outcome = PipelineOutcome {
            items: vec![
                mention("李明", "brain:Person"),
                // Non-ASCII, open-vocab predicate — must persist, must not abort
                // the whole memory.
                stmt("李明", "brain:作用于", "warfarin", StatementKind::Fact),
                // Entity-subject Event with no event time — must downgrade to
                // Fact and persist (not drop).
                stmt(
                    "李明",
                    "brain:traveled_to",
                    "Shanghai",
                    StatementKind::Event,
                ),
                // Pronoun subject — un-anchorable; skipped + counted, must not
                // abort the others.
                stmt("they", "brain:likes", "noise", StatementKind::Fact),
                // Relation whose object endpoint ("Sam") was not separately
                // surfaced — minted best-effort, relation persists.
                ExtractedItem::RelationMention(RelationMention {
                    relation_type_qname: "brain:collaborates_with".into(),
                    subject_text: "李明".into(),
                    object_text: "Sam".into(),
                    confidence: 0.9,
                    extractor_id: 3,
                    extractor_version: 1,
                }),
            ],
            pattern: tier_status::ABSENT,
            classifier: tier_status::ABSENT,
            llm: tier_status::RAN,
            failure_reason: None,
            llm_failure_class: ExtractionFailureClass::Unclassified,
            llm_cost_micro_usd: 0,
        };

        let memory_id = MemoryId::pack(0, 1, 1);
        let applied =
            futures_lite::future::block_on(apply_outcome(&worker, &ctx, memory_id, &outcome))
                .expect("apply_outcome must not error on per-item problems");
        // Two entity-subject statements landed (作用于 + traveled_to-as-Fact);
        // the pronoun triple did not.
        assert_eq!(applied.counts.statements, 2, "both real facts must persist");
        assert_eq!(
            applied.counts.relations, 1,
            "object-only relation must persist"
        );

        let (zuoyongyu, traveled_to, collaborates) = {
            let wtxn = metadata.write_txn().unwrap();
            let a = predicate_intern_or_get(&wtxn, "brain", "作用于", 0, 0).unwrap();
            let b = predicate_intern_or_get(&wtxn, "brain", "traveled_to", 0, 0).unwrap();
            let c = relation_type_intern_or_get(&wtxn, "brain", "collaborates_with", 0, 0).unwrap();
            wtxn.commit().unwrap();
            (a, b, c)
        };

        let rtxn = metadata.read_txn().unwrap();
        let liming = entity_lookup_by_canonical_name(&rtxn, EntityType::PERSON_ID, "李明")
            .unwrap()
            .expect("李明 minted in pass 1");

        // Non-ASCII predicate statement persisted.
        let zuo = statement_list(
            &rtxn,
            &StatementListFilter {
                subject: Some(liming),
                predicate: Some(zuoyongyu),
                ..StatementListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(zuo.len(), 1, "non-ASCII predicate fact must persist");

        // Event-with-no-timestamp downgraded to Fact and persisted.
        let trav = statement_list(
            &rtxn,
            &StatementListFilter {
                subject: Some(liming),
                predicate: Some(traveled_to),
                ..StatementListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(
            trav.len(),
            1,
            "mis-kinded Event must persist as Fact, not drop"
        );
        assert_eq!(
            trav[0].kind,
            StatementKind::Fact,
            "entity-subject Event without a timestamp is downgraded to Fact"
        );

        // Object-only relation endpoint minted; relation persisted.
        assert!(
            !brain_metadata::entity_resolve_canonical_all_types(&rtxn, "Sam")
                .unwrap()
                .is_empty(),
            "object-only relation endpoint 'Sam' must be minted"
        );
        let rels = relation_list_from(
            &rtxn,
            liming,
            &RelationListFilter {
                relation_type: Some(collaborates),
                ..RelationListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(rels.len(), 1, "object-only relation must persist");

        // The pronoun subject was skipped + counted as signal loss, not minted.
        assert!(
            brain_metadata::entity_resolve_canonical_all_types(&rtxn, "they")
                .unwrap()
                .is_empty(),
            "pronoun subject must not be minted"
        );
        let dropped = worker.metrics().snapshot().apply_dropped_total;
        assert!(
            dropped.get("subject_unresolved").copied().unwrap_or(0) >= 1,
            "the dropped pronoun triple must be counted, not silent: {dropped:?}"
        );
    }

    /// Object axis (#1) + entity-subject event time (#2):
    /// - an entity object the LLM flags (`object_is_entity`) but that wasn't
    ///   separately surfaced is minted and linked as an `Entity` object;
    /// - a literal object stays a text `Value` (never minted as junk);
    /// - an entity-subject Event WITH a resolved time persists as an Event
    ///   carrying `event_at`; one WITHOUT a time downgrades to Fact.
    #[test]
    #[allow(clippy::arc_with_non_send_sync)]
    fn apply_object_axis_and_event_time() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        use brain_core::{EntityType, MemoryId, StatementKind, StatementObject};
        use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
        use brain_index::{IndexParams, SharedHnsw};
        use brain_metadata::entity::ops::entity_lookup_by_canonical_name;
        use brain_metadata::schema::predicate::predicate_intern_or_get;
        use brain_metadata::statement::{statement_list, StatementListFilter};
        use brain_metadata::MetadataDb;
        use brain_ops::RealWriterHandle;
        use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};

        use brain_extractors::StatementMention;

        use crate::context::WorkerContext;

        struct ZeroDispatcher;
        impl Dispatcher for ZeroDispatcher {
            fn embed(&self, _t: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
                Ok([0.0; VECTOR_DIM])
            }
            fn embed_batch(&self, t: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
                Ok(vec![[0.0; VECTOR_DIM]; t.len()])
            }
            fn fingerprint(&self) -> [u8; 16] {
                [0xCD; 16]
            }
        }

        let tempdir = tempfile::tempdir().unwrap();
        let metadata: SharedMetadataDb =
            Arc::new(MetadataDb::open(tempdir.path().join("md.redb")).unwrap());
        let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
        let executor = ExecutorContext::new(
            Arc::new(ZeroDispatcher) as Arc<dyn Dispatcher>,
            shared,
            metadata.clone(),
            writer.clone() as Arc<dyn WriterHandle>,
        );
        let ops = Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor));
        let ctx = WorkerContext {
            ops,
            shutdown: Arc::new(AtomicBool::new(false)),
        };
        let (_tx, rx) = flume::unbounded();
        let worker = ExtractorWorker::new(rx);

        let mention = |text: &str| {
            ExtractedItem::EntityMention(EntityMention {
                entity_type_qname: "brain:Person".into(),
                text: text.into(),
                start: 0,
                end: text.chars().count(),
                confidence: 0.95,
                extractor_id: 2,
                extractor_version: 1,
            })
        };
        #[allow(clippy::too_many_arguments)]
        let stmt = |predicate: &str,
                    object: &str,
                    object_is_entity: bool,
                    kind: StatementKind,
                    event_at: Option<u64>| {
            ExtractedItem::StatementMention(StatementMention {
                kind: statement_kind_to_byte(kind),
                subject_text: Some("Alice".into()),
                subject_is_memory: false,
                predicate_qname: predicate.into(),
                object_text: Some(object.into()),
                confidence: 0.9,
                extractor_id: 3,
                extractor_version: 1,
                is_stateful: false,
                object_is_entity,
                event_at_unix_nanos: event_at,
                subject_is_self: false,
                retract: false,
            })
        };
        const T: u64 = 1_577_836_800_000_000_000; // 2020-01-01
        let outcome = PipelineOutcome {
            items: vec![
                mention("Alice"),
                // Entity object (Tokyo) the LLM flags but that wasn't separately
                // surfaced → minted + linked as an Entity object. Also an Event
                // WITH a resolved time → persists as an Event carrying event_at.
                stmt(
                    "brain:traveled_to",
                    "Tokyo",
                    true,
                    StatementKind::Event,
                    Some(T),
                ),
                // Literal object → stays a text Value, never minted as an entity.
                stmt(
                    "brain:favorite_color",
                    "blue",
                    false,
                    StatementKind::Fact,
                    None,
                ),
                // Event WITHOUT a time → downgraded to Fact (still persists).
                stmt("brain:visited", "Berlin", true, StatementKind::Event, None),
            ],
            pattern: tier_status::ABSENT,
            classifier: tier_status::ABSENT,
            llm: tier_status::RAN,
            failure_reason: None,
            llm_failure_class: ExtractionFailureClass::Unclassified,
            llm_cost_micro_usd: 0,
        };

        let memory_id = MemoryId::pack(0, 1, 1);
        futures_lite::future::block_on(apply_outcome(&worker, &ctx, memory_id, &outcome))
            .expect("apply_outcome");

        let (traveled_to, favorite_color, visited) = {
            let wtxn = metadata.write_txn().unwrap();
            let a = predicate_intern_or_get(&wtxn, "brain", "traveled_to", 0, 0).unwrap();
            let b = predicate_intern_or_get(&wtxn, "brain", "favorite_color", 0, 0).unwrap();
            let c = predicate_intern_or_get(&wtxn, "brain", "visited", 0, 0).unwrap();
            wtxn.commit().unwrap();
            (a, b, c)
        };
        let rtxn = metadata.read_txn().unwrap();
        let alice = entity_lookup_by_canonical_name(&rtxn, EntityType::PERSON_ID, "Alice")
            .unwrap()
            .expect("Alice minted");
        let one = |pred| {
            let v = statement_list(
                &rtxn,
                &StatementListFilter {
                    subject: Some(alice),
                    predicate: Some(pred),
                    ..StatementListFilter::default()
                },
            )
            .unwrap();
            assert_eq!(
                v.len(),
                1,
                "expected exactly one statement for the predicate"
            );
            v.into_iter().next().unwrap()
        };

        // #2: Event with a time persists as an Event carrying event_at.
        let trav = one(traveled_to);
        assert_eq!(
            trav.kind,
            StatementKind::Event,
            "kept as Event (has a time)"
        );
        assert_eq!(trav.event_at_unix_nanos, Some(T), "event_at plumbed");
        // #1: the flagged entity object (Tokyo) is minted + linked as an Entity.
        assert!(
            matches!(trav.object, StatementObject::Entity(_)),
            "flagged entity object must be an Entity ref, got {:?}",
            trav.object
        );
        assert!(
            !brain_metadata::entity_resolve_canonical_all_types(&rtxn, "Tokyo")
                .unwrap()
                .is_empty(),
            "object entity 'Tokyo' must be minted"
        );

        // #1: a literal object stays a text Value and is NOT minted.
        let fav = one(favorite_color);
        assert!(
            matches!(fav.object, StatementObject::Value(_)),
            "literal object must stay a Value, got {:?}",
            fav.object
        );
        assert!(
            brain_metadata::entity_resolve_canonical_all_types(&rtxn, "blue")
                .unwrap()
                .is_empty(),
            "literal 'blue' must NOT be minted as an entity"
        );

        // #2: an Event with no time downgrades to Fact but still persists (and
        // its flagged entity object is still minted/linked).
        let vis = one(visited);
        assert_eq!(
            vis.kind,
            StatementKind::Fact,
            "timeless Event downgraded to Fact"
        );
        assert_eq!(vis.event_at_unix_nanos, None);
        assert!(
            matches!(vis.object, StatementObject::Entity(_)),
            "flagged entity object minted even when the Event downgraded"
        );
    }
}
