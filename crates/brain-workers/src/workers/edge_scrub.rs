//! Edge scrub worker.
//!
//! Removes dangling edge entries left behind by slot reclamation.
//! 1: when memory M is reclaimed, rows keyed at M are
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
//! - Pre-compute scrub at reclamation time (4) is out of
//!   scope. Periodic full-scan is simpler.

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use brain_core::NodeRef;
use brain_metadata::tables::edge::{EdgeKey, EDGES_REVERSE_TABLE, EDGES_TABLE};
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
    /// Encoded `EdgeKey` cursor into the unified edge table. `None`
    /// means start of table; lost on restart, idempotent (already-
    /// deleted rows are no-ops).
    out_cursor: Mutex<Option<Vec<u8>>>,
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

    // ── Step A: collect orphans from EDGES_OUT above cursor. ────
    let start_cursor = worker.out_cursor.lock().clone();
    let out_orphans = collect_orphans_out(
        &metadata,
        start_cursor,
        cfg.batch_size,
        &started,
        cfg.max_runtime,
    )?;

    // ── Step B: delete EDGES_OUT orphans + their mirrors. ───────
    let mut total_removed = 0usize;
    if !out_orphans.victims.is_empty() {
        let wtxn = metadata
            .write_txn()
            .map_err(|e| WorkerError::Ops(format!("scrub out wtxn: {e:?}")))?;
        {
            let mut out = wtxn
                .open_table(EDGES_TABLE)
                .map_err(|e| WorkerError::Ops(format!("open EDGES: {e:?}")))?;
            let mut rev = wtxn
                .open_table(EDGES_REVERSE_TABLE)
                .map_err(|e| WorkerError::Ops(format!("open EDGES_REVERSE: {e:?}")))?;
            for key in &out_orphans.victims {
                let fwd_bytes = key.encode();
                let rev_key = EdgeKey {
                    from: key.to,
                    kind: key.kind,
                    to: key.from,
                    disambiguator: key.disambiguator,
                };
                let rev_bytes = rev_key.encode();
                out.remove(fwd_bytes.as_slice())
                    .map_err(|e| WorkerError::Ops(format!("EDGES remove: {e:?}")))?;
                rev.remove(rev_bytes.as_slice())
                    .map_err(|e| WorkerError::Ops(format!("EDGES_REVERSE mirror remove: {e:?}")))?;
                total_removed += 1;
            }
        }
        wtxn.commit()
            .map_err(|e| WorkerError::Ops(format!("scrub out commit: {e:?}")))?;
    }

    // Advance / wrap cursor based on step A's scan progress.
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

    // ── Step C: full pass over EDGES_IN; catch orphans where the
    //    source is dead. ──────────────────────────────────────────
    if !ctx.is_shutdown() && started.elapsed() < cfg.max_runtime {
        let in_orphans = collect_orphans_in(&metadata, cfg.batch_size, &started, cfg.max_runtime)?;
        if !in_orphans.victims.is_empty() {
            let wtxn = metadata
                .write_txn()
                .map_err(|e| WorkerError::Ops(format!("scrub in wtxn: {e:?}")))?;
            {
                let mut rev = wtxn
                    .open_table(EDGES_REVERSE_TABLE)
                    .map_err(|e| WorkerError::Ops(format!("open EDGES_REVERSE: {e:?}")))?;
                let mut out = wtxn
                    .open_table(EDGES_TABLE)
                    .map_err(|e| WorkerError::Ops(format!("open EDGES: {e:?}")))?;
                for key in &in_orphans.victims {
                    // `key` is keyed from the reverse table perspective
                    // (from = victim's target, to = victim's source).
                    let rev_bytes = key.encode();
                    let fwd_key = EdgeKey {
                        from: key.to,
                        kind: key.kind,
                        to: key.from,
                        disambiguator: key.disambiguator,
                    };
                    let fwd_bytes = fwd_key.encode();
                    rev.remove(rev_bytes.as_slice())
                        .map_err(|e| WorkerError::Ops(format!("EDGES_REVERSE remove: {e:?}")))?;
                    out.remove(fwd_bytes.as_slice())
                        .map_err(|e| WorkerError::Ops(format!("EDGES mirror remove: {e:?}")))?;
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
    /// Decoded edge keys (forward perspective) ready to remove.
    victims: Vec<EdgeKey>,
    /// Cursor bytes (raw encoded key) where the last scan stopped.
    last_scanned: Option<Vec<u8>>,
    scanned_to_end: bool,
}

struct InOrphans {
    /// Decoded edge keys (reverse perspective: `from` is the
    /// reverse-table anchor = forward target).
    victims: Vec<EdgeKey>,
}

fn collect_orphans_out(
    metadata: &brain_planner::SharedMetadataDb,
    start_cursor: Option<Vec<u8>>,
    batch_size: usize,
    started: &Instant,
    max_runtime: std::time::Duration,
) -> Result<OutOrphans, WorkerError> {
    let rtxn = metadata
        .read_txn()
        .map_err(|e| WorkerError::Ops(format!("scrub out rtxn: {e:?}")))?;
    let out = rtxn
        .open_table(EDGES_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open EDGES: {e:?}")))?;
    let memories = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open MEMORIES: {e:?}")))?;

    // Cursor strategy: start from the cursor bytes + 0x00 suffix so the
    // next scan begins strictly after the last row we saw. An empty
    // cursor starts at the table beginning.
    let from_bytes: Vec<u8> = match start_cursor.as_ref() {
        Some(b) => {
            let mut next = b.clone();
            next.push(0);
            next
        }
        None => Vec::new(),
    };

    let mut victims = Vec::with_capacity(batch_size.min(1024));
    let mut last_scanned: Option<Vec<u8>> = start_cursor;
    let mut scanned_to_end = true;
    let mut scanned = 0usize;

    for entry in out
        .range::<&[u8]>(from_bytes.as_slice()..)
        .map_err(|e| WorkerError::Ops(format!("EDGES range: {e:?}")))?
    {
        let (key, _) = entry.map_err(|e| WorkerError::Ops(format!("EDGES row: {e:?}")))?;
        let bytes = key.value().to_vec();
        let decoded = EdgeKey::decode(&bytes)
            .map_err(|e| WorkerError::Ops(format!("EDGES key decode: {e:?}")))?;
        last_scanned = Some(bytes);
        scanned += 1;

        if !endpoints_alive(&memories, decoded.from, decoded.to)? {
            victims.push(decoded);
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
    let rtxn = metadata
        .read_txn()
        .map_err(|e| WorkerError::Ops(format!("scrub in rtxn: {e:?}")))?;
    let in_t = rtxn
        .open_table(EDGES_REVERSE_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open EDGES_REVERSE: {e:?}")))?;
    let memories = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open MEMORIES: {e:?}")))?;

    let mut victims = Vec::with_capacity(batch_size.min(1024));
    let mut scanned = 0usize;

    for entry in in_t
        .iter()
        .map_err(|e| WorkerError::Ops(format!("EDGES_REVERSE iter: {e:?}")))?
    {
        let (key, _) = entry.map_err(|e| WorkerError::Ops(format!("EDGES_REVERSE row: {e:?}")))?;
        let decoded = EdgeKey::decode(key.value())
            .map_err(|e| WorkerError::Ops(format!("EDGES_REVERSE key decode: {e:?}")))?;
        scanned += 1;

        if !endpoints_alive(&memories, decoded.from, decoded.to)? {
            victims.push(decoded);
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

/// Are both endpoints live in `MEMORIES_TABLE`? Non-`Memory` endpoints
/// (entities) are considered alive — entity liveness is the typed-graph
/// layer's responsibility, not this scrub worker.
fn endpoints_alive(
    memories: &redb::ReadOnlyTable<[u8; 16], brain_metadata::tables::memory::MemoryMetadata>,
    from: NodeRef,
    to: NodeRef,
) -> Result<bool, WorkerError> {
    let alive = |n: NodeRef| -> Result<bool, WorkerError> {
        match n {
            NodeRef::Memory(m) => {
                let row = memories
                    .get(m.to_be_bytes())
                    .map_err(|e| WorkerError::Ops(format!("memory get: {e:?}")))?;
                Ok(row.is_some())
            }
            NodeRef::Entity(_) => Ok(true),
        }
    };
    Ok(alive(from)? && alive(to)?)
}
