//! AutoEdgeWorker — derives `SimilarTo` substrate edges from HNSW
//! k-nearest-neighbour searches after every successful ENCODE.
//!
//! ## Why this exists
//!
//! Before this worker landed, every substrate edge was client-supplied
//! (via `ENCODE_REQ.edges` or a separate `LINK`). The memory graph was
//! empty by default, which made the planner's memory-anchor graph
//! retriever (Phase A's hybrid recall) a no-op on any deployment that
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
//!    runs HNSW knn and writes `SimilarTo` edges whose cosine
//!    similarity is above the configured threshold.
//! 3. `brain_metadata::tables::edge::link` handles the symmetric
//!    mirror automatically (each logical pair = two physical forward
//!    rows + two reverse rows).
//!
//! ## Idempotency and FORGET
//!
//! Re-draining the same `MemoryId` is safe: `edge::link` overwrites
//! the existing `EdgeData`. The writer's HNSW already filters
//! tombstoned ids out of `search_active`, and we double-check via
//! `is_tombstoned(source)` so a memory FORGOTTEN between enqueue and
//! drain produces zero edges instead of pointing into a tombstoned
//! source.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use brain_core::MemoryId;
use brain_ops::{AutoEdgeEnqueue, AutoEdgeMetrics};
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
pub const DEFAULT_AUTO_EDGE_SIMILARITY_THRESHOLD: f32 = 0.85;
pub const DEFAULT_EF_SEARCH: usize = 64;

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
    /// scheduler doesn't wire one — keeps tests / substrate-only
    /// fixtures compiling without an extra setter call.
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

    // Read phase: drain up to `batch_size` (or the per-cycle wall-clock
    // budget) from the channel, run knn for each, collect link tuples.
    // We never `await` on the channel itself (try_recv only) — that
    // would block the entire scheduler if the queue empties.
    let mut to_link: Vec<(MemoryId, MemoryId, f32)> = Vec::new();
    let mut processed = 0usize;
    let mut neighbours_found = 0u64;
    while processed < cfg.batch_size {
        if started.elapsed() >= cfg.max_runtime {
            break;
        }
        if ctx.is_shutdown() {
            break;
        }
        let Ok((source_id, vector)) = worker.queue.try_recv() else {
            break;
        };
        processed += 1;

        // Source was FORGOTTEN between enqueue and now — skip it
        // entirely. We treat the source's tombstone state as
        // authoritative because the writer already pushed a vector
        // that's now dangling; HNSW search would still return
        // neighbours, but linking a tombstoned memory to anything
        // would only feed the edge_scrub queue.
        if index.is_tombstoned(source_id) {
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
            if similarity < knobs.similarity_threshold {
                continue;
            }
            to_link.push((source_id, neighbour, similarity));
            neighbours_found += 1;
        }

        // Cooperative yield every few drains so the scheduler stays
        // responsive when the queue is deep. Cheap when nothing else
        // is ready.
        if processed.is_multiple_of(16) {
            glommio::executor().yield_if_needed().await;
        }
    }

    // Write phase: one wtxn for the whole cycle. The auto-mirror
    // inside `edge::link` doubles the row count; `write_auto_edges`
    // returns the *logical* count (matches what we asked for).
    let written = if to_link.is_empty() {
        0
    } else {
        ctx.ops
            .write_auto_edges(&to_link)
            .map_err(WorkerError::Ops)?
    };

    let elapsed = started.elapsed();
    worker.metrics.add_edges_written(written as u64);
    worker.metrics.observe_neighbours_found(neighbours_found);
    worker.metrics.observe_cycle_duration(elapsed.as_secs_f64());

    trace!(
        drained = processed,
        edges_logical = written,
        cycle_ms = elapsed.as_millis() as u64,
        "auto_edge cycle",
    );
    Ok(processed)
}
