//! Slot reclamation worker (sub-task 8.7). Spec §11/06.
//!
//! Scans for memories whose `tombstoned_at_unix_nanos + grace_period`
//! is past, and reclaims them: delete the MEMORIES row + adjacent
//! edges. Spec §8 — one wtxn per memory keeps lock duration small.
//!
//! ## v1 deviations (documented)
//!
//! - **No arena / free-list push.** Spec §3 step 6 wants the slot id
//!   pushed back onto the arena free list. v1 has no arena and never
//!   reuses slots (each ENCODE mints a fresh slot from a monotonic
//!   counter), so the push is a no-op until Phase 9.
//! - **No SLOT_VERSIONS table / version bump.** v1 doesn't reuse
//!   slots, so the version bump (spec §4) has no observable effect.
//!   `MemoryId` already encodes the version, and stale references
//!   already mismatch via the existing slot-version field.
//! - **Adjacent edges only.** Spec §6: we delete `EDGES_OUT` where
//!   `source = id` and `EDGES_IN` where `target = id`. Other-direction
//!   dangling edges (`EDGES_OUT` where `target = id`) survive — the
//!   edge-scrub worker (sub-task 8.9) cleans those up.
//! - **No HNSW node deletion.** Spec §7 — the HNSW node referencing
//!   the reclaimed slot is left for the maintenance worker (8.5) to
//!   rebuild away.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use brain_core::MemoryId;
use brain_metadata::tables::edge::{EDGES_IN_TABLE, EDGES_OUT_TABLE};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use redb::ReadableTable;
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Spec §14 — default 7-day FORGET grace window. Configurable per
/// worker via [`SlotReclamationWorker::with_grace_period`].
pub const DEFAULT_FORGET_GRACE: Duration = Duration::from_secs(7 * 24 * 3600);

pub struct SlotReclamationWorker {
    config: WorkerConfig,
    grace_period: Duration,
}

impl SlotReclamationWorker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::SlotReclamation),
            grace_period: DEFAULT_FORGET_GRACE,
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    #[must_use]
    pub fn with_grace_period(mut self, d: Duration) -> Self {
        self.grace_period = d;
        self
    }

    #[must_use]
    pub fn grace_period(&self) -> Duration {
        self.grace_period
    }
}

impl Default for SlotReclamationWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for SlotReclamationWorker {
    fn name(&self) -> &'static str {
        WorkerKind::SlotReclamation.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::SlotReclamation
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_reclaim_cycle(self, ctx))
    }
}

async fn do_reclaim_cycle(
    worker: &SlotReclamationWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    let cfg = worker.config.clone();
    if cfg.batch_size == 0 {
        return Ok(0);
    }
    let grace_nanos = u64::try_from(worker.grace_period.as_nanos()).unwrap_or(u64::MAX);
    let now_nanos = now_unix_nanos();
    let cutoff_nanos = now_nanos.saturating_sub(grace_nanos);
    let metadata = ctx.ops.executor.metadata.clone();
    let started = Instant::now();

    // ── Scan phase: collect candidates above the cutoff. Bounded by
    //    batch_size + max_runtime. Reads run inside one read txn
    //    with the mutex held; no .await crosses the lock. ─────────
    let candidates: Vec<MemoryId> = {
        let db = metadata.lock();
        let rtxn = db
            .read_txn()
            .map_err(|e| WorkerError::Ops(format!("reclaim read_txn: {e:?}")))?;
        let table = rtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| WorkerError::Ops(format!("open MEMORIES: {e:?}")))?;
        let mut out = Vec::with_capacity(cfg.batch_size.min(1024));
        for entry in table
            .iter()
            .map_err(|e| WorkerError::Ops(format!("iter MEMORIES: {e:?}")))?
        {
            let (key, value) = entry.map_err(|e| WorkerError::Ops(format!("row: {e:?}")))?;
            let meta = value.value();
            let Some(ts) = meta.tombstoned_at_unix_nanos else {
                continue;
            };
            if ts >= cutoff_nanos {
                continue;
            }
            out.push(MemoryId::from_be_bytes(key.value()));
            if out.len() >= cfg.batch_size {
                break;
            }
        }
        out
    };

    if candidates.is_empty() {
        return Ok(0);
    }

    // ── Reclaim phase: one wtxn per memory (spec §8). ────────────
    let mut reclaimed = 0usize;
    for id in candidates {
        if started.elapsed() >= cfg.max_runtime {
            break;
        }
        if ctx.is_shutdown() {
            break;
        }
        if reclaim_one(&metadata, id, cutoff_nanos)? {
            reclaimed += 1;
        }
        // Yield between reclamations so we don't monopolise the mutex.
        glommio::executor().yield_if_needed().await;
    }

    trace!(
        reclaimed,
        cycle_ms = started.elapsed().as_millis() as u64,
        "slot reclamation cycle"
    );
    Ok(reclaimed)
}

/// Atomically delete one tombstoned memory + its adjacent edges.
/// Returns `true` if the row was reclaimed; `false` if the race-guard
/// rejected it (row gone, no longer tombstoned, or no longer past
/// cutoff). All in one wtxn.
fn reclaim_one(
    metadata: &brain_planner::SharedMetadataDb,
    id: MemoryId,
    cutoff_nanos: u64,
) -> Result<bool, WorkerError> {
    let mut db = metadata.lock();
    let wtxn = db
        .write_txn()
        .map_err(|e| WorkerError::Ops(format!("reclaim_one write_txn: {e:?}")))?;
    let did_remove = {
        let mut memories = wtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| WorkerError::Ops(format!("open MEMORIES: {e:?}")))?;
        let key = id.to_be_bytes();
        let row = memories
            .get(key)
            .map_err(|e| WorkerError::Ops(format!("memories get: {e:?}")))?
            .map(|a| a.value());
        // Race guards:
        //   - row gone (vanished between scan and reclaim) → false.
        //   - tombstoned_at unset (defensive; covers a future
        //     ADMIN_RESTORE) → false.
        //   - tombstoned_at >= cutoff (set-once but defensive) → false.
        let eligible = matches!(
            row.as_ref().and_then(|m| m.tombstoned_at_unix_nanos),
            Some(ts) if ts < cutoff_nanos
        );
        if eligible {
            memories
                .remove(key)
                .map_err(|e| WorkerError::Ops(format!("memories remove: {e:?}")))?;
        }
        eligible
    };

    if did_remove {
        purge_adjacent_edges(&wtxn, id)?;
    }

    wtxn.commit()
        .map_err(|e| WorkerError::Ops(format!("reclaim commit: {e:?}")))?;
    Ok(did_remove)
}

/// Remove all rows from `EDGES_OUT` keyed by `(id, *, *)` and from
/// `EDGES_IN` keyed by `(id, *, *)`. Spec §6 — "adjacent" edges only;
/// dangling edges from other memories pointing to `id` are left for
/// the edge-scrub worker.
fn purge_adjacent_edges(wtxn: &redb::WriteTransaction, id: MemoryId) -> Result<(), WorkerError> {
    let id_bytes = id.to_be_bytes();
    let lo = (id_bytes, 0u8, [0u8; 16]);
    let hi = (id_bytes, u8::MAX, [0xFFu8; 16]);

    // EDGES_OUT: source = id.
    {
        let mut out = wtxn
            .open_table(EDGES_OUT_TABLE)
            .map_err(|e| WorkerError::Ops(format!("open EDGES_OUT: {e:?}")))?;
        let victims: Vec<_> = out
            .range(lo..=hi)
            .map_err(|e| WorkerError::Ops(format!("EDGES_OUT range: {e:?}")))?
            .map(|entry| match entry {
                Ok((k, _)) => Ok(k.value()),
                Err(e) => Err(WorkerError::Ops(format!("EDGES_OUT row: {e:?}"))),
            })
            .collect::<Result<Vec<_>, _>>()?;
        for k in victims {
            out.remove(&k)
                .map_err(|e| WorkerError::Ops(format!("EDGES_OUT remove: {e:?}")))?;
        }
    }

    // EDGES_IN: target = id (key is (target, kind, source)).
    {
        let mut in_table = wtxn
            .open_table(EDGES_IN_TABLE)
            .map_err(|e| WorkerError::Ops(format!("open EDGES_IN: {e:?}")))?;
        let victims: Vec<_> = in_table
            .range(lo..=hi)
            .map_err(|e| WorkerError::Ops(format!("EDGES_IN range: {e:?}")))?
            .map(|entry| match entry {
                Ok((k, _)) => Ok(k.value()),
                Err(e) => Err(WorkerError::Ops(format!("EDGES_IN row: {e:?}"))),
            })
            .collect::<Result<Vec<_>, _>>()?;
        for k in victims {
            in_table
                .remove(&k)
                .map_err(|e| WorkerError::Ops(format!("EDGES_IN remove: {e:?}")))?;
        }
    }

    Ok(())
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
