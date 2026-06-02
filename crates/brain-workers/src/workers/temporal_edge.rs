//! TemporalEdgeWorker — derives `FollowedBy` substrate edges by
//! walking the per-agent timeline index after every ENCODE.
//!
//! ## Why this exists
//!
//! Real agents stream observations in order. Each new memory almost
//! always *follows* a previous one in a narrative sense — but until
//! this worker landed every `FollowedBy` edge had to be hand-attached
//! by the caller via `--edge followed_by:<prev_id>`. The
//! AutoEdgeWorker covers similarity; this covers narrative adjacency.
//!
//! ## Flow
//!
//! 1. The writer's ENCODE handler pushes
//!    `(memory_id, agent_id, context_id, created_at_unix_nanos)`
//!    into a per-shard `flume::Sender` after redb commit. Non-blocking;
//!    full channel drops with a counter bump.
//! 2. The worker drains the receiver in bounded batches every
//!    `interval_ms`. For each enqueue it walks
//!    `MEMORIES_BY_AGENT_TIMELINE_TABLE` backwards from the new
//!    memory's timestamp to find the predecessor in the same agent +
//!    context, computes a linear-decay weight from the gap, and writes
//!    a `FollowedBy` edge from predecessor → new memory.
//! 3. The worker builds a single `Write { phases: Vec<Phase::Link> }`
//!    and calls `RealWriterHandle::submit`. The unified write path
//!    WALs every edge, commits the redb rows, and publishes
//!    `EdgeAdded(AUTO_DERIVED, derived_by=TEMPORAL_WORKER)` envelopes
//!    via the subscribe bus — subscribe replay sees derived edges
//!    alongside explicit ones.
//!
//! ## What's *not* in scope
//!
//! - Cross-agent edges (different agent_id → no edge).
//! - Cross-context by default (`cross_context = true` knob to opt in).
//! - Multi-strand temporal threading. If three memories arrive in
//!   rapid succession this worker builds a chain, not a fan.
//! - Backfilling existing memories. Migration is for the timeline
//!   index itself; this worker only writes from live enqueues.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use brain_core::{AgentId, ContextId, EdgeKind, EdgeKindRef, MemoryId, MemoryKind, NodeRef};
use brain_metadata::tables::edge::{derived_by, origin, zero_disambiguator, EdgeKey};
use brain_metadata::tables::memory::{
    agent_timeline_prefix_agent_time, AGENT_TIMELINE_KEY_LEN, MEMORIES_BY_AGENT_TIMELINE_TABLE,
    MEMORIES_TABLE,
};
use brain_ops::{
    EventEnvelope, Phase, RealWriterHandle, TemporalEdgeEnqueue, TemporalEdgeMetrics,
    TemporalSkipReason, Write, WriteId,
};
use brain_protocol::shared::enums::{
    EventType, StageKind, StageOutcome, StagePayload, StageTemporalEdgePayload,
};
use futures_lite::FutureExt;
use glommio::timer::sleep;
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Knobs that don't fit `WorkerConfig`'s generic shape. Defaults match
/// `plans/temporal-edge-worker-impl.md`.
#[derive(Clone, Copy, Debug)]
pub struct TemporalEdgeKnobs {
    /// Maximum gap (in seconds) for a candidate predecessor to
    /// produce a `FollowedBy` edge.
    pub window_seconds: u64,
    /// Hard floor on the decay-weight curve. Edges below this don't
    /// get written; keeps the table from filling with near-zero
    /// weight rows.
    pub weight_min: f32,
    /// Allow `FollowedBy` across context boundaries. Defaults to
    /// `false` — most narratives are scoped to a single context.
    pub cross_context: bool,
    /// Minimum cosine similarity between the new memory and its
    /// candidate predecessor for the edge to be written.
    ///
    /// Without this filter, every in-window successor receives a
    /// `FollowedBy` edge regardless of content — "I had lunch" then
    /// "deployed to prod" would link despite zero shared topic.
    ///
    /// Strict (≥0.5): tight thematic chains; the agent's narrative
    /// stays on-topic. Loose (~0.3): broader narrative; cross-topic
    /// adjacency captured. 0.4 is the operator-tunable middle.
    pub topical_threshold: f32,
}

pub const DEFAULT_WINDOW_SECONDS: u64 = 300;
pub const DEFAULT_WEIGHT_MIN: f32 = 0.1;
pub const DEFAULT_CROSS_CONTEXT: bool = false;
pub const DEFAULT_TEMPORAL_EDGE_TOPICAL_THRESHOLD: f32 = 0.4;

/// Environment variable for overriding
/// [`DEFAULT_TEMPORAL_EDGE_TOPICAL_THRESHOLD`]. Accepts an `f32` in
/// `[0.0, 1.0]`. Out-of-range or unparseable values fall back to the
/// default with a tracing warn.
pub const TEMPORAL_EDGE_TOPICAL_THRESHOLD_ENV: &str = "BRAIN_TEMPORAL_EDGE_TOPICAL_THRESHOLD";

/// Resolve the topical threshold from the env var (if set + valid)
/// or fall back to `default`. Same shape as
/// `auto_edge::resolved_threshold` so operators get consistent
/// override semantics across both derivation workers.
#[must_use]
pub fn resolved_topical_threshold(default: f32) -> f32 {
    resolved_topical_threshold_from(
        std::env::var(TEMPORAL_EDGE_TOPICAL_THRESHOLD_ENV).ok(),
        default,
    )
}

/// Pure parse step extracted from [`resolved_topical_threshold`] so
/// tests can validate the value-validation logic without mutating
/// process-wide env state (forbidden under the project's no-`unsafe`
/// rule).
#[must_use]
pub fn resolved_topical_threshold_from(raw: Option<String>, default: f32) -> f32 {
    let Some(raw) = raw else {
        return default;
    };
    match raw.parse::<f32>() {
        Ok(v) if (0.0..=1.0).contains(&v) => v,
        Ok(v) => {
            tracing::warn!(
                env = TEMPORAL_EDGE_TOPICAL_THRESHOLD_ENV,
                value = v,
                "temporal-edge topical threshold out of [0.0, 1.0]; using default"
            );
            default
        }
        Err(e) => {
            tracing::warn!(
                env = TEMPORAL_EDGE_TOPICAL_THRESHOLD_ENV,
                error = %e,
                "temporal-edge topical threshold not a valid f32; using default"
            );
            default
        }
    }
}

impl Default for TemporalEdgeKnobs {
    fn default() -> Self {
        Self {
            window_seconds: DEFAULT_WINDOW_SECONDS,
            weight_min: DEFAULT_WEIGHT_MIN,
            cross_context: DEFAULT_CROSS_CONTEXT,
            topical_threshold: resolved_topical_threshold(DEFAULT_TEMPORAL_EDGE_TOPICAL_THRESHOLD),
        }
    }
}

/// Cosine similarity between two equal-length f32 vectors. Returns
/// `None` when either magnitude is effectively zero — callers treat
/// that as "no topical signal" and skip the gate rather than writing a
/// NaN-weighted edge. Kept in this module (rather than only in tests)
/// so the gate can swap to a direct vector path if HNSW gains a
/// per-id vector accessor.
#[cfg(test)]
fn cosine_similarity(a: &[f32], b: &[f32]) -> Option<f32> {
    debug_assert_eq!(a.len(), b.len());
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        return None;
    }
    let denom = (na.sqrt()) * (nb.sqrt());
    if denom <= f32::EPSILON {
        return None;
    }
    let cos = dot / denom;
    if cos.is_finite() {
        Some(cos.clamp(-1.0, 1.0))
    } else {
        None
    }
}

pub struct TemporalEdgeWorker {
    config: WorkerConfig,
    knobs: TemporalEdgeKnobs,
    queue: flume::Receiver<TemporalEdgeEnqueue>,
    metrics: Arc<TemporalEdgeMetrics>,
}

impl TemporalEdgeWorker {
    #[must_use]
    pub fn new(queue: flume::Receiver<TemporalEdgeEnqueue>) -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::TemporalEdge),
            knobs: TemporalEdgeKnobs::default(),
            queue,
            metrics: Arc::new(TemporalEdgeMetrics::new()),
        }
    }

    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<TemporalEdgeMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    #[must_use]
    pub fn metrics(&self) -> Arc<TemporalEdgeMetrics> {
        self.metrics.clone()
    }

    #[must_use]
    pub fn with_config(mut self, config: WorkerConfig) -> Self {
        self.config = config;
        self
    }

    #[must_use]
    pub fn with_knobs(mut self, knobs: TemporalEdgeKnobs) -> Self {
        self.knobs = knobs;
        self
    }

    #[must_use]
    pub fn knobs(&self) -> TemporalEdgeKnobs {
        self.knobs
    }
}

impl Worker for TemporalEdgeWorker {
    fn name(&self) -> &'static str {
        WorkerKind::TemporalEdge.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::TemporalEdge
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_temporal_edge_cycle(self, ctx))
    }
}

async fn do_temporal_edge_cycle(
    worker: &TemporalEdgeWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    let cfg = worker.config.clone();
    if cfg.batch_size == 0 {
        return Ok(0);
    }
    let knobs = worker.knobs;
    let started = Instant::now();
    let window_nanos: u64 = knobs.window_seconds.saturating_mul(1_000_000_000);

    let mut to_link: Vec<(MemoryId, MemoryId, f32)> = Vec::new();
    let mut processed = 0usize;
    // Every drained memory needs a `StageCompleted{TemporalEdge}`
    // event so wait helpers can tick the pending-stage checklist —
    // even when the predecessor lookup fails or the gap exceeds the
    // window. `per_source_edges` counts the rows the cycle will
    // commit for each source.
    let mut drained_sources: Vec<MemoryId> = Vec::new();
    let mut per_source_edges: HashMap<MemoryId, u32> = HashMap::new();

    while processed < cfg.batch_size {
        if started.elapsed() >= cfg.max_runtime {
            break;
        }
        if ctx.is_shutdown() {
            break;
        }
        // First iteration blocks on the queue (raced against the
        // tick interval). The queue's wake fires on every writer
        // enqueue, so the encode→edge derivation path has no
        // per-interval latency floor. Subsequent iterations drain
        // without blocking so a burst batches into one cycle.
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
        let (new_memory_id, agent_id, context_id, new_ts, new_vector) = item;
        processed += 1;
        drained_sources.push(new_memory_id);

        // Look up the predecessor via the agent-timeline index in
        // its own read txn. One tiny rtxn per enqueue keeps the
        // lock contention surface minimal.
        let prev_lookup = lookup_predecessor(
            ctx,
            agent_id,
            context_id,
            new_ts,
            new_memory_id,
            knobs.cross_context,
        );
        let (prev_id, prev_ts) = match prev_lookup {
            PredecessorOutcome::Found(id, ts) => (id, ts),
            PredecessorOutcome::Skip(reason) => {
                worker.metrics.inc_skip(reason);
                continue;
            }
        };

        // Gap & window check.
        let gap_nanos = new_ts.saturating_sub(prev_ts);
        if gap_nanos == 0 {
            // Equal timestamps would have failed the "strictly newer"
            // check in lookup_predecessor; the saturating_sub here is
            // defensive.
            worker.metrics.inc_skip(TemporalSkipReason::OutOfOrder);
            continue;
        }
        if gap_nanos > window_nanos {
            worker.metrics.inc_skip(TemporalSkipReason::WindowExceeded);
            continue;
        }
        let gap_seconds = (gap_nanos as f64) / 1.0e9;
        worker.metrics.observe_gap_seconds(gap_seconds);

        // Topical gate. Without this every in-window successor would
        // get a `FollowedBy` edge regardless of content — "I had
        // lunch" followed by "deployed to prod" would link. We need
        // the predecessor's vector to compute cosine; HNSW exposes no
        // per-id vector accessor, so we instead piggy-back on a knn
        // search of the new vector and read the predecessor's
        // similarity from the result set. If the predecessor doesn't
        // appear in the top-K (with K wide enough for reasonable
        // recall), it's far enough in cosine space that the gate would
        // have rejected it anyway — treat as below-topical and drop.
        //
        // All-zero stub embeddings (no model wired) produce no usable
        // topical signal; `cosine_similarity` returns `None` and we
        // skip the gate so test fixtures and pre-embedder deployments
        // still derive temporal edges.
        if !new_vector.iter().all(|c| *c == 0.0) {
            // K = 64 matches default ef_search. The widest practical
            // case is "the predecessor is a top-64 neighbour" — true
            // for any topical link in agent-journaling workloads;
            // very-distant predecessors are precisely the ones we
            // want to drop.
            const TOPICAL_KNN_K: usize = 64;
            const TOPICAL_KNN_EF: usize = 128;
            let hits = ctx.ops.executor.index.search_active(
                &new_vector,
                TOPICAL_KNN_K,
                Some(TOPICAL_KNN_EF),
            );
            let prev_similarity = hits.iter().find(|(id, _)| *id == prev_id).map(|(_, s)| *s);
            // Synthesise a `None`-as-below-threshold signal: if HNSW
            // didn't return the predecessor in the top-K, the cosine
            // is below the smallest result in `hits` — necessarily
            // below the threshold for any reasonable K.
            let topical_signal = prev_similarity;
            if let Some(sim) = topical_signal {
                if sim.is_finite() && sim < knobs.topical_threshold {
                    tracing::debug!(
                        target: "brain_workers::temporal_edge",
                        source = ?new_memory_id,
                        predecessor = ?prev_id,
                        cos = sim,
                        threshold = knobs.topical_threshold,
                        "temporal-edge candidate below topical threshold; dropping"
                    );
                    worker.metrics.inc_skip(TemporalSkipReason::BelowTopical);
                    continue;
                }
            } else {
                // Predecessor not in top-K → drop. Documented above.
                tracing::debug!(
                    target: "brain_workers::temporal_edge",
                    source = ?new_memory_id,
                    predecessor = ?prev_id,
                    threshold = knobs.topical_threshold,
                    "temporal-edge predecessor not in top-K; treating as below topical threshold"
                );
                worker.metrics.inc_skip(TemporalSkipReason::BelowTopical);
                continue;
            }
        }

        // Linear decay between (1.0, weight_min) over the window.
        let normalized = 1.0 - (gap_seconds / knobs.window_seconds as f64).clamp(0.0, 1.0);
        let weight = (normalized * (1.0 - knobs.weight_min as f64) + knobs.weight_min as f64)
            .max(0.0) as f32;
        if weight < knobs.weight_min {
            // Belt-and-suspenders; shouldn't fire given the formula.
            continue;
        }

        to_link.push((prev_id, new_memory_id, weight));
        *per_source_edges.entry(new_memory_id).or_insert(0) += 1;

        if processed.is_multiple_of(16) {
            glommio::executor().yield_if_needed().await;
        }
    }

    // Write phase. One Write per cycle on the unified path: WAL
    // append, redb commit, subscribe-event burst all flow through
    // `submit(Write)`. Retries of the same cycle collapse to the
    // cached ack via the BLAKE3-hashed sorted batch.
    let created_at = now_unix_nanos();
    let written = if to_link.is_empty() {
        0
    } else {
        let phases: Vec<Phase> = to_link
            .iter()
            .map(|(from, to, weight)| Phase::Link {
                from: NodeRef::Memory(*from),
                to: NodeRef::Memory(*to),
                kind: EdgeKindRef::Builtin(EdgeKind::FollowedBy),
                weight: *weight,
                origin: origin::AUTO_DERIVED,
                derived_by: derived_by::TEMPORAL_WORKER,
                disambiguator: zero_disambiguator(),
                created_at_unix_nanos: created_at,
            })
            .collect();
        let request_hash = hash_temporal_batch(&to_link);
        let write = Write::from_phases(WriteId::new(), AgentId::default(), phases)
            .with_request_hash(request_hash);
        let real_writer = ctx
            .ops
            .executor
            .writer
            .as_any()
            .downcast_ref::<RealWriterHandle>()
            .ok_or_else(|| {
                WorkerError::Ops("temporal_edge: unified path requires RealWriterHandle".into())
            })?;
        real_writer
            .submit(write)
            .await
            .map_err(|e| WorkerError::Ops(format!("submit: {e:?}")))?;
        to_link.len()
    };

    let elapsed = started.elapsed();
    worker.metrics.add_edges_written(written as u64);
    worker.metrics.observe_cycle_duration(elapsed.as_secs_f64());

    // Publish per-source `StageCompleted{TemporalEdge}` events so wait
    // helpers can tick the checklist.
    let ts = now_unix_nanos();
    for memory_id in drained_sources {
        let edges_written = per_source_edges.get(&memory_id).copied().unwrap_or(0);
        let outcome = if edges_written > 0 {
            StageOutcome::Ok
        } else {
            StageOutcome::Empty
        };
        let envelope = EventEnvelope {
            lsn: 0,
            event_type: EventType::StageCompleted,
            memory_id,
            context_id: ContextId::default(),
            kind: MemoryKind::Episodic,
            salience: 0.0,
            timestamp_unix_nanos: ts,
            text: None,
            graph_payload: None,
            edge_payload: None,
            stage_kind: Some(StageKind::TemporalEdge),
            stage_outcome: Some(outcome),
            stage_payload: Some(StagePayload::TemporalEdge(StageTemporalEdgePayload {
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
        "temporal_edge cycle",
    );
    Ok(processed)
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Deterministic hash of a batch of `(predecessor, successor, weight)`
/// tuples. Sorted by `(predecessor, successor)` so retries hash the
/// same way regardless of drain ordering. Weight is excluded — the
/// timestamp range + window knob determine which pairs land, so the
/// pair set is the invariant the idempotency cache keys on.
fn hash_temporal_batch(pairs: &[(MemoryId, MemoryId, f32)]) -> [u8; 32] {
    let mut sorted: Vec<(MemoryId, MemoryId)> = pairs.iter().map(|(s, t, _)| (*s, *t)).collect();
    sorted.sort();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"temporal_edge:followed_by:v1");
    for (s, t) in &sorted {
        hasher.update(&s.to_be_bytes());
        hasher.update(&t.to_be_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// Outcome of the predecessor lookup. Kept as an explicit enum so the
/// `inc_skip` reason is paired with each `Skip` variant — no separate
/// "what was the reason" inference at the call site.
enum PredecessorOutcome {
    Found(MemoryId, u64),
    Skip(TemporalSkipReason),
}

fn lookup_predecessor(
    ctx: &WorkerContext,
    agent_id: AgentId,
    context_id: ContextId,
    new_ts: u64,
    new_memory_id: MemoryId,
    cross_context: bool,
) -> PredecessorOutcome {
    use brain_metadata::tables::memory::flags as memory_flags;

    let db = ctx.ops.executor.metadata.as_ref();
    let rtxn = match db.read_txn() {
        Ok(t) => t,
        Err(_) => return PredecessorOutcome::Skip(TemporalSkipReason::NoPrev),
    };
    let timeline_t = match rtxn.open_table(MEMORIES_BY_AGENT_TIMELINE_TABLE) {
        Ok(t) => t,
        Err(_) => return PredecessorOutcome::Skip(TemporalSkipReason::NoPrev),
    };

    // Range scan: from the start of this agent's rows up to (but not
    // including) the new memory's key. The last entry in that range is
    // the most recent predecessor. The timeline key is:
    //   [agent(16)] [created_at_be(8)] [context(8)] [memory_id(16)]
    // so a prefix `[agent(16)] [new_ts_be(8)]` and a `..` exclusive
    // upper bound is exactly what we want.
    let lower: [u8; 16] = agent_id.0.into_bytes();
    let upper_24 = agent_timeline_prefix_agent_time(agent_id.0.into_bytes(), new_ts);

    let lower_slice = lower.as_slice();
    let upper_slice = upper_24.as_slice();
    let iter = match timeline_t.range::<&[u8]>(lower_slice..upper_slice) {
        Ok(it) => it,
        Err(_) => return PredecessorOutcome::Skip(TemporalSkipReason::NoPrev),
    };

    // Walk to the last row by consuming the iterator. Most agents
    // produce ≤1 memory per cycle; the iterator length is bounded by
    // how many rows the agent has within the window-equivalent slice.
    // For tighter performance once it's a hot path we can add a
    // reverse-iter API to redb; for now, forward + last() is fine.
    let mut last_key: Option<Vec<u8>> = None;
    for entry in iter {
        let Ok((k, _v)) = entry else { continue };
        last_key = Some(k.value().to_vec());
    }
    drop(timeline_t);
    let Some(last_key_bytes) = last_key else {
        return PredecessorOutcome::Skip(TemporalSkipReason::NoPrev);
    };
    if last_key_bytes.len() != AGENT_TIMELINE_KEY_LEN {
        // Corrupt key length — treat as no predecessor.
        return PredecessorOutcome::Skip(TemporalSkipReason::NoPrev);
    }

    // Decode the key.
    let prev_ts = u64::from_be_bytes(last_key_bytes[16..24].try_into().unwrap_or([0; 8]));
    let prev_context = u64::from_be_bytes(last_key_bytes[24..32].try_into().unwrap_or([0; 8]));
    let mut prev_mem_bytes = [0u8; 16];
    prev_mem_bytes.copy_from_slice(&last_key_bytes[32..48]);
    let prev_id = MemoryId::from_be_bytes(prev_mem_bytes);

    // Order check — the range scan is upper-exclusive on the
    // (agent, ts)-prefix, so prev_ts < new_ts MUST hold. Defensive.
    if prev_ts >= new_ts {
        return PredecessorOutcome::Skip(TemporalSkipReason::OutOfOrder);
    }
    if prev_id == new_memory_id {
        // The new memory hasn't been committed for an external
        // observer yet, so it can't be its own predecessor — but
        // belt-and-suspenders.
        return PredecessorOutcome::Skip(TemporalSkipReason::OutOfOrder);
    }
    if !cross_context && prev_context != context_id.0 {
        return PredecessorOutcome::Skip(TemporalSkipReason::CrossContext);
    }

    // Tombstone check via the main metadata table.
    let memories_t = match rtxn.open_table(MEMORIES_TABLE) {
        Ok(t) => t,
        Err(_) => return PredecessorOutcome::Skip(TemporalSkipReason::NoPrev),
    };
    let row = match memories_t.get(prev_mem_bytes) {
        Ok(Some(g)) => g.value(),
        _ => return PredecessorOutcome::Skip(TemporalSkipReason::NoPrev),
    };
    if row.flags & (memory_flags::HARD_FORGOTTEN | (1 << 4/* tombstoned, see flags */)) != 0
        || row.tombstoned_at_unix_nanos.is_some()
    {
        return PredecessorOutcome::Skip(TemporalSkipReason::Tombstoned);
    }

    // Suppress unused-import warnings when this module's tests strip
    // some helpers.
    let _ = (EdgeKey::from_kind_prefix, NodeRef::Memory(prev_id));

    PredecessorOutcome::Found(prev_id, prev_ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topical_threshold_default_when_none() {
        assert!((resolved_topical_threshold_from(None, 0.4) - 0.4).abs() < f32::EPSILON);
    }

    #[test]
    fn topical_threshold_env_override() {
        let v = resolved_topical_threshold_from(Some("0.6".to_string()), 0.4);
        assert!((v - 0.6).abs() < 1e-6);
    }

    #[test]
    fn topical_threshold_invalid_falls_back() {
        let v = resolved_topical_threshold_from(Some("nope".to_string()), 0.4);
        assert!((v - 0.4).abs() < f32::EPSILON);
        let v = resolved_topical_threshold_from(Some("2.0".to_string()), 0.4);
        assert!((v - 0.4).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_similarity_orthogonal_returns_zero() {
        let a = [1.0, 0.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0, 0.0];
        let cos = cosine_similarity(&a, &b).expect("finite cosine for non-zero vectors");
        assert!(cos.abs() < 1e-6, "orthogonal cosine ≈ 0, got {cos}");
    }

    #[test]
    fn cosine_similarity_identical_returns_one() {
        let a = [1.0_f32, 2.0, 3.0];
        let cos = cosine_similarity(&a, &a).expect("finite cosine for non-zero vectors");
        assert!((cos - 1.0).abs() < 1e-6, "identical cosine ≈ 1, got {cos}");
    }

    #[test]
    fn cosine_similarity_zero_vector_returns_none() {
        let a = [0.0_f32, 0.0, 0.0];
        let b = [1.0_f32, 2.0, 3.0];
        assert!(cosine_similarity(&a, &b).is_none());
        assert!(cosine_similarity(&a, &a).is_none());
    }
}
