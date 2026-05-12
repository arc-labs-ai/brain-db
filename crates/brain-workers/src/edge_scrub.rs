//! Edge scrub worker (sub-task 8.9). Spec §11/08 §1.
//!
//! Removes dangling edge entries left behind by slot reclamation.
//! Spec §1.1: when memory M is reclaimed, rows keyed at M are
//! deleted, but the paired entries from live memories survive:
//!
//! - `EDGES_OUT[X, kind, M]` — X alive, target M dead.
//! - `EDGES_IN[X, kind, M]` — X alive, source M dead.
//!
//! Each cycle walks both tables and removes any row whose source or
//! target is no longer in `MEMORIES_TABLE`. The mirror in the other
//! direction is removed in the same wtxn.
//!
//! ## v1 trade-offs
//!
//! - `EDGES_OUT` is cursor-driven across cycles. `EDGES_IN` gets a
//!   full pass per cycle. After slot reclamation does its job
//!   `EDGES_IN` is the smaller of the two; a second cursor adds
//!   complexity v1 doesn't need.
//! - Pre-compute scrub at reclamation time (spec §1.4) is out of
//!   scope. Periodic full-scan is simpler.

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use brain_core::MemoryId;
use brain_metadata::tables::edge::{EdgeKey, EDGES_IN_TABLE, EDGES_OUT_TABLE};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use parking_lot::Mutex;
use redb::ReadableTable;
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

pub struct EdgeScrubWorker {
    config: WorkerConfig,
    /// Cursor into `EDGES_OUT`. `None` means start of table. Spec
    /// §11/00 §10: lost on restart, idempotent (already-deleted
    /// rows are no-ops).
    out_cursor: Mutex<Option<EdgeKey>>,
}

impl EdgeScrubWorker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::EdgeScrub),
            out_cursor: Mutex::new(None),
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }
}

impl Default for EdgeScrubWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for EdgeScrubWorker {
    fn name(&self) -> &'static str {
        WorkerKind::EdgeScrub.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::EdgeScrub
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_scrub_cycle(self, ctx))
    }
}

async fn do_scrub_cycle(
    worker: &EdgeScrubWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    let cfg = worker.config.clone();
    if cfg.batch_size == 0 {
        return Ok(0);
    }
    let metadata = ctx.ops.executor.metadata.clone();
    let started = Instant::now();

    // ── Phase A: collect orphans from EDGES_OUT above cursor. ────
    let start_cursor = *worker.out_cursor.lock();
    let out_orphans = collect_orphans_out(
        &metadata,
        start_cursor,
        cfg.batch_size,
        &started,
        cfg.max_runtime,
    )?;

    // ── Phase B: delete EDGES_OUT orphans + their mirrors. ───────
    let mut total_removed = 0usize;
    if !out_orphans.victims.is_empty() {
        let mut db = metadata.lock();
        let wtxn = db
            .write_txn()
            .map_err(|e| WorkerError::Ops(format!("scrub out wtxn: {e:?}")))?;
        {
            let mut out = wtxn
                .open_table(EDGES_OUT_TABLE)
                .map_err(|e| WorkerError::Ops(format!("open EDGES_OUT: {e:?}")))?;
            let mut in_t = wtxn
                .open_table(EDGES_IN_TABLE)
                .map_err(|e| WorkerError::Ops(format!("open EDGES_IN: {e:?}")))?;
            for (source, kind, target) in &out_orphans.victims {
                let s = source.to_be_bytes();
                let t = target.to_be_bytes();
                out.remove(&(s, *kind, t))
                    .map_err(|e| WorkerError::Ops(format!("EDGES_OUT remove: {e:?}")))?;
                // Mirror in EDGES_IN keyed (target, kind, source).
                in_t.remove(&(t, *kind, s))
                    .map_err(|e| WorkerError::Ops(format!("EDGES_IN mirror remove: {e:?}")))?;
                total_removed += 1;
            }
        }
        wtxn.commit()
            .map_err(|e| WorkerError::Ops(format!("scrub out commit: {e:?}")))?;
    }

    // Advance / wrap cursor based on phase A's scan progress.
    {
        let mut cursor = worker.out_cursor.lock();
        *cursor = if out_orphans.scanned_to_end {
            None
        } else {
            out_orphans.last_scanned
        };
    }

    // Yield between phases (mutex never held across .await).
    glommio::executor().yield_if_needed().await;

    // ── Phase C: full pass over EDGES_IN; catch orphans where the
    //    source is dead. ──────────────────────────────────────────
    if !ctx.is_shutdown() && started.elapsed() < cfg.max_runtime {
        let in_orphans = collect_orphans_in(&metadata, cfg.batch_size, &started, cfg.max_runtime)?;
        if !in_orphans.victims.is_empty() {
            let mut db = metadata.lock();
            let wtxn = db
                .write_txn()
                .map_err(|e| WorkerError::Ops(format!("scrub in wtxn: {e:?}")))?;
            {
                let mut in_t = wtxn
                    .open_table(EDGES_IN_TABLE)
                    .map_err(|e| WorkerError::Ops(format!("open EDGES_IN: {e:?}")))?;
                let mut out = wtxn
                    .open_table(EDGES_OUT_TABLE)
                    .map_err(|e| WorkerError::Ops(format!("open EDGES_OUT: {e:?}")))?;
                for (target, kind, source) in &in_orphans.victims {
                    let t = target.to_be_bytes();
                    let s = source.to_be_bytes();
                    in_t.remove(&(t, *kind, s))
                        .map_err(|e| WorkerError::Ops(format!("EDGES_IN remove: {e:?}")))?;
                    // Mirror in EDGES_OUT keyed (source, kind, target).
                    out.remove(&(s, *kind, t))
                        .map_err(|e| WorkerError::Ops(format!("EDGES_OUT mirror remove: {e:?}")))?;
                    total_removed += 1;
                }
            }
            wtxn.commit()
                .map_err(|e| WorkerError::Ops(format!("scrub in commit: {e:?}")))?;
        }
    }

    trace!(
        removed = total_removed,
        cycle_ms = started.elapsed().as_millis() as u64,
        "edge scrub cycle"
    );
    Ok(total_removed)
}

// ---------------------------------------------------------------------------
// Phase helpers.
// ---------------------------------------------------------------------------

struct OutOrphans {
    /// Tuples (source, kind_byte, target) ready to remove.
    victims: Vec<(MemoryId, u8, MemoryId)>,
    last_scanned: Option<EdgeKey>,
    scanned_to_end: bool,
}

struct InOrphans {
    /// Tuples (target, kind_byte, source) — EDGES_IN's natural key.
    victims: Vec<(MemoryId, u8, MemoryId)>,
}

fn collect_orphans_out(
    metadata: &brain_planner::SharedMetadataDb,
    start_cursor: Option<EdgeKey>,
    batch_size: usize,
    started: &Instant,
    max_runtime: std::time::Duration,
) -> Result<OutOrphans, WorkerError> {
    let db = metadata.lock();
    let rtxn = db
        .read_txn()
        .map_err(|e| WorkerError::Ops(format!("scrub out rtxn: {e:?}")))?;
    let out = rtxn
        .open_table(EDGES_OUT_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open EDGES_OUT: {e:?}")))?;
    let memories = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open MEMORIES: {e:?}")))?;

    let from_key: EdgeKey = match start_cursor {
        Some(k) => bump_edge_key(k),
        None => ([0u8; 16], 0u8, [0u8; 16]),
    };

    let mut victims = Vec::with_capacity(batch_size.min(1024));
    let mut last_scanned = start_cursor;
    let mut scanned_to_end = true;
    let mut scanned = 0usize;

    for entry in out
        .range(from_key..)
        .map_err(|e| WorkerError::Ops(format!("EDGES_OUT range: {e:?}")))?
    {
        let (key, _) = entry.map_err(|e| WorkerError::Ops(format!("EDGES_OUT row: {e:?}")))?;
        let (s, k, t) = key.value();
        last_scanned = Some((s, k, t));
        scanned += 1;

        let source = MemoryId::from_be_bytes(s);
        let target = MemoryId::from_be_bytes(t);
        let source_alive = memories
            .get(s)
            .map_err(|e| WorkerError::Ops(format!("memory get src: {e:?}")))?
            .is_some();
        let target_alive = memories
            .get(t)
            .map_err(|e| WorkerError::Ops(format!("memory get tgt: {e:?}")))?
            .is_some();
        if !source_alive || !target_alive {
            victims.push((source, k, target));
        }

        if scanned >= batch_size {
            scanned_to_end = false;
            break;
        }
        if started.elapsed() >= max_runtime {
            scanned_to_end = false;
            break;
        }
    }

    Ok(OutOrphans {
        victims,
        last_scanned,
        scanned_to_end,
    })
}

fn collect_orphans_in(
    metadata: &brain_planner::SharedMetadataDb,
    batch_size: usize,
    started: &Instant,
    max_runtime: std::time::Duration,
) -> Result<InOrphans, WorkerError> {
    let db = metadata.lock();
    let rtxn = db
        .read_txn()
        .map_err(|e| WorkerError::Ops(format!("scrub in rtxn: {e:?}")))?;
    let in_t = rtxn
        .open_table(EDGES_IN_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open EDGES_IN: {e:?}")))?;
    let memories = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open MEMORIES: {e:?}")))?;

    let mut victims = Vec::with_capacity(batch_size.min(1024));
    let mut scanned = 0usize;

    for entry in in_t
        .iter()
        .map_err(|e| WorkerError::Ops(format!("EDGES_IN iter: {e:?}")))?
    {
        let (key, _) = entry.map_err(|e| WorkerError::Ops(format!("EDGES_IN row: {e:?}")))?;
        let (t, k, s) = key.value();
        scanned += 1;

        let target = MemoryId::from_be_bytes(t);
        let source = MemoryId::from_be_bytes(s);
        let source_alive = memories
            .get(s)
            .map_err(|e| WorkerError::Ops(format!("memory get src: {e:?}")))?
            .is_some();
        let target_alive = memories
            .get(t)
            .map_err(|e| WorkerError::Ops(format!("memory get tgt: {e:?}")))?
            .is_some();
        if !source_alive || !target_alive {
            victims.push((target, k, source));
        }

        if scanned >= batch_size {
            break;
        }
        if started.elapsed() >= max_runtime {
            break;
        }
    }

    Ok(InOrphans { victims })
}

/// Big-endian increment on the composite `EdgeKey` so we range
/// "strictly above" the cursor in the next scan. Saturates at the
/// max key.
fn bump_edge_key(key: EdgeKey) -> EdgeKey {
    let (mut s, mut k, mut t) = key;
    // Increment t first; if it wraps, bump k; if k wraps, bump s.
    let mut overflow = true;
    for i in (0..16).rev() {
        if !overflow {
            break;
        }
        let (v, o) = t[i].overflowing_add(1);
        t[i] = v;
        overflow = o;
    }
    if overflow {
        let (v, o) = k.overflowing_add(1);
        k = v;
        overflow = o;
    }
    if overflow {
        let mut over2 = true;
        for i in (0..16).rev() {
            if !over2 {
                break;
            }
            let (v, o) = s[i].overflowing_add(1);
            s[i] = v;
            over2 = o;
        }
        if over2 {
            // All bits set — return saturated max.
            return ([0xFFu8; 16], 0xFF, [0xFFu8; 16]);
        }
    }
    (s, k, t)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn bump_edge_key_increments_target() {
        let k0 = ([1u8; 16], 0u8, [0u8; 16]);
        let k1 = bump_edge_key(k0);
        assert_eq!(k1.0, k0.0);
        assert_eq!(k1.1, k0.1);
        assert_eq!(k1.2[15], 1);
    }

    #[test]
    fn bump_edge_key_carries_into_kind() {
        let k0 = ([1u8; 16], 5u8, [0xFFu8; 16]);
        let k1 = bump_edge_key(k0);
        assert_eq!(k1.0, k0.0);
        assert_eq!(k1.1, 6);
        assert_eq!(k1.2, [0u8; 16]);
    }

    #[test]
    fn bump_edge_key_saturates() {
        let k = bump_edge_key(([0xFFu8; 16], 0xFFu8, [0xFFu8; 16]));
        assert_eq!(k, ([0xFFu8; 16], 0xFF, [0xFFu8; 16]));
    }
}
