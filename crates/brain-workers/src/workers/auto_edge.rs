//! AutoEdgeWorker — derives `SimilarTo` substrate edges from HNSW
//! k-nearest-neighbour searches after every successful ENCODE.
//!
//! ## Why this exists
//!
//! Before this worker landed, every substrate edge was client-supplied
//! (via `ENCODE_REQ.edges` or a separate `LINK`). The memory graph was
//! empty by default, which made the planner's memory-anchor graph
//! retriever (the retrieval recall) a no-op on any deployment that
//! didn't manually LINK things. AutoEdgeWorker fills that gap: the
//! substrate now produces a real graph clients can traverse without
//! lifting a finger.
//!
//! ## Flow
//!
//! 1. The writer's ENCODE handler pushes `(memory_id, vector)` into a
//!    per-shard `flume::Sender` after WAL fsync + redb commit + HNSW
//!    insert. The push is non-blocking; a full channel drops the
//!    enqueue with a tracing warn (encodes themselves never fail
//!    because of auto-edge backpressure).
//! 2. AutoEdgeWorker drains the receiver in bounded batches on its
//!    cycle interval (default 100 ms). For each drained memory it
//!    runs HNSW knn and collects `(source, neighbour, similarity)`
//!    triples whose cosine similarity clears the configured threshold.
//! 3. The worker batches the triples into a single
//!    `Write { phases: Vec<Phase::Link> }` and calls
//!    `RealWriterHandle::submit`. The unified write path appends a
//!    WAL record per Link, commits the redb rows, publishes the
//!    `EdgeAdded` envelope on the subscribe bus, and stamps idempotency
//!    via the BLAKE3-hashed sorted batch — retries of the same cycle
//!    collapse to the cached ack.
//! 4. `brain_metadata::tables::edge::link` (invoked from the apply
//!    layer) handles the symmetric mirror automatically (each logical
//!    pair = two physical forward rows + two reverse rows).
//!
//! ## Idempotency and FORGET
//!
//! Re-draining the same `MemoryId` is safe: `edge::link` overwrites
//! the existing `EdgeData`. The writer's HNSW already filters
//! tombstoned ids out of `search_active`, and we double-check via
//! `is_tombstoned(source)` so a memory FORGOTTEN between enqueue and
//! drain produces zero edges instead of pointing into a tombstoned
//! source.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use brain_core::{AgentId, ContextId, EdgeKind, EdgeKindRef, MemoryId, MemoryKind, NodeRef};
use brain_metadata::tables::edge::{
    derived_by, list_memory_edges_from, origin, zero_disambiguator,
};
use brain_ops::{
    AutoEdgeEnqueue, AutoEdgeMetrics, EventEnvelope, Phase, RealWriterHandle, Write, WriteId,
};
use brain_protocol::shared::enums::{
    EventType, StageAutoEdgePayload, StageKind, StageOutcome, StagePayload,
};
use futures_lite::FutureExt;
use glommio::timer::sleep;
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Knobs that don't fit `WorkerConfig`'s generic shape. Defaults match
/// the master plan's latency budget: ~0.5 ms HNSW query × 5
/// neighbours × 1000 encodes/sec ≈ 500 ms per second of background
/// work, fits in Brain's ≥2 reserved per-shard background lanes.
#[derive(Clone, Copy, Debug)]
pub struct AutoEdgeKnobs {
    /// How many nearest neighbours to fetch from HNSW per memory. The
    /// worker fetches `top_k + 1` (the +1 covers the self-hit, which
    /// the worker filters explicitly).
    pub top_k: usize,
    /// Minimum cosine similarity for a neighbour to receive an edge.
    /// Conservative default matches the master-plan analysis on a
    /// medium corpus; tune per workload.
    pub similarity_threshold: f32,
    /// Per-query HNSW `ef`. `None` uses index default
    /// (`IndexParams::ef_search`); the worker overrides because the
    /// `top_k + 1` fetch is small and the dedicated lane can afford a
    /// wider beam for better recall.
    pub ef_search: Option<usize>,
}

/// Knob defaults. Override via [`AutoEdgeWorker::with_knobs`] from
/// the server's config materialiser.
pub const DEFAULT_TOP_K: usize = 5;
/// Cosine similarity floor for auto-derived `SimilarTo` edges.
///
/// 0.85 is the classical "near-duplicate" floor — paraphrases of the
/// same sentence, or an agent restating itself. Below it, BGE-small
/// embeddings still score 0.75–0.80 for pairs that are merely
/// topically near but semantically distinct ("Priya works at Stripe"
/// vs. "Priya now works at OpenAI" — same entity, contradictory
/// fact); admitting those manufactures false `SimilarTo` edges and
/// turns popular memories into hub nodes that drown the graph
/// retriever. We keep the floor at the near-duplicate boundary so an
/// edge means "these say the same thing." Operators who want a looser
/// topical clustering lower it via `[workers.auto_edge]
/// similarity_threshold` in the server config.
pub const DEFAULT_AUTO_EDGE_SIMILARITY_THRESHOLD: f32 = 0.85;
pub const DEFAULT_EF_SEARCH: usize = 64;

/// Maximum cumulative `SimilarTo` out-edges any one memory may
/// accumulate across cycles. Each cycle adds at most `top_k`, but
/// nothing else bounds the total: a memory that keeps surfacing as a
/// near-neighbour would otherwise grow an unbounded fan-out and become
/// a hub node that the graph retriever has to expand on every walk,
/// inflating latency and burying the genuinely-relevant edges. 16
/// near-duplicate links is far more than any single memory needs to be
/// reachable; past that, more edges add cost without adding recall.
pub const MAX_SIMILAR_TO_OUT_DEGREE: usize = 16;

impl Default for AutoEdgeKnobs {
    fn default() -> Self {
        Self {
            top_k: DEFAULT_TOP_K,
            similarity_threshold: DEFAULT_AUTO_EDGE_SIMILARITY_THRESHOLD,
            ef_search: Some(DEFAULT_EF_SEARCH),
        }
    }
}

/// Per-shard worker. Owns the receiver end of the writer-fed channel
/// plus the knobs.
pub struct AutoEdgeWorker {
    config: WorkerConfig,
    knobs: AutoEdgeKnobs,
    queue: flume::Receiver<AutoEdgeEnqueue>,
    /// Shared with the writer's enqueue path; both sides bump the
    /// same atomics. Defaults to a fresh local instance when the
    /// scheduler doesn't wire one — keeps tests and fixtures with
    /// no metrics sink compiling without an extra setter call.
    metrics: Arc<AutoEdgeMetrics>,
}

impl AutoEdgeWorker {
    /// Wire up the worker. The matching `flume::Sender` must be
    /// installed on the writer via `RealWriterHandle::set_auto_edge_sender`
    /// before any ENCODE runs; otherwise the queue stays empty.
    #[must_use]
    pub fn new(queue: flume::Receiver<AutoEdgeEnqueue>) -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::AutoEdge),
            knobs: AutoEdgeKnobs::default(),
            queue,
            metrics: Arc::new(AutoEdgeMetrics::new()),
        }
    }

    /// Install the shared metric handle. Production calls this with
    /// the same `Arc<AutoEdgeMetrics>` it handed to
    /// `RealWriterHandle::set_auto_edge_metrics`; tests pass a fresh
    /// instance to assert on.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<AutoEdgeMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Read accessor — tests assert on counter state through this.
    #[must_use]
    pub fn metrics(&self) -> Arc<AutoEdgeMetrics> {
        self.metrics.clone()
    }

    /// Override the scheduler config (interval / batch_size /
    /// max_runtime / enabled). Tests use this to shorten the cycle;
    /// operators wire it from `[workers.auto_edge]` TOML.
    #[must_use]
    pub fn with_config(mut self, config: WorkerConfig) -> Self {
        self.config = config;
        self
    }

    /// Override the worker-specific knobs (top_k / threshold /
    /// ef_search). Server config maps the `[workers.auto_edge]` TOML
    /// fields here.
    #[must_use]
    pub fn with_knobs(mut self, knobs: AutoEdgeKnobs) -> Self {
        self.knobs = knobs;
        self
    }

    /// Read accessor for tests.
    #[must_use]
    pub fn knobs(&self) -> AutoEdgeKnobs {
        self.knobs
    }
}

impl Worker for AutoEdgeWorker {
    fn name(&self) -> &'static str {
        WorkerKind::AutoEdge.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::AutoEdge
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_auto_edge_cycle(self, ctx))
    }
}

async fn do_auto_edge_cycle(
    worker: &AutoEdgeWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    let cfg = worker.config.clone();
    if cfg.batch_size == 0 {
        return Ok(0);
    }
    let knobs = worker.knobs;
    let started = Instant::now();
    let index = ctx.ops.executor.index.clone();
    let metadata = ctx.ops.executor.metadata.clone();

    // Remaining `SimilarTo` out-edge budget per source for this cycle.
    // Seeded lazily from the persisted out-degree on first sight of a
    // source, then decremented as we queue edges — so a source drained
    // twice in one cycle doesn't blow past the cap by reading the same
    // stale persisted count both times.
    let mut similar_to_budget: HashMap<MemoryId, usize> = HashMap::new();

    // Read phase: drain up to `batch_size` (or the per-cycle wall-clock
    // budget) from the channel, run knn for each, collect link tuples.
    // We never `await` on the channel itself (try_recv only) — that
    // would block the entire scheduler if the queue empties.
    //
    // `drained_sources` tracks every memory we pulled from the queue —
    // including ones whose stage produced no edges — so we can publish
    // a `StageCompleted{AutoEdge}` event for each. Wait helpers depend
    // on per-source completion signals; skipping the "no edges" case
    // would hang the client.
    let mut to_link: Vec<(MemoryId, MemoryId, f32)> = Vec::new();
    let mut drained_sources: Vec<MemoryId> = Vec::new();
    let mut per_source_edges: HashMap<MemoryId, u32> = HashMap::new();
    let mut processed = 0usize;
    let mut neighbours_found = 0u64;
    while processed < cfg.batch_size {
        if started.elapsed() >= cfg.max_runtime {
            break;
        }
        if ctx.is_shutdown() {
            break;
        }
        // First iteration blocks on the queue (raced against the
        // tick interval) so an enqueue from the writer wakes the
        // cycle immediately — no per-interval latency floor.
        // Subsequent iterations drain without blocking so a burst
        // batches into one cycle.
        let item = if processed == 0 {
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
        let (source_id, vector) = item;
        processed += 1;
        drained_sources.push(source_id);

        // Source was FORGOTTEN between enqueue and now — skip it
        // entirely. We treat the source's tombstone state as
        // authoritative because the writer already pushed a vector
        // that's now dangling; HNSW search would still return
        // neighbours, but linking a tombstoned memory to anything
        // would only feed the edge_scrub queue.
        if index.is_tombstoned(source_id) {
            continue;
        }

        // Zero-vector guard. Until the real embedder lands (the
        // BGE-small CpuDispatcher), the stub dispatcher hands
        // every encode a [0; VECTOR_DIM] vector. Two such memories in
        // HNSW make cosine similarity compute 0/0 = NaN, which
        // contaminates the edge weight and crashes downstream consumers
        // that expect a finite f32. Skip the memory outright — there's
        // no useful similarity work to do when every vector is zero,
        // and silently writing NaN-weighted edges is worse than
        // refusing to write any.
        if vector.iter().all(|component| *component == 0.0) {
            continue;
        }

        // Seed this source's remaining out-edge budget from the count
        // already persisted in redb (first time we see it this cycle),
        // so the cap holds across cycles and not just within one. A read
        // failure is non-fatal: rather than drop the source's edges
        // entirely we fall back to a fresh budget — over-linking on a
        // transient metadata error is the lesser evil, and the
        // counter_reconcile worker keeps the persisted count honest.
        let budget = match similar_to_budget.entry(source_id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let existing = metadata
                    .read_txn()
                    .ok()
                    .and_then(|rtxn| {
                        list_memory_edges_from(&rtxn, source_id, Some(EdgeKind::SimilarTo)).ok()
                    })
                    .map_or(0, |edges| edges.len());
                e.insert(MAX_SIMILAR_TO_OUT_DEGREE.saturating_sub(existing))
            }
        };
        if *budget == 0 {
            continue;
        }

        // Over-fetch by one so the self-hit doesn't eat into the
        // requested k. HNSW's search_active already filters tombstones,
        // so per-neighbour is_tombstoned checks would be redundant.
        let fetch_k = knobs.top_k.saturating_add(1);
        let hits = index.search_active(&vector, fetch_k, knobs.ef_search);
        for (neighbour, similarity) in hits {
            if neighbour == source_id {
                continue;
            }
            // NaN/Inf filter. The threshold comparison alone won't
            // reject NaN because `NaN < threshold` is `false` per IEEE
            // 754, so an unguarded path would push NaN-weighted edges
            // into the link Phase. Belt-and-suspenders alongside the
            // zero-vector guard above: that guard covers the source
            // side, this one covers the neighbour-side path if HNSW
            // ever returns a non-finite score for any other reason.
            if !similarity.is_finite() {
                continue;
            }
            if similarity < knobs.similarity_threshold {
                continue;
            }
            // Stop once the source has spent its out-edge budget. HNSW
            // returns neighbours strongest-first, so the edges we keep
            // are the highest-similarity ones — the cap sheds the
            // weakest links, not arbitrary ones.
            if *budget == 0 {
                break;
            }
            to_link.push((source_id, neighbour, similarity));
            *per_source_edges.entry(source_id).or_insert(0) += 1;
            *budget -= 1;
            neighbours_found += 1;
        }

        // Cooperative yield every few drains so the scheduler stays
        // responsive when the queue is deep. Cheap when nothing else
        // is ready.
        if processed.is_multiple_of(16) {
            glommio::executor().yield_if_needed().await;
        }
    }

    // Write phase: one Write per cycle, one Phase::Link per derived
    // edge. submit() handles WAL append, redb commit, HNSW maint, and
    // the EdgeAdded event burst — same path as explicit LINK. Workers
    // that retry on transient errors collapse to the cached ack via
    // the BLAKE3-hashed batch (sorted source/target/kind tuples).
    let created_at = now_unix_nanos();
    let written = if to_link.is_empty() {
        0
    } else {
        let phases: Vec<Phase> = to_link
            .iter()
            .map(|(from, to, sim)| Phase::Link {
                from: NodeRef::Memory(*from),
                to: NodeRef::Memory(*to),
                kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
                weight: *sim,
                origin: origin::AUTO_DERIVED,
                derived_by: derived_by::SIMILARITY_WORKER,
                disambiguator: zero_disambiguator(),
                created_at_unix_nanos: created_at,
            })
            .collect();
        let request_hash = hash_link_batch(&to_link);
        let write = Write::from_phases(WriteId::new(), AgentId::default(), phases)
            .with_request_hash(request_hash);
        let real_writer = ctx
            .ops
            .executor
            .writer
            .as_any()
            .downcast_ref::<RealWriterHandle>()
            .ok_or_else(|| {
                WorkerError::Ops("auto_edge: unified path requires RealWriterHandle".into())
            })?;
        real_writer
            .submit(write)
            .await
            .map_err(|e| WorkerError::Ops(format!("submit: {e:?}")))?;
        to_link.len()
    };

    let elapsed = started.elapsed();
    worker.metrics.add_edges_written(written as u64);
    worker.metrics.observe_neighbours_found(neighbours_found);
    worker.metrics.observe_cycle_duration(elapsed.as_secs_f64());

    // Publish one `StageCompleted{AutoEdge}` per drained source — even
    // for sources that produced zero edges. Wait helpers tick the
    // pending-stage checklist off as these arrive; missing the
    // zero-edges case would leave the client blocked forever.
    let ts = now_unix_nanos();
    for source_id in drained_sources {
        let edges_written = per_source_edges.get(&source_id).copied().unwrap_or(0);
        let outcome = if edges_written > 0 {
            StageOutcome::Ok
        } else {
            StageOutcome::Empty
        };
        let envelope = EventEnvelope {
            lsn: 0,
            event_type: EventType::StageCompleted,
            memory_id: source_id,
            context_id: ContextId::default(),
            kind: MemoryKind::Episodic,
            salience: 0.0,
            timestamp_unix_nanos: ts,
            text: None,
            graph_payload: None,
            edge_payload: None,
            stage_kind: Some(StageKind::AutoEdge),
            stage_outcome: Some(outcome),
            stage_payload: Some(StagePayload::AutoEdge(StageAutoEdgePayload {
                edges_written,
            })),
            agent_id: AgentId::default(),
        };
        let _ = ctx.ops.events.publish(envelope);
    }

    trace!(
        drained = processed,
        edges_logical = written,
        cycle_ms = elapsed.as_millis() as u64,
        "auto_edge cycle",
    );
    Ok(processed)
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Deterministic hash of a batch of `(source, target, weight)` tuples.
/// Sorted by `(source, target)` first so two retries of the same batch
/// produce the same hash regardless of HNSW result ordering. Weight is
/// excluded because the similarity threshold makes the set of pairs the
/// invariant the writer's idempotency cache should key on — re-running
/// HNSW with the same threshold over the same memory must round-trip
/// to the same `request_hash`.
fn hash_link_batch(pairs: &[(MemoryId, MemoryId, f32)]) -> [u8; 32] {
    let mut sorted: Vec<(MemoryId, MemoryId)> = pairs.iter().map(|(s, t, _)| (*s, *t)).collect();
    sorted.sort();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"auto_edge:similar_to:v1");
    for (s, t) in &sorted {
        hasher.update(&s.to_be_bytes());
        hasher.update(&t.to_be_bytes());
    }
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_knobs_match_constants() {
        let k = AutoEdgeKnobs::default();
        assert!((k.similarity_threshold - DEFAULT_AUTO_EDGE_SIMILARITY_THRESHOLD).abs() < f32::EPSILON);
        assert_eq!(k.top_k, DEFAULT_TOP_K);
        assert_eq!(k.ef_search, Some(DEFAULT_EF_SEARCH));
    }
}
