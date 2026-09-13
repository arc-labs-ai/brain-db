//! HNSW maintenance worker.
//!
//! Every 5 min, the worker:
//!   1. Collects `IndexStats` from `SharedHnsw`.
//!   2. Evaluates `decide_action(stats, thresholds)`.
//!   3. On `FullRebuild`, asks the pluggable [`RebuildSource`] for a
//!      `(MemoryId, [f32; D])` stream and rebuilds the index, folding
//!      in the `SharedHnsw` pending buffer so no writes are lost.
//!   4. Publishes the new epoch via `SharedHnsw::flush_with_rebuild`,
//!      which atomically swaps main and drains pending in one step.
//!
//! ## v1 deviations (documented)
//!
//! - **No `recall_estimate`**: query-sample logging is not yet wired.
//!   The worker stamps `recall_estimate = 1.0` so the recall-based
//!   thresholds never fire; `decide_action` is still tested for them
//!   via pure-function tests.
//! - **No catch-up phase**: wants WAL replay between build-start LSN
//!   and swap; no WAL is wired yet.
//! - **No partial rebuild**: is an open question.
//! - **No `ann.rebuild_max_memory_gb` cap**: server config.
//! - **Default [`DisabledRebuildSource`]**: production deployments
//!   inject an arena-backed source. Until then, the worker collects +
//!   logs but doesn't rebuild.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use brain_core::MemoryId;
use brain_embed::VECTOR_DIM;
use thiserror::Error;
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

// ---------------------------------------------------------------------------
// Stats + decision logic.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IndexStats {
    /// `SharedHnsw::len()` — total entries including tombstones.
    pub total_entries: usize,
    pub tombstone_count: usize,
    /// `tombstone_count / total_entries`, with `total_entries == 0`
    /// mapped to 0.0.
    pub tombstone_ratio: f32,
    /// — sampled recall@K estimate. v1 always
    /// `1.0` (no query-sample logging yet).
    pub recall_estimate: f32,
}

/// — configurable thresholds for the decision
/// function.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RebuildThresholds {
    pub tombstone_full_rebuild: f32,
    pub recall_full_rebuild: f32,
    pub tombstone_schedule: f32,
    pub recall_schedule: f32,
}

impl Default for RebuildThresholds {
    fn default() -> Self {
        Self {
            tombstone_full_rebuild: 0.30,
            recall_full_rebuild: 0.90,
            tombstone_schedule: 0.15,
            recall_schedule: 0.93,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    None,
    ScheduleRebuildSoon,
    FullRebuild,
}

/// decision function. Pure; unit-testable
/// without a runtime.
#[must_use]
pub fn decide_action(stats: IndexStats, t: RebuildThresholds) -> Action {
    if stats.tombstone_ratio > t.tombstone_full_rebuild {
        return Action::FullRebuild;
    }
    if stats.recall_estimate < t.recall_full_rebuild {
        return Action::FullRebuild;
    }
    if stats.tombstone_ratio > t.tombstone_schedule || stats.recall_estimate < t.recall_schedule {
        return Action::ScheduleRebuildSoon;
    }
    Action::None
}

// ---------------------------------------------------------------------------
// RebuildSource trait — an arena-backed impl is injected here.
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum RebuildSourceError {
    /// v1 default. No arena → no per-id vector lookup → no rebuild.
    #[error("rebuild source disabled")]
    Disabled,
    #[error("rebuild source failed: {0}")]
    Failed(String),
}

/// Future returned by [`RebuildSource::snapshot_vectors`].
///
/// `!Send` because real adapters (`ArenaRebuildSource`) hold
/// `Rc<RefCell<ArenaFile>>` and run on the per-shard Glommio executor.
pub type SnapshotFuture<'a, const D: usize> =
    Pin<Box<dyn Future<Output = Result<Vec<(MemoryId, [f32; D])>, RebuildSourceError>> + 'a>>;

/// Produces a snapshot of active `(MemoryId, vector)` pairs to feed
/// into `HnswIndex::rebuild`. Production deployments inject an
/// arena-backed impl. Same `Pin<Box<Future>>` pattern as
/// the `Summarizer` trait — no `async-trait` dep.
///
/// The trait is `!Send + !Sync`: the per-shard
/// `WorkerScheduler` runs on Glommio (`!Send` futures), so the trait
/// only needs `'static` for `Arc<dyn …>` storage in the worker.
pub trait RebuildSource<const D: usize>: 'static {
    fn snapshot_vectors(&self) -> SnapshotFuture<'_, D>;
}

/// Default no-op source. Always returns `Disabled`.
pub struct DisabledRebuildSource;

impl<const D: usize> RebuildSource<D> for DisabledRebuildSource {
    fn snapshot_vectors(&self) -> SnapshotFuture<'_, D> {
        Box::pin(async { Err(RebuildSourceError::Disabled) })
    }
}

// ---------------------------------------------------------------------------
// HnswMaintenanceWorker.
// ---------------------------------------------------------------------------

pub struct HnswMaintenanceWorker {
    config: WorkerConfig,
    thresholds: RebuildThresholds,
    rebuild_source: Arc<dyn RebuildSource<{ VECTOR_DIM }>>,
}

impl HnswMaintenanceWorker {
    #[must_use]
    pub fn new(rebuild_source: Arc<dyn RebuildSource<{ VECTOR_DIM }>>) -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::HnswMaintenance),
            thresholds: RebuildThresholds::default(),
            rebuild_source,
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    #[must_use]
    pub fn with_thresholds(mut self, t: RebuildThresholds) -> Self {
        self.thresholds = t;
        self
    }
}

impl Worker for HnswMaintenanceWorker {
    fn name(&self) -> &'static str {
        WorkerKind::HnswMaintenance.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::HnswMaintenance
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_maintenance_cycle(self, ctx))
    }
}

async fn do_maintenance_cycle(
    worker: &HnswMaintenanceWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    let index = ctx.ops.executor.index.clone();

    // 1. Collect stats.
    let total = index.len();
    let tombstoned = index.tombstone_count();
    let tombstone_ratio = if total == 0 {
        0.0
    } else {
        tombstoned as f32 / total as f32
    };
    let stats = IndexStats {
        total_entries: total,
        tombstone_count: tombstoned,
        tombstone_ratio,
        recall_estimate: 1.0, // v1: no query-sample logging
    };
    let action = decide_action(stats, worker.thresholds);

    match action {
        Action::None => {
            trace!(?stats, "hnsw maintenance: no action");
            Ok(0)
        }
        Action::ScheduleRebuildSoon => {
            // v1: just log it. The rebuild can later be deferred to a
            // less-busy window.
            trace!(?stats, "hnsw maintenance: schedule rebuild soon");
            Ok(0)
        }
        Action::FullRebuild => {
            if ctx.is_shutdown() {
                return Ok(0);
            }
            match worker.rebuild_source.snapshot_vectors().await {
                Ok(vectors) => {
                    let params = index.params();
                    // Fold the arena snapshot together with the pending
                    // buffer so writes that landed after the snapshot
                    // started are preserved in the new epoch. The
                    // flush_with_rebuild call atomically publishes the
                    // new main and drains pending.
                    let flush = index
                        .flush_with_rebuild(move |pending_snapshot| {
                            let mut combined: Vec<(MemoryId, [f32; VECTOR_DIM])> = vectors;
                            // Deduplicate against the arena snapshot:
                            // pending entries shadow arena entries for
                            // the same id (latest vector wins).
                            let arena_ids: std::collections::HashSet<MemoryId> =
                                combined.iter().map(|(id, _)| *id).collect();
                            for entry in pending_snapshot {
                                if entry.tombstoned {
                                    continue;
                                }
                                if arena_ids.contains(&entry.memory_id) {
                                    if let Some(slot) =
                                        combined.iter_mut().find(|(id, _)| *id == entry.memory_id)
                                    {
                                        slot.1 = entry.vector;
                                    }
                                } else {
                                    combined.push((entry.memory_id, entry.vector));
                                }
                            }
                            let (idx, _) = brain_index::rebuild::rebuild_impl(params, combined)?;
                            Ok(idx)
                        })
                        .map_err(|e| WorkerError::Ops(format!("rebuild: {e:?}")))?;
                    trace!(
                        ?stats,
                        new_epoch = flush.new_epoch,
                        entries_flushed = flush.entries_flushed,
                        "hnsw maintenance: full rebuild complete"
                    );
                    Ok(1)
                }
                Err(RebuildSourceError::Disabled) => {
                    trace!(
                        ?stats,
                        "hnsw maintenance: rebuild needed but source disabled (v1)"
                    );
                    Ok(0)
                }
                Err(RebuildSourceError::Failed(e)) => {
                    Err(WorkerError::Ops(format!("rebuild source: {e}")))
                }
            }
        }
    }
}
