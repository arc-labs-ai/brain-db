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
//! 3. `OpsContext::write_temporal_edges` persists the edges + publishes
//!    `EdgeAdded(AUTO_DERIVED)` events.
//!
//! ## What's *not* in scope
//!
//! - Cross-agent edges (different agent_id → no edge).
//! - Cross-context by default (`cross_context = true` knob to opt in).
//! - Multi-strand temporal threading. If three memories arrive in
//!   rapid succession this worker builds a chain, not a fan.
//! - Backfilling existing memories. Migration is for the timeline
//!   index itself; this worker only writes from live enqueues.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use brain_core::{AgentId, ContextId, MemoryId, NodeRef};
use brain_metadata::tables::edge::EdgeKey;
use brain_metadata::tables::memory::{
    agent_timeline_prefix_agent_time, AGENT_TIMELINE_KEY_LEN, MEMORIES_BY_AGENT_TIMELINE_TABLE,
    MEMORIES_TABLE,
};
use brain_ops::{TemporalEdgeEnqueue, TemporalEdgeMetrics, TemporalSkipReason};
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
}

pub const DEFAULT_WINDOW_SECONDS: u64 = 300;
pub const DEFAULT_WEIGHT_MIN: f32 = 0.1;
pub const DEFAULT_CROSS_CONTEXT: bool = false;

impl Default for TemporalEdgeKnobs {
    fn default() -> Self {
        Self {
            window_seconds: DEFAULT_WINDOW_SECONDS,
            weight_min: DEFAULT_WEIGHT_MIN,
            cross_context: DEFAULT_CROSS_CONTEXT,
        }
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

    while processed < cfg.batch_size {
        if started.elapsed() >= cfg.max_runtime {
            break;
        }
        if ctx.is_shutdown() {
            break;
        }
        let Ok((new_memory_id, agent_id, context_id, new_ts)) = worker.queue.try_recv() else {
            break;
        };
        processed += 1;

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

        // Linear decay between (1.0, weight_min) over the window.
        let normalized = 1.0 - (gap_seconds / knobs.window_seconds as f64).clamp(0.0, 1.0);
        let weight = (normalized * (1.0 - knobs.weight_min as f64) + knobs.weight_min as f64)
            .max(0.0) as f32;
        if weight < knobs.weight_min {
            // Belt-and-suspenders; shouldn't fire given the formula.
            continue;
        }

        to_link.push((prev_id, new_memory_id, weight));

        if processed.is_multiple_of(16) {
            glommio::executor().yield_if_needed().await;
        }
    }

    // Write phase. One wtxn per cycle inside write_temporal_edges.
    let written = if to_link.is_empty() {
        0
    } else {
        ctx.ops
            .write_temporal_edges(&to_link)
            .map_err(WorkerError::Ops)?
    };

    let elapsed = started.elapsed();
    worker.metrics.add_edges_written(written as u64);
    worker.metrics.observe_cycle_duration(elapsed.as_secs_f64());

    trace!(
        drained = processed,
        edges_logical = written,
        cycle_ms = elapsed.as_millis() as u64,
        "temporal_edge cycle",
    );
    Ok(processed)
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

    let db = ctx.ops.executor.metadata.lock();
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
