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

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::workers::hype::{HypeGenOutcome, HypeGenerator};
use brain_core::{
    AuditId, EntityId, ExtractorId, Memory as CoreMemory, MemoryId, MemoryKind, Salience,
    SessionId, SpaceId,
};
use brain_core::{StatementKind, StatementObject, StatementValue, SubjectRef};
use brain_extractors::{
    build_registry_with_gate,
    resolver::{
        resolve_or_create_with_deps, Disambiguation, EmbeddingDeps, EntityDisambiguator,
        PendingVerdict, PrecomputedVerdicts, ResolutionTier, ResolverError, StagedEntityVectors,
    },
    EntityMention, ExtractedItem, ExtractionContext, ExtractionFailureClass, ExtractionResult,
    ExtractionStatus, Extractor, ExtractorContext, ExtractorRegistry, MaterializeDeps,
    StatementMention, TemporalExtractor, TierGate, TriggerDecision, SYSTEM_NAMESPACE,
};
use brain_metadata::audit_write;
use brain_metadata::relation::types::relation_type_intern_or_get;
use brain_metadata::schema::predicate::predicate_intern_or_get;
use brain_metadata::tables::audit::{
    extraction_status, resolution_outcome, ExtractionAudit, ResolutionAudit,
};
use brain_metadata::tables::edge::{
    self, derived_by, origin, zero_disambiguator, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE,
};
use brain_metadata::tables::extractor_audit::{
    pipeline_status, record_extracted, tier_status, ExtractorItemCounts,
    ExtractorPipelineAuditEntry,
};
use brain_metadata::tables::predicate::{PREDICATES_TABLE, PREDICATE_EMBEDDINGS_TABLE};
use brain_metadata::tables::relation::RELATION_TYPE_EMBEDDINGS_TABLE;
use brain_metadata::{
    entity_get_inside_wtxn, hype_has_vectors, pipeline_has_extracted, resolution_audit_write,
};
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
    EventType, StageAuditStatus, StageExtractorPayload, StageHypePayload, StageKind, StageOutcome,
    StagePayload,
};
use futures_lite::FutureExt;
use glommio::timer::sleep;
use parking_lot::Mutex;
use redb::ReadableTable;
use tracing::{debug, trace, warn};

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

/// How many rows `load_pending_batch` over-drains per `want`: it scans up
/// to `want * FACTOR` front rows (bounded by [`EXTRACTION_QUEUE_MAX_SCAN`])
/// to page past a front window stuck in transient-failure backoff, so a
/// due row deeper in the queue is still found and processed this cycle
/// instead of being starved behind the backing-off front.
const EXTRACTION_QUEUE_OVERDRAIN_FACTOR: usize = 8;

/// Hard ceiling on rows scanned per `load_pending_batch` call so the
/// over-drain stays bounded even when `want` is large and the whole front
/// of the queue is backing off.
const EXTRACTION_QUEUE_MAX_SCAN: usize = 1024;

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
    /// Materialize dependencies + tier gate captured at shard spawn so
    /// the worker can rebuild the registry when a `SCHEMA_UPLOAD` adds
    /// or changes an extractor. `None` (tests, substrate deployments)
    /// leaves the registry static: the dirty flag is never consumed and
    /// the boot-time registry serves for the shard's lifetime.
    rebuild_deps: Option<RegistryRebuildDeps>,
}

/// The heavy dependencies a registry rebuild needs: the classifier
/// model / LLM router / cache bundle (`MaterializeDeps`) and the
/// deploy-time per-tier gate. Captured once at shard spawn and reused
/// on every live rebuild so the request path never has to thread them.
#[derive(Clone)]
pub struct RegistryRebuildDeps {
    /// Classifier model + LLM router + cache. `entity_type_qnames` is
    /// re-snapshotted from the live schema on each rebuild (a schema
    /// upload can add entity types), so the value carried here is only
    /// the startup fallback for that one field.
    pub deps: MaterializeDeps,
    /// Deploy-time `extractors.{pattern,classifier,llm}.enabled` gate.
    pub gate: TierGate,
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
            rebuild_deps: None,
        }
    }

    /// Wire the registry-rebuild dependencies. Without this call the
    /// worker never rebuilds the registry: the boot-time registry is
    /// static and a `SCHEMA_UPLOAD` that adds an extractor only takes
    /// effect after a restart. Production shards set this so the
    /// registry refreshes live. Tests and substrate deployments that
    /// don't exercise schema-driven extractor changes leave it unset.
    #[must_use]
    pub fn with_registry_rebuild_deps(mut self, deps: MaterializeDeps, gate: TierGate) -> Self {
        self.rebuild_deps = Some(RegistryRebuildDeps { deps, gate });
        self
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

    // Consume a pending SCHEMA_UPLOAD registry-dirty signal before draining
    // so this cycle's extraction runs against the just-declared extractors.
    // Off the request path by design — the rebuild materialises the
    // classifier model / LLM router, which the handler must not touch.
    maybe_rebuild_registry(worker, ctx);

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

    // Drain up to the full `drain_per_cycle` each cycle, processing it in
    // `batch_size` micro-batches (the `while processed < cycle_cap` loop
    // below). Previously this was `min(drain_per_cycle, batch_size)`, which
    // silently pinned throughput to one micro-batch (8) even though
    // `drain_per_cycle` was 32 — starving derivation under a write burst.
    let cycle_cap = worker.knobs.drain_per_cycle;
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
            let produced_statements = counts.statements > 0;
            let space_id = memory_scope_and_session(ctx, *memory_id).0.space();
            publish_extracted_graph(ctx, *memory_id, space_id, counts, audit_status).await;
            // Reclassify the memory kind from what extraction produced: an
            // Event-shaped statement → Episodic, timeless facts/preferences →
            // Semantic. Only on a terminal apply (`!keep_queued`) that produced
            // statements; a no-statement memory keeps the Episodic default.
            if !keep_queued && produced_statements {
                writeback_memory_kind(ctx, *memory_id);
            }
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

/// Rebuild the extractor registry from the live `EXTRACTORS_TABLE` when
/// a `SCHEMA_UPLOAD` flagged it dirty, then swap the fresh registry in.
///
/// This is what makes a user-declared extractor fire without a shard
/// restart: the boot-time registry is built once, and this is the only
/// path that refreshes it. Runs at the top of the extractor cycle so the
/// same cycle's extraction already sees the new extractors, and off the
/// request path so the heavy build dependencies (classifier model, LLM
/// router) stay out of the `SCHEMA_UPLOAD` handler.
///
/// Failure-safe: a build that yields materialisation errors (a corrupt
/// or unknown-kind row) leaves the prior working registry untouched
/// rather than swapping in a partial one — the shard never ends up with
/// missing extractors. The dirty flag is cleared once the signal is
/// consumed (success or a deterministic build failure) so a single bad
/// row doesn't rebuild every cycle; a fresh upload re-flips it.
///
/// No lock is held across an `.await`: the whole rebuild is synchronous
/// (redb reads + `build_registry_with_gate`), and the write-lock swap is
/// a single move. Single-writer-per-shard means the handler that sets
/// the flag and this consumer never run concurrently.
fn maybe_rebuild_registry(worker: &ExtractorWorker, ctx: &WorkerContext) {
    use std::sync::atomic::Ordering;

    let Some(rebuild) = worker.rebuild_deps.as_ref() else {
        return;
    };
    if !ctx.ops.extractors_dirty.load(Ordering::Acquire) {
        return;
    }

    // Read the current definitions + a fresh entity-type snapshot in one
    // read txn so a classifier declared alongside new entity types
    // materialises active rather than degraded.
    let (defs, entity_types) = {
        let rtxn = match ctx.ops.executor.metadata.read_txn() {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    target: "brain_workers::extractor",
                    error = %e,
                    "registry rebuild: read_txn failed; keeping prior registry, will retry",
                );
                return;
            }
        };
        let defs = match brain_metadata::extractor_list(&rtxn) {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    target: "brain_workers::extractor",
                    error = %e,
                    "registry rebuild: extractor_list failed; keeping prior registry, will retry",
                );
                return;
            }
        };
        let entity_types = brain_metadata::entity_type_label_qnames(&rtxn).unwrap_or_default();
        (defs, entity_types)
    };

    let mut deps = rebuild.deps.clone();
    deps.entity_type_qnames = Arc::new(entity_types);
    let (mut reg, errors) = build_registry_with_gate(&defs, &deps, rebuild.gate);
    if !errors.is_empty() {
        // A genuinely-broken definition (bad JSON blob, unknown kind).
        // Operator LLM misconfigurations register as degraded extractors,
        // not errors, so this only fires on corruption. Don't swap in a
        // partial registry — that could drop the previously-working
        // extractors. Consume the signal (clear dirty) so we don't rebuild
        // this same corrupt state every cycle; a re-upload re-flips it.
        let detail = errors
            .iter()
            .map(|(id, err)| format!("extractor {}: {err}", id.raw()))
            .collect::<Vec<_>>()
            .join("; ");
        warn!(
            target: "brain_workers::extractor",
            errors = %detail,
            "registry rebuild produced materialisation errors; keeping prior registry",
        );
        ctx.ops.extractors_dirty.store(false, Ordering::Release);
        return;
    }

    // Re-register the native built-in temporal-expressions extractor: it's
    // wired programmatically at shard spawn (not materialisable from a
    // schema row), so a fresh registry would otherwise lose it.
    reg.register(Arc::new(TemporalExtractor::new()));

    let new_count = reg.iter_enabled().count();
    *ctx.ops.extractor_registry.write() = reg;
    ctx.ops.extractors_dirty.store(false, Ordering::Release);
    debug!(
        target: "brain_workers::extractor",
        enabled_extractors = new_count,
        "registry rebuilt after SCHEMA_UPLOAD",
    );
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
    //
    // The over-drain is what prevents head-of-line blocking: the queue is
    // keyed by MemoryId, so a full front window of `limit` rows stuck in
    // backoff (e.g. the oldest rows that reliably time out, widening their
    // backoff) would otherwise yield zero due rows and stall every newer
    // due row behind them. We page past the front, scanning up to a bounded
    // multiple of `limit` rows, and return the first `limit` DUE rows found
    // — bounding the total scan per cycle while never starving deeper rows.
    let now = now_unix_nanos();
    if limit == 0 {
        return Ok(Vec::new());
    }
    let scan_cap = limit
        .saturating_mul(EXTRACTION_QUEUE_OVERDRAIN_FACTOR)
        .min(EXTRACTION_QUEUE_MAX_SCAN);
    let pending = brain_metadata::extraction_queue_drain(&rtxn, scan_cap)
        .map_err(|e| format!("extraction_queue_drain: {e}"))?;
    let mut due = Vec::with_capacity(limit.min(pending.len()));
    for (id, _) in pending {
        if due.len() >= limit {
            break;
        }
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
/// Row-derived facts about one queued memory, read once per micro-batch.
///
/// Carries the event/write timestamps (so the temporal extractor can
/// anchor relative dates to the real event time), the owning namespace
/// and space (so the LLM tier scopes extractor selection AND keys its
/// response cache by the memory's real space), the session, and the real
/// stored kind (so a trigger `where memory.kind = ...` evaluates against
/// the stored kind). A row miss omits the id; callers fall back to
/// anonymous defaults — never a drop.
struct RowFacts {
    created_ns: u64,
    occurred: Option<u64>,
    namespace_id: u32,
    space: SpaceId,
    session_id: SessionId,
    kind: MemoryKind,
}

/// Read each live memory's [`RowFacts`] in a single read txn. Ids whose
/// row is absent (forgotten before extraction ran) are omitted.
fn load_row_facts(
    ctx: &WorkerContext,
    live: &[(usize, MemoryId, Arc<str>)],
) -> HashMap<MemoryId, RowFacts> {
    use brain_metadata::tables::memory::MEMORIES_TABLE;
    let mut m = HashMap::with_capacity(live.len());
    if let Ok(rtxn) = ctx.ops.executor.metadata.read_txn() {
        if let Ok(t) = rtxn.open_table(MEMORIES_TABLE) {
            for (_, mid, _) in live {
                if let Ok(Some(g)) = t.get(&mid.to_be_bytes()) {
                    let row = g.value();
                    m.insert(
                        *mid,
                        RowFacts {
                            created_ns: row.created_at_unix_nanos,
                            occurred: row.occurred_at_unix_nanos,
                            namespace_id: row.namespace_id,
                            space: row.space_id(),
                            session_id: SessionId::from(row.session_id),
                            kind: row.kind().unwrap_or(MemoryKind::Episodic),
                        },
                    );
                }
            }
        }
    }
    m
}

/// Build the [`CoreMemory`] the extraction tiers see for each live row.
///
/// Threads each memory's REAL `(space, session)` from its row facts so
/// the LLM tier's response-cache key is stable across cycles for the same
/// `(text, space)`. Minting a fresh space per call would make the key
/// change every cycle, so the per-shard LLM cache would never hit and the
/// always-on tier's cost/latency control would be defeated. A row miss
/// falls back to the anonymous defaults, never a drop.
fn build_extraction_core_memories(
    live: &[(usize, MemoryId, Arc<str>)],
    row_facts: &HashMap<MemoryId, RowFacts>,
) -> Vec<CoreMemory> {
    live.iter()
        .map(|(_, mid, text)| {
            let (created_ns, occurred, space, session_id, kind) = row_facts
                .get(mid)
                .map(|f| (f.created_ns, f.occurred, f.space, f.session_id, f.kind))
                .unwrap_or((0, None, SpaceId::NIL, SessionId(0), MemoryKind::Episodic));
            CoreMemory {
                id: *mid,
                space,
                session_id,
                kind,
                salience: Salience::default(),
                text: Some(text.to_string()),
                created_at_unix_ms: created_ns / 1_000_000,
                last_accessed_at_unix_ms: 0,
                occurred_at_unix_nanos: occurred,
            }
        })
        .collect()
}

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

    // Fetch each live memory's row facts in one read txn for the whole
    // micro-batch: the event/write timestamps (so the temporal extractor
    // can anchor relative dates like "last week" to the real event time —
    // `occurred_at`, else `created_at` — rather than to zero), the owning
    // namespace id (so the LLM tier can scope extractor selection to a
    // memory's own namespace), and the real memory kind (so a trigger
    // `where memory.kind = ...` evaluates against the stored kind, not a
    // hardcoded one). A row miss falls back to zero timestamps, the system
    // namespace, and Episodic — never drops the memory.
    let row_facts = load_row_facts(ctx, &live);
    let live_mems: Vec<CoreMemory> = build_extraction_core_memories(&live, &row_facts);

    // Resolve each live memory's owning namespace to its name so the LLM
    // tier can scope selection: a namespace's own enabled LLM extractor
    // replaces the seeded `brain` default for that namespace's memories.
    // Distinct ids resolve once against one read txn; an unresolved id (or
    // a row miss) falls back to the system namespace so the system default
    // runs — never a silent drop, never a cross-tenant over-run.
    let mem_namespaces: Vec<String> = {
        let rtxn = ctx.ops.executor.metadata.read_txn().ok();
        let mut cache: HashMap<u32, String> = HashMap::new();
        live.iter()
            .map(|(_, mid, _)| {
                let ns_id = row_facts
                    .get(mid)
                    .map(|f| f.namespace_id)
                    .unwrap_or_else(|| brain_core::NamespaceId::SYSTEM.raw());
                if let Some(name) = cache.get(&ns_id) {
                    return name.clone();
                }
                let name = rtxn
                    .as_ref()
                    .and_then(|r| {
                        brain_metadata::namespace::namespace_name(
                            r,
                            brain_core::NamespaceId::from(ns_id),
                        )
                        .ok()
                        .flatten()
                    })
                    .unwrap_or_else(|| SYSTEM_NAMESPACE.to_string());
                cache.insert(ns_id, name.clone());
                name
            })
            .collect()
    };

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

    // Snapshot the active schema's entity types + kind taxonomy as the
    // batch-level prompt blocks for the LLM tier. Read per cycle from ONE
    // read txn so both blocks reflect a single consistent schema view; a
    // user's SCHEMA_UPLOAD takes effect on the next batch without a restart.
    // Empty string on any read error degrades to an unconstrained prompt
    // rather than failing extraction.
    //
    // These are batch-level (identical for every memory). The per-memory
    // candidate-predicate blocks are computed separately below because they
    // depend on each memory's text embedding.
    //
    // The classifier (GLiNER) tier's entity-type label set is read from the
    // same txn so a `SCHEMA_UPLOAD` that adds entity types reaches the
    // classifier on the next batch without a shard restart (the labels baked
    // in at spawn are only the fallback). Empty on read error → the classifier
    // falls back to its construction-time labels.
    let (declared_entity_types_block, declared_kinds_block, entity_type_labels): (
        String,
        String,
        Vec<String>,
    ) = match ctx.ops.executor.metadata.read_txn() {
        Ok(rtxn) => (
            brain_metadata::render_declared_entity_types_block(&rtxn).unwrap_or_default(),
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
    let declared_entity_types = if declared_entity_types_block.is_empty() {
        None
    } else {
        Some(declared_entity_types_block.as_str())
    };
    let declared_kinds = if declared_kinds_block.is_empty() {
        None
    } else {
        Some(declared_kinds_block.as_str())
    };

    // Per-memory candidate predicates: for each memory, the top-K existing
    // `brain:` predicates nearest its text, so the LLM reuses this DB's real
    // relation vocabulary instead of coining a near-duplicate (the embedding
    // recalls the neighborhood; the LLM picks the exact synonym / converse).
    // Best-effort: computed only when an embedder is wired and an LLM tier
    // will consume it; any embed/scan hiccup degrades a memory to an empty
    // block rather than failing extraction.
    let candidate_predicate_map: Option<HashMap<MemoryId, String>> =
        match worker.embed_deps.as_ref() {
            Some(deps) if !skip_llm_budget_exhausted && llm_exts_count(&extractors) > 0 => ctx
                .ops
                .executor
                .metadata
                .read_txn()
                .ok()
                .map(|rtxn| build_candidate_predicate_map(&rtxn, deps, &live_mems)),
            _ => None,
        };

    // Extraction and HyPE are independent, unordered async stages (spec
    // §05/17a). `build_neighborhood` resolves the entities a memory mentions
    // against the *persisted* registry, not this batch's just-written graph,
    // so HyPE needs only the memory text — it does not depend on extraction's
    // output. Run the two LLM round-trips CONCURRENTLY instead of
    // extraction-then-HyPE: on this single glommio task they interleave only
    // at await points, so the network round-trips overlap while every redb /
    // HNSW write stays serialized (single-writer preserved). This roughly
    // halves the full write-path latency. The only thing given up is a
    // memory's own just-extracted facts feeding its own bridge questions;
    // prior-graph bridging is unaffected.
    let extract_and_apply = async {
        let outcomes = run_pipeline_batch(
            extractors,
            &live_mems,
            &mem_namespaces,
            skip_llm_budget_exhausted,
            extractor_context_map,
            declared_entity_types,
            candidate_predicate_map.as_ref(),
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
                    // wtxn rolled back, nothing persisted. Do NOT write a
                    // FAILURE audit — that would bar re-extraction via the
                    // idempotency gate and permanently abandon a real memory's
                    // graph for a momentary hiccup. Leave the durable queue row
                    // in place so the next cycle retries.
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
    };

    futures_lite::future::zip(extract_and_apply, run_hype_pass(worker, ctx, items)).await;

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
    // Per-memory HyPE generation is independent (spec §05/17a): fan the LLM
    // calls out concurrently so a whole micro-batch costs ~one round-trip
    // instead of N serial ones. Every future runs on this single glommio task
    // and interleaves only at await points — the network round-trips overlap
    // while redb reads and HNSW index inserts stay serialized (single-writer
    // preserved). The per-cycle LLM budget is a soft cap here: a concurrent
    // fan-out can overshoot by at most the in-flight set (bounded by the
    // micro-batch size), which is the intended trade for the latency win.
    let futs = items.iter().map(|(memory_id, text)| async move {
        if cycle_budget > 0 && *worker.llm_spend.lock() >= cycle_budget {
            return;
        }
        // Skip memories that already own question vectors — HyPE generates
        // once per memory, idempotent across re-ingest, and a second insert
        // would duplicate vectors in the live HNSW index.
        let already = match ctx.ops.executor.metadata.as_ref().read_txn() {
            Ok(rtxn) => hype_has_vectors(&rtxn, *memory_id).unwrap_or(false),
            Err(_) => false,
        };
        if already {
            return;
        }
        // Render the typed-graph facts already known about the entities this
        // memory mentions, so HyPE can write multi-hop bridge questions that
        // span more than this one memory (read-side multi-hop then resolves to
        // a single cheap ANN probe — no read LLM). Empty for the first memory
        // about a subject; fills in as the graph grows (and on re-ingest).
        let scope = memory_scope_and_session(ctx, *memory_id).0;
        let neighborhood = build_neighborhood(ctx, scope, text.as_ref());
        let outcome = hype
            .generate_for(*memory_id, text.as_ref(), &neighborhood)
            .await;
        // Stage journal (S4 HyPE). Success was previously metrics-only;
        // this makes the per-memory question count observable so the eval
        // probe can tell an empty HyPE pass (0 questions) apart from a
        // skipped one.
        tracing::debug!(
            target: "brain_debug::stage",
            stage = "S4_hype",
            memory_id = memory_id.raw(),
            questions_written = outcome.questions_written,
            cost_micro_usd = outcome.cost_micro_usd,
            "write stage: HyPE hypothetical questions generated",
        );
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
        // Publish a `StageCompleted{Hype}` event so a `--wait`/SUBSCRIBE
        // caller watching this memory's derivation can tick HyPE off its
        // pending-stage checklist — mirrors `publish_extracted_graph`'s
        // per-memory publish below.
        publish_hype_completed(ctx, *memory_id, scope.space(), outcome).await;
    });
    futures_util::future::join_all(futs).await;
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
        let neighborhood = build_neighborhood(
            ctx,
            memory_scope_and_session(ctx, *memory_id).0,
            text.as_str(),
        );
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
/// Read a memory's `(namespace, space)` scope AND its `session_id` from
/// `MEMORIES_TABLE`. Falls back to the system scope + default session
/// when the row is absent — the neighborhood enrichment it feeds is
/// best-effort prompt context, so a miss simply yields an empty
/// (system-scoped) neighborhood rather than crossing tenants.
fn memory_scope_and_session(
    ctx: &WorkerContext,
    memory_id: MemoryId,
) -> (brain_metadata::RowScope, brain_core::SessionId) {
    use brain_metadata::tables::memory::MEMORIES_TABLE;
    ctx.ops
        .executor
        .metadata
        .as_ref()
        .read_txn()
        .ok()
        .and_then(|rtxn| {
            rtxn.open_table(MEMORIES_TABLE).ok().and_then(|t| {
                t.get(&memory_id.to_be_bytes()).ok().flatten().map(|g| {
                    let m = g.value();
                    (
                        brain_metadata::RowScope::from_bytes(m.namespace_id, m.space_id_bytes),
                        brain_core::SessionId::from(m.session_id),
                    )
                })
            })
        })
        .unwrap_or_else(|| {
            (
                brain_metadata::RowScope::from_bytes(
                    brain_core::NamespaceId::SYSTEM.raw(),
                    [0u8; 16],
                ),
                brain_core::SessionId::DEFAULT,
            )
        })
}

/// Reclassify a memory's [`MemoryKind`] from the statements extraction produced
/// for it, and write it back. A time-bound `Event` statement makes the memory
/// `Episodic`; a memory whose statements are only timeless `Fact`/`Preference`
/// becomes `Semantic`. The caller invokes this only when extraction produced at
/// least one statement — a memory that yielded none (e.g. a chat ack) keeps the
/// `Episodic` default set at ENCODE.
///
/// Modeled on the decay worker's salience write-back (`workers/decay.rs`): a
/// DERIVED, recomputable metadata update. Written directly to redb with no WAL
/// record — like decayed salience, the kind is re-derivable by re-running
/// extraction, so it is not a WAL-durable acknowledged mutation — and IN PLACE,
/// so the `MemoryId` slot-version is unchanged and no HNSW touch is needed (kind
/// isn't indexed). No-op guarded: writes only when the kind actually changes.
///
/// Durability note: unlike decay (which re-runs every cycle and self-heals a
/// missed write), this runs once after extraction. A crash in the window between
/// the statement commit and this write-back leaves the memory at the `Episodic`
/// default until a future re-extraction — an acceptable cosmetic gap for a
/// derived classification (kind drives decay half-life and display, not answer
/// correctness).
fn writeback_memory_kind(ctx: &WorkerContext, memory_id: MemoryId) {
    use brain_metadata::tables::memory::MEMORIES_TABLE;
    use brain_metadata::tables::statement::{STATEMENTS_BY_EVIDENCE_TABLE, STATEMENTS_TABLE};

    let scope = memory_scope_and_session(ctx, memory_id).0;
    let metadata = ctx.ops.executor.metadata.as_ref();
    let mid = memory_id.to_be_bytes();

    // Derive from the statements this memory is evidence for (reverse index),
    // reading each statement's kind. First Event wins → Episodic.
    let had_event = {
        let Ok(rtxn) = metadata.read_txn() else {
            return;
        };
        let (Ok(by_ev), Ok(stmts)) = (
            rtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE),
            rtxn.open_table(STATEMENTS_TABLE),
        ) else {
            return;
        };
        let lo = (scope.namespace_id, scope.space_id_bytes, mid, [0u8; 16]);
        let hi = (scope.namespace_id, scope.space_id_bytes, mid, [0xFFu8; 16]);
        let Ok(range) = by_ev.range(lo..=hi) else {
            return;
        };
        let mut had_event = false;
        for row in range {
            let Ok((key, _)) = row else { continue };
            let (_, _, _, stmt_id) = key.value();
            if let Ok(Some(g)) = stmts.get(&stmt_id) {
                if g.value().kind() == Some(StatementKind::Event) {
                    had_event = true;
                    break;
                }
            }
        }
        had_event
    };

    let new_kind = if had_event {
        MemoryKind::Episodic
    } else {
        MemoryKind::Semantic
    };

    // Write back only when the stored kind actually differs, so an unchanged row
    // never dirties a redb page (the decay-worker no-op guard).
    let Ok(wtxn) = metadata.write_txn() else {
        return;
    };
    {
        let Ok(mut table) = wtxn.open_table(MEMORIES_TABLE) else {
            return;
        };
        let Some(mut meta) = table.get(&mid).ok().flatten().map(|g| g.value()) else {
            return;
        };
        if meta.kind().ok() == Some(new_kind) {
            return;
        }
        meta.kind = new_kind as u8;
        if table.insert(&mid, meta).is_err() {
            return;
        }
    }
    let _ = wtxn.commit();
}

fn build_neighborhood(ctx: &WorkerContext, scope: brain_metadata::RowScope, text: &str) -> String {
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
        let Ok(ids) = entity_resolve_canonical_all_types(&rtxn, scope, &s) else {
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
            scope,
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
        if let Ok(out) = relation_list_from(&rtxn, scope, eid, &rfilter) {
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
        if let Ok(inc) = relation_list_to(&rtxn, scope, eid, &rfilter) {
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
        same_session_only: true,
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

/// Per-call audit facts for one tier of a pipeline run: which extractor
/// produced the tier's recorded outcome, its version, the precise
/// [`extraction_status`] byte, and any non-success reason. Captured
/// alongside the coarse `tier_status` byte so `run_apply_body` can emit
/// one historical [`ExtractionAudit`] row per extractor that actually ran
/// (or was skipped) — `None` means the tier was absent (no extractor
/// present), which produces no audit row.
#[derive(Clone)]
struct TierAudit {
    extractor_id: u32,
    extractor_version: u32,
    /// One of [`extraction_status`] bytes; == `ExtractionStatus::as_u8()`.
    status: u8,
    /// `result.status_reason`; empty on Success.
    reason: String,
}

/// Aggregate of one pipeline run across all enabled extractors.
struct PipelineOutcome {
    items: Vec<ExtractedItem>,
    pattern: u8,
    classifier: u8,
    llm: u8,
    /// Per-call audit facts for the extractor whose outcome each tier's
    /// status byte reflects. `None` when the tier was absent.
    pattern_audit: Option<TierAudit>,
    classifier_audit: Option<TierAudit>,
    llm_audit: Option<TierAudit>,
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
#[allow(clippy::too_many_arguments)]
async fn run_pipeline_batch(
    extractors: Vec<Arc<dyn Extractor>>,
    mems: &[CoreMemory],
    mem_namespaces: &[String],
    skip_llm_budget_exhausted: bool,
    extractor_context_map: Option<HashMap<MemoryId, ExtractorContext>>,
    declared_entity_types: Option<&str>,
    candidate_predicate_map: Option<&HashMap<MemoryId, String>>,
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
            pattern_audit: None,
            classifier_audit: None,
            llm_audit: None,
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
            declared_entity_types,
            candidate_predicates: None,
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
            declared_entity_types,
            candidate_predicates: None,
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
        // Attribute the skip to the first configured LLM extractor so the
        // per-call audit records a SkippedBudget row rather than dropping
        // the tier silently.
        let llm_ident = llm_exts
            .first()
            .map(|e| (e.id().raw(), e.extractor_version()));
        for o in &mut outcomes {
            o.llm = tier_status::SKIPPED;
            if let Some((extractor_id, extractor_version)) = llm_ident {
                o.llm_audit = Some(TierAudit {
                    extractor_id,
                    extractor_version,
                    status: extraction_status::SKIPPED_BUDGET,
                    reason: "cycle LLM budget exhausted".to_string(),
                });
            }
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
            declared_entity_types,
            candidate_predicates: candidate_predicate_map,
            declared_kinds,
            entity_type_labels: None,
        };
        run_llm_tier_into(&llm_exts, &ctx, mems, mem_namespaces, &mut outcomes).await;
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
    for extractor in tier_exts {
        let ext_id = extractor.id().raw();
        let ext_version = extractor.extractor_version();
        let results = extractor.run_batch(ctx, mems).await;
        debug_assert_eq!(results.len(), mems.len());
        for (i, result) in results.into_iter().enumerate() {
            fold_tier_result(
                &mut outcomes[i],
                result,
                tier_kind,
                mems[i].id,
                ext_id,
                ext_version,
            );
        }
    }
}

/// Fold one extractor's per-memory [`ExtractionResult`] into the memory's
/// `PipelineOutcome`: record the tier byte, add the (LLM-only) provider
/// cost, log the per-tier contribution, merge success items, capture the
/// first non-skip failure reason, and stash the LLM failure class. Shared
/// by the full-batch tier runner ([`run_tier_into`]) and the per-memory
/// namespace-scoped LLM runner ([`run_llm_tier_into`]) so both fold
/// identically.
fn fold_tier_result(
    slot: &mut PipelineOutcome,
    result: ExtractionResult,
    tier_kind: brain_core::ExtractorKind,
    memory_id: MemoryId,
    ext_id: u32,
    ext_version: u32,
) {
    use brain_core::ExtractorKind;
    let outcome_byte = tier_outcome_for(&result);
    // Two extractors can share one tier. Combine rather than overwrite so a
    // later extractor's SKIP never erases an earlier extractor's real outcome
    // (A=RAN + B=SkippedFilter must record RAN, not SKIPPED). Mirrors the LLM
    // runner's ABSENT→SKIP guard; among two real outcomes the later wins, so
    // RAN-vs-FAILED ordering (and the LLM retry decision it feeds) is
    // unchanged.
    let current = match tier_kind {
        ExtractorKind::Pattern => slot.pattern,
        ExtractorKind::Classifier => slot.classifier,
        ExtractorKind::Llm => slot.llm,
    };
    // Whether the incoming outcome wins the combine — recomputed with the
    // exact predicate `combine_tier_status` uses, so the captured per-call
    // audit always describes the extractor whose byte we record.
    let took_incoming = {
        let cur_real = matches!(current, tier_status::RAN | tier_status::FAILED);
        let inc_real = matches!(outcome_byte, tier_status::RAN | tier_status::FAILED);
        !(cur_real && !inc_real)
    };
    let combined = combine_tier_status(current, outcome_byte);
    match tier_kind {
        ExtractorKind::Pattern => slot.pattern = combined,
        ExtractorKind::Classifier => slot.classifier = combined,
        ExtractorKind::Llm => slot.llm = combined,
    }
    if took_incoming {
        // Precise status byte for the historical audit row: `as_u8()`
        // is byte-equal to `extraction_status::*` (Success=1 … Disabled=6),
        // so a SkippedFilter / SkippedDuplicate keeps its exact reason
        // rather than collapsing to the coarse tier byte.
        let tier_audit = TierAudit {
            extractor_id: ext_id,
            extractor_version: ext_version,
            status: result.status.as_u8(),
            reason: result.status_reason.clone(),
        };
        match tier_kind {
            ExtractorKind::Pattern => slot.pattern_audit = Some(tier_audit),
            ExtractorKind::Classifier => slot.classifier_audit = Some(tier_audit),
            ExtractorKind::Llm => slot.llm_audit = Some(tier_audit),
        }
    }
    // Real provider cost flows from the LLM extractor's result into the
    // per-memory outcome, which the caller sums into the per-cycle spend
    // gate and the cost metric. Non-LLM tiers report zero, so the
    // unconditional add is correct.
    slot.llm_cost_micro_usd = slot
        .llm_cost_micro_usd
        .saturating_add(result.cost_micro_usd);
    // Per-tier visibility: log exactly what THIS tier's extractor produced
    // for THIS memory, before the items are merged into the cumulative
    // slot. Lets an operator see the pattern → classifier → LLM
    // contribution split for any encode.
    match &result.status {
        ExtractionStatus::Success => tracing::info!(
            target: "brain_debug::extractor",
            tier = ?tier_kind,
            memory_id = ?memory_id,
            extracted = result.items.len(),
            items = %summarize_extracted_items(&result.items),
            "extractor tier output",
        ),
        ExtractionStatus::SkippedDisabled => tracing::debug!(
            target: "brain_debug::extractor",
            tier = ?tier_kind,
            memory_id = ?memory_id,
            "extractor tier skipped (disabled)",
        ),
        other => tracing::info!(
            target: "brain_debug::extractor",
            tier = ?tier_kind,
            memory_id = ?memory_id,
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
        slot.failure_reason = Some(format!("{:?}: {}", result.status, result.status_reason));
    }
    // Capture the LLM tier's transient/permanent verdict so the worker can
    // keep transient failures reprocessable (backoff retry) and terminate
    // permanent ones. Only the LLM tier sets a meaningful class; other
    // tiers leave it Unclassified.
    if matches!(tier_kind, ExtractorKind::Llm) && matches!(result.status, ExtractionStatus::Failure)
    {
        slot.llm_failure_class = result.failure_class;
    }
}

/// LLM-tier dispatch with per-memory, namespace-scoped extractor
/// selection and ENCODE-trigger honoring.
///
/// A namespace's own enabled LLM extractor REPLACES the seeded system
/// `brain` LLM extractor for that namespace's memories — this kills the
/// double-LLM cost (system + user both running over every memory) and
/// keeps one tenant's extractor from running over another's rows. For a
/// memory owned by namespace `N`:
///   - if any enabled LLM extractor is declared under `N`, only those run;
///   - otherwise the system (`brain`) LLM extractor(s) run as the default.
///
/// A memory whose namespace couldn't be resolved is passed as `brain` by
/// the caller, so it falls through to the system default — never a silent
/// drop. Selection is strictly LLM-only: the cheap, always-on pattern and
/// classifier tiers still run over every memory unchanged.
///
/// Each selected (extractor, memory) pair is then gated by the extractor's
/// ENCODE trigger: an `on encode where <cond>` that doesn't match the
/// memory records a `SkippedFilter`; `on demand` / `periodic` /
/// `on schema_change` never run on this path.
async fn run_llm_tier_into(
    llm_exts: &[Arc<dyn Extractor>],
    ctx: &ExtractionContext<'_>,
    mems: &[CoreMemory],
    mem_namespaces: &[String],
    outcomes: &mut [PipelineOutcome],
) {
    use brain_core::ExtractorKind;
    if llm_exts.is_empty() {
        return;
    }
    // Namespaces that declare at least one enabled LLM extractor of their
    // own. A memory whose namespace is in this set runs ONLY its own
    // extractors; otherwise the system default covers it.
    let owning: std::collections::HashSet<&str> = llm_exts.iter().map(|e| e.namespace()).collect();

    for ext in llm_exts {
        let ext_ns = ext.namespace();
        let ext_id = ext.id().raw();
        let ext_version = ext.extractor_version();
        // Build this extractor's sub-batch: the memories it is effective
        // for (namespace rule) AND whose ENCODE trigger fires. Running a
        // per-extractor sub-batch preserves the concurrent-fan-out of
        // `LlmExtractor::run_batch` while confining each call set to the
        // right namespace.
        let mut sub_idx: Vec<usize> = Vec::new();
        let mut sub_mems: Vec<CoreMemory> = Vec::new();
        for (i, m) in mems.iter().enumerate() {
            let mem_ns = mem_namespaces
                .get(i)
                .map(String::as_str)
                .unwrap_or(SYSTEM_NAMESPACE);
            if !llm_extractor_effective_for(ext_ns, mem_ns, &owning) {
                continue;
            }
            match ext.encode_trigger_decision(m) {
                TriggerDecision::Run => {
                    sub_idx.push(i);
                    sub_mems.push(m.clone());
                }
                TriggerDecision::SkipFilter => {
                    // The `where` clause didn't match this memory. Record a
                    // filter-skip for the pair unless another effective
                    // extractor already ran for this memory (don't clobber
                    // a RAN with a SKIPPED).
                    if outcomes[i].llm == tier_status::ABSENT {
                        outcomes[i].llm = tier_status::SKIPPED;
                        outcomes[i].llm_audit = Some(TierAudit {
                            extractor_id: ext_id,
                            extractor_version: ext_version,
                            status: extraction_status::SKIPPED_FILTER,
                            reason: "encode trigger where-clause did not match".to_string(),
                        });
                    }
                    tracing::debug!(
                        target: "brain_debug::extractor",
                        tier = ?ExtractorKind::Llm,
                        memory_id = ?m.id,
                        extractor = %ext.name(),
                        "llm extractor skipped by trigger where-clause",
                    );
                }
                TriggerDecision::NotOnEncode => {
                    // Non-encode trigger (`on demand` / `periodic` /
                    // `on schema_change`) — inert on the encode path. Leave
                    // the slot untouched.
                }
            }
        }
        if sub_mems.is_empty() {
            continue;
        }
        let results = ext.run_batch(ctx, &sub_mems).await;
        debug_assert_eq!(results.len(), sub_mems.len());
        for (k, result) in results.into_iter().enumerate() {
            let i = sub_idx[k];
            fold_tier_result(
                &mut outcomes[i],
                result,
                ExtractorKind::Llm,
                mems[i].id,
                ext_id,
                ext_version,
            );
        }
    }
}

/// Whether an LLM extractor declared under `ext_ns` is effective for a
/// memory owned by `mem_ns`, given the set of namespaces that declare
/// their own LLM extractor. See [`run_llm_tier_into`] for the policy: a
/// namespace with its own extractor runs only those; every other
/// namespace falls back to the system (`brain`) default.
fn llm_extractor_effective_for(
    ext_ns: &str,
    mem_ns: &str,
    owning: &std::collections::HashSet<&str>,
) -> bool {
    if owning.contains(mem_ns) {
        ext_ns == mem_ns
    } else {
        ext_ns == SYSTEM_NAMESPACE
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
/// 4. **Not a temporal expression.** "January 2026", "yesterday",
///    "2020-01-15" name a TIME, not a thing. A date reaches the graph
///    through a statement's `event_at` (its reified Time slot), never as
///    a node of its own — an orphan date node has no referent, joins
///    unrelated memories that merely share a month, and pollutes the
///    entity HNSW. The tiers guard their own entity projections, but a
///    date also arrives here as a coined statement subject / object /
///    relation endpoint (via [`statement_subject_mintable`]), which no
///    tier guard covers — so the apply layer, the last place before an
///    entity row is minted, enforces it for every path at once.
fn entity_mention_is_acceptable(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    if brain_extractors::is_temporal_expression_surface(t) {
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

/// Fold a later extractor's tier outcome into the tier's running status byte
/// when two extractors share one tier. A real outcome (`RAN` / `FAILED`) must
/// never be clobbered by a `SKIP` (or the initial `ABSENT`); among two real
/// outcomes the later one wins, preserving the pre-existing RAN-vs-FAILED
/// behavior the LLM retry path depends on.
fn combine_tier_status(current: u8, incoming: u8) -> u8 {
    let current_is_real = matches!(current, tier_status::RAN | tier_status::FAILED);
    let incoming_is_real = matches!(incoming, tier_status::RAN | tier_status::FAILED);
    if current_is_real && !incoming_is_real {
        current
    } else {
        incoming
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

/// What one run of the transactional apply body produces. Returned to the
/// `apply_outcome` orchestrator so the post-commit fan-out (causal-edge
/// enqueue, statement-text indexing) runs against the values from the
/// pass that actually committed — never a rolled-back plan pass.
struct ApplyBody {
    counts: ExtractorItemCounts,
    status_byte: u8,
    retry_pending: bool,
    causal_enqueues: Vec<brain_core::StatementId>,
    created_statement_ids: Vec<brain_core::StatementId>,
    /// Entity vectors this pass minted, published into the in-RAM entity
    /// HNSW only after the pass commits. The HNSW has no removal, so a
    /// discarded pass MUST drop these rather than insert them — dropping
    /// this `ApplyBody` is exactly that.
    staged_entity_vectors: StagedEntityVectors,
}

/// Minimum extractor confidence for a `retract: true` mention to actually
/// tombstone a matching stored fact. Retraction is destructive (the fact
/// vanishes from every read until a sweep or manual restore), so a
/// low-confidence LLM negation — the exact output most prone to
/// hallucinated polarity ("not at Google") — must not be able to retire a
/// real fact. The subject/predicate/object triple must still match an
/// existing current statement, so this floor is the second gate on top of
/// that exact-match requirement.
const RETRACT_MIN_CONFIDENCE: f32 = 0.7;

/// Nanoseconds per day. Resolved temporal dates land at midnight UTC while a
/// memory's anchor carries a wall-clock time-of-day, so the anchor-vs-event
/// comparison is done at whole-day granularity (`nanos / NANOS_PER_DAY`).
const NANOS_PER_DAY: u64 = 86_400_000_000_000;

/// BLAKE3 of the memory's source text, read from `TEXTS_TABLE` inside the
/// open extraction `wtxn`. Feeds `ExtractionAudit::input_hash` so an
/// operator can tell whether a re-extraction ran over edited text. Returns
/// the all-zero hash when the text row is missing (shouldn't happen for a
/// queued memory) rather than failing the audit.
fn memory_text_hash(wtxn: &redb::WriteTransaction, memory_id: MemoryId) -> [u8; 32] {
    use brain_metadata::tables::text::TEXTS_TABLE;
    wtxn.open_table(TEXTS_TABLE)
        .ok()
        .and_then(|t| {
            t.get(&memory_id.to_be_bytes())
                .ok()
                .flatten()
                .map(|g| *blake3::hash(g.value()).as_bytes())
        })
        .unwrap_or([0u8; 32])
}

/// Emit one historical [`ExtractionAudit`] row per extractor tier that ran
/// (or was skipped) for `memory_id`, into `EXTRACTOR_AUDIT_TABLE` + its
/// three secondary indexes, inside the caller's extraction `wtxn` so the
/// audit rows and the extracted graph commit atomically. A tier that was
/// absent (no extractor present) yields no row — we never audit a call that
/// didn't happen.
///
/// Best-effort observability: a write error is logged and skipped, never
/// propagated, so a redb hiccup in the audit log can't fail the durable
/// extraction it describes (matching how the post-commit fan-outs treat
/// their own failures).
fn emit_per_call_extraction_audit(
    wtxn: &redb::WriteTransaction,
    memory_id: MemoryId,
    outcome: &PipelineOutcome,
    now: u64,
    input_hash: [u8; 32],
) {
    // Only the LLM tier carries provider cost; pattern + classifier are free.
    let tiers = [
        (&outcome.pattern_audit, 0u64),
        (&outcome.classifier_audit, 0u64),
        (&outcome.llm_audit, outcome.llm_cost_micro_usd),
    ];
    for (tier_audit, cost_micro_usd) in tiers {
        let Some(t) = tier_audit else { continue };
        // `schema_version` is 1: the pipeline runs every tier at
        // `ExtractionContext { schema_version: 1, .. }`. `outputs` is left
        // empty — per-tier attribution of committed entity/statement/relation
        // ids is not tracked at this site (the surface-dedup collapses across
        // tiers); the aggregate counts live on the per-memory
        // `extractor_pipeline_audit` state row.
        let mut row = if t.status == extraction_status::SUCCESS {
            ExtractionAudit::success(
                AuditId::new(),
                memory_id,
                t.extractor_id,
                t.extractor_version,
                1,
                now,
                now,
                Vec::new(),
                input_hash,
            )
        } else {
            ExtractionAudit::non_success(
                AuditId::new(),
                memory_id,
                t.extractor_id,
                t.extractor_version,
                1,
                now,
                now,
                t.status,
                t.reason.clone(),
                input_hash,
            )
        };
        row.cost_micro_usd = cost_micro_usd;
        if let Err(e) = audit_write(wtxn, &row) {
            warn!(
                target: "brain_workers::extractor",
                memory_id = ?memory_id,
                extractor_id = t.extractor_id,
                error = %e,
                "per-call extraction audit write failed (best-effort; extraction unaffected)",
            );
        }
    }
}

async fn apply_outcome(
    worker: &ExtractorWorker,
    ctx: &WorkerContext,
    memory_id: MemoryId,
    outcome: &PipelineOutcome,
) -> Result<ApplyOutcome, ApplyError> {
    // One clock for every pass, so a plan pass and its apply pass write
    // byte-identical timestamps.
    let now = now_unix_nanos();
    let db_guard = ctx.ops.executor.metadata.as_ref();

    // Two-phase entity disambiguation keeps the LLM OFF the shard reactor.
    // When a disambiguator is wired we first run the apply body in
    // `Collect` mode against a plan txn — discovering which ambiguous-band
    // candidates need an LLM verdict — then, if any turned up, roll that
    // txn back, `.await` the verdicts off the reactor, and re-run the body
    // in `Replay` mode against the committing txn. The common case (no
    // ambiguity) commits the plan txn directly: one pass, no LLM, no
    // reactor stall — exactly the pre-disambiguator behaviour.
    //
    // EITHER pass can be the one that commits, so neither may touch
    // non-transactional state while it runs. Both therefore return their
    // entity-HNSW inserts staged in the `ApplyBody`; only the body of the
    // pass that actually committed reaches the flush below, and the
    // discarded plan body is dropped with its staging intact-but-unused.
    let body = if let Some(dis) = worker.entity_disambiguator.as_deref() {
        let mut pending: Vec<PendingVerdict> = Vec::new();
        let plan_txn = db_guard
            .write_txn()
            .map_err(|e| ApplyError::Storage(format!("write_txn: {e:?}")))?;
        let (plan_txn, plan_body) = run_apply_body(
            worker,
            ctx,
            memory_id,
            outcome,
            now,
            plan_txn,
            &mut Disambiguation::Collect(&mut pending),
        )?;
        if pending.is_empty() {
            // No ambiguous candidate surfaced: the plan pass IS the
            // correct result (Collect == no-disambiguator when it collects
            // nothing), so commit it directly instead of re-running.
            plan_txn
                .commit()
                .map_err(|e| ApplyError::Storage(format!("commit: {e:?}")))?;
            plan_body
        } else {
            drop(plan_txn); // roll back the plan pass; re-run with verdicts
            let mut verdicts = PrecomputedVerdicts::new();
            for p in pending {
                let verdict = dis.confirm(&p.view, &p.raw_surface).await;
                verdicts.insert(p.norm_surface, p.candidate, verdict);
            }
            let wtxn = db_guard
                .write_txn()
                .map_err(|e| ApplyError::Storage(format!("write_txn: {e:?}")))?;
            let (wtxn, body) = run_apply_body(
                worker,
                ctx,
                memory_id,
                outcome,
                now,
                wtxn,
                &mut Disambiguation::Replay(&verdicts),
            )?;
            wtxn.commit()
                .map_err(|e| ApplyError::Storage(format!("commit: {e:?}")))?;
            body
        }
    } else {
        let wtxn = db_guard
            .write_txn()
            .map_err(|e| ApplyError::Storage(format!("write_txn: {e:?}")))?;
        let (wtxn, body) = run_apply_body(
            worker,
            ctx,
            memory_id,
            outcome,
            now,
            wtxn,
            &mut Disambiguation::Off,
        )?;
        wtxn.commit()
            .map_err(|e| ApplyError::Storage(format!("commit: {e:?}")))?;
        body
    };

    let ApplyBody {
        counts,
        status_byte,
        retry_pending,
        causal_enqueues,
        created_statement_ids,
        staged_entity_vectors,
    } = body;

    // Publish the committed pass's entity vectors into the in-RAM entity
    // HNSW. Post-commit for the same reason the fan-outs below are: the
    // HNSW cannot un-insert, so a vector must never precede the durable
    // row it describes. This is what keeps tier-3b alive — without it the
    // index stays empty for the whole session and every paraphrase the
    // trigram tiers miss mints a duplicate entity.
    if let Some(deps) = worker.embed_deps.as_ref() {
        let inserted = staged_entity_vectors.flush_into_hnsw(deps);
        if inserted > 0 {
            trace!(
                target: "brain_workers::extractor",
                memory_id = ?memory_id,
                inserted,
                "published new entity vectors into the entity HNSW",
            );
        }
    }

    // Fan out to the CausalEdgeWorker only after the commit succeeds —
    // a rolled-back txn never produces phantom enqueues. The channel is
    // bounded; on `Full` we bump the drop counter and move on (the
    // statement is durable; only its derived edges are deferred).
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

    // Merge the freshly-committed typed graph into this memory's durable
    // write-artifact bundle (MEMORY_INSPECT), reading it back through the same
    // enrichment resolver RECALL uses. Post-commit only, so the read sees the
    // rows we just wrote. Gated on non-empty extraction so a memory that
    // produced no graph costs no extra transaction; the sync bundle already
    // holds vector + record. Best-effort: a failure is logged, never fatal —
    // the durable graph itself already committed.
    if counts.entities + counts.statements + counts.relations > 0 {
        let metadata = ctx.ops.executor.metadata.as_ref();
        if let Err(e) = brain_ops::memory_artifact::merge_graph_from_committed(metadata, memory_id)
        {
            warn!(
                target: "brain_workers::extractor",
                memory_id = ?memory_id,
                error = %e,
                "artifact graph merge failed (durable graph is committed; bundle graph deferred)",
            );
        }
    }

    Ok(ApplyOutcome {
        counts,
        status_byte,
        retry_pending,
    })
}

/// Run the transactional apply body once against `wtxn` and hand the txn
/// back to the caller UNCOMMITTED (so `apply_outcome` can commit it or
/// roll it back). Fully synchronous: the only async work — entity
/// disambiguation — is lifted into the orchestrator via the
/// [`Disambiguation`] mode, so this body never `.await`s and never blocks
/// the shard reactor while the write txn is open.
fn run_apply_body(
    worker: &ExtractorWorker,
    ctx: &WorkerContext,
    memory_id: MemoryId,
    outcome: &PipelineOutcome,
    now: u64,
    wtxn: redb::WriteTransaction,
    disambiguation: &mut Disambiguation<'_>,
) -> Result<(redb::WriteTransaction, ApplyBody), ApplyError> {
    let mut counts = ExtractorItemCounts::zero();
    let mut entity_map: HashMap<String, EntityId> = HashMap::new();
    // Every entity this memory already has a `Mentions` edge to, written by
    // this apply. Pass 1 fills it from the entity-mention loop; the endpoint
    // resolver consults it so an entity that is genuinely mentioned by this
    // memory gets exactly ONE mention edge, no matter how many statements or
    // relations reference it (and none at all if pass 1 already linked it).
    let mut mentioned: HashSet<EntityId> = HashSet::new();
    // Drained by the orchestrator after commit to fan out onto the
    // CausalEdgeWorker channel — never before commit, so a rolled-back txn
    // never produces phantom enqueues.
    let mut causal_enqueues: Vec<brain_core::StatementId> = Vec::new();
    // Drained by the orchestrator after commit to feed the statement text
    // indexer. Extractor-created statements would otherwise never reach
    // `statements.tantivy/`. Post-commit only, same rolled-back-txn reason.
    let mut created_statement_ids: Vec<brain_core::StatementId> = Vec::new();
    // Entity vectors minted by the resolver during this pass. The durable
    // half (the `entity_vectors` row) is written inside `wtxn`; the in-RAM
    // HNSW insert waits for the orchestrator's post-commit flush, because
    // an HNSW insert can't be undone if this pass is the one that gets
    // rolled back. Pass-local by construction: a discarded pass drops it.
    let mut staged = StagedEntityVectors::new();

    let db_guard = ctx.ops.executor.metadata.as_ref();

    // Read this memory's prior attempt count so a retryable failure
    // advances the retry budget. Single-writer-per-shard means no
    // concurrent writer can change it between this read and the audit
    // write below. Absent row → 0. Recomputed per pass, but a rolled-back
    // plan pass leaves it unchanged, so both passes see the same value.
    let prior_attempts = match db_guard.read_txn() {
        Ok(rtxn) => brain_metadata::pipeline_extraction_attempts(&rtxn, memory_id).unwrap_or(0),
        Err(_) => 0,
    };

    // The writing space's self-entity. First-person statement subjects
    // ("I prefer dark roast") resolve to `EntityId::from(space_id)` — the
    // SAME identity `MATERIALIZE_PROCEDURAL` reads — so an space's facts
    // about itself persist and stay queryable instead of being dropped as
    // non-referential pronouns. Per-space by construction (the id is the
    // space's), so multi-space deployments never collapse onto one node.
    // A missing memory row (shouldn't happen for a queued memory) yields a
    // zero space; first-person routing simply falls back to the drop path.
    let self_entity_id: Option<EntityId> = {
        use brain_metadata::tables::memory::MEMORIES_TABLE;
        wtxn.open_table(MEMORIES_TABLE)
            .ok()
            // Copy the space bytes out while the table guard is still alive —
            // returning the guard itself would borrow the dropped table.
            .and_then(|t| {
                t.get(&memory_id.to_be_bytes())
                    .ok()
                    .flatten()
                    .map(|g| g.value().space_id_bytes)
            })
            .filter(|b| *b != [0u8; 16])
            .map(EntityId::from)
    };

    // The source memory's `(namespace, space)` scope. Every typed-graph
    // row this extraction writes — entities, statements, relations — is
    // stamped with the SAME scope as the memory it was extracted from,
    // so an extracted fact can never escape its source tenant. A missing
    // memory row (shouldn't happen for a queued memory) falls back to the
    // system scope, which the apply path treats as the `brain` namespace.
    // Read the scope AND the per-utterance session together: every typed-
    // graph row this extraction writes (statements, relations) is stamped
    // with the SAME session as the memory it came from, so a session-scoped
    // read sees the memory's facts alongside the memory. (Entities carry
    // session only as first-mention provenance; see `entity_put`.)
    let (source_scope, source_session): (brain_metadata::RowScope, brain_core::SessionId) = {
        use brain_metadata::tables::memory::MEMORIES_TABLE;
        wtxn.open_table(MEMORIES_TABLE)
            .ok()
            .and_then(|t| {
                t.get(&memory_id.to_be_bytes()).ok().flatten().map(|g| {
                    let m = g.value();
                    (
                        brain_metadata::RowScope::from_bytes(m.namespace_id, m.space_id_bytes),
                        brain_core::SessionId::from(m.session_id),
                    )
                })
            })
            .unwrap_or_else(|| {
                (
                    brain_metadata::RowScope::from_bytes(
                        brain_core::NamespaceId::SYSTEM.raw(),
                        [0u8; 16],
                    ),
                    brain_core::SessionId::DEFAULT,
                )
            })
    };

    // The source memory's ANCHOR day — its client-supplied `occurred_at`, else
    // its record `created_at` — expressed as whole days since the unix epoch.
    // Three times are first-class and DISTINCT keys: the record time
    // (`created_at`), the MESSAGE time (`occurred_at`, the anchor), and the
    // EVENT time (an Event fact's `event_at`). The temporal extractor resolves
    // every date it finds in the text, INCLUDING the message date itself
    // ("[25 May 2023] I ran a race last Saturday" resolves both 25 May and
    // 20 May). The message date is ALREADY on the memory record as
    // `occurred_at`, so re-materializing it as a fact would be redundant AND
    // would make the date->fact join see two distinct dates and skip. We
    // therefore compare each resolved date's DAY against this anchor day and
    // treat only the DIFFERING date as a genuine event time. Day granularity
    // is required because the anchor carries a wall-clock time-of-day while
    // resolved dates land at midnight UTC. A missing memory row (shouldn't
    // happen for a queued memory) yields `None` → no anchor exclusion.
    let anchor_day: Option<u64> = {
        use brain_metadata::tables::memory::MEMORIES_TABLE;
        wtxn.open_table(MEMORIES_TABLE)
            .ok()
            .and_then(|t| {
                t.get(&memory_id.to_be_bytes()).ok().flatten().map(|g| {
                    let m = g.value();
                    m.occurred_at_unix_nanos.unwrap_or(m.created_at_unix_nanos)
                })
            })
            .filter(|&anchor_nanos| anchor_nanos != 0)
            .map(|anchor_nanos| anchor_nanos / NANOS_PER_DAY)
    };

    // Pass 1 — entity mentions, in source order. Resolving early gives
    // statements + relations a populated `entity_map` to look up
    // surface forms against.
    let embed_deps = worker.embed_deps.as_ref();
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
        let (entity_id, tier, confidence) = resolve_entity_mention(
            &wtxn,
            source_scope,
            em,
            now,
            embed_deps,
            &mut staged,
            disambiguation,
        )?;
        // Log this mention→entity resolution as a derivation. Emitted here,
        // after per-surface dedup, so a surface that several tiers proposed is
        // resolved — and audited — exactly once. The entity_type_id is read
        // back from the resolved row inside the txn so the audit records the
        // referent's actual type (cross-type resolution can differ from the
        // mention's hinted type). Best-effort, append-only; see
        // `emit_resolution_audit`.
        let resolved_type_id = entity_get_inside_wtxn(&wtxn, entity_id)
            .ok()
            .flatten()
            .map_or(0, |e| e.entity_type.raw());
        emit_resolution_audit(
            &wtxn,
            &em.text,
            resolved_type_id,
            entity_id,
            tier,
            confidence,
            now,
        );
        // Stage journal (S8 entity resolution). The resolver's own logs
        // never carry `memory_id`, so a resolve verdict couldn't be tied
        // back to the encode that triggered it. Log it here, where
        // `memory_id` is in scope: which surface resolved to which entity
        // via which tier (Created = a genuinely new entity row).
        tracing::debug!(
            target: "brain_debug::stage",
            stage = "S8_resolve",
            memory_id = memory_id.raw(),
            surface = %em.text,
            tier = ?tier,
            entity_id = ?entity_id,
            "write stage: entity mention resolved",
        );
        worker
            .metrics
            .inc_resolver_outcome(resolution_tier_to_metric(tier));
        entity_map.insert(em.text.clone(), entity_id);
        write_mention_edge(&wtxn, memory_id, entity_id, &em.text, em.confidence, now)?;
        mentioned.insert(entity_id);
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
    // Watermark for the mention edges pass 1 wrote, so the endpoint-minted
    // ones the apply loop adds below can be counted separately.
    let mentions_after_pass_1 = mentioned.len();

    // Normalize statement mentions before persisting: prune vague / sub-floor
    // triples and split a compound VALUE object into atomic ones, so the typed
    // graph holds queryable facts instead of prose fragments ("counseling and
    // mental health jobs" → "counseling", "mental health jobs"). Entity mentions
    // (already consumed by Pass 1) and relations pass through untouched.
    let normalized_items = normalize_statement_items(&outcome.items, &worker.metrics);

    // Pre-scan (first pass of a two-pass apply): the temporal extractor files
    // each resolved memory date as a memory-subject mention. Such a date belongs
    // on the Event fact it modifies (its reified Time slot), NOT re-materialized
    // as a redundant statement — the message date already lives on the memory
    // record as `occurred_at`. This deterministic date->fact join is the PRIMARY,
    // certain mechanism for filling an Event's Time slot: when the memory has a
    // single genuine EVENT date, the create pass below stamps it on that memory's
    // Event fact(s) FOR SURE — no dependence on the LLM. (The `{ANCHOR_DATE}` LLM
    // prompt is only a complement: it helps the LLM classify actions as Event and
    // catch dates this deterministic extractor missed.) Collect the DISTINCT set
    // of resolved dates here, before any entity statement is created, EXCLUDING
    // the anchor day: a resolved date equal to the memory's message day is not an
    // event time — it is the anchor itself, already on the record, so it must
    // neither be joined nor counted toward ambiguity. Exactly one remaining
    // distinct date → we know which date every Event in this memory happened on →
    // join it. Zero remaining dates → nothing to join (expected: a dateless
    // memory, or a same-day event whose only date equalled the anchor; that Event
    // reads its time back from the memory's own `occurred_at`). Many remaining
    // distinct dates → the one genuinely ambiguous case: we can't deterministically
    // pair each date to a fact (memory-subject mentions carry no per-statement
    // span), so we skip and leave those Time slots to the LLM's own per-statement
    // `event_at`. Read-only: a bad/unparseable date is simply not collected and
    // never aborts extraction.
    let sole_memory_date: Option<u64> = {
        let mut distinct: Vec<u64> = Vec::new();
        for item in &normalized_items {
            if let ExtractedItem::StatementMention(sm) = item {
                if !sm.subject_is_memory {
                    continue;
                }
                // The resolved `event_at_unix_nanos` wins, else the legacy
                // unix-nanos object text.
                if let Some(ts) = sm.event_at_unix_nanos.or_else(|| {
                    sm.object_text
                        .as_deref()
                        .and_then(|t| t.parse::<u64>().ok())
                }) {
                    // Anchor exclusion: a date landing on the memory's own
                    // message day is the anchor, not a distinct event time.
                    if let Some(day) = anchor_day {
                        if ts / NANOS_PER_DAY == day {
                            continue;
                        }
                    }
                    if !distinct.contains(&ts) {
                        distinct.push(ts);
                    }
                }
            }
        }
        if distinct.len() == 1 {
            Some(distinct[0])
        } else {
            None
        }
    };

    // Pass 2 — statements + relations. These reference entities by
    // surface form; we look them up in `entity_map`. Items whose
    // referenced surface form wasn't in the entity-mention pass are
    // dropped with a trace (the LLM tier occasionally emits implicit
    // entities; auto-creating them here would produce ghost entities
    // without a mention edge).
    for item in &normalized_items {
        match item {
            ExtractedItem::EntityMention(_) => {}
            ExtractedItem::StatementMention(sm) if sm.subject_is_memory => {
                // Memory-subject temporal mention (the temporal extractor's
                // resolved date). We DO NOT materialize it as a statement. The
                // three times are distinct first-class keys, and a memory date
                // is already the MESSAGE time on the memory record
                // (`occurred_at`) — re-filing it as a `(memory) --occurred_at-->`
                // row was redundant AND ambiguous: it also re-emitted the same
                // date the anchor already carries, which made the date->fact join
                // see two distinct dates and skip, so no Event ever got its
                // `event_at`. These mentions were only ever consumed by the
                // pre-scan above (to find the memory's sole genuine EVENT date,
                // now with the anchor day excluded); no read, recall, recency, or
                // edge worker reads memory-subject `occurred_at` rows. So the
                // mention has already served its purpose — drop it here without
                // creating a row.
                let _ = sm;
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
                    source_scope,
                    memory_id,
                    sm,
                    &mut entity_map,
                    &mut mentioned,
                    self_entity_id,
                    embed_deps,
                    &mut staged,
                    disambiguation,
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
                    let pid = match resolve_or_intern_predicate(
                        &wtxn,
                        embed_deps,
                        &worker.metrics,
                        ns,
                        name,
                        now,
                    ) {
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
                    let used_qname = (ns.to_string(), name.to_string());

                    // Object axis: the predicate's declared object constraint
                    // (Entity→mint / Value→text) wins; else the LLM's per-object
                    // entity-vs-value flag. Only on the ENTITY axis is the
                    // surface looked up — one already surfaced this memory links,
                    // and a real entity not yet surfaced is minted best-effort
                    // (cross-type reuse). A literal stays text even when some
                    // earlier tier minted an entity for the same span.
                    // A tier that emitted no object text at all (a malformed LLM
                    // response that slipped past schema validation) has no fact
                    // to persist — drop the triple rather than fabricate an
                    // empty-string value.
                    let Some(object) = resolve_statement_object(
                        &wtxn,
                        source_scope,
                        memory_id,
                        sm,
                        pid,
                        &mut entity_map,
                        &mut mentioned,
                        embed_deps,
                        &mut staged,
                        disambiguation,
                        now,
                    )?
                    else {
                        worker.metrics.inc_apply_dropped("object_missing");
                        warn!(
                            memory_id = ?memory_id,
                            predicate = %sm.predicate_qname,
                            "statement object missing; skipping triple",
                        );
                        continue;
                    };

                    // RETRACTION: the source text says this fact no longer holds
                    // ("not at Google anymore"). Retire the matching current
                    // fact(s) instead of creating a new row — covering BOTH stores
                    // a fact can live in: the statements table (value/entity-object
                    // facts) and the relations table (entity↔entity links like a
                    // `works_at` edge). We read a committed snapshot: the prior
                    // facts were written in an earlier cycle, so a read txn sees
                    // them; tombstoning happens in the live `wtxn`. A subject/object
                    // freshly minted this cycle simply has no prior row → no-op.
                    if sm.retract && sm.confidence >= RETRACT_MIN_CONFIDENCE {
                        let mut retired = 0u64;
                        if let Ok(snap) = db_guard.read_txn() {
                            if let Ok(rows) = brain_metadata::statement_list(
                                &snap,
                                source_scope,
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
                                        source_scope,
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
                    let declared_kind = predicate_declared_kind_in_write_txn(&wtxn, pid)?;
                    let mut kind =
                        declared_kind.unwrap_or_else(|| statement_kind_from_byte(sm.kind));
                    // Reified time slot: when a tier resolved an event date for THIS
                    // entity statement, the fact must OWN its own time — but only an
                    // Event may carry `event_at_unix_nanos` (the data model rejects a
                    // non-Event with a time set). So promote an open-vocab statement
                    // that carries a resolved date to Event, which stamps the time on
                    // the entity fact itself instead of orphaning it to a separate
                    // memory-subject `occurred_at` row. A predicate with a DECLARED
                    // kind keeps its semantics (a `brain:prefers` fact never becomes an
                    // Event just because a date appeared); its resolved date is dropped
                    // rather than violating the declaration.
                    if declared_kind.is_none() && sm.event_at_unix_nanos.is_some() {
                        kind = StatementKind::Event;
                    }
                    // Effective Time slot for this fact. An `event_at` the LLM
                    // already resolved for this exact statement wins (never
                    // overridden).
                    let mut effective_event_at = sm.event_at_unix_nanos;
                    // Deterministic date->fact join (PRIMARY Time-slot mechanism):
                    // when this fact IS an Event (so it temporally happened) and the
                    // memory has a resolved date, stamp it so the fact owns its Time
                    // slot instead of the date living only on an orphaned
                    // memory-subject `occurred_at` row. This is certain, not a
                    // fallback — a resolved date is joined FOR SURE. Guards decide
                    // WHICH fact, not whether to run: only Event-kind (a declared
                    // atemporal kind — Preference/Attribute/Directive — never
                    // reaches here because `kind != Event`), only when no
                    // per-statement `event_at` was set (don't override the LLM), and
                    // only when `sole_memory_date` is `Some` (exactly one distinct
                    // date; the multi-date ambiguous case was skipped in the
                    // pre-scan). No date → no stamp, which is the expected, correct
                    // outcome for a dateless memory.
                    if kind == StatementKind::Event && effective_event_at.is_none() {
                        if let Some(d) = sole_memory_date {
                            effective_event_at = Some(d);
                        }
                    }
                    // An action (participated_in, traveled_to, joined) IS an Event
                    // even when no distinct date was resolved: it temporally
                    // happened, and a dateless action must stay an Event so the read
                    // path can answer "when did S P?" from the evidence memory's
                    // `occurred_at` fallback. Downgrading a dateless Event to Fact
                    // would strip the Time slot and make every dateless action
                    // permanently unanswerable to a temporal cue — the exact
                    // coherence gap this write path exists to close. So we do NOT
                    // downgrade: the Event persists with `event_at_unix_nanos = None`
                    // and the reader supplies the memory-time fallback.

                    // Diagnostic: what the tier emitted (raw kind byte) vs the final
                    // stored kind + resolved time. Lets one re-ingest settle whether
                    // actions now land as Events (fix suffices) or still arrive as
                    // Fact from the tier (needs a prompt/classifier change).
                    tracing::debug!(
                        target: "brain_debug::extractor",
                        memory_id = ?memory_id,
                        predicate = %sm.predicate_qname,
                        mention_kind_byte = sm.kind,
                        final_kind = ?kind,
                        event_at = ?effective_event_at,
                        "apply: statement kind/time",
                    );

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
                            session_id: source_session,
                        };
                        match relation_create_internal(&wtxn, source_scope, &payload) {
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
                        effective_event_at
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
                        session_id: source_session,
                    };
                    match statement_create_internal(&wtxn, source_scope, &payload) {
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
                    source_scope,
                    memory_id,
                    &rm.subject_text,
                    rm.confidence,
                    &mut entity_map,
                    &mut mentioned,
                    embed_deps,
                    &mut staged,
                    disambiguation,
                    now,
                )?;
                let to = resolve_relation_endpoint(
                    &wtxn,
                    source_scope,
                    memory_id,
                    &rm.object_text,
                    rm.confidence,
                    &mut entity_map,
                    &mut mentioned,
                    embed_deps,
                    &mut staged,
                    disambiguation,
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
                    session_id: source_session,
                };
                match relation_create_internal(&wtxn, source_scope, &payload) {
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

    // Mention edges the apply loop wrote for entities that no tier filed as an
    // entity mention — they only surfaced as a statement subject, a statement
    // object, or a relation endpoint. They are real edges, so the audit row and
    // the metric count them alongside pass 1's.
    let coined_mentions = mentioned.len().saturating_sub(mentions_after_pass_1);
    if coined_mentions > 0 {
        counts.mention_edges = counts
            .mention_edges
            .saturating_add(coined_mentions.min(u32::MAX as usize) as u32);
        worker
            .metrics
            .add_items_written(ExtractorItemKind::Mention, coined_mentions as u64);
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
    // A retryable failure is specifically the LLM tier failing with a
    // transient cause. A TRANSIENT failure (timeout / rate-limit / 5xx) keeps
    // the memory queued and is retried with backoff until it succeeds — a
    // passing provider outage must never permanently strip a memory's
    // grounding. A PERMANENT failure (bad key, no balance, malformed) is
    // terminal at once: retrying can't help and only hides the problem.
    //
    // A retry re-runs the whole pipeline, so the already-committed
    // pattern/classifier-tier rows are re-applied. That is safe because
    // relation/statement creation is content-idempotent at the apply layer:
    // `relation_create` / `statement_create` no-op on a byte-identical active
    // tuple rather than minting a duplicate (a user-declared pattern Relation
    // with `Many` cardinality has no cardinality conflict to catch the repeat,
    // so the content-dedup is what prevents the leak). Distinct multi-values
    // still coexist; only exact re-applications collapse.
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

    // Per-call historical audit: one `ExtractionAudit` row per tier that
    // ran (or was skipped), written into THIS txn so the audit rows and the
    // extracted graph commit atomically (no extra fsync). Append-only —
    // each row mints a fresh UUIDv7 id, so a re-extraction adds rows rather
    // than overwriting. Best-effort inside the helper: a write error is
    // logged, never propagated, so the audit log can't fail the extraction.
    let input_hash = memory_text_hash(&wtxn, memory_id);
    emit_per_call_extraction_audit(&wtxn, memory_id, outcome, now, input_hash);

    // Hand the uncommitted txn back to the orchestrator, which commits
    // (or, for a plan pass that discovered ambiguity, rolls back) and then
    // fans out the causal-edge + statement-text work post-commit.
    Ok((
        wtxn,
        ApplyBody {
            counts,
            status_byte,
            retry_pending,
            causal_enqueues,
            created_statement_ids,
            staged_entity_vectors: staged,
        },
    ))
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

/// Publish a `StageCompleted{Extractor}` event so subscribers waiting
/// via `--wait` can decrement their pending-stage checklist for this
/// memory. Durable: WAL-appended (via
/// [`brain_ops::OpsContext::publish_stage_event`]) as well as
/// bus-published, so a subscriber that registers after this event
/// fires can still recover it through WAL-tail replay.
async fn publish_extracted_graph(
    ctx: &WorkerContext,
    memory_id: MemoryId,
    space_id: SpaceId,
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
        session_id: SessionId::default(),
        kind: MemoryKind::Episodic,
        salience: 0.0,
        timestamp_unix_nanos: now_unix_nanos(),
        text: None,
        graph_payload: None,
        edge_payload: None,
        stage_kind: Some(StageKind::Extractor),
        stage_outcome: Some(outcome),
        stage_payload: Some(payload),
        space_id,
        vector: None,
    };
    ctx.ops.publish_stage_event(envelope).await;
}

/// Publish a `StageCompleted{Hype}` event so subscribers waiting via
/// `--wait` can decrement their pending-stage checklist for this memory.
/// Called once per memory that actually ran [`HypeGenerator::generate_for`]
/// (skipped entirely for memories the idempotency check or the per-cycle
/// LLM budget bypassed — those never reach this point). `generate_for` is
/// best-effort and infallible: an internal LLM/embed/persist failure is
/// logged and folded into a zero-question outcome rather than surfaced as
/// an error, so there is no `Failed` case here — a zero outcome (LLM
/// failure, empty reply, or memory text genuinely yielding no questions)
/// and an "already had vectors" skip both round to `StageOutcome::Empty`,
/// same as the extractor's `Skipped` mapping in `publish_extracted_graph`.
async fn publish_hype_completed(
    ctx: &WorkerContext,
    memory_id: MemoryId,
    space_id: SpaceId,
    outcome: HypeGenOutcome,
) {
    let stage_outcome = if outcome.questions_written > 0 {
        StageOutcome::Ok
    } else {
        StageOutcome::Empty
    };
    let payload = StagePayload::Hype(StageHypePayload {
        questions_written: outcome.questions_written as u32,
        cost_micro_usd: outcome.cost_micro_usd,
    });
    let envelope = EventEnvelope {
        lsn: 0,
        event_type: EventType::StageCompleted,
        memory_id,
        session_id: SessionId::default(),
        kind: MemoryKind::Episodic,
        salience: 0.0,
        timestamp_unix_nanos: now_unix_nanos(),
        text: None,
        graph_payload: None,
        edge_payload: None,
        stage_kind: Some(StageKind::Hype),
        stage_outcome: Some(stage_outcome),
        stage_payload: Some(payload),
        space_id,
        vector: None,
    };
    ctx.ops.publish_stage_event(envelope).await;
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
    scope: brain_metadata::RowScope,
    em: &EntityMention,
    now: u64,
    embed_deps: Option<&EmbeddingDeps>,
    staged: &mut StagedEntityVectors,
    disambiguation: &mut Disambiguation<'_>,
) -> Result<(EntityId, ResolutionTier, f32), ApplyError> {
    let res = resolve_or_create_with_deps(
        wtxn,
        scope,
        &em.text,
        &em.entity_type_qname,
        em.confidence,
        now,
        embed_deps,
        staged,
        disambiguation,
    )
    .map_err(ApplyError::from)?;
    Ok((res.entity_id, res.tier, res.confidence))
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

/// Map a resolver tier to the durable `resolution_outcome` byte written on a
/// per-mention resolution audit row. `Alias` maps to `TIER_1_EXACT`: an alias
/// hit (including the trigram-fuzzy and coref tiers, which alias the surface
/// onto the matched entity) is an exact match against the alias index, so it
/// belongs with the tier-1 exact-identity bucket. The resolver always resolves
/// or mints an entity at this site, so the `AMBIGUOUS` / `NOT_RESOLVED`
/// outcomes never arise here — they exist for callers that can abstain.
fn resolution_tier_to_outcome(tier: ResolutionTier) -> u8 {
    match tier {
        ResolutionTier::Exact | ResolutionTier::Alias => resolution_outcome::TIER_1_EXACT,
        ResolutionTier::Fuzzy => resolution_outcome::TIER_2_FUZZY,
        ResolutionTier::Embedding => resolution_outcome::TIER_3_EMBEDDING,
        ResolutionTier::Disambiguated => resolution_outcome::TIER_4_LLM,
        ResolutionTier::Created => resolution_outcome::CREATED,
    }
}

/// Append one per-mention resolution audit row inside the extraction write txn
/// (no extra fsync — it rides the existing commit). A mention→entity
/// resolution is a derivation, so the acceptance suite's "all derivations
/// logged" line requires it to be queryable via `GET /v1/audit?by=resolution`.
///
/// Best-effort: a write failure is logged and swallowed, never failing the
/// extraction — matching the extraction-audit emission and the sibling
/// post-commit fan-outs. The row is append-only (a fresh `AuditId` per call),
/// so re-extracting the same memory adds rows rather than overwriting, which
/// preserves the resolution history.
fn emit_resolution_audit(
    wtxn: &redb::WriteTransaction,
    candidate_name: &str,
    entity_type_id: u32,
    resolved: EntityId,
    tier: ResolutionTier,
    confidence: f32,
    now: u64,
) {
    let mut audit = ResolutionAudit::new(
        AuditId::new(),
        candidate_name.to_string(),
        entity_type_id,
        resolution_tier_to_outcome(tier),
        confidence,
        now,
    );
    audit.resolved_entity_bytes = Some(resolved.to_bytes());
    if let Err(e) = resolution_audit_write(wtxn, &audit) {
        warn!(
            target: "brain_workers::extractor",
            surface = %candidate_name,
            error = %e,
            "resolution audit write failed; skipping (extraction unaffected)",
        );
    }
}

/// Link `memory_id --Mentions--> entity_id`, annotated with the surface form
/// that produced the entity. Keyed by `(memory, Mentions, entity)`, so a
/// repeat call for the same pair upserts rather than duplicating.
fn write_mention_edge(
    wtxn: &redb::WriteTransaction,
    memory_id: MemoryId,
    entity_id: EntityId,
    surface: &str,
    confidence: f32,
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
        weight: confidence.clamp(0.0, 1.0),
        origin: origin::AUTO_DERIVED,
        derived_by: derived_by::SIMILARITY_WORKER,
        created_at_unix_nanos: now,
        annotation: Some(surface.to_string()),
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

/// Cosine floor for embedding-based predicate consolidation.
///
/// BGE-small places ANTONYMS close in embedding space: `reports_to`↔
/// `manages`, `likes`↔`dislikes`, `parent_of`↔`child_of` sit around cosine
/// 0.85–0.90. Merging any of those would be a correctness disaster — it
/// makes "who does X report to" and "who does X manage" return the same
/// answer. The floor is therefore deliberately high: it should catch only
/// morphological / near-identical surface variants (`keen_on`/`keen_about`),
/// NOT semantically-adjacent-but-distinct predicates. Do not lower it.
const PREDICATE_CONSOLIDATION_FLOOR: f32 = 0.95;

/// Cosine similarity of two vectors, defensive on degenerate input:
/// mismatched length or a zero-norm operand yields `0.0` (never a NaN or a
/// panic), matching the read-path `grounded.rs` helper. A consolidation
/// hiccup must degrade to "no match / mint fresh", never abort a triple.
fn predicate_cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Pick the canonical predicate a would-be-fresh open-vocab predicate should
/// reuse, or `None` to mint fresh. `query` is the fresh predicate's
/// embedding; `candidates` are `(id, embedding, is_schema_declared)` for
/// same-namespace predicates that already carry a vector.
///
/// Selection rules (all deliberate — see [`PREDICATE_CONSOLIDATION_FLOOR`]):
/// - schema-declared predicates are NEVER merge targets — declared
///   vocabulary is authoritative and must not absorb open-vocab drift;
/// - only candidates at cosine ≥ floor qualify;
/// - highest cosine wins; ties break to the LOWEST (oldest) id so repeated
///   ingests of the same surface form converge on one canonical id.
///
/// Returned as `(id, cosine)` so the caller can log the score.
fn select_consolidation_target(
    query: &[f32],
    candidates: &[(brain_core::PredicateId, Vec<f32>, bool)],
) -> Option<(brain_core::PredicateId, f32)> {
    // Cosines within a whisker of each other count as a tie; the oldest id
    // then breaks it. An epsilon (not float `==`) keeps the tiebreak stable
    // and the lint quiet.
    const TIE_EPS: f32 = 1e-6;
    let mut best: Option<(brain_core::PredicateId, f32)> = None;
    for (id, vec, is_declared) in candidates {
        if *is_declared {
            continue;
        }
        let sim = predicate_cosine(query, vec);
        if sim < PREDICATE_CONSOLIDATION_FLOOR {
            continue;
        }
        let replace = match best {
            None => true,
            // Strictly higher cosine wins; on a tie the oldest (lowest) id
            // wins so repeated ingests converge on one canonical id.
            Some((best_id, best_sim)) => {
                sim > best_sim + TIE_EPS || ((sim - best_sim).abs() <= TIE_EPS && *id < best_id)
            }
        };
        if replace {
            best = Some((*id, sim));
        }
    }
    best
}

/// How many existing predicates to surface as reuse candidates in the LLM
/// prompt. Wide enough to cover the memory's relation neighborhood, tight
/// enough to stay a hint rather than a wall of vocabulary.
const CANDIDATE_PREDICATE_K: usize = 20;

/// Pick the top-K existing predicate NAMES nearest a memory's embedding, so
/// the extractor prompt can invite the LLM to reuse this DB's real relation
/// vocabulary. Pure function over `(query, candidates, k)` for isolated
/// testing; `candidates` are `(id, name, embedding, is_declared)` as returned
/// by [`brain_metadata::schema::predicate::predicate_consolidation_candidates_rtxn`].
///
/// Rules:
/// - `behavior_`-prefixed predicates are excluded — they are procedural-memory
///   sinks materialized by `MATERIALIZE_PROCEDURAL`, not extraction targets;
/// - only candidates at positive cosine qualify (a zero-norm / mismatched
///   vector scores `0.0` and is dropped);
/// - highest cosine wins; ties break to the LOWEST (oldest) id so the block
///   is deterministic across cycles;
/// - at most `k` names, in ranked order.
///
/// `is_declared` is intentionally ignored: BOTH declared and open-vocab
/// existing predicates are valid reuse targets — the goal is convergence on
/// whatever name already exists, not schema enforcement.
fn select_candidate_predicates(
    query: &[f32],
    candidates: &[brain_metadata::schema::predicate::PredicateConsolidationCandidate],
    k: usize,
) -> Vec<String> {
    const TIE_EPS: f32 = 1e-6;
    let mut scored: Vec<(f32, brain_core::PredicateId, &str)> = candidates
        .iter()
        .filter(|(_, name, _, _)| !name.starts_with("behavior_"))
        .filter_map(|(id, name, emb, _)| {
            let sim = predicate_cosine(query, emb);
            if sim > 0.0 {
                Some((sim, *id, name.as_str()))
            } else {
                None
            }
        })
        .collect();
    scored.sort_by(|a, b| {
        // Higher cosine first; within an epsilon the older (lower) id wins so
        // the block is deterministic across cycles.
        if (a.0 - b.0).abs() <= TIE_EPS {
            a.1.cmp(&b.1)
        } else {
            b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
        }
    });
    scored
        .into_iter()
        .take(k)
        .map(|(_, _, name)| name.to_string())
        .collect()
}

/// Build the per-memory candidate-predicate blocks for a batch: for each
/// memory, embed its text and render the top-[`CANDIDATE_PREDICATE_K`]
/// nearest existing `brain:` predicates as `- brain:<name>` bullets.
///
/// The reuse pool is the `brain` namespace: the default extractor emits
/// `brain:<snake_case>` predicates and consolidates them there, so that is
/// where every extracted relation already lives and the only namespace whose
/// names the LLM should reuse. (A future extractor that emits namespaced
/// predicates would union its caller namespace here.)
///
/// Best-effort throughout — a candidate-scan error yields no blocks and a
/// per-memory embed failure simply omits that memory, so extraction proceeds
/// with an empty `{CANDIDATE_PREDICATES}` rather than failing.
fn build_candidate_predicate_map(
    rtxn: &redb::ReadTransaction,
    deps: &EmbeddingDeps,
    mems: &[CoreMemory],
) -> HashMap<MemoryId, String> {
    let mut out = HashMap::new();
    let candidates =
        match brain_metadata::schema::predicate::predicate_consolidation_candidates_rtxn(
            rtxn, "brain",
        ) {
            Ok(c) if !c.is_empty() => c,
            _ => return out,
        };
    for m in mems {
        let Some(text) = m.text.as_deref() else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let Ok(query) = deps.embedder.embed(text) else {
            continue;
        };
        let names = select_candidate_predicates(&query, &candidates, CANDIDATE_PREDICATE_K);
        if names.is_empty() {
            continue;
        }
        let mut block = String::new();
        for name in names {
            block.push_str("- brain:");
            block.push_str(&name);
            block.push('\n');
        }
        out.insert(m.id, block);
    }
    out
}

/// Embed the bare predicate name and scan same-namespace open-vocab
/// predicates for a near-synonym to reuse. Returns `(canonical id,
/// canonical name, cosine)` on a clear hit, else `None` (mint fresh).
///
/// Best-effort throughout: an embed failure or a candidate-scan error
/// returns `None` so the caller degrades to a normal fresh intern — a
/// consolidation hiccup must never lose the triple.
fn try_consolidate_predicate(
    wtxn: &redb::WriteTransaction,
    deps: &EmbeddingDeps,
    ns: &str,
    name: &str,
) -> Option<(brain_core::PredicateId, String, f32)> {
    // Embed the same natural phrase `embed_predicate_if_absent` stores, so
    // the query and the candidate vectors live in one space.
    let phrase = name.replace('_', " ");
    let query = deps.embedder.embed(&phrase).ok()?;
    let candidates =
        brain_metadata::schema::predicate::predicate_consolidation_candidates(wtxn, ns).ok()?;
    if candidates.is_empty() {
        return None;
    }
    let name_by_id: HashMap<brain_core::PredicateId, String> = candidates
        .iter()
        .map(|(id, n, _, _)| (*id, n.clone()))
        .collect();
    let reduced: Vec<(brain_core::PredicateId, Vec<f32>, bool)> = candidates
        .into_iter()
        .map(|(id, _, vec, declared)| (id, vec, declared))
        .collect();
    let (id, cosine) = select_consolidation_target(&query, &reduced)?;
    let into = name_by_id.get(&id).cloned().unwrap_or_default();
    Some((id, into, cosine))
}

/// Intern a predicate for the apply path, consolidating near-synonym open-
/// vocab surface forms onto an existing predicate id when their embeddings
/// are near-identical (cosine ≥ [`PREDICATE_CONSOLIDATION_FLOOR`]).
///
/// The LLM extractor emits a free vocabulary, so the same relation is coined
/// under many surface forms across ingests (`keen_on`, `keen_about`,
/// `interested_in`). Minting a distinct predicate per form fragments the
/// typed graph, scattering what the grounded read lane and `STATEMENT_LIST`
/// see. This folds the morphological variants onto one canonical id.
///
/// Fast path is unchanged: an exact qname repeat returns its id directly and
/// embeds-if-absent as before. A would-be-fresh predicate is embedded and
/// matched against existing same-namespace OPEN-VOCAB predicates; a clear hit
/// aliases the new surface form onto that id (so future exact lookups
/// converge) and returns it, otherwise the predicate is minted fresh exactly
/// as before. Schema-declared predicates (e.g. seeded `brain:occurred_at`)
/// are never merge targets, so the memory-subject Event path is a no-op here.
///
/// Any failure in the consolidation lookup DEGRADES to a normal fresh
/// intern — a scan hiccup must never lose the triple.
fn resolve_or_intern_predicate(
    wtxn: &redb::WriteTransaction,
    embed_deps: Option<&EmbeddingDeps>,
    metrics: &ExtractorMetrics,
    ns: &str,
    name: &str,
    now: u64,
) -> Result<brain_core::PredicateId, brain_metadata::schema::predicate::PredicateOpError> {
    // Exact repeat mention — unchanged fast path, no scan.
    if let Some(pid) = brain_metadata::schema::predicate::predicate_id_by_qname(wtxn, ns, name)? {
        embed_predicate_if_absent(wtxn, embed_deps, pid, name);
        return Ok(pid);
    }

    // Would-be-fresh. Attempt embedding-based consolidation when an embedder
    // is wired. Best-effort: any hiccup falls through to the fresh mint.
    if let Some(deps) = embed_deps {
        if let Some((canonical, into_name, cosine)) =
            try_consolidate_predicate(wtxn, deps, ns, name)
        {
            match brain_metadata::schema::predicate::predicate_alias_qname(
                wtxn, ns, name, canonical,
            ) {
                Ok(()) => {
                    metrics.inc_predicate_consolidated();
                    debug!(
                        target: "brain_workers::extractor",
                        from = %name,
                        into = %into_name,
                        cosine,
                        "predicate consolidated onto near-synonym",
                    );
                    return Ok(canonical);
                }
                Err(e) => {
                    // Alias refused (target vanished / became declared) → mint.
                    trace!(predicate = %name, error = %e, "predicate alias failed; minting fresh");
                }
            }
        }
    }

    // Fresh mint — unchanged path (intern + embed-if-absent).
    let pid = predicate_intern_or_get(wtxn, ns, name, 0, now)?;
    embed_predicate_if_absent(wtxn, embed_deps, pid, name);
    Ok(pid)
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
/// Precedence: (1) the predicate's declared object constraint wins — `Entity`
/// → mint best-effort, `Value` → keep text; (2) otherwise the mention's own
/// `object_is_entity` flag decides. Only once the axis says ENTITY do we look
/// the surface up: an object already surfaced as an entity this cycle links,
/// and a real entity not yet surfaced is minted best-effort (reusing the
/// relation-endpoint path: cross-type reuse + mintability guard). A literal —
/// or a non-mintable surface — stays text, so values like "blue"/"200" never
/// become junk entities. Returns `None` when
/// the mention carries no object text at all (a malformed extraction that
/// slipped past schema validation) — the caller drops the triple rather than
/// have this fabricate an empty-string value (a core-invariant violation:
/// never persist data that wasn't actually captured).
#[allow(clippy::too_many_arguments)]
fn resolve_statement_object(
    wtxn: &redb::WriteTransaction,
    scope: brain_metadata::RowScope,
    memory_id: MemoryId,
    sm: &StatementMention,
    pid: brain_core::PredicateId,
    entity_map: &mut HashMap<String, EntityId>,
    mentioned: &mut HashSet<EntityId>,
    embed_deps: Option<&EmbeddingDeps>,
    staged: &mut StagedEntityVectors,
    disambiguation: &mut Disambiguation<'_>,
    now: u64,
) -> Result<Option<StatementObject>, ApplyError> {
    let Some(text) = sm.object_text.as_deref() else {
        return Ok(None);
    };
    // Decide the object AXIS before looking any surface up. Object constraint
    // byte: 1 = Entity, 2 = Value (see brain-metadata predicate table). An
    // open predicate (None) defers to the mention's own `object_is_entity`.
    //
    // The axis decision must precede the `entity_map` lookup. `object_is_entity`
    // is a deliberate per-triple judgment the LLM makes under constrained
    // decoding ("senior engineer" is a role VALUE, not a thing to hold facts
    // about). An earlier tier having minted an entity for the same surface is
    // evidence the span was MENTIONED — never evidence that this triple asserts
    // about it. Consulting the map first let that incidental hit silently
    // override both the explicit flag and a schema-declared `Value` constraint,
    // binding literals as entity objects.
    let want_entity = match predicate_declared_object_constraint_in_write_txn(wtxn, pid)? {
        Some(1) => true,
        Some(2) => false,
        _ => sm.object_is_entity,
    };
    if want_entity {
        // An object already surfaced as an entity this cycle links straight
        // through. A first-person object the subject pass cached (the space
        // self-entity) lands here too — a standalone first-person object
        // ("report to me") is follow-up; the dominant first-person case is the
        // subject ("I prefer …"), handled in `resolve_statement_subject`.
        //
        // No mention edge to write here: every site that populates `entity_map`
        // (pass 1, `resolve_statement_subject`, `resolve_relation_endpoint`)
        // links the entity to this memory as it inserts, so an `entity_map` hit
        // is by construction already in `mentioned`. That invariant is unchanged
        // by the gating above — the map is still a strict subset of `mentioned`;
        // we simply no longer consult it for a triple whose object is a value.
        if let Some(id) = entity_map.get(text).copied() {
            return Ok(Some(StatementObject::Entity(id)));
        }
        if let Some(id) = resolve_relation_endpoint(
            wtxn,
            scope,
            memory_id,
            text,
            sm.confidence,
            entity_map,
            mentioned,
            embed_deps,
            staged,
            disambiguation,
            now,
        )? {
            return Ok(Some(StatementObject::Entity(id)));
        }
    }
    Ok(Some(StatementObject::Value(StatementValue::Text(
        text.to_string(),
    ))))
}

// ---------------------------------------------------------------------------
// Statement-object normalization (pre-apply pass).
//
// The LLM tier occasionally emits verbose, compound, or vague object phrases
// ("counseling and mental health jobs", "people to grow") that persist as
// `Text(...)` values the grounded read path can neither match nor aggregate.
// This pass runs once over the pipeline's items before Pass 2 apply: it prunes
// vague / sub-floor statement mentions and splits a compound VALUE object into
// one atomic statement per fragment, so the typed graph holds queryable facts.
// ---------------------------------------------------------------------------

/// Confidence floor applied to statement mentions at persist time. A mention
/// below this is dropped rather than written — a low-signal triple pollutes the
/// graph and can never win a grounded read. The LLM tier already gates at its
/// projection threshold (0.7); this modest floor catches lower-tier statement
/// mentions without fighting that gate.
const APPLY_STATEMENT_MIN_CONFIDENCE: f32 = 0.35;

/// Head nouns that make a value object un-queryable. A value whose head token
/// is one of these ("people", "things", "someone") asserts nothing a read can
/// retrieve — the vague residue of an over-eager extraction. Kept tight: only
/// unambiguous placeholders, so the only false positives ("people skills") are
/// themselves low-value.
const GENERIC_OBJECT_HEADS: &[&str] = &[
    "people",
    "person",
    "someone",
    "somebody",
    "something",
    "anything",
    "everything",
    "nothing",
    "thing",
    "things",
    "stuff",
    "others",
    "everyone",
    "anyone",
    "them",
    "they",
    "it",
    "this",
    "that",
];

/// Connectives ignored when deciding whether a value phrase is entirely
/// generic. Not an exhaustive stopword list — just the glue words in a generic
/// phrase ("a lot of stuff", "things to do").
const OBJECT_FUNCTION_WORDS: &[&str] = &[
    "a", "an", "the", "to", "of", "for", "and", "or", "with", "in", "on", "some", "lot", "more",
];

/// Separators a compound object phrase is split on. `&` and `/` are absent so
/// proper names ("Ben & Jerry's", "AT&T", "TCP/IP") and ratios stay whole.
const OBJECT_SPLIT_SEPARATORS: &[&str] = &[" and ", " or ", ",", ";"];

/// Closed-class pronouns/possessives. Their presence anywhere in a VALUE
/// object betrays a clause: a value ("charity race", "mental health") never
/// carries a subject or object pronoun, whereas prose the LLM occasionally
/// emits as an object does ("painting helps ME explore MY identity"). This is
/// a grammatical function-word class, not a domain vocabulary list.
const CLAUSE_PRONOUNS: &[&str] = &[
    "i",
    "me",
    "my",
    "mine",
    "myself",
    "you",
    "your",
    "yours",
    "yourself",
    "we",
    "us",
    "our",
    "ours",
    "ourselves",
    "he",
    "him",
    "his",
    "himself",
    "she",
    "her",
    "hers",
    "herself",
    "they",
    "them",
    "their",
    "theirs",
    "themselves",
    "who",
    "whom",
    "whose",
];

/// Closed-class WH-words / subordinators. A value object that LEADS with one of
/// these opens a subordinate or interrogative clause ("how much I've grown",
/// "that it matters", "because ...") rather than naming a value. Grammatical
/// function words, not domain vocabulary.
const CLAUSE_LEAD_MARKERS: &[&str] = &[
    "how", "what", "why", "when", "where", "whether", "if", "that", "because", "since", "while",
    "although", "though", "which", "who",
];

/// Delexical / light-verb gerunds. When one of these LEADS a multi-word phrase
/// it opens a predicate ("being able to give a voice ...", "having a hard
/// time") rather than naming an activity. A contentful gerund
/// ("volunteering at the shelter", "giving back") is deliberately absent so
/// legitimate activity values survive.
const CLAUSE_LEAD_LIGHT_GERUNDS: &[&str] = &["being", "having", "doing", "getting", "going"];

/// Word-count ceiling for a value object. A noun-phrase value is short; genuine
/// values top out around four content-bearing words ("mental health support
/// group"). Anything longer is almost always a clause the LLM emitted as an
/// object. Conservative on purpose: 5+ words, not 4+, so ordinary compound
/// noun phrases pass.
const CLAUSE_MAX_WORDS: usize = 4;

/// True when a VALUE object is sentence/clause-shaped rather than an atomic
/// value or entity. The rule is purely STRUCTURAL — it inspects word count and
/// closed-class grammatical markers, never domain vocabulary — so it collapses
/// prose objects ("painting helps me explore my identity", "how much I've
/// developed since coming out", "being able to give a voice to the trans
/// community") while leaving legitimate short values untouched ("charity race",
/// "LGBTQ support group", "mental health"). A value is a clause when ANY holds:
///
/// * it leads with a WH-word / subordinator, or
/// * it leads (multi-word) with a delexical light-verb gerund, or
/// * it contains any personal/relative pronoun, or
/// * it exceeds [`CLAUSE_MAX_WORDS`] content tokens.
///
/// Runs per-fragment after compound splitting, so a clause that happens to
/// contain " and " ("I like running and I enjoy swimming") is caught on each
/// resulting fragment.
fn is_clause_like(s: &str) -> bool {
    let tokens = object_tokens(s);
    let Some(head) = tokens.first() else {
        return false;
    };
    if CLAUSE_LEAD_MARKERS.contains(&head.as_str()) {
        return true;
    }
    if tokens.len() >= 2 && CLAUSE_LEAD_LIGHT_GERUNDS.contains(&head.as_str()) {
        return true;
    }
    if tokens.iter().any(|t| CLAUSE_PRONOUNS.contains(&t.as_str())) {
        return true;
    }
    tokens.len() > CLAUSE_MAX_WORDS
}

/// Lowercased alphanumeric tokens of `s`, splitting on any non-alphanumeric.
fn object_tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// True when a value object carries no retrievable fact: empty, or its head
/// token is a generic placeholder, or every meaningful token is generic.
fn is_vague_object(s: &str) -> bool {
    let tokens = object_tokens(s);
    let Some(head) = tokens.first() else {
        return true;
    };
    if GENERIC_OBJECT_HEADS.contains(&head.as_str()) {
        return true;
    }
    let meaningful: Vec<&String> = tokens
        .iter()
        .filter(|t| !OBJECT_FUNCTION_WORDS.contains(&t.as_str()))
        .collect();
    !meaningful.is_empty()
        && meaningful
            .iter()
            .all(|t| GENERIC_OBJECT_HEADS.contains(&t.as_str()))
}

/// Split a compound object phrase into atomic fragments, dropping vague and
/// duplicate ones. A phrase with no separator yields a single fragment (or
/// none, if it is vague). An all-vague compound yields an empty vec so the
/// caller can drop the whole statement.
fn split_compound_object(s: &str) -> Vec<String> {
    // Rewrite every separator to one sentinel, then split once. `\u{1}` can't
    // appear in a sane object phrase, so it's an unambiguous marker.
    let mut work = s.to_string();
    for sep in OBJECT_SPLIT_SEPARATORS {
        work = work.replace(sep, "\u{1}");
    }
    let mut out: Vec<String> = Vec::new();
    for frag in work.split('\u{1}') {
        let frag = frag
            .trim()
            .trim_matches(|c: char| c == '.' || c == '"' || c == '\'')
            .trim();
        if frag.is_empty() || is_vague_object(frag) {
            continue;
        }
        if !out.iter().any(|e| e.eq_ignore_ascii_case(frag)) {
            out.push(frag.to_string());
        }
    }
    out
}

/// Normalize a pipeline's extracted items before Pass 2 apply. Entity and
/// relation mentions pass through untouched; statement mentions are pruned
/// (vague object, sub-floor confidence, or a clause/sentence-shaped VALUE
/// object) and a compound VALUE object is split into one atomic statement per
/// fragment. Temporal (memory-subject) and
/// retraction mentions pass through unchanged — their object is a timestamp or
/// the exact prior value a retraction must match, neither of which may be
/// split. Dropped mentions bump the `apply_dropped` metric so the loss is
/// observable rather than silent.
fn normalize_statement_items(
    items: &[ExtractedItem],
    metrics: &ExtractorMetrics,
) -> Vec<ExtractedItem> {
    let mut out: Vec<ExtractedItem> = Vec::with_capacity(items.len());
    for item in items {
        let ExtractedItem::StatementMention(sm) = item else {
            out.push(item.clone());
            continue;
        };
        // Timestamps (temporal events) and retractions carry an object that
        // must persist verbatim — never split or vague-prune them.
        if sm.subject_is_memory || sm.retract {
            out.push(item.clone());
            continue;
        }
        if sm.confidence < APPLY_STATEMENT_MIN_CONFIDENCE {
            metrics.inc_apply_dropped("statement_low_confidence");
            continue;
        }
        let Some(obj) = sm.object_text.as_deref() else {
            // No inline object text (Memory / Statement object kinds) — nothing
            // to normalize.
            out.push(item.clone());
            continue;
        };
        if sm.object_is_entity {
            // Proper-name object: don't split (would break "Ben & Jerry's" /
            // "Johnson and Johnson"), but still prune a vague placeholder.
            if is_vague_object(obj) {
                metrics.inc_apply_dropped("vague_object");
            } else {
                out.push(item.clone());
            }
            continue;
        }
        // Literal value object: split compound phrases into atomic values and
        // drop vague fragments.
        let fragments = split_compound_object(obj);
        if fragments.is_empty() {
            metrics.inc_apply_dropped("vague_object");
            continue;
        }
        for frag in fragments {
            // A clause/sentence object ("painting helps me explore my identity")
            // names no atomic value the grounded read can match or embed
            // cleanly. Without a POS model there is no reliable domain-general
            // way to salvage the head noun phrase, so drop the triple rather
            // than persist prose — counted so the loss stays observable.
            if is_clause_like(&frag) {
                metrics.inc_apply_dropped("clause_object");
                continue;
            }
            let mut split = sm.clone();
            split.object_text = Some(frag);
            out.push(ExtractedItem::StatementMention(split));
        }
    }
    out
}

#[cfg(test)]
mod object_normalize_tests {
    use super::*;

    fn stmt(object: &str, object_is_entity: bool, confidence: f32) -> ExtractedItem {
        ExtractedItem::StatementMention(StatementMention {
            kind: 1,
            subject_text: Some("Caroline".into()),
            predicate_qname: "brain:keen_on".into(),
            object_text: Some(object.into()),
            confidence,
            extractor_id: 7,
            extractor_version: 1,
            is_stateful: false,
            subject_is_memory: false,
            object_is_entity,
            event_at_unix_nanos: None,
            subject_is_self: false,
            retract: false,
        })
    }

    fn objects(items: &[ExtractedItem]) -> Vec<String> {
        items
            .iter()
            .filter_map(|i| match i {
                ExtractedItem::StatementMention(m) => m.object_text.clone(),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn compound_value_object_splits_into_atomic_statements() {
        let metrics = ExtractorMetrics::new();
        let items = vec![stmt("counseling and mental health jobs", false, 0.9)];
        let out = normalize_statement_items(&items, &metrics);
        assert_eq!(objects(&out), vec!["counseling", "mental health jobs"]);
        // Every other field is preserved on each split statement.
        for i in &out {
            let ExtractedItem::StatementMention(m) = i else {
                panic!("expected statement");
            };
            assert_eq!(m.predicate_qname, "brain:keen_on");
            assert_eq!(m.subject_text.as_deref(), Some("Caroline"));
        }
    }

    #[test]
    fn comma_and_or_separators_all_split() {
        let metrics = ExtractorMetrics::new();
        let items = vec![stmt("hiking, biking or swimming", false, 0.9)];
        let out = normalize_statement_items(&items, &metrics);
        assert_eq!(objects(&out), vec!["hiking", "biking", "swimming"]);
    }

    #[test]
    fn clean_atomic_value_passes_through_unchanged() {
        let metrics = ExtractorMetrics::new();
        let items = vec![stmt("Senior Engineer", false, 0.9)];
        let out = normalize_statement_items(&items, &metrics);
        assert_eq!(out, items);
    }

    #[test]
    fn vague_value_object_is_pruned() {
        let metrics = ExtractorMetrics::new();
        let items = vec![
            stmt("people", false, 0.9),
            stmt("people to grow", false, 0.9),
            stmt("things", false, 0.9),
        ];
        let out = normalize_statement_items(&items, &metrics);
        assert!(out.is_empty(), "all vague objects should be pruned");
    }

    #[test]
    fn sub_floor_confidence_is_pruned() {
        let metrics = ExtractorMetrics::new();
        let items = vec![stmt("counseling", false, 0.1)];
        let out = normalize_statement_items(&items, &metrics);
        assert!(out.is_empty());
    }

    #[test]
    fn entity_object_is_not_split_but_vague_is_pruned() {
        let metrics = ExtractorMetrics::new();
        // A proper-name entity object with a conjunction stays whole.
        let whole = vec![stmt("Johnson and Johnson", true, 0.9)];
        let out = normalize_statement_items(&whole, &metrics);
        assert_eq!(objects(&out), vec!["Johnson and Johnson"]);
        // A vague entity object is still pruned.
        let vague = vec![stmt("people", true, 0.9)];
        let out = normalize_statement_items(&vague, &metrics);
        assert!(out.is_empty());
    }

    #[test]
    fn compound_with_one_vague_fragment_keeps_the_specific_one() {
        let metrics = ExtractorMetrics::new();
        let items = vec![stmt("counseling and people", false, 0.9)];
        let out = normalize_statement_items(&items, &metrics);
        assert_eq!(objects(&out), vec!["counseling"]);
    }

    #[test]
    fn retraction_and_temporal_objects_pass_through_untouched() {
        let metrics = ExtractorMetrics::new();
        let ExtractedItem::StatementMention(mut retract) = stmt("a and b", false, 0.9) else {
            unreachable!()
        };
        retract.retract = true;
        let ExtractedItem::StatementMention(mut temporal) = stmt("a and b", false, 0.9) else {
            unreachable!()
        };
        temporal.subject_is_memory = true;
        let items = vec![
            ExtractedItem::StatementMention(retract),
            ExtractedItem::StatementMention(temporal),
        ];
        let out = normalize_statement_items(&items, &metrics);
        assert_eq!(out, items, "retraction/temporal objects must not be split");
    }

    #[test]
    fn non_statement_items_pass_through() {
        let metrics = ExtractorMetrics::new();
        let items = vec![ExtractedItem::EntityMention(EntityMention {
            entity_type_qname: "brain:Person".into(),
            text: "Caroline".into(),
            start: 0,
            end: 8,
            confidence: 0.9,
            extractor_id: 7,
            extractor_version: 1,
        })];
        let out = normalize_statement_items(&items, &metrics);
        assert_eq!(out, items);
    }

    #[test]
    fn is_vague_object_unit() {
        assert!(is_vague_object("people"));
        assert!(is_vague_object("  things  "));
        assert!(is_vague_object("people to grow"));
        assert!(is_vague_object(""));
        assert!(!is_vague_object("counseling"));
        assert!(!is_vague_object("mental health"));
        assert!(!is_vague_object("Senior Engineer"));
    }

    #[test]
    fn split_compound_object_dedupes_case_insensitively() {
        assert_eq!(
            split_compound_object("Coffee and coffee and tea"),
            vec!["Coffee", "tea"]
        );
    }

    #[test]
    fn clause_value_objects_are_dropped() {
        let metrics = ExtractorMetrics::new();
        let items = vec![
            // pronoun-bearing prose
            stmt("painting helps me explore my identity", false, 0.9),
            // leading WH-word
            stmt("how much I've developed since coming out", false, 0.9),
            // leading light-verb gerund + over-length
            stmt(
                "being able to give a voice to the trans community",
                false,
                0.9,
            ),
        ];
        let out = normalize_statement_items(&items, &metrics);
        assert!(out.is_empty(), "clause objects should be dropped: {out:?}");
    }

    #[test]
    fn legitimate_short_values_survive_clause_filter() {
        let metrics = ExtractorMetrics::new();
        let items = vec![
            stmt("charity race", false, 0.9),
            stmt("LGBTQ support group", false, 0.9),
            stmt("mental health", false, 0.9),
            // contentful gerund activity — NOT a light-verb gerund
            stmt("volunteering at shelter", false, 0.9),
            // four content words is the ceiling, not over it
            stmt("mental health support group", false, 0.9),
        ];
        let out = normalize_statement_items(&items, &metrics);
        assert_eq!(
            objects(&out),
            vec![
                "charity race",
                "LGBTQ support group",
                "mental health",
                "volunteering at shelter",
                "mental health support group",
            ]
        );
    }

    #[test]
    fn entity_object_clause_is_left_untouched() {
        let metrics = ExtractorMetrics::new();
        // Entity (proper-name) objects bypass the value-object clause path
        // entirely — only vague ones are pruned, and this one is not vague.
        let items = vec![stmt("Doctors Without Borders", true, 0.9)];
        let out = normalize_statement_items(&items, &metrics);
        assert_eq!(objects(&out), vec!["Doctors Without Borders"]);
    }

    #[test]
    fn compound_of_clause_and_value_keeps_only_the_value() {
        let metrics = ExtractorMetrics::new();
        // Splitting yields "coffee" and "it makes me happy"; the latter is a
        // clause (pronoun) and is dropped, the former survives.
        let items = vec![stmt("coffee and it makes me happy", false, 0.9)];
        let out = normalize_statement_items(&items, &metrics);
        assert_eq!(objects(&out), vec!["coffee"]);
    }

    #[test]
    fn is_clause_like_unit() {
        // clauses
        assert!(is_clause_like("painting helps me explore my identity"));
        assert!(is_clause_like("how much I've developed"));
        assert!(is_clause_like("that it matters"));
        assert!(is_clause_like("being able to give a voice"));
        // over the word-count ceiling
        assert!(is_clause_like("a really long phrase that runs on"));
        // non-clauses
        assert!(!is_clause_like("charity race"));
        assert!(!is_clause_like("LGBTQ support group"));
        assert!(!is_clause_like("mental health"));
        assert!(!is_clause_like("volunteering at shelter"));
        assert!(!is_clause_like("mental health support group"));
        assert!(!is_clause_like("hiking")); // single gerund word
        assert!(!is_clause_like("walking tour")); // gerund but not light-verb
        assert!(!is_clause_like(""));
    }
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
///
/// A subject resolved here IS mentioned by this memory even when no tier filed
/// it as an entity mention, so — exactly as for a relation endpoint — it also
/// gets a `Mentions` edge. Readers enumerate a memory's entities by walking
/// `Mentions`; without the edge the subject node is missing from that list and
/// the statement's own `from` end renders against a node nobody listed.
/// `mentioned` carries the entities already linked this apply, so the edge is
/// written once per entity and never for one an earlier pass covered.
#[allow(clippy::too_many_arguments)]
fn resolve_statement_subject(
    wtxn: &redb::WriteTransaction,
    scope: brain_metadata::RowScope,
    memory_id: MemoryId,
    sm: &StatementMention,
    entity_map: &mut HashMap<String, EntityId>,
    mentioned: &mut HashSet<EntityId>,
    self_entity_id: Option<EntityId>,
    embed_deps: Option<&EmbeddingDeps>,
    staged: &mut StagedEntityVectors,
    disambiguation: &mut Disambiguation<'_>,
    now: u64,
) -> Result<Option<EntityId>, ApplyError> {
    let Some(text) = sm.subject_text.as_deref() else {
        return Ok(None);
    };
    let entity_id = if let Some(id) = entity_map.get(text).copied() {
        id
    } else if let Some(self_id) = self_entity_id.filter(|_| sm.subject_is_self) {
        // First person ("I prefer …") refers to the writing space — route to
        // its self-entity rather than dropping it as a non-referential pronoun.
        // The judgment is the LLM's (`subject_is_self`), so it holds across any
        // language with NO hardcoded pronoun list — "I", "yo", "私", "ich" all
        // flow through the same flag. Must precede the non-referential drop (a
        // bare first-person surface would otherwise be discarded). Cached so a
        // later object/endpoint reuses the same id.
        //
        // statement_create requires the subject entity to exist, so
        // materialize the space's self-entity row on first use (idempotent).
        ensure_space_self_entity(wtxn, scope, self_id, now)?;
        entity_map.insert(text.to_string(), self_id);
        self_id
    } else if !statement_subject_mintable(text) {
        return Ok(None);
    } else if let Some(id) = reuse_cross_type_exact(wtxn, scope, text)? {
        // Cross-type reuse before minting a generic node: if exactly one
        // already existing entity (of any type) matches this exact canonical
        // name, it is almost certainly the same referent — reuse it so a coined
        // Concept doesn't permanently split from a correctly-typed entity
        // ("aspirin" the Drug). 0 or >1 matches fall through to the normal
        // type-scoped mint.
        entity_map.insert(text.to_string(), id);
        id
    } else {
        let res = resolve_or_create_with_deps(
            wtxn,
            scope,
            text,
            COINED_SUBJECT_ENTITY_TYPE,
            sm.confidence,
            now,
            embed_deps,
            staged,
            disambiguation,
        )
        .map_err(ApplyError::from)?;
        // Coined-subject resolution is a mention→entity derivation not covered
        // by the pass-1 entity-mention loop (this surface was never filed as an
        // EntityMention), so log it here. The other branches above are an
        // entity_map cache hit (already audited when first resolved), a
        // self-entity routing (deterministic, not a tier decision), or a
        // cross-type exact reuse (deterministic exact-name match) — none is a
        // fresh tiered resolution.
        let resolved_type_id = entity_get_inside_wtxn(wtxn, res.entity_id)
            .ok()
            .flatten()
            .map_or(0, |e| e.entity_type.raw());
        emit_resolution_audit(
            wtxn,
            text,
            resolved_type_id,
            res.entity_id,
            res.tier,
            res.confidence,
            now,
        );
        entity_map.insert(text.to_string(), res.entity_id);
        res.entity_id
    };
    if mentioned.insert(entity_id) {
        write_mention_edge(wtxn, memory_id, entity_id, text, sm.confidence, now)?;
    }
    Ok(Some(entity_id))
}

/// Idempotently materialize the writing space's self-entity row so that
/// first-person statements (which use `EntityId::from(space_id)` as their
/// subject) pass `statement_create`'s subject-existence check. The canonical
/// name is the space's own id in hex (`space:<32 hex>`) — globally unique per
/// space, so multi-space self-entities never collide, and it can never clash
/// with a real extracted person's name. Typed `Person`: the space/user is a
/// person-like referent. A no-op when the row already exists.
fn ensure_space_self_entity(
    wtxn: &redb::WriteTransaction,
    scope: brain_metadata::RowScope,
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
    let canonical = format!("space:{:032x}", u128::from_be_bytes(self_id.to_bytes()));
    let entity = brain_core::Entity::new_active(
        self_id,
        brain_core::EntityType::PERSON_ID,
        canonical.clone(),
        brain_metadata::entity::ops::normalize_name(&canonical),
        now,
    );
    // The space self-entity is a space-level, session-agnostic identity;
    // its first-mention session is meaningless, so it lands in the default
    // session.
    brain_metadata::entity::ops::entity_put(wtxn, scope, brain_core::SessionId::DEFAULT, &entity)
        .map_err(|e| ApplyError::Storage(format!("entity_put(self): {e}")))?;
    Ok(())
}

/// Reuse an existing entity for a coined surface when exactly one entity of
/// any type carries this exact canonical name. A single cross-type hit is a
/// strong same-referent signal; 0 or >1 returns `None` so the caller mints
/// under the generic coined type rather than risk a wrong merge.
fn reuse_cross_type_exact(
    wtxn: &redb::WriteTransaction,
    scope: brain_metadata::RowScope,
    text: &str,
) -> Result<Option<EntityId>, ApplyError> {
    let hits = brain_metadata::entity_resolve_canonical_all_types_wtxn(wtxn, scope, text)
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
///
/// An endpoint resolved here IS mentioned by this memory even though no tier
/// filed it as an entity mention, so it also gets a `Mentions` edge — without
/// one the entity is invisible to every reader that enumerates a memory's
/// entities by walking `Mentions` (graph enrichment, encode artifacts), and a
/// relation would render pointing at a node that doesn't appear. `mentioned`
/// carries the entities already linked this apply, so the edge is written once
/// per entity and never for one pass 1 already covered.
#[allow(clippy::too_many_arguments)]
fn resolve_relation_endpoint(
    wtxn: &redb::WriteTransaction,
    scope: brain_metadata::RowScope,
    memory_id: MemoryId,
    text: &str,
    confidence: f32,
    entity_map: &mut HashMap<String, EntityId>,
    mentioned: &mut HashSet<EntityId>,
    embed_deps: Option<&EmbeddingDeps>,
    staged: &mut StagedEntityVectors,
    disambiguation: &mut Disambiguation<'_>,
    now: u64,
) -> Result<Option<EntityId>, ApplyError> {
    let entity_id = if let Some(id) = entity_map.get(text).copied() {
        id
    } else if !statement_subject_mintable(text) {
        return Ok(None);
    } else if let Some(id) = reuse_cross_type_exact(wtxn, scope, text)? {
        entity_map.insert(text.to_string(), id);
        id
    } else {
        let res = resolve_or_create_with_deps(
            wtxn,
            scope,
            text,
            COINED_SUBJECT_ENTITY_TYPE,
            confidence,
            now,
            embed_deps,
            staged,
            disambiguation,
        )
        .map_err(ApplyError::from)?;
        // A relation / statement-object endpoint resolved through the gauntlet
        // is a mention→entity derivation; log it (same rationale and branch
        // exclusions as `resolve_statement_subject`).
        let resolved_type_id = entity_get_inside_wtxn(wtxn, res.entity_id)
            .ok()
            .flatten()
            .map_or(0, |e| e.entity_type.raw());
        emit_resolution_audit(
            wtxn,
            text,
            resolved_type_id,
            res.entity_id,
            res.tier,
            res.confidence,
            now,
        );
        entity_map.insert(text.to_string(), res.entity_id);
        res.entity_id
    };
    if mentioned.insert(entity_id) {
        write_mention_edge(wtxn, memory_id, entity_id, text, confidence, now)?;
    }
    Ok(Some(entity_id))
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
    fn __ts() -> brain_metadata::RowScope {
        brain_metadata::RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xA1; 16])
    }

    /// Seed a `MEMORIES_TABLE` row for `memory_id` owned by `scope`, so the
    /// apply pass derives the same `(namespace, space)` it was extracted under
    /// (real ENCODE always has the row; a synthetic id needs it planted, else
    /// apply hits the degenerate missing-row fallback and stamps a different
    /// space than the test reads back with).
    fn __seed_memory_row(
        metadata: &brain_metadata::MetadataDb,
        memory_id: brain_core::MemoryId,
        scope: brain_metadata::RowScope,
    ) {
        use brain_core::{MemoryKind, SessionId};
        use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
        let row = MemoryMetadata::new_active(
            memory_id,
            scope.namespace(),
            scope.space(),
            SessionId(0),
            0,
            0,
            MemoryKind::Episodic,
            [0u8; 16],
            1.0,
            0,
            0,
        );
        let wtxn = metadata.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            t.insert(&memory_id.to_be_bytes(), &row).unwrap();
        }
        wtxn.commit().unwrap();
    }

    /// Seed a `MEMORIES_TABLE` row carrying an explicit `occurred_at` message
    /// time, so the apply pass has a real ANCHOR day to exclude resolved dates
    /// against.
    fn __seed_memory_row_occurred_at(
        metadata: &brain_metadata::MetadataDb,
        memory_id: brain_core::MemoryId,
        scope: brain_metadata::RowScope,
        occurred_at: u64,
    ) {
        use brain_core::{MemoryKind, SessionId};
        use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
        let row = MemoryMetadata::new_active(
            memory_id,
            scope.namespace(),
            scope.space(),
            SessionId(0),
            0,
            0,
            MemoryKind::Episodic,
            [0u8; 16],
            1.0,
            0,
            occurred_at,
        )
        .with_occurred_at(Some(occurred_at));
        let wtxn = metadata.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            t.insert(&memory_id.to_be_bytes(), &row).unwrap();
        }
        wtxn.commit().unwrap();
    }

    use super::*;

    // ----- Predicate consolidation selection (pure logic). -----

    use brain_core::PredicateId;

    /// A unit vector at angle `theta` (radians) in the x/y plane, padded with
    /// zeros — a compact way to hand-set an exact cosine between two vectors
    /// (cos of the angle between them) without shipping real BGE outputs.
    fn unit_at(theta: f32) -> Vec<f32> {
        vec![theta.cos(), theta.sin(), 0.0]
    }

    #[test]
    fn consolidation_merges_morphological_variant_above_floor() {
        // Two surfaces ~4.4° apart → cosine ≈ 0.997, well over the 0.95 floor.
        let existing = unit_at(0.0);
        let query = unit_at(0.077);
        let cands = vec![(PredicateId::from(1), existing, false)];
        let (id, cos) = select_consolidation_target(&query, &cands).expect("should merge");
        assert_eq!(id, PredicateId::from(1));
        assert!(cos >= PREDICATE_CONSOLIDATION_FLOOR, "cos {cos}");
    }

    #[test]
    fn consolidation_keeps_antonym_like_pair_separate() {
        // ~31° apart → cosine ≈ 0.857, the antonym danger band. Must NOT merge.
        let existing = unit_at(0.0);
        let query = unit_at(0.541);
        let cands = vec![(PredicateId::from(1), existing, false)];
        assert!(
            select_consolidation_target(&query, &cands).is_none(),
            "antonym-band pair must stay separate",
        );
    }

    #[test]
    fn consolidation_never_targets_schema_declared() {
        // An identical vector, but the only candidate is schema-declared — the
        // incoming free predicate must still mint fresh (never fold onto a
        // declared predicate like seeded `occurred_at`).
        let query = unit_at(0.0);
        let cands = vec![(PredicateId::from(7), unit_at(0.0), true)];
        assert!(
            select_consolidation_target(&query, &cands).is_none(),
            "declared predicate must never be a merge target",
        );
    }

    #[test]
    fn consolidation_ties_break_to_oldest_id() {
        // Two candidates at the SAME cosine (both identical to the query):
        // the lowest (oldest) id wins so re-ingests converge deterministically.
        let query = unit_at(0.0);
        let cands = vec![
            (PredicateId::from(5), unit_at(0.0), false),
            (PredicateId::from(2), unit_at(0.0), false),
            (PredicateId::from(9), unit_at(0.0), false),
        ];
        let (id, _) = select_consolidation_target(&query, &cands).expect("should merge");
        assert_eq!(id, PredicateId::from(2), "oldest id wins the tie");
    }

    #[test]
    fn consolidation_picks_highest_cosine_over_lower() {
        // A closer variant outranks a barely-qualifying one even if the latter
        // has a lower id — cosine dominates, id only breaks exact ties.
        let query = unit_at(0.0);
        let cands = vec![
            (PredicateId::from(1), unit_at(0.30), false), // cos ≈ 0.955
            (PredicateId::from(2), unit_at(0.05), false), // cos ≈ 0.9988
        ];
        let (id, _) = select_consolidation_target(&query, &cands).expect("should merge");
        assert_eq!(id, PredicateId::from(2), "closest cosine wins");
    }

    #[test]
    fn consolidation_cosine_defensive_on_degenerate_input() {
        assert_eq!(predicate_cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
        assert_eq!(predicate_cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    fn cand(
        id: u32,
        name: &str,
        vec: Vec<f32>,
        declared: bool,
    ) -> brain_metadata::schema::predicate::PredicateConsolidationCandidate {
        (PredicateId::from(id), name.to_string(), vec, declared)
    }

    #[test]
    fn candidate_predicates_orders_by_cosine() {
        // Query points along theta=0; the closest candidate must lead.
        let query = unit_at(0.0);
        let cands = vec![
            cand(1, "far", unit_at(1.2), false),   // low cosine
            cand(2, "near", unit_at(0.05), false), // high cosine
            cand(3, "mid", unit_at(0.6), false),   // middling
        ];
        let names = select_candidate_predicates(&query, &cands, 20);
        assert_eq!(names, vec!["near", "mid", "far"], "ranked by cosine desc");
    }

    #[test]
    fn candidate_predicates_caps_at_k() {
        let query = unit_at(0.0);
        // Ten qualifying candidates, all positive cosine.
        let cands: Vec<_> = (0..10)
            .map(|i| cand(i, &format!("p{i}"), unit_at(0.01 * i as f32), false))
            .collect();
        let names = select_candidate_predicates(&query, &cands, 3);
        assert_eq!(names.len(), 3, "K cap applied");
        // The three smallest angles (closest) are p0, p1, p2.
        assert_eq!(names, vec!["p0", "p1", "p2"]);
    }

    #[test]
    fn candidate_predicates_excludes_behavior_prefixed() {
        let query = unit_at(0.0);
        let cands = vec![
            cand(1, "behavior_tone", unit_at(0.0), false), // perfect cosine but excluded
            cand(2, "works_at", unit_at(0.1), false),
        ];
        let names = select_candidate_predicates(&query, &cands, 20);
        assert_eq!(
            names,
            vec!["works_at"],
            "behavior_ sinks are never reuse candidates",
        );
    }

    #[test]
    fn candidate_predicates_includes_declared_and_open_vocab() {
        // Both a schema-declared and an open-vocab predicate are valid reuse
        // targets — is_declared must NOT filter here (unlike consolidation).
        let query = unit_at(0.0);
        let cands = vec![
            cand(1, "declared_pred", unit_at(0.02), true),
            cand(2, "open_pred", unit_at(0.05), false),
        ];
        let names = select_candidate_predicates(&query, &cands, 20);
        assert_eq!(names, vec!["declared_pred", "open_pred"]);
    }

    #[test]
    fn candidate_predicates_empty_on_no_candidates() {
        let query = unit_at(0.0);
        assert!(select_candidate_predicates(&query, &[], 20).is_empty());
    }

    #[test]
    fn candidate_predicates_drops_degenerate_cosine() {
        // A zero-norm candidate scores 0.0 and is dropped, not ranked.
        let query = unit_at(0.0);
        let cands = vec![
            cand(1, "zero", vec![0.0, 0.0, 0.0], false),
            cand(2, "real", unit_at(0.1), false),
        ];
        let names = select_candidate_predicates(&query, &cands, 20);
        assert_eq!(names, vec!["real"]);
    }

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
            pattern_audit: None,
            classifier_audit: None,
            llm_audit: None,
            failure_reason: None,
            llm_failure_class: ExtractionFailureClass::Unclassified,
            llm_cost_micro_usd: 0,
        }
    }

    /// Two extractors in one tier: the first succeeds (and yields an item),
    /// the second skips on its where-clause. The tier byte must record the
    /// real outcome (RAN), not be clobbered to SKIPPED by the later extractor,
    /// and the successful extractor's items must be merged (not lost).
    #[test]
    fn fold_two_extractors_one_tier_success_then_skip_keeps_success() {
        use brain_core::ExtractorKind;
        let mem_id = MemoryId::pack(0, 1, 0);
        let mut slot = outcome(
            tier_status::ABSENT,
            tier_status::ABSENT,
            tier_status::ABSENT,
        );

        let item = ExtractedItem::EntityMention(EntityMention {
            entity_type_qname: "brain:Person".into(),
            text: "Alice".into(),
            start: 0,
            end: 5,
            confidence: 0.9,
            extractor_id: 1,
            extractor_version: 1,
        });
        // Extractor A: success with one item.
        fold_tier_result(
            &mut slot,
            ExtractionResult::success(vec![item], 0, 0),
            ExtractorKind::Pattern,
            mem_id,
            1,
            1,
        );
        assert_eq!(slot.pattern, tier_status::RAN);

        // Extractor B in the SAME tier: filtered out by its where-clause.
        fold_tier_result(
            &mut slot,
            ExtractionResult::skipped(ExtractionStatus::SkippedFilter, "where-clause", 0),
            ExtractorKind::Pattern,
            mem_id,
            1,
            1,
        );

        assert_eq!(
            slot.pattern,
            tier_status::RAN,
            "a later SKIP must not clobber an earlier real outcome"
        );
        assert_eq!(
            slot.items.len(),
            1,
            "the successful extractor's item is preserved"
        );
    }

    /// The reverse order (skip first, success second) must also land on RAN.
    #[test]
    fn fold_two_extractors_one_tier_skip_then_success_keeps_success() {
        use brain_core::ExtractorKind;
        let mem_id = MemoryId::pack(0, 1, 0);
        let mut slot = outcome(
            tier_status::ABSENT,
            tier_status::ABSENT,
            tier_status::ABSENT,
        );

        fold_tier_result(
            &mut slot,
            ExtractionResult::skipped(ExtractionStatus::SkippedFilter, "where-clause", 0),
            ExtractorKind::Classifier,
            mem_id,
            1,
            1,
        );
        assert_eq!(slot.classifier, tier_status::SKIPPED);

        fold_tier_result(
            &mut slot,
            ExtractionResult::success(Vec::new(), 0, 0),
            ExtractorKind::Classifier,
            mem_id,
            1,
            1,
        );
        assert_eq!(
            slot.classifier,
            tier_status::RAN,
            "a real outcome upgrades a prior SKIP"
        );
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
        MemoryId as TestMemoryId, MemoryKind, Salience, SessionId as TestSessionId,
        SpaceId as TestSpaceId,
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
            space: TestSpaceId::new(),
            session_id: TestSessionId(0),
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

        let mem_namespaces = vec!["brain".to_string(); mems.len()];
        let outcomes = futures_lite::future::block_on(run_pipeline_batch(
            vec![classifier as Arc<dyn Extractor>],
            &mems,
            &mem_namespaces,
            false,
            None,
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
        // The LLM double is declared under namespace `stub` (its qname is
        // `stub:llm`); the memory must be owned by `stub` for the
        // namespace-scoped selection to route to it.
        let mem_namespaces = vec!["stub".to_string(); mems.len()];

        let _ = futures_lite::future::block_on(run_pipeline_batch(
            vec![llm, classifier], // intentional reverse order — pipeline must reorder.
            &mems,
            &mem_namespaces,
            false,
            None,
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

    #[test]
    fn llm_selection_replaces_system_with_own_namespace_extractor() {
        use std::collections::HashSet;
        // Two namespaces declare their own LLM extractor; `globex` doesn't.
        let owning: HashSet<&str> = ["brain", "acme"].into_iter().collect();

        // A memory in `acme` runs ONLY acme's extractor — the system default
        // is suppressed (kills the double cost).
        assert!(llm_extractor_effective_for("acme", "acme", &owning));
        assert!(!llm_extractor_effective_for("brain", "acme", &owning));

        // A memory in `globex` (no own extractor) falls back to the system
        // `brain` default; a user extractor from another namespace never
        // runs over it (no cross-tenant over-run).
        assert!(llm_extractor_effective_for("brain", "globex", &owning));
        assert!(!llm_extractor_effective_for("acme", "globex", &owning));

        // A memory in `brain` (the system namespace) runs the system default.
        assert!(llm_extractor_effective_for("brain", "brain", &owning));
        assert!(!llm_extractor_effective_for("acme", "brain", &owning));
    }

    /// End-to-end routing through `run_llm_tier_into`: a mixed-namespace
    /// batch must send each memory only to the LLM extractor(s) effective
    /// for its namespace — the system default runs solely over memories
    /// whose namespace has no own LLM extractor.
    #[test]
    fn run_llm_tier_routes_each_memory_to_its_namespace_extractor() {
        use brain_core::ExtractorId as TestExtractorId;
        use brain_core::ExtractorKind;
        use brain_extractors::{ExtractionFuture, ExtractionResult, Extractor as TestExtractor};
        use std::sync::Mutex as StdMutex;

        struct RecordingNsLlm {
            id: TestExtractorId,
            name: String,
            seen: Arc<StdMutex<Vec<u128>>>,
        }
        impl TestExtractor for RecordingNsLlm {
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
                _ctx: &'a ExtractionContext<'a>,
                mem: &'a CoreMemory,
            ) -> ExtractionFuture<'a> {
                let seen = self.seen.clone();
                let id = mem.id.raw();
                Box::pin(async move {
                    seen.lock().unwrap().push(id);
                    ExtractionResult::success(Vec::new(), 0, 0)
                })
            }
        }

        let sys_seen = Arc::new(StdMutex::new(Vec::new()));
        let acme_seen = Arc::new(StdMutex::new(Vec::new()));
        let sys: Arc<dyn Extractor> = Arc::new(RecordingNsLlm {
            id: TestExtractorId::from(1),
            name: "brain:llm".into(),
            seen: sys_seen.clone(),
        });
        let acme: Arc<dyn Extractor> = Arc::new(RecordingNsLlm {
            id: TestExtractorId::from(2),
            name: "acme:llm".into(),
            seen: acme_seen.clone(),
        });

        // m0 owned by acme (own extractor → suppress system),
        // m1 by brain (system), m2 by globex (no own → system default).
        let mems = vec![
            make_mem(10, "acme memory"),
            make_mem(11, "brain memory"),
            make_mem(12, "globex memory"),
        ];
        let mem_namespaces = vec![
            "acme".to_string(),
            "brain".to_string(),
            "globex".to_string(),
        ];

        let empty_reg = ExtractorRegistry::new();
        let ctx = ExtractionContext {
            schema_version: 1,
            now_unix_nanos: 0,
            registry: &empty_reg,
            prior_tier_items: None,
            extractor_context: None,
            declared_entity_types: None,
            candidate_predicates: None,
            declared_kinds: None,
            entity_type_labels: None,
        };
        let mut outcomes: Vec<PipelineOutcome> = (0..mems.len())
            .map(|_| PipelineOutcome {
                items: Vec::new(),
                pattern: tier_status::ABSENT,
                classifier: tier_status::ABSENT,
                llm: tier_status::ABSENT,
                pattern_audit: None,
                classifier_audit: None,
                llm_audit: None,
                failure_reason: None,
                llm_failure_class: ExtractionFailureClass::Unclassified,
                llm_cost_micro_usd: 0,
            })
            .collect();

        futures_lite::future::block_on(run_llm_tier_into(
            &[sys, acme],
            &ctx,
            &mems,
            &mem_namespaces,
            &mut outcomes,
        ));

        let m0 = mems[0].id.raw();
        let m1 = mems[1].id.raw();
        let m2 = mems[2].id.raw();

        let mut acme_ran = acme_seen.lock().unwrap().clone();
        acme_ran.sort_unstable();
        assert_eq!(
            acme_ran,
            vec![m0],
            "acme extractor runs over its own memory only"
        );

        let mut sys_ran = sys_seen.lock().unwrap().clone();
        sys_ran.sort_unstable();
        assert_eq!(
            sys_ran,
            vec![m1, m2],
            "system default covers brain + the ownerless globex memory, and is suppressed for acme",
        );

        // Every memory's LLM tier byte records a run.
        for o in &outcomes {
            assert_eq!(o.llm, tier_status::RAN);
        }
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
            pattern_audit: None,
            classifier_audit: None,
            llm_audit: None,
            failure_reason: None,
            llm_failure_class: ExtractionFailureClass::Unclassified,
            llm_cost_micro_usd: 0,
        };

        let memory_id = MemoryId::pack(0, 1, 1);
        __seed_memory_row(&metadata, memory_id, __ts());
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
        let priya = entity_lookup_by_canonical_name(&rtxn, __ts(), EntityType::PERSON_ID, "Priya")
            .unwrap()
            .expect("Priya created during apply pass 1");

        // (a) The reports_to StatementMention is written as a *statement* on the
        // coined reports_to predicate — axis-faithful, not flipped to a relation.
        let stmts = statement_list(
            &rtxn,
            __ts(),
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
            __ts(),
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
            __ts(),
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
            __ts(),
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
            brain_metadata::entity_resolve_canonical_all_types(&rtxn, __ts(), "Melanie's kids")
                .unwrap();
        assert_eq!(
            kids.len(),
            1,
            "coined subject 'Melanie's kids' should be minted"
        );
        let kids_stmts = statement_list(
            &rtxn,
            __ts(),
            &StatementListFilter {
                subject: Some(kids[0]),
                predicate: Some(likes_id),
                ..StatementListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(kids_stmts.len(), 1, "the coined-subject fact must persist");
        assert!(
            brain_metadata::entity_resolve_canonical_all_types(&rtxn, __ts(), "they")
                .unwrap()
                .is_empty(),
            "pronoun subject 'they' must not be minted"
        );
    }

    /// Accuracy + domain/language generality through `apply_outcome`:
    /// - a non-ASCII predicate persists (open-vocab, no per-memory abort),
    /// - one un-anchorable triple (pronoun subject) is skipped + counted while
    ///   every other item in the SAME memory still lands (per-item skip),
    /// - an entity-subject Event with no timestamp persists AS an Event
    ///   (event_at = None) — the reader answers "when" from the memory time,
    ///   so a dateless action keeps its Time slot instead of being flattened,
    ///   (requires the data model to accept an Event with no event_at),
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
                // Entity-subject Event with no event time — must persist AS an
                // Event (event_at = None), keeping its Time slot for the read-path
                // memory-time fallback.
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
            pattern_audit: None,
            classifier_audit: None,
            llm_audit: None,
            failure_reason: None,
            llm_failure_class: ExtractionFailureClass::Unclassified,
            llm_cost_micro_usd: 0,
        };

        let memory_id = MemoryId::pack(0, 1, 1);
        __seed_memory_row(&metadata, memory_id, __ts());
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
        let liming = entity_lookup_by_canonical_name(&rtxn, __ts(), EntityType::PERSON_ID, "李明")
            .unwrap()
            .expect("李明 minted in pass 1");

        // Non-ASCII predicate statement persisted.
        let zuo = statement_list(
            &rtxn,
            __ts(),
            &StatementListFilter {
                subject: Some(liming),
                predicate: Some(zuoyongyu),
                ..StatementListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(zuo.len(), 1, "non-ASCII predicate fact must persist");

        // Event-with-no-timestamp persisted AS an Event, event_at = None.
        let trav = statement_list(
            &rtxn,
            __ts(),
            &StatementListFilter {
                subject: Some(liming),
                predicate: Some(traveled_to),
                ..StatementListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(trav.len(), 1, "dateless Event must persist, not drop");
        assert_eq!(
            trav[0].kind,
            StatementKind::Event,
            "entity-subject action stays an Event even without a timestamp"
        );
        assert_eq!(
            trav[0].event_at_unix_nanos, None,
            "no distinct date resolved; the reader supplies the memory-time fallback"
        );

        // Object-only relation endpoint minted; relation persisted.
        assert!(
            !brain_metadata::entity_resolve_canonical_all_types(&rtxn, __ts(), "Sam")
                .unwrap()
                .is_empty(),
            "object-only relation endpoint 'Sam' must be minted"
        );
        let rels = relation_list_from(
            &rtxn,
            __ts(),
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
            brain_metadata::entity_resolve_canonical_all_types(&rtxn, __ts(), "they")
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
    ///   carrying `event_at`; one WITHOUT a time stays an Event with
    ///   `event_at = None` (answered via the read-path memory-time fallback).
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
                // Event WITHOUT a time → stays an Event (event_at = None),
                // keeping its Time slot for the read-path memory-time fallback.
                stmt("brain:visited", "Berlin", true, StatementKind::Event, None),
                // Reified time slot: the tier emitted this fact as a plain Fact
                // but resolved an event date for it. The entity statement must
                // OWN that time — promoted to Event carrying event_at, not
                // orphaned to a memory-subject occurred_at row.
                stmt(
                    "brain:ran",
                    "charity race",
                    false,
                    StatementKind::Fact,
                    Some(T),
                ),
            ],
            pattern: tier_status::ABSENT,
            classifier: tier_status::ABSENT,
            llm: tier_status::RAN,
            pattern_audit: None,
            classifier_audit: None,
            llm_audit: None,
            failure_reason: None,
            llm_failure_class: ExtractionFailureClass::Unclassified,
            llm_cost_micro_usd: 0,
        };

        let memory_id = MemoryId::pack(0, 1, 1);
        __seed_memory_row(&metadata, memory_id, __ts());
        futures_lite::future::block_on(apply_outcome(&worker, &ctx, memory_id, &outcome))
            .expect("apply_outcome");

        let (traveled_to, favorite_color, visited, ran) = {
            let wtxn = metadata.write_txn().unwrap();
            let a = predicate_intern_or_get(&wtxn, "brain", "traveled_to", 0, 0).unwrap();
            let b = predicate_intern_or_get(&wtxn, "brain", "favorite_color", 0, 0).unwrap();
            let c = predicate_intern_or_get(&wtxn, "brain", "visited", 0, 0).unwrap();
            let d = predicate_intern_or_get(&wtxn, "brain", "ran", 0, 0).unwrap();
            wtxn.commit().unwrap();
            (a, b, c, d)
        };
        let rtxn = metadata.read_txn().unwrap();
        let alice = entity_lookup_by_canonical_name(&rtxn, __ts(), EntityType::PERSON_ID, "Alice")
            .unwrap()
            .expect("Alice minted");
        let one = |pred| {
            let v = statement_list(
                &rtxn,
                __ts(),
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
            !brain_metadata::entity_resolve_canonical_all_types(&rtxn, __ts(), "Tokyo")
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
            brain_metadata::entity_resolve_canonical_all_types(&rtxn, __ts(), "blue")
                .unwrap()
                .is_empty(),
            "literal 'blue' must NOT be minted as an entity"
        );

        // #2: an Event with no time stays an Event (event_at = None) and still
        // persists (and its flagged entity object is still minted/linked).
        let vis = one(visited);
        assert_eq!(
            vis.kind,
            StatementKind::Event,
            "timeless action stays an Event, keeping its Time slot"
        );
        assert_eq!(vis.event_at_unix_nanos, None);
        assert!(
            matches!(vis.object, StatementObject::Entity(_)),
            "flagged entity object minted for the dateless Event"
        );

        // #2 (reified time slot): a Fact-kinded mention that carries a resolved
        // event date is promoted to an Event so the entity fact OWNS its time.
        let ran = one(ran);
        assert_eq!(
            ran.kind,
            StatementKind::Event,
            "fact with a resolved date promoted to Event"
        );
        assert_eq!(
            ran.event_at_unix_nanos,
            Some(T),
            "event_at stamped on the entity statement itself"
        );
    }

    /// Whole date pipeline for a natural-language `Month YYYY` date, driven by
    /// the REAL pattern-tier temporal extractor rather than a hand-written
    /// timestamp: "Diego joined the billing team as a senior engineer in
    /// January 2026." must land `event_at = 2026-01-01` on the persisted
    /// `Diego --brain:joined--> billing team` statement.
    ///
    /// Three stages have to agree for that to happen and each has failed
    /// independently before, so this test exercises all of them end to end:
    /// the temporal extractor recognising `Month YYYY` at all, the apply pass's
    /// pre-scan electing it as the memory's sole event date (the anchor day is
    /// the ingest day, a different day, so it must NOT be excluded), and the
    /// date->fact join stamping it on the entity statement that carries no
    /// per-statement time of its own.
    #[test]
    #[allow(clippy::arc_with_non_send_sync)]
    fn apply_stamps_pattern_resolved_month_year_on_entity_statement() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        use brain_core::{
            EntityType, Memory, MemoryId, MemoryKind, Salience, SessionId, SpaceId, StatementKind,
        };
        use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
        use brain_index::{IndexParams, SharedHnsw};
        use brain_metadata::entity::ops::entity_lookup_by_canonical_name;
        use brain_metadata::schema::predicate::predicate_intern_or_get;
        use brain_metadata::statement::{statement_list, StatementListFilter};
        use brain_metadata::MetadataDb;
        use brain_ops::RealWriterHandle;
        use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};

        use brain_extractors::{
            ExtractionContext, Extractor, ExtractorRegistry, StatementMention, TemporalExtractor,
        };

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
                [0xDE; 16]
            }
        }

        const TEXT: &str = "Diego joined the billing team as a senior engineer in January 2026.";
        /// 2026-01-01T00:00:00Z — what "January 2026" must resolve to.
        const JAN_2026: u64 = 1_767_225_600_000_000_000;
        /// 2026-07-20T00:00:00Z — the ingest ANCHOR (a different day, so the
        /// resolved date is a genuine event time, not the message date).
        const INGEST: u64 = 1_784_505_600_000_000_000;

        // --- Stage 1: the real pattern tier on the real sentence. -----------
        let registry = ExtractorRegistry::new();
        let memory = Memory {
            id: MemoryId::pack(0, 1, 1),
            space: SpaceId::new(),
            session_id: SessionId(0),
            kind: MemoryKind::Episodic,
            salience: Salience::default(),
            text: Some(TEXT.to_string()),
            // The console sends no client `occurred_at`; the anchor is the
            // server write time.
            created_at_unix_ms: INGEST / 1_000_000,
            last_accessed_at_unix_ms: 0,
            occurred_at_unix_nanos: None,
        };
        let ectx = ExtractionContext {
            schema_version: 1,
            now_unix_nanos: INGEST,
            registry: &registry,
            prior_tier_items: None,
            extractor_context: None,
            declared_entity_types: None,
            candidate_predicates: None,
            declared_kinds: None,
            entity_type_labels: None,
        };
        let temporal_items =
            futures_lite::future::block_on(TemporalExtractor::new().run(&ectx, &memory)).items;
        assert_eq!(
            temporal_items.len(),
            1,
            "temporal tier must resolve exactly one date from {TEXT:?}, got {temporal_items:?}"
        );
        let ExtractedItem::StatementMention(temporal) = &temporal_items[0] else {
            panic!("temporal tier must emit a StatementMention, got {temporal_items:?}");
        };
        assert!(
            temporal.subject_is_memory,
            "the temporal mention is memory-anchored"
        );
        assert_eq!(
            temporal.event_at_unix_nanos,
            Some(JAN_2026),
            "\"January 2026\" must resolve to 2026-01-01T00:00:00Z"
        );

        // --- Stage 2+3: apply the whole memory's extraction. ----------------
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

        let mention = |text: &str, ty: &str| {
            ExtractedItem::EntityMention(EntityMention {
                entity_type_qname: ty.into(),
                text: text.into(),
                start: 0,
                end: text.chars().count(),
                confidence: 0.95,
                extractor_id: 2,
                extractor_version: 1,
            })
        };
        // What the LLM tier emits for this sentence: an Event triple with NO
        // per-statement time (verified against a live extraction). The date has
        // to reach it through the memory-date join, not from the mention.
        let joined = ExtractedItem::StatementMention(StatementMention {
            kind: statement_kind_to_byte(StatementKind::Event),
            subject_text: Some("Diego".into()),
            subject_is_memory: false,
            predicate_qname: "brain:joined".into(),
            object_text: Some("billing team".into()),
            confidence: 0.9,
            extractor_id: 3,
            extractor_version: 1,
            is_stateful: false,
            object_is_entity: true,
            event_at_unix_nanos: None,
            subject_is_self: false,
            retract: false,
        });
        let mut items = vec![
            mention("Diego", "brain:Person"),
            mention("billing team", "brain:Organization"),
            joined,
        ];
        items.extend(temporal_items.iter().cloned());
        let outcome = PipelineOutcome {
            items,
            pattern: tier_status::RAN,
            classifier: tier_status::RAN,
            llm: tier_status::RAN,
            pattern_audit: None,
            classifier_audit: None,
            llm_audit: None,
            failure_reason: None,
            llm_failure_class: ExtractionFailureClass::Unclassified,
            llm_cost_micro_usd: 0,
        };

        let memory_id = MemoryId::pack(0, 1, 1);
        __seed_memory_row_occurred_at(&metadata, memory_id, __ts(), INGEST);
        futures_lite::future::block_on(apply_outcome(&worker, &ctx, memory_id, &outcome))
            .expect("apply_outcome");

        let joined_pid = {
            let wtxn = metadata.write_txn().unwrap();
            let p = predicate_intern_or_get(&wtxn, "brain", "joined", 0, 0).unwrap();
            wtxn.commit().unwrap();
            p
        };
        let rtxn = metadata.read_txn().unwrap();
        let diego = entity_lookup_by_canonical_name(&rtxn, __ts(), EntityType::PERSON_ID, "Diego")
            .unwrap()
            .expect("Diego minted");
        let stmts = statement_list(
            &rtxn,
            __ts(),
            &StatementListFilter {
                subject: Some(diego),
                predicate: Some(joined_pid),
                ..StatementListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(stmts.len(), 1, "the joined statement must persist");
        assert_eq!(
            stmts[0].kind,
            StatementKind::Event,
            "a statement carrying a resolved date is an Event"
        );
        assert_eq!(
            stmts[0].event_at_unix_nanos,
            Some(JAN_2026),
            "the memory's sole resolved date must be stamped on the joined statement"
        );
    }

    /// Both halves of "a date is a TIME, not a thing", asserted on one input:
    /// "Diego joined the billing team in January 2026." must persist the
    /// `brain:joined` statement with `event_at = 2026-01-01` AND mint no
    /// entity node for the date.
    ///
    /// The two properties are one mechanism seen from two ends, and each has
    /// broken on its own: dropping the date's entity mention is what keeps the
    /// node out, while the memory-date join is what puts the timestamp on the
    /// fact — a change that suppresses the date early enough to lose the join
    /// buys the first property with the second. So this feeds apply the
    /// unguarded shapes a tier can still produce (a typed `January 2026`
    /// entity mention AND a statement whose OBJECT is the date surface, which
    /// no tier-level projection guard covers because it isn't an entity
    /// mention) alongside the real temporal mention, and requires both
    /// properties to hold simultaneously.
    #[test]
    #[allow(clippy::arc_with_non_send_sync)]
    fn apply_date_is_stamped_as_event_time_and_never_minted_as_an_entity() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        use brain_core::{
            EntityType, Memory, MemoryId, MemoryKind, Salience, SessionId, SpaceId, StatementKind,
        };
        use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
        use brain_index::{IndexParams, SharedHnsw};
        use brain_metadata::entity::ops::entity_lookup_by_canonical_name;
        use brain_metadata::schema::predicate::predicate_intern_or_get;
        use brain_metadata::statement::{statement_list, StatementListFilter};
        use brain_metadata::tables::entity::ENTITIES_TABLE;
        use brain_metadata::MetadataDb;
        use brain_ops::RealWriterHandle;
        use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
        use redb::ReadableTable;

        use brain_extractors::{
            ExtractionContext, Extractor, ExtractorRegistry, StatementMention, TemporalExtractor,
        };

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
                [0xDE; 16]
            }
        }

        const TEXT: &str = "Diego joined the billing team in January 2026.";
        const DATE_SURFACE: &str = "January 2026";
        /// 2026-01-01T00:00:00Z.
        const JAN_2026: u64 = 1_767_225_600_000_000_000;
        /// 2026-07-20T00:00:00Z — the ingest anchor, a different day.
        const INGEST: u64 = 1_784_505_600_000_000_000;

        // The real pattern tier resolves the date off the real sentence.
        let registry = ExtractorRegistry::new();
        let memory = Memory {
            id: MemoryId::pack(0, 1, 1),
            space: SpaceId::new(),
            session_id: SessionId(0),
            kind: MemoryKind::Episodic,
            salience: Salience::default(),
            text: Some(TEXT.to_string()),
            created_at_unix_ms: INGEST / 1_000_000,
            last_accessed_at_unix_ms: 0,
            occurred_at_unix_nanos: None,
        };
        let ectx = ExtractionContext {
            schema_version: 1,
            now_unix_nanos: INGEST,
            registry: &registry,
            prior_tier_items: None,
            extractor_context: None,
            declared_entity_types: None,
            candidate_predicates: None,
            declared_kinds: None,
            entity_type_labels: None,
        };
        let temporal_items =
            futures_lite::future::block_on(TemporalExtractor::new().run(&ectx, &memory)).items;

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

        let mention = |text: &str, ty: &str| {
            ExtractedItem::EntityMention(EntityMention {
                entity_type_qname: ty.into(),
                text: text.into(),
                start: 0,
                end: text.chars().count(),
                confidence: 0.95,
                extractor_id: 2,
                extractor_version: 1,
            })
        };
        let stmt = |predicate: &str, object: &str, object_is_entity: bool| {
            ExtractedItem::StatementMention(StatementMention {
                kind: statement_kind_to_byte(StatementKind::Event),
                subject_text: Some("Diego".into()),
                subject_is_memory: false,
                predicate_qname: predicate.into(),
                object_text: Some(object.into()),
                confidence: 0.9,
                extractor_id: 3,
                extractor_version: 1,
                is_stateful: false,
                object_is_entity,
                event_at_unix_nanos: None,
                subject_is_self: false,
                retract: false,
            })
        };
        let mut items = vec![
            mention("Diego", "brain:Person"),
            mention("billing team", "brain:Organization"),
            // A tier that failed to suppress the date surface files it as a
            // typed entity mention …
            mention(DATE_SURFACE, "brain:Event"),
            stmt("brain:joined", "billing team", true),
            // … and as a statement object, the shape no tier-level entity
            // projection guard sees at all.
            stmt("brain:joined_on", DATE_SURFACE, true),
        ];
        items.extend(temporal_items.iter().cloned());
        let outcome = PipelineOutcome {
            items,
            pattern: tier_status::RAN,
            classifier: tier_status::RAN,
            llm: tier_status::RAN,
            pattern_audit: None,
            classifier_audit: None,
            llm_audit: None,
            failure_reason: None,
            llm_failure_class: ExtractionFailureClass::Unclassified,
            llm_cost_micro_usd: 0,
        };

        let memory_id = MemoryId::pack(0, 1, 1);
        __seed_memory_row_occurred_at(&metadata, memory_id, __ts(), INGEST);
        futures_lite::future::block_on(apply_outcome(&worker, &ctx, memory_id, &outcome))
            .expect("apply_outcome");

        let joined_pid = {
            let wtxn = metadata.write_txn().unwrap();
            let p = predicate_intern_or_get(&wtxn, "brain", "joined", 0, 0).unwrap();
            wtxn.commit().unwrap();
            p
        };
        let rtxn = metadata.read_txn().unwrap();

        // Property 1 — the date is nowhere in the entity table, under any
        // type. Scanned rather than looked up by (type, name): the point is
        // that NO node names the date, whichever type a tier proposed.
        let entities = rtxn.open_table(ENTITIES_TABLE).unwrap();
        let named: Vec<String> = entities
            .iter()
            .unwrap()
            .flatten()
            .map(|(_, v)| v.value().canonical_name)
            .collect();
        let date_key = brain_metadata::normalize_name(DATE_SURFACE);
        assert!(
            !named
                .iter()
                .any(|n| brain_metadata::normalize_name(n) == date_key),
            "{DATE_SURFACE:?} must not be an entity node; entities present: {named:?}"
        );

        // Property 2 — the same date IS the statement's event time.
        let diego = entity_lookup_by_canonical_name(&rtxn, __ts(), EntityType::PERSON_ID, "Diego")
            .unwrap()
            .expect("Diego minted");
        let stmts = statement_list(
            &rtxn,
            __ts(),
            &StatementListFilter {
                subject: Some(diego),
                predicate: Some(joined_pid),
                ..StatementListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(stmts.len(), 1, "the joined statement must persist");
        assert_eq!(
            stmts[0].event_at_unix_nanos,
            Some(JAN_2026),
            "the date must reach the statement as its event time"
        );
    }

    /// A `StatementMention` with `object_text: None` (an LLM response that
    /// slipped past schema validation with the `"object"` key missing) must
    /// be dropped, not persisted as a fabricated empty-string `Value` —
    /// `resolve_statement_object` returning `None` is the defense-in-depth
    /// backstop for exactly this case (core invariant: never persist data
    /// that wasn't actually captured).
    #[test]
    #[allow(clippy::arc_with_non_send_sync)]
    fn apply_object_missing_drops_not_fabricates() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        use brain_core::{EntityType, MemoryId, StatementKind};
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
                [0xAB; 16]
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

        let outcome = PipelineOutcome {
            items: vec![
                ExtractedItem::EntityMention(EntityMention {
                    entity_type_qname: "brain:Person".into(),
                    text: "Priya".into(),
                    start: 0,
                    end: 5,
                    confidence: 0.95,
                    extractor_id: 2,
                    extractor_version: 1,
                }),
                // The LLM emitted subject + predicate but no "object" key at
                // all — the exact malformed shape schema validation should
                // normally catch upstream; this exercises the backstop.
                ExtractedItem::StatementMention(StatementMention {
                    kind: statement_kind_to_byte(StatementKind::Fact),
                    subject_text: Some("Priya".into()),
                    subject_is_memory: false,
                    predicate_qname: "brain:manages".into(),
                    object_text: None,
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
            pattern_audit: None,
            classifier_audit: None,
            llm_audit: None,
            failure_reason: None,
            llm_failure_class: ExtractionFailureClass::Unclassified,
            llm_cost_micro_usd: 0,
        };

        let memory_id = MemoryId::pack(0, 1, 1);
        __seed_memory_row(&metadata, memory_id, __ts());
        futures_lite::future::block_on(apply_outcome(&worker, &ctx, memory_id, &outcome))
            .expect("apply_outcome");

        let manages = {
            let wtxn = metadata.write_txn().unwrap();
            let pid = predicate_intern_or_get(&wtxn, "brain", "manages", 0, 0).unwrap();
            wtxn.commit().unwrap();
            pid
        };
        let rtxn = metadata.read_txn().unwrap();
        let priya = entity_lookup_by_canonical_name(&rtxn, __ts(), EntityType::PERSON_ID, "Priya")
            .unwrap()
            .expect("Priya minted from the entity mention");

        // No statement was persisted for the object-missing mention — in
        // particular, never a fabricated `Value(Text(""))`.
        let stmts = statement_list(
            &rtxn,
            __ts(),
            &StatementListFilter {
                subject: Some(priya),
                predicate: Some(manages),
                ..StatementListFilter::default()
            },
        )
        .unwrap();
        assert!(
            stmts.is_empty(),
            "object-missing mention must not persist any statement, got {stmts:?}"
        );

        // The drop is observable, not silent.
        let dropped = worker.metrics().snapshot().apply_dropped_total;
        assert!(
            dropped.get("object_missing").copied().unwrap_or(0) >= 1,
            "the object-missing mention must be counted, not silent: {dropped:?}"
        );
    }

    /// An entity that only ever surfaces as a statement's entity object or a
    /// relation endpoint is minted by the endpoint resolver — and the source
    /// memory must also record that it MENTIONS it. Every reader that
    /// enumerates a memory's entities does so by walking `Mentions` (graph
    /// enrichment, the encode write-trace artifacts), so without that edge the
    /// minted node is invisible and an edge pointing at it renders against a
    /// node nobody listed. Also pins the dedup: one edge per entity, and none
    /// re-written for an entity pass 1 already linked.
    #[test]
    fn apply_minted_endpoint_gets_a_mention_edge() {
        use brain_core::{EdgeKindRef, EntityType, MemoryId, NodeRef, StatementKind};
        use brain_metadata::entity::ops::entity_lookup_by_canonical_name;
        use brain_metadata::tables::edge::walk_outgoing;

        let (worker, ctx, metadata) = __join_env();
        let memory_id = MemoryId::pack(0, 1, 1);
        __seed_memory_row(&metadata, memory_id, __ts());

        let outcome = __outcome(vec![
            // The only surface any tier filed as an entity mention.
            __alice(),
            // Entity object never surfaced as a mention → minted inside
            // `resolve_statement_object` -> `resolve_relation_endpoint`.
            __entity_stmt(
                "brain:manages",
                "billing platform team",
                true,
                StatementKind::Fact,
                None,
            ),
            // Relation endpoint never surfaced either → minted by
            // `resolve_relation_endpoint` on the direct relation path.
            ExtractedItem::RelationMention(brain_extractors::RelationMention {
                relation_type_qname: "brain:works_at".into(),
                subject_text: "Alice".into(),
                object_text: "Stripe".into(),
                confidence: 0.9,
                extractor_id: 3,
                extractor_version: 1,
            }),
        ]);

        futures_lite::future::block_on(apply_outcome(&worker, &ctx, memory_id, &outcome))
            .expect("apply_outcome");

        let rtxn = metadata.read_txn().unwrap();
        let alice = entity_lookup_by_canonical_name(&rtxn, __ts(), EntityType::PERSON_ID, "Alice")
            .unwrap()
            .expect("Alice minted from the entity mention");
        let minted = |name: &str| {
            let hits = brain_metadata::entity_resolve_canonical_all_types(&rtxn, __ts(), name)
                .unwrap_or_default();
            assert_eq!(hits.len(), 1, "'{name}' must be minted exactly once");
            hits[0]
        };
        let team = minted("billing platform team");
        let stripe = minted("Stripe");

        let mention_targets: Vec<brain_core::EntityId> = walk_outgoing(
            &rtxn,
            NodeRef::Memory(memory_id),
            Some(EdgeKindRef::Mentions),
        )
        .unwrap()
        .into_iter()
        .filter_map(|(_, to, _, _)| match to {
            NodeRef::Entity(e) => Some(e),
            _ => None,
        })
        .collect();

        for (id, name) in [
            (alice, "Alice"),
            (team, "billing platform team"),
            (stripe, "Stripe"),
        ] {
            assert!(
                mention_targets.contains(&id),
                "'{name}' must be reachable from the memory via a Mentions edge, got {mention_targets:?}"
            );
        }
        // Exactly three: one per distinct entity, no duplicate for "Alice"
        // (already linked by pass 1, then reused as the relation's `from`).
        assert_eq!(
            mention_targets.len(),
            3,
            "one Mentions edge per distinct entity, got {mention_targets:?}"
        );
    }

    /// The sibling of the endpoint case: a COINED SUBJECT — a subject surface
    /// no tier filed as an entity mention ("Melanie's kids") — is minted so the
    /// fact persists, and must likewise be recorded as mentioned by the source
    /// memory. Otherwise the statement's own `from` end names an entity absent
    /// from the memory's entity list, the exact shape that renders as a
    /// zero id.
    #[test]
    fn apply_coined_subject_gets_a_mention_edge() {
        use brain_core::{EdgeKindRef, EntityType, MemoryId, NodeRef, StatementKind};
        use brain_metadata::entity::ops::entity_lookup_by_canonical_name;
        use brain_metadata::tables::edge::walk_outgoing;

        let (worker, ctx, metadata) = __join_env();
        let memory_id = MemoryId::pack(0, 1, 1);
        __seed_memory_row(&metadata, memory_id, __ts());

        let outcome = __outcome(vec![
            // The one surface a tier filed as an entity mention.
            __alice(),
            // Subject never surfaced as a mention → minted by
            // `resolve_statement_subject`. Value object, so the OBJECT axis
            // mints nothing: this pins the subject path on its own.
            ExtractedItem::StatementMention(brain_extractors::StatementMention {
                kind: statement_kind_to_byte(StatementKind::Preference),
                subject_text: Some("Melanie's kids".into()),
                subject_is_memory: false,
                predicate_qname: "brain:likes".into(),
                object_text: Some("ice cream".into()),
                confidence: 0.9,
                extractor_id: 3,
                extractor_version: 1,
                is_stateful: false,
                object_is_entity: false,
                event_at_unix_nanos: None,
                subject_is_self: false,
                retract: false,
            }),
        ]);

        futures_lite::future::block_on(apply_outcome(&worker, &ctx, memory_id, &outcome))
            .expect("apply_outcome");

        let rtxn = metadata.read_txn().unwrap();
        let alice = entity_lookup_by_canonical_name(&rtxn, __ts(), EntityType::PERSON_ID, "Alice")
            .unwrap()
            .expect("Alice minted from the entity mention");
        let kids =
            brain_metadata::entity_resolve_canonical_all_types(&rtxn, __ts(), "Melanie's kids")
                .unwrap_or_default();
        assert_eq!(kids.len(), 1, "the coined subject must be minted once");

        let mention_targets: Vec<brain_core::EntityId> = walk_outgoing(
            &rtxn,
            NodeRef::Memory(memory_id),
            Some(EdgeKindRef::Mentions),
        )
        .unwrap()
        .into_iter()
        .filter_map(|(_, to, _, _)| match to {
            NodeRef::Entity(e) => Some(e),
            _ => None,
        })
        .collect();

        assert!(
            mention_targets.contains(&kids[0]),
            "the coined subject must be reachable from the memory via a Mentions edge, got {mention_targets:?}"
        );
        assert!(
            mention_targets.contains(&alice),
            "pass 1's edge must remain"
        );
        // No literal-object entity, no duplicates.
        assert_eq!(
            mention_targets.len(),
            2,
            "one Mentions edge per distinct entity, got {mention_targets:?}"
        );
    }

    /// The mention's own entity-vs-value decision outranks an incidental
    /// `entity_map` hit. The classifier tier tags noun-phrase spans liberally —
    /// "senior engineer" in "Diego joined the billing team as a senior engineer"
    /// comes back as a Person span and gets minted in pass 1 — while the LLM
    /// deliberately emits the role as a VALUE (`object_is_entity: false`) under
    /// constrained decoding. Resolving the object against `entity_map` first let
    /// that incidental mint win, so a literal role bound as an entity object and
    /// the fact became a link between two "people".
    ///
    /// The entity itself is NOT unminted: the span really was mentioned, so it
    /// keeps its `Mentions` edge and stays a queryable node future writes can
    /// attach to — it is "mentioned but not asserted about", not dangling.
    #[test]
    fn apply_value_object_beats_a_prior_entity_mention_of_the_same_surface() {
        use brain_core::{
            EdgeKindRef, EntityType, MemoryId, NodeRef, StatementKind, StatementObject,
            StatementValue,
        };
        use brain_metadata::entity::ops::entity_lookup_by_canonical_name;
        use brain_metadata::schema::predicate::predicate_intern_or_get;
        use brain_metadata::statement::{statement_list, StatementListFilter};
        use brain_metadata::tables::edge::walk_outgoing;

        let (worker, ctx, metadata) = __join_env();
        let memory_id = MemoryId::pack(0, 1, 1);
        __seed_memory_row(&metadata, memory_id, __ts());

        let outcome = __outcome(vec![
            __alice(),
            // The classifier's span for the role — minted as an entity in pass 1,
            // which puts "senior engineer" into `entity_map`.
            ExtractedItem::EntityMention(EntityMention {
                entity_type_qname: "brain:Person".into(),
                text: "senior engineer".into(),
                start: 0,
                end: 15,
                confidence: 0.9,
                extractor_id: 2,
                extractor_version: 1,
            }),
            // The LLM's explicit decision: the role is a VALUE, not a thing.
            __entity_stmt(
                "brain:role",
                "senior engineer",
                false,
                StatementKind::Fact,
                None,
            ),
        ]);

        futures_lite::future::block_on(apply_outcome(&worker, &ctx, memory_id, &outcome))
            .expect("apply_outcome");

        let role = {
            let wtxn = metadata.write_txn().unwrap();
            let p = predicate_intern_or_get(&wtxn, "brain", "role", 0, 0).unwrap();
            wtxn.commit().unwrap();
            p
        };
        let rtxn = metadata.read_txn().unwrap();
        let alice = entity_lookup_by_canonical_name(&rtxn, __ts(), EntityType::PERSON_ID, "Alice")
            .unwrap()
            .expect("Alice minted");
        let rows = statement_list(
            &rtxn,
            __ts(),
            &StatementListFilter {
                subject: Some(alice),
                predicate: Some(role),
                ..StatementListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(rows.len(), 1, "exactly one role statement");
        assert!(
            matches!(
                &rows[0].object,
                StatementObject::Value(StatementValue::Text(t)) if t == "senior engineer"
            ),
            "an explicit value object must stay a Value even when an earlier tier \
             minted an entity for the same surface, got {:?}",
            rows[0].object
        );

        // The pass-1 entity survives and keeps its mention edge: the span WAS
        // mentioned; nothing is asserted about it by this write.
        let role_entity =
            brain_metadata::entity_resolve_canonical_all_types(&rtxn, __ts(), "senior engineer")
                .unwrap();
        assert_eq!(role_entity.len(), 1, "the classifier's span stays minted");
        let mention_targets: Vec<brain_core::EntityId> = walk_outgoing(
            &rtxn,
            NodeRef::Memory(memory_id),
            Some(EdgeKindRef::Mentions),
        )
        .unwrap()
        .into_iter()
        .filter_map(|(_, to, _, _)| match to {
            NodeRef::Entity(e) => Some(e),
            _ => None,
        })
        .collect();
        assert!(
            mention_targets.contains(&role_entity[0]),
            "the minted span keeps its Mentions edge, so it is mentioned-but-unasserted \
             rather than dangling, got {mention_targets:?}"
        );
    }

    // ----- Deterministic apply-time date->fact join (Change 2). -----

    /// Build an isolated apply environment (worker + ctx + metadata) with a
    /// zero-vector embedder. Returns the pieces the join tests drive.
    #[cfg(test)]
    #[allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send by design
    fn __join_env() -> (
        ExtractorWorker,
        crate::context::WorkerContext,
        brain_planner::SharedMetadataDb,
    ) {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
        use brain_index::{IndexParams, SharedHnsw};
        use brain_metadata::MetadataDb;
        use brain_ops::RealWriterHandle;
        use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};

        use crate::context::WorkerContext;

        struct JoinDispatcher;
        impl Dispatcher for JoinDispatcher {
            fn embed(&self, _t: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
                Ok([0.0; VECTOR_DIM])
            }
            fn embed_batch(&self, t: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
                Ok(vec![[0.0; VECTOR_DIM]; t.len()])
            }
            fn fingerprint(&self) -> [u8; 16] {
                [0xEF; 16]
            }
        }

        let tempdir = tempfile::tempdir().unwrap();
        let metadata: SharedMetadataDb =
            Arc::new(MetadataDb::open(tempdir.path().join("md.redb")).unwrap());
        let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
        let executor = ExecutorContext::new(
            Arc::new(JoinDispatcher) as Arc<dyn Dispatcher>,
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
        // Keep the on-disk temp dir alive past this helper's return: the redb
        // Database holds it open, so the tests read/write it after we return.
        // A test-only, bounded leak of one temp dir per test.
        std::mem::forget(tempdir);
        (ExtractorWorker::new(rx), ctx, metadata)
    }

    /// The "Alice" entity mention, so the subject resolves to a typed Person
    /// entity the tests can look up by canonical name.
    #[cfg(test)]
    fn __alice() -> ExtractedItem {
        ExtractedItem::EntityMention(EntityMention {
            entity_type_qname: "brain:Person".into(),
            text: "Alice".into(),
            start: 0,
            end: 5,
            confidence: 0.95,
            extractor_id: 2,
            extractor_version: 1,
        })
    }

    /// An entity-subject statement mention ("Alice --pred--> object").
    #[cfg(test)]
    fn __entity_stmt(
        predicate: &str,
        object: &str,
        object_is_entity: bool,
        kind: StatementKind,
        event_at: Option<u64>,
    ) -> ExtractedItem {
        ExtractedItem::StatementMention(brain_extractors::StatementMention {
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
    }

    /// A memory-subject `occurred_at` mention carrying a resolved date — what
    /// the temporal extractor emits for a memory's date. Both `object_text`
    /// (decimal nanos) and `event_at_unix_nanos` are set, as in production.
    #[cfg(test)]
    fn __memory_date(date_nanos: u64) -> ExtractedItem {
        ExtractedItem::StatementMention(brain_extractors::StatementMention {
            kind: statement_kind_to_byte(StatementKind::Event),
            subject_text: None,
            subject_is_memory: true,
            predicate_qname: "brain:occurred_at".into(),
            object_text: Some(date_nanos.to_string()),
            confidence: 0.6,
            extractor_id: 4,
            extractor_version: 1,
            is_stateful: false,
            object_is_entity: false,
            event_at_unix_nanos: Some(date_nanos),
            subject_is_self: false,
            retract: false,
        })
    }

    #[cfg(test)]
    fn __outcome(items: Vec<ExtractedItem>) -> PipelineOutcome {
        PipelineOutcome {
            items,
            pattern: tier_status::RAN,
            classifier: tier_status::ABSENT,
            llm: tier_status::RAN,
            pattern_audit: None,
            classifier_audit: None,
            llm_audit: None,
            failure_reason: None,
            llm_failure_class: ExtractionFailureClass::Unclassified,
            llm_cost_micro_usd: 0,
        }
    }

    /// Apply `outcome` for a fresh memory and return the single statement
    /// stored under `brain:<name>` for the minted "Alice" entity (or panic).
    #[cfg(test)]
    fn __apply_and_get(
        worker: &ExtractorWorker,
        ctx: &crate::context::WorkerContext,
        metadata: &brain_planner::SharedMetadataDb,
        memory_id: brain_core::MemoryId,
        outcome: &PipelineOutcome,
        name: &str,
    ) -> brain_core::Statement {
        use brain_core::EntityType;
        use brain_metadata::entity::ops::entity_lookup_by_canonical_name;
        use brain_metadata::schema::predicate::predicate_intern_or_get;
        use brain_metadata::statement::{statement_list, StatementListFilter};

        __seed_memory_row(metadata, memory_id, __ts());
        futures_lite::future::block_on(apply_outcome(worker, ctx, memory_id, outcome))
            .expect("apply_outcome");

        let pid = {
            let wtxn = metadata.write_txn().unwrap();
            let p = predicate_intern_or_get(&wtxn, "brain", name, 0, 0).unwrap();
            wtxn.commit().unwrap();
            p
        };
        let rtxn = metadata.read_txn().unwrap();
        let alice = entity_lookup_by_canonical_name(&rtxn, __ts(), EntityType::PERSON_ID, "Alice")
            .unwrap()
            .expect("Alice minted");
        let v = statement_list(
            &rtxn,
            __ts(),
            &StatementListFilter {
                subject: Some(alice),
                predicate: Some(pid),
                ..StatementListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(
            v.len(),
            1,
            "expected exactly one statement for brain:{name}"
        );
        v.into_iter().next().unwrap()
    }

    /// Per-call extraction audit: applying an outcome writes one
    /// `ExtractionAudit` row per tier that ran (absent tiers write none),
    /// each reachable through the by-memory / by-extractor / by-time
    /// indexes, and a re-apply ADDS rows rather than overwriting (history
    /// preserved — the whole point of the append-only UUIDv7 log).
    #[test]
    fn apply_emits_per_call_extraction_audit_rows() {
        use brain_metadata::{audit_by_extractor, audit_by_memory, audit_recent};

        let (worker, ctx, metadata) = __join_env();
        let memory_id = brain_core::MemoryId::pack(0, 7, 1);
        __seed_memory_row(&metadata, memory_id, __ts());

        // A pattern tier and an LLM tier both ran, attributed to distinct
        // extractor ids; the classifier tier is absent (no extractor).
        let mut outcome = __outcome(vec![
            __alice(),
            __entity_stmt("brain:likes", "coffee", false, StatementKind::Fact, None),
        ]);
        outcome.pattern_audit = Some(TierAudit {
            extractor_id: 11,
            extractor_version: 1,
            status: extraction_status::SUCCESS,
            reason: String::new(),
        });
        outcome.classifier_audit = None;
        outcome.llm_audit = Some(TierAudit {
            extractor_id: 22,
            extractor_version: 3,
            status: extraction_status::SUCCESS,
            reason: String::new(),
        });
        outcome.llm_cost_micro_usd = 500;

        futures_lite::future::block_on(apply_outcome(&worker, &ctx, memory_id, &outcome))
            .expect("apply_outcome");

        {
            let rtxn = metadata.read_txn().unwrap();
            // Two tiers ran → two rows, reachable by-memory.
            let by_mem = audit_by_memory(&rtxn, memory_id, 100).unwrap();
            assert_eq!(by_mem.len(), 2, "one audit row per tier that ran");
            // by-extractor isolates each tier's extractor.
            assert_eq!(audit_by_extractor(&rtxn, 11, 100).unwrap().len(), 1);
            let llm_rows = audit_by_extractor(&rtxn, 22, 100).unwrap();
            assert_eq!(llm_rows.len(), 1);
            assert_eq!(llm_rows[0].cost_micro_usd, 500, "LLM cost attributed");
            // The absent classifier tier wrote nothing.
            assert!(audit_by_extractor(&rtxn, 33, 100).unwrap().is_empty());
            // by-time returns both.
            assert_eq!(audit_recent(&rtxn, 0, 100).unwrap().len(), 2);
        }

        // A SECOND apply ADDS new rows (append-only history), never overwrites.
        futures_lite::future::block_on(apply_outcome(&worker, &ctx, memory_id, &outcome))
            .expect("apply_outcome re-run");
        {
            let rtxn = metadata.read_txn().unwrap();
            assert_eq!(
                audit_by_memory(&rtxn, memory_id, 100).unwrap().len(),
                4,
                "re-extraction ADDS rows; never overwrites",
            );
        }
    }

    /// A failed tier records a Failure row carrying its reason; a
    /// budget-skipped LLM tier records a SkippedBudget row.
    #[test]
    fn apply_emits_failure_and_skip_audit_rows() {
        use brain_metadata::audit_by_extractor;

        let (worker, ctx, metadata) = __join_env();
        let memory_id = brain_core::MemoryId::pack(0, 8, 1);
        __seed_memory_row(&metadata, memory_id, __ts());

        let mut outcome = __outcome(vec![__alice()]);
        outcome.pattern_audit = Some(TierAudit {
            extractor_id: 44,
            extractor_version: 1,
            status: extraction_status::FAILURE,
            reason: "boom".to_string(),
        });
        outcome.classifier_audit = None;
        outcome.llm_audit = Some(TierAudit {
            extractor_id: 55,
            extractor_version: 1,
            status: extraction_status::SKIPPED_BUDGET,
            reason: "cycle LLM budget exhausted".to_string(),
        });

        futures_lite::future::block_on(apply_outcome(&worker, &ctx, memory_id, &outcome))
            .expect("apply_outcome");

        let rtxn = metadata.read_txn().unwrap();
        let failed = audit_by_extractor(&rtxn, 44, 10).unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].status, extraction_status::FAILURE);
        assert_eq!(failed[0].status_reason, "boom");
        let skipped = audit_by_extractor(&rtxn, 55, 10).unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].status, extraction_status::SKIPPED_BUDGET);
    }

    const D1: u64 = 1_684_540_800_000_000_000; // 2023-05-20
    const D2: u64 = 1_579_046_400_000_000_000; // 2020-01-15

    /// Single resolved date + one Event fact (no per-statement time): the date
    /// is joined onto the entity fact's Time slot FOR SURE. The charity-race
    /// case — the LLM emitted an Event but missed "last Saturday", so the
    /// deterministic join fills it.
    #[test]
    fn join_single_date_stamps_event_fact() {
        let (worker, ctx, metadata) = __join_env();
        let outcome = __outcome(vec![
            __alice(),
            __memory_date(D1),
            __entity_stmt(
                "brain:ran",
                "charity race",
                false,
                StatementKind::Event,
                None,
            ),
        ]);
        let ran = __apply_and_get(
            &worker,
            &ctx,
            &metadata,
            brain_core::MemoryId::pack(0, 1, 1),
            &outcome,
            "ran",
        );
        assert_eq!(ran.kind, StatementKind::Event, "stays Event via the join");
        assert_eq!(
            ran.event_at_unix_nanos,
            Some(D1),
            "memory's sole date joined onto the Event fact",
        );
    }

    /// Single date + an Event fact AND an atemporal fact: only the Event gets
    /// the date; the atemporal statement's Time slot stays empty.
    #[test]
    fn join_single_date_excludes_atemporal_fact() {
        let (worker, ctx, metadata) = __join_env();
        let memory_id = brain_core::MemoryId::pack(0, 2, 1);
        let outcome = __outcome(vec![
            __alice(),
            __memory_date(D1),
            __entity_stmt(
                "brain:ran",
                "charity race",
                false,
                StatementKind::Event,
                None,
            ),
            // A plain Fact-kinded statement (LLM did not classify it as Event):
            // never stamped, because the join is gated to Event-kind.
            __entity_stmt(
                "brain:favorite_color",
                "blue",
                false,
                StatementKind::Fact,
                None,
            ),
        ]);
        __seed_memory_row(&metadata, memory_id, __ts());
        futures_lite::future::block_on(apply_outcome(&worker, &ctx, memory_id, &outcome))
            .expect("apply_outcome");

        use brain_core::EntityType;
        use brain_metadata::entity::ops::entity_lookup_by_canonical_name;
        use brain_metadata::schema::predicate::predicate_intern_or_get;
        use brain_metadata::statement::{statement_list, StatementListFilter};
        let (ran_pid, color_pid) = {
            let wtxn = metadata.write_txn().unwrap();
            let a = predicate_intern_or_get(&wtxn, "brain", "ran", 0, 0).unwrap();
            let b = predicate_intern_or_get(&wtxn, "brain", "favorite_color", 0, 0).unwrap();
            wtxn.commit().unwrap();
            (a, b)
        };
        let rtxn = metadata.read_txn().unwrap();
        let alice = entity_lookup_by_canonical_name(&rtxn, __ts(), EntityType::PERSON_ID, "Alice")
            .unwrap()
            .expect("Alice minted");
        let one = |pid| {
            statement_list(
                &rtxn,
                __ts(),
                &StatementListFilter {
                    subject: Some(alice),
                    predicate: Some(pid),
                    ..StatementListFilter::default()
                },
            )
            .unwrap()
            .into_iter()
            .next()
            .expect("statement present")
        };
        let ran = one(ran_pid);
        let color = one(color_pid);
        assert_eq!(ran.event_at_unix_nanos, Some(D1), "Event fact stamped");
        assert_eq!(
            color.event_at_unix_nanos, None,
            "atemporal Fact never stamped with the memory date",
        );
        assert_eq!(color.kind, StatementKind::Fact, "atemporal fact stays Fact");
    }

    /// Two DISTINCT dates in one memory: ambiguous which date pairs with which
    /// fact, so the deterministic join stamps nothing (leaves the Time slots to
    /// the LLM's per-statement event_at). Assert no wrong stamping.
    #[test]
    fn join_multiple_dates_skips() {
        let (worker, ctx, metadata) = __join_env();
        let outcome = __outcome(vec![
            __alice(),
            __memory_date(D1),
            __memory_date(D2),
            __entity_stmt(
                "brain:ran",
                "charity race",
                false,
                StatementKind::Event,
                None,
            ),
        ]);
        let ran = __apply_and_get(
            &worker,
            &ctx,
            &metadata,
            brain_core::MemoryId::pack(0, 3, 1),
            &outcome,
            "ran",
        );
        // Event with no resolvable time stays an Event (event_at = None) rather
        // than being stamped with an arbitrary one of the two ambiguous dates —
        // the reader answers "when" from the memory's own time.
        assert_eq!(
            ran.event_at_unix_nanos, None,
            "multi-date memory must not stamp a guessed date",
        );
        assert_eq!(
            ran.kind,
            StatementKind::Event,
            "action stays an Event even when no date can be joined",
        );
    }

    /// The LLM already set `event_at` on the fact: the deterministic join
    /// never overrides it, even when the memory carries a single (different)
    /// date.
    #[test]
    fn join_never_overrides_llm_event_at() {
        let (worker, ctx, metadata) = __join_env();
        let outcome = __outcome(vec![
            __alice(),
            __memory_date(D1),
            __entity_stmt(
                "brain:ran",
                "charity race",
                false,
                StatementKind::Event,
                Some(D2),
            ),
        ]);
        let ran = __apply_and_get(
            &worker,
            &ctx,
            &metadata,
            brain_core::MemoryId::pack(0, 4, 1),
            &outcome,
            "ran",
        );
        assert_eq!(
            ran.event_at_unix_nanos,
            Some(D2),
            "the LLM-set event_at is preserved, not overridden by the join",
        );
    }

    /// No resolved date: the Event simply gets no distinct time and stays an
    /// Event (event_at = None). The expected, correct outcome for a dateless
    /// memory — no guessing; the reader supplies the memory-time fallback.
    #[test]
    fn join_no_date_leaves_event_timeless() {
        let (worker, ctx, metadata) = __join_env();
        let outcome = __outcome(vec![
            __alice(),
            __entity_stmt(
                "brain:ran",
                "charity race",
                false,
                StatementKind::Event,
                None,
            ),
        ]);
        let ran = __apply_and_get(
            &worker,
            &ctx,
            &metadata,
            brain_core::MemoryId::pack(0, 5, 1),
            &outcome,
            "ran",
        );
        assert_eq!(ran.event_at_unix_nanos, None, "no date to join");
        assert_eq!(
            ran.kind,
            StatementKind::Event,
            "timeless action stays an Event",
        );
    }

    // 2023-05-25 00:00 UTC (5 days after D1) + a 13:14 wall-clock offset — the
    // memory's MESSAGE time. The temporal extractor resolves this same day at
    // midnight, so the anchor exclusion must compare at DAY granularity.
    const ANCHOR_MIDNIGHT: u64 = 1_684_972_800_000_000_000;
    const ANCHOR_OCCURRED: u64 = ANCHOR_MIDNIGHT + 47_640_000_000_000;

    /// Apply an outcome for a memory with an explicit `occurred_at`, then return
    /// the single statement stored under `brain:ran` for the minted "Alice".
    #[cfg(test)]
    fn __apply_ran_with_anchor(
        worker: &ExtractorWorker,
        ctx: &crate::context::WorkerContext,
        metadata: &brain_planner::SharedMetadataDb,
        memory_id: brain_core::MemoryId,
        occurred_at: u64,
        items: Vec<ExtractedItem>,
    ) -> brain_core::Statement {
        use brain_core::EntityType;
        use brain_metadata::entity::ops::entity_lookup_by_canonical_name;
        use brain_metadata::schema::predicate::predicate_intern_or_get;
        use brain_metadata::statement::{statement_list, StatementListFilter};

        __seed_memory_row_occurred_at(metadata, memory_id, __ts(), occurred_at);
        let outcome = __outcome(items);
        futures_lite::future::block_on(apply_outcome(worker, ctx, memory_id, &outcome))
            .expect("apply_outcome");
        let pid = {
            let wtxn = metadata.write_txn().unwrap();
            let p = predicate_intern_or_get(&wtxn, "brain", "ran", 0, 0).unwrap();
            wtxn.commit().unwrap();
            p
        };
        let rtxn = metadata.read_txn().unwrap();
        let alice = entity_lookup_by_canonical_name(&rtxn, __ts(), EntityType::PERSON_ID, "Alice")
            .unwrap()
            .expect("Alice minted");
        let v = statement_list(
            &rtxn,
            __ts(),
            &StatementListFilter {
                subject: Some(alice),
                predicate: Some(pid),
                ..StatementListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(v.len(), 1, "expected exactly one brain:ran statement");
        v.into_iter().next().unwrap()
    }

    /// Anchor exclusion: two resolved dates, one equal to the memory's message
    /// day (the anchor) and one distinct. The anchor date is dropped — it is
    /// already on the memory record — so the pre-scan sees exactly ONE genuine
    /// event date and stamps it on the Event fact. (The "last Saturday" case.)
    #[test]
    fn join_anchor_equal_date_excluded_distinct_stamped() {
        let (worker, ctx, metadata) = __join_env();
        let ran = __apply_ran_with_anchor(
            &worker,
            &ctx,
            &metadata,
            brain_core::MemoryId::pack(0, 6, 1),
            ANCHOR_OCCURRED,
            vec![
                __alice(),
                // The message day itself (midnight) — must be excluded.
                __memory_date(ANCHOR_MIDNIGHT),
                // "last Saturday" — the genuine, distinct event date.
                __memory_date(D1),
                __entity_stmt(
                    "brain:ran",
                    "charity race",
                    false,
                    StatementKind::Event,
                    None,
                ),
            ],
        );
        assert_eq!(ran.kind, StatementKind::Event, "stays Event via the join");
        assert_eq!(
            ran.event_at_unix_nanos,
            Some(D1),
            "the distinct event date (not the excluded anchor) is stamped",
        );
    }

    /// Same-day event: the memory's only resolved date equals the anchor day, so
    /// after exclusion there is NO distinct event date. The Event carries no
    /// `event_at` but stays an Event; the read falls back to the memory's own
    /// occurred_at for "when".
    #[test]
    fn join_same_day_event_leaves_event_timeless() {
        let (worker, ctx, metadata) = __join_env();
        let ran = __apply_ran_with_anchor(
            &worker,
            &ctx,
            &metadata,
            brain_core::MemoryId::pack(0, 7, 1),
            ANCHOR_OCCURRED,
            vec![
                __alice(),
                // Only the message day is resolved ("ran a race today").
                __memory_date(ANCHOR_MIDNIGHT),
                __entity_stmt(
                    "brain:ran",
                    "charity race",
                    false,
                    StatementKind::Event,
                    None,
                ),
            ],
        );
        assert_eq!(
            ran.event_at_unix_nanos, None,
            "same-day event carries no distinct event_at",
        );
        assert_eq!(
            ran.kind,
            StatementKind::Event,
            "same-day action stays an Event",
        );
    }

    /// After excluding the anchor day, two genuinely-distinct event dates remain
    /// — still ambiguous, so the join stamps nothing.
    #[test]
    fn join_two_distinct_after_anchor_exclusion_skips() {
        let (worker, ctx, metadata) = __join_env();
        let ran = __apply_ran_with_anchor(
            &worker,
            &ctx,
            &metadata,
            brain_core::MemoryId::pack(0, 8, 1),
            ANCHOR_OCCURRED,
            vec![
                __alice(),
                __memory_date(ANCHOR_MIDNIGHT),
                __memory_date(D1),
                __memory_date(D2),
                __entity_stmt(
                    "brain:ran",
                    "charity race",
                    false,
                    StatementKind::Event,
                    None,
                ),
            ],
        );
        assert_eq!(
            ran.event_at_unix_nanos, None,
            "two distinct event dates remain ambiguous → no stamp",
        );
        assert_eq!(
            ran.kind,
            StatementKind::Event,
            "action stays an Event; ambiguous dates just aren't stamped",
        );
    }

    // ----- Extraction CoreMemory space threading + queue over-drain. -----

    /// Minimal worker fixture: a temp metadata db (seeds the brain: system
    /// schema) wired into an ops context, plus the metadata handle and the
    /// tempdir the metadata db lives in (kept alive by the caller).
    #[allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send by design
    fn __worker_fixture() -> (
        crate::context::WorkerContext,
        brain_planner::SharedMetadataDb,
        tempfile::TempDir,
    ) {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
        use brain_index::{IndexParams, SharedHnsw};
        use brain_metadata::MetadataDb;
        use brain_ops::RealWriterHandle;
        use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};

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
            writer as Arc<dyn WriterHandle>,
        );
        let ops = Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor));
        let ctx = WorkerContext {
            ops,
            shutdown: Arc::new(AtomicBool::new(false)),
        };
        (ctx, metadata, tempdir)
    }

    #[test]
    fn extraction_core_memory_carries_real_space_for_stable_cache_key() {
        use brain_core::{MemoryId, NamespaceId, SessionId, SpaceId};
        use std::sync::Arc;

        let (ctx, metadata, _tempdir) = __worker_fixture();

        // Seed a memory row owning a known, non-NIL space.
        let ns = NamespaceId::from(9);
        let space = SpaceId::derive_from_string("tenant9", "space-x");
        assert_ne!(space, SpaceId::NIL);
        let mid = MemoryId::pack(0, 42, 1);
        __seed_memory_row(&metadata, mid, brain_metadata::RowScope::new(ns, space));

        let live: Vec<(usize, MemoryId, Arc<str>)> = vec![(0, mid, Arc::from("hello world"))];
        let facts = load_row_facts(&ctx, &live);
        let mems = build_extraction_core_memories(&live, &facts);
        assert_eq!(mems.len(), 1);
        assert_eq!(
            mems[0].space, space,
            "CoreMemory must carry the memory's REAL space (the LLM cache key folds it), \
             not a freshly-minted per-call SpaceId"
        );
        assert_eq!(mems[0].session_id, SessionId(0));

        // The old bug minted a fresh SpaceId every call, so two builds of the
        // same id diverged and the response cache never hit across cycles.
        // With the fix the space is stable, so the cache-key input is stable.
        let mems2 = build_extraction_core_memories(&live, &facts);
        assert_eq!(
            mems[0].space, mems2[0].space,
            "the extraction CoreMemory space must be stable across cycles"
        );
        assert_eq!(
            <[u8; 16]>::from(mems[0].space),
            <[u8; 16]>::from(space),
            "the exact 16 bytes the LLM cache key folds must match the stored space"
        );
    }

    #[test]
    fn load_pending_batch_pages_past_backing_off_front_rows() {
        use brain_core::MemoryId;
        use brain_metadata::{
            extraction_queue_enqueue, failure_class, pipeline_record_extracted, pipeline_status,
            tier_status, ExtractorItemCounts, ExtractorPipelineAuditEntry,
        };

        let (ctx, metadata, _tempdir) = __worker_fixture();

        // shard=0 keeps `slot` in the high bytes, so the queue's MemoryId
        // byte order is slot order: the low-slot front rows come first, the
        // high-slot due row last.
        let front: Vec<MemoryId> = (1..=4).map(|s| MemoryId::pack(0, s, 1)).collect();
        let due_id = MemoryId::pack(0, 100, 1);

        let now = now_unix_nanos();
        {
            let wtxn = metadata.write_txn().unwrap();
            for id in &front {
                extraction_queue_enqueue(&wtxn, *id, now).unwrap();
                // A retryable transient LLM failure with a high attempt count:
                // its exponential backoff (capped at 1h) has NOT elapsed, so
                // the row is queued-but-not-due this cycle.
                let entry = ExtractorPipelineAuditEntry::new(
                    *id,
                    now,
                    pipeline_status::FAILURE,
                    String::new(),
                    tier_status::SKIPPED,
                    tier_status::SKIPPED,
                    tier_status::FAILED,
                    ExtractorItemCounts::zero(),
                    0,
                )
                .with_attempts(20)
                .with_failure_class(failure_class::TRANSIENT);
                pipeline_record_extracted(&wtxn, &entry).unwrap();
            }
            // A newer row deeper in the queue with no audit row → due now.
            extraction_queue_enqueue(&wtxn, due_id, now).unwrap();
            wtxn.commit().unwrap();
        }

        // `want` is smaller than the backing-off front window: draining exactly
        // `want` (the old behaviour) would return only front rows, filter them
        // all out, and make zero progress. The over-drain must page past them
        // and surface the due row this cycle.
        let due = load_pending_batch(&ctx, 2).unwrap();
        assert!(
            due.contains(&due_id),
            "a due row behind a backing-off front window must be found this cycle"
        );
        for id in &front {
            assert!(
                !due.contains(id),
                "backing-off front rows must not be returned as due"
            );
        }
    }
}

/// Live registry-refresh on SCHEMA_UPLOAD: a newly-declared extractor
/// must appear in the running registry after the worker consumes the
/// dirty flag — without a shard restart.
#[cfg(test)]
mod registry_refresh_tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
    use brain_extractors::{MaterializeDeps, TierGate};
    use brain_index::{IndexParams, SharedHnsw};
    use brain_metadata::MetadataDb;
    use brain_ops::RealWriterHandle;
    use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
    use brain_protocol::schema::{parse_schema, validate};

    use super::{maybe_rebuild_registry, ExtractorWorker};
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

    #[test]
    #[allow(clippy::arc_with_non_send_sync)]
    fn schema_upload_refreshes_registry_without_restart() {
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
            ops: ops.clone(),
            shutdown: Arc::new(AtomicBool::new(false)),
        };

        // Worker wired with rebuild deps (no classifier model / LLM router
        // needed for a pattern extractor).
        let (_tx, rx) = flume::unbounded();
        let worker = ExtractorWorker::new(rx)
            .with_registry_rebuild_deps(MaterializeDeps::default(), TierGate::all_enabled());

        // Boot-time registry is empty (nothing declared yet).
        assert_eq!(
            ctx.ops.extractor_registry.read().iter_enabled().count(),
            0,
            "registry starts empty",
        );

        // Persist a schema declaring a new pattern extractor, exactly as
        // SCHEMA_UPLOAD apply would, then flip the dirty flag the handler sets.
        let src = r#"
            namespace t
            define entity_type Person {
            }
            define extractor person_mentions {
                kind: pattern
                target: entity Person
                patterns [
                    /\b([A-Z][a-z]+)\b/
                ]
                confidence: 0.7
            }
        "#;
        let validated = validate(&parse_schema(src).expect("parse")).expect("validate");
        {
            let wtxn = metadata.write_txn().unwrap();
            brain_metadata::schema::store::schema_upload(&wtxn, &validated, 1_700_000_000_000)
                .expect("schema_upload");
            wtxn.commit().unwrap();
        }
        let expected_id = brain_metadata::extractor::ops::extractor_lookup_by_qname(
            &metadata.read_txn().unwrap(),
            "t",
            "person_mentions",
        )
        .unwrap()
        .expect("extractor row persisted")
        .id();
        ctx.ops.extractors_dirty.store(true, Ordering::Release);

        // Consume the dirty flag: the worker rebuilds the registry in place.
        maybe_rebuild_registry(&worker, &ctx);

        // The declared extractor is now live, and the dirty flag is cleared.
        let reg = ctx.ops.extractor_registry.read();
        assert!(
            reg.iter_enabled().any(|e| e.id() == expected_id),
            "the SCHEMA_UPLOAD-declared extractor must be in the live registry",
        );
        assert!(
            !ctx.ops.extractors_dirty.load(Ordering::Acquire),
            "dirty flag cleared once the rebuild consumed it",
        );
    }

    #[test]
    #[allow(clippy::arc_with_non_send_sync)]
    fn rebuild_is_noop_when_flag_unset() {
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
        let worker = ExtractorWorker::new(rx)
            .with_registry_rebuild_deps(MaterializeDeps::default(), TierGate::all_enabled());

        // Persist an extractor but leave the flag unset: rebuild must not run,
        // so the (empty) boot-time registry stays untouched.
        let src = r#"
            namespace t
            define entity_type Person {
            }
            define extractor person_mentions {
                kind: pattern
                target: entity Person
                patterns [
                    /\b([A-Z][a-z]+)\b/
                ]
                confidence: 0.7
            }
        "#;
        let validated = validate(&parse_schema(src).expect("parse")).expect("validate");
        {
            let wtxn = metadata.write_txn().unwrap();
            brain_metadata::schema::store::schema_upload(&wtxn, &validated, 1_700_000_000_000)
                .expect("schema_upload");
            wtxn.commit().unwrap();
        }

        maybe_rebuild_registry(&worker, &ctx);
        assert_eq!(
            ctx.ops.extractor_registry.read().iter_enabled().count(),
            0,
            "no rebuild without the dirty flag",
        );
    }
}
