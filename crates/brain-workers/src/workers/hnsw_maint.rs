//! HNSW maintenance worker (sub-task 8.5). Spec §11/04, §06/07.
//!
//! Every 5 min, the worker:
//!   1. Collects `IndexStats` from `SharedHnsw`.
//!   2. Evaluates `decide_action(stats, thresholds)`.
//!   3. On `FullRebuild`, asks the pluggable [`RebuildSource`] for a
//!      `(MemoryId, [f32; D])` stream and rebuilds the index.
//!   4. Calls `SharedHnsw::swap()` to atomically install the result.
//!
//! ## v1 deviations (documented)
//!
//! - **No `recall_estimate`**: query-sample logging lands in Phase 9.
//!   The worker stamps `recall_estimate = 1.0` so the recall-based
//!   thresholds never fire; `decide_action` is still tested for them
//!   via pure-function tests.
//! - **No catch-up phase**: spec §11/04 §7 wants WAL replay between
//!   build-start LSN and swap; no WAL is wired yet, so Phase 9.
//! - **No partial rebuild**: spec §06/07 §8 is an open question.
//! - **No `ann.rebuild_max_memory_gb` cap**: Phase 9 server config.
//! - **Default [`DisabledRebuildSource`]**: production deployments
//!   inject an arena-backed source in Phase 9. Until then, the worker
//!   collects + logs but doesn't rebuild.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use brain_core::MemoryId;
use brain_embed::VECTOR_DIM;
use brain_index::HnswIndex;
use thiserror::Error;
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

// ---------------------------------------------------------------------------
// Stats + decision logic. Spec §11/04 §2, §06/07 §3.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IndexStats {
    /// `SharedHnsw::len()` — total entries including tombstones.
    pub total_entries: usize,
    pub tombstone_count: usize,
    /// `tombstone_count / total_entries`, with `total_entries == 0`
    /// mapped to 0.0.
    pub tombstone_ratio: f32,
    /// Spec §11/04 §3 — sampled recall@K estimate. v1 always
    /// `1.0` (no query-sample logging yet).
    pub recall_estimate: f32,
}

/// Spec §11/04 §2 — configurable thresholds for the decision
/// function. Defaults match the spec's literal values.
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

/// Spec §11/04 §2 / §06/07 §3 decision function. Pure; unit-testable
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
// RebuildSource trait — Phase 9 injects an arena-backed impl here.
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
/// `!Send` because real adapters (Phase 9.8 `ArenaRebuildSource`) hold
/// `Rc<RefCell<ArenaFile>>` and run on the per-shard Glommio executor.
pub type SnapshotFuture<'a, const D: usize> =
    Pin<Box<dyn Future<Output = Result<Vec<(MemoryId, [f32; D])>, RebuildSourceError>> + 'a>>;

/// Produces a snapshot of active `(MemoryId, vector)` pairs to feed
/// into `HnswIndex::rebuild`. Production deployments inject an
/// arena-backed impl (Phase 9.8). Same `Pin<Box<Future>>` pattern as
/// the `Summarizer` trait — no `async-trait` dep.
///
/// Post-9.8 the trait is `!Send + !Sync`: the per-shard
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
            // v1: just log it. Phase 9 will defer the rebuild to a
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
                    let (new_idx, _report) = HnswIndex::<{ VECTOR_DIM }>::rebuild(params, vectors)
                        .map_err(|e| WorkerError::Ops(format!("rebuild: {e:?}")))?;
                    index.swap(new_idx);
                    trace!(?stats, "hnsw maintenance: full rebuild complete");
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
