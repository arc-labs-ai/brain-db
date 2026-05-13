//! Counter reconciliation worker (sub-task 8.10). Spec §11/08 §2.
//!
//! Walks `MEMORIES_TABLE` and verifies each row's `edges_out_count` /
//! `edges_in_count` against the live edge tables. Drift gets fixed.
//! Spec §2.3 — drift is expected to be near-zero in normal operation;
//! a non-trivial rate indicates a bug worth investigating.
//!
//! v1 reconciles **only** per-memory edge counts. Other counters spec
//! §2.1 lists (`ContextMetadata.memory_count`, `AgentMetadata`
//! counters, per-shard cluster totals) lack the v1 plumbing — no
//! CONTEXTS_TABLE, no agent admin ops, no cluster layer. Phase 9.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use brain_core::MemoryId;
use brain_metadata::tables::edge::{EDGES_IN_TABLE, EDGES_OUT_TABLE};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use parking_lot::Mutex;
use redb::ReadableTable;
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

pub struct CounterReconcileWorker {
    config: WorkerConfig,
    /// Cursor across cycles. Walks `MEMORIES_TABLE` above this key.
    /// `None` means start. Lost on restart per spec §11/00 §10.
    cursor: Mutex<Option<MemoryId>>,
}

impl CounterReconcileWorker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::CounterReconcile),
            cursor: Mutex::new(None),
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }
}

impl Default for CounterReconcileWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for CounterReconcileWorker {
    fn name(&self) -> &'static str {
        WorkerKind::CounterReconcile.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::CounterReconcile
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_reconcile_cycle(self, ctx))
    }
}

async fn do_reconcile_cycle(
    worker: &CounterReconcileWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    let cfg = worker.config.clone();
    if cfg.batch_size == 0 {
        return Ok(0);
    }
    let metadata = ctx.ops.executor.metadata.clone();
    let started = Instant::now();
    let start_cursor = *worker.cursor.lock();

    // ── Phase A: read txn — collect mismatches (id, true_out,
    //    true_in) for candidates above the cursor. ────────────────
    let snapshot = collect_mismatches(&metadata, start_cursor, &cfg, &started)?;

    // ── Phase B: wtxn fixes the rows. ────────────────────────────
    let mut fixed = 0usize;
    if !snapshot.mismatches.is_empty() {
        let mut db = metadata.lock();
        let wtxn = db
            .write_txn()
            .map_err(|e| WorkerError::Ops(format!("reconcile wtxn: {e:?}")))?;
        {
            let mut memories = wtxn
                .open_table(MEMORIES_TABLE)
                .map_err(|e| WorkerError::Ops(format!("open MEMORIES: {e:?}")))?;
            for (id, true_out, true_in) in &snapshot.mismatches {
                let key = id.to_be_bytes();
                let row = memories
                    .get(key)
                    .map_err(|e| WorkerError::Ops(format!("memory get: {e:?}")))?
                    .map(|a| a.value());
                let Some(mut row) = row else { continue };
                // Re-check: drift may have been fixed by a writer
                // between phase A and B. Idempotent.
                if row.edges_out_count == *true_out && row.edges_in_count == *true_in {
                    continue;
                }
                row.edges_out_count = *true_out;
                row.edges_in_count = *true_in;
                memories
                    .insert(key, row)
                    .map_err(|e| WorkerError::Ops(format!("memory update: {e:?}")))?;
                fixed += 1;
            }
        }
        wtxn.commit()
            .map_err(|e| WorkerError::Ops(format!("reconcile commit: {e:?}")))?;
    }

    // Advance / wrap cursor.
    {
        let mut cursor = worker.cursor.lock();
        *cursor = if snapshot.scanned_to_end {
            None
        } else {
            snapshot.last_scanned
        };
    }

    if !snapshot.candidates_checked.is_empty() {
        let drift_rate =
            snapshot.mismatches.len() as f32 / snapshot.candidates_checked.len() as f32;
        trace!(
            scanned = snapshot.candidates_checked.len(),
            mismatched = snapshot.mismatches.len(),
            fixed,
            drift_rate,
            cycle_ms = started.elapsed().as_millis() as u64,
            "counter reconciliation cycle"
        );
    }
    Ok(fixed)
}

struct ReconcileSnapshot {
    candidates_checked: Vec<MemoryId>,
    /// (memory_id, true_edges_out, true_edges_in) for rows whose
    /// stored counts disagree with reality.
    mismatches: Vec<(MemoryId, u32, u32)>,
    last_scanned: Option<MemoryId>,
    scanned_to_end: bool,
}

fn collect_mismatches(
    metadata: &brain_planner::SharedMetadataDb,
    start_cursor: Option<MemoryId>,
    cfg: &WorkerConfig,
    started: &Instant,
) -> Result<ReconcileSnapshot, WorkerError> {
    let db = metadata.lock();
    let rtxn = db
        .read_txn()
        .map_err(|e| WorkerError::Ops(format!("reconcile rtxn: {e:?}")))?;
    let memories = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open MEMORIES: {e:?}")))?;
    let out_edges = rtxn
        .open_table(EDGES_OUT_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open EDGES_OUT: {e:?}")))?;
    let in_edges = rtxn
        .open_table(EDGES_IN_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open EDGES_IN: {e:?}")))?;

    let from_key: [u8; 16] = match start_cursor {
        Some(id) => bump_be_u128(id.to_be_bytes()),
        None => [0u8; 16],
    };

    let mut candidates_checked = Vec::with_capacity(cfg.batch_size.min(1024));
    let mut mismatches = Vec::new();
    let mut last_scanned: Option<MemoryId> = start_cursor;
    let mut scanned_to_end = true;

    for entry in memories
        .range(from_key..)
        .map_err(|e| WorkerError::Ops(format!("MEMORIES range: {e:?}")))?
    {
        let (key, value) = entry.map_err(|e| WorkerError::Ops(format!("memory row: {e:?}")))?;
        let id = MemoryId::from_be_bytes(key.value());
        last_scanned = Some(id);
        let meta: MemoryMetadata = value.value();

        let true_out = count_range(&out_edges, id)?;
        let true_in = count_range(&in_edges, id)?;
        candidates_checked.push(id);
        if meta.edges_out_count as u64 != true_out || meta.edges_in_count as u64 != true_in {
            // Saturating cast — u32 truncation is acceptable; counts
            // exceeding u32::MAX are pathological.
            let to = u32::try_from(true_out).unwrap_or(u32::MAX);
            let ti = u32::try_from(true_in).unwrap_or(u32::MAX);
            mismatches.push((id, to, ti));
        }

        if candidates_checked.len() >= cfg.batch_size {
            scanned_to_end = false;
            break;
        }
        if started.elapsed() >= cfg.max_runtime {
            scanned_to_end = false;
            break;
        }
    }

    Ok(ReconcileSnapshot {
        candidates_checked,
        mismatches,
        last_scanned,
        scanned_to_end,
    })
}

/// Count rows in a (source, kind, target)-keyed edge table where the
/// first 16 bytes match `id`. Works for both `EDGES_OUT` (counts
/// outgoing from `id`) and `EDGES_IN` (counts incoming to `id`).
fn count_range<T>(table: &T, id: MemoryId) -> Result<u64, WorkerError>
where
    T: ReadableTable<brain_metadata::tables::edge::EdgeKey, brain_metadata::tables::edge::EdgeData>,
{
    let id_bytes = id.to_be_bytes();
    let lo = (id_bytes, 0u8, [0u8; 16]);
    let hi = (id_bytes, u8::MAX, [0xFFu8; 16]);
    let r = table
        .range(lo..=hi)
        .map_err(|e| WorkerError::Ops(format!("edge range: {e:?}")))?;
    let mut n = 0u64;
    for entry in r {
        entry.map_err(|e| WorkerError::Ops(format!("edge row: {e:?}")))?;
        n += 1;
    }
    Ok(n)
}

fn bump_be_u128(mut bytes: [u8; 16]) -> [u8; 16] {
    for i in (0..16).rev() {
        let (v, overflow) = bytes[i].overflowing_add(1);
        bytes[i] = v;
        if !overflow {
            return bytes;
        }
    }
    [0xFFu8; 16]
}

// Silence unused-import in some build configs.
#[allow(dead_code)]
fn _bind_duration(_: Duration) {}
