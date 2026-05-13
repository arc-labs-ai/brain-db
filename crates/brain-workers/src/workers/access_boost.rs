//! Access-boost worker (sub-task 8.3). Spec §11/02 §7, §8, §16.
//!
//! Drains the per-shard `AccessBuffer` (filled by RECALL responses)
//! and applies a `salience × (1 + boost_factor)` bump, capped at 1.0.
//! Default cadence 10 s; default boost 10 %. Memories already at the
//! cap are skipped. Missing rows (FORGET-then-RECALL race) are
//! silently skipped.

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use brain_metadata::tables::memory::MEMORIES_TABLE;
use redb::ReadableTable;
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Spec §11/02 §10 — default 10 % boost per access cycle.
pub const DEFAULT_BOOST_FACTOR: f32 = 0.10;

/// Spec §11/02 §2 — salience caps at 1.0.
pub const MAX_SALIENCE: f32 = 1.0;

/// Spec §11/02 §7 boost formula. Pure; unit-testable without a runtime.
#[must_use]
pub fn boosted_salience(current: f32, boost_factor: f32) -> f32 {
    let raw = current * (1.0 + boost_factor);
    raw.clamp(0.0, MAX_SALIENCE)
}

pub struct AccessBoostWorker {
    config: WorkerConfig,
    boost_factor: f32,
}

impl AccessBoostWorker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::AccessBoost),
            boost_factor: DEFAULT_BOOST_FACTOR,
        }
    }

    /// Override the default config. Tests use this to tighten the
    /// interval; operators can use it to enlarge batch_size for
    /// access-heavy workloads.
    #[must_use]
    pub fn with_config(mut self, config: WorkerConfig) -> Self {
        self.config = config;
        self
    }

    /// Override the default 10 % boost factor. `0.0` effectively
    /// disables the worker without flipping `enabled=false`.
    #[must_use]
    pub fn with_boost_factor(mut self, f: f32) -> Self {
        self.boost_factor = f;
        self
    }

    /// Current boost factor (for tests / introspection).
    #[must_use]
    pub fn boost_factor(&self) -> f32 {
        self.boost_factor
    }
}

impl Default for AccessBoostWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for AccessBoostWorker {
    fn name(&self) -> &'static str {
        WorkerKind::AccessBoost.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::AccessBoost
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_boost_cycle(self, ctx))
    }
}

async fn do_boost_cycle(
    worker: &AccessBoostWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    let cfg = worker.config.clone();
    if cfg.batch_size == 0 || worker.boost_factor == 0.0 {
        return Ok(0);
    }

    // Drain the buffer up front; whatever exceeds batch_size we
    // re-queue at the end so a future cycle catches them.
    let ids = ctx.ops.access_buffer.drain();
    if ids.is_empty() {
        return Ok(0);
    }
    let take_n = ids.len().min(cfg.batch_size);

    let metadata = ctx.ops.executor.metadata.clone();
    let started = Instant::now();
    let mut applied = 0usize;
    let mut stopped_early = false;

    {
        let mut db = metadata.lock();
        let wtxn = db
            .write_txn()
            .map_err(|e| WorkerError::Ops(format!("boost write_txn: {e:?}")))?;
        {
            let mut table = wtxn
                .open_table(MEMORIES_TABLE)
                .map_err(|e| WorkerError::Ops(format!("boost open MEMORIES: {e:?}")))?;

            for (i, id) in ids.iter().take(take_n).enumerate() {
                if started.elapsed() >= cfg.max_runtime {
                    stopped_early = true;
                    // Carry the rest of `ids` back into the buffer
                    // (including the current id, which we haven't
                    // applied).
                    let _ = i;
                    break;
                }
                if ctx.is_shutdown() {
                    stopped_early = true;
                    break;
                }
                let key = id.to_be_bytes();
                let prior = table
                    .get(key)
                    .map_err(|e| WorkerError::Ops(format!("boost get: {e:?}")))?
                    .map(|access| access.value());
                let Some(mut meta) = prior else {
                    continue; // tombstoned / deleted, skip silently
                };
                let new_salience = boosted_salience(meta.salience, worker.boost_factor);
                if (new_salience - meta.salience).abs() < f32::EPSILON {
                    continue; // already at cap or no change
                }
                meta.salience = new_salience;
                meta.access_count = meta.access_count.saturating_add(1);
                table
                    .insert(key, meta)
                    .map_err(|e| WorkerError::Ops(format!("boost insert: {e:?}")))?;
                applied += 1;
            }
        }
        wtxn.commit()
            .map_err(|e| WorkerError::Ops(format!("boost commit: {e:?}")))?;
    }

    // Re-queue overflow (everything past `take_n` or after early
    // exit). Drain semantics + re-record keeps the contract simple:
    // ids that didn't make it this cycle are picked up next.
    let requeue_from = if stopped_early {
        // Conservatively re-queue everything past `applied`. Some of
        // those may have been skip-no-changes; recording them again is
        // a no-op (dedup at record).
        applied
    } else {
        take_n
    };
    for id in &ids[requeue_from.min(ids.len())..] {
        ctx.ops.access_buffer.record(*id);
    }

    trace!(
        drained = ids.len(),
        applied,
        cycle_ms = started.elapsed().as_millis() as u64,
        "access-boost cycle"
    );

    Ok(applied)
}
