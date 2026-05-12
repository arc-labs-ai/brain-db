//! Decay worker (sub-task 8.2). Spec §11/02.
//!
//! Applies time-based salience decay to memories. The closed-form
//! `s(t) = s_0 × 2^(-t/h)` is recomputed each cycle from
//! `salience_initial` and `created_at_unix_nanos` — spec §2 + §8.
//! Boost (sub-task 8.3) re-asserts its 10 % bump on the next 10 s
//! cycle; decay re-asserts the closed form on the next 1 h cycle.
//! Decay is therefore idempotent and restart-safe (spec §11/00 §13).
//!
//! Cycle structure: one read txn snapshots up to `batch_size`
//! memories above the in-process cursor; one write txn applies the
//! deltas. Memories whose new salience differs from current by
//! < 0.001 are skipped (spec §6). The cursor advances to the last
//! *scanned* id (not last updated) so the minor-change filter
//! doesn't stall progress.

use std::future::Future;
use std::pin::Pin;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use brain_core::{MemoryId, MemoryKind};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use parking_lot::Mutex;
use redb::ReadableTable;
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

// ---------------------------------------------------------------------------
// Decay constants — spec §10.
// ---------------------------------------------------------------------------

pub const EPISODIC_HALF_LIFE_DAYS: f64 = 30.0;
pub const SEMANTIC_HALF_LIFE_DAYS: f64 = 365.0;
pub const CONSOLIDATED_HALF_LIFE_DAYS: f64 = 90.0;

/// Spec §6 — writes below this delta are skipped (avoids many tiny
/// no-op updates dirtying redb pages).
pub const MIN_DELTA_FOR_WRITE: f32 = 0.001;

const NANOS_PER_DAY: f64 = 86_400.0 * 1_000_000_000.0;

/// Spec §10 — half-life by `MemoryKind`.
#[must_use]
pub fn half_life_days(kind: MemoryKind) -> f64 {
    match kind {
        MemoryKind::Episodic => EPISODIC_HALF_LIFE_DAYS,
        MemoryKind::Semantic => SEMANTIC_HALF_LIFE_DAYS,
        MemoryKind::Consolidated => CONSOLIDATED_HALF_LIFE_DAYS,
    }
}

/// Spec §2 — closed-form decay. Reads only immutable post-ENCODE
/// fields (`salience_initial`, `created_at_unix_nanos`, `kind`),
/// so the result is deterministic regardless of prior decay/boost
/// writes. Clamps at `>= 0.0`.
#[must_use]
pub fn decayed_salience(salience_initial: f32, age_unix_nanos: u64, kind: MemoryKind) -> f32 {
    let age_days = (age_unix_nanos as f64) / NANOS_PER_DAY;
    let h = half_life_days(kind);
    let factor = (-age_days / h).exp2();
    let s = (f64::from(salience_initial) * factor).max(0.0);
    // f64 → f32 cast saturates at f32::MAX; we never get NaN/Inf here
    // because age_days is finite and h > 0.
    s as f32
}

/// Compute the new salience for a memory row using `now_unix_nanos`
/// as the reference clock. Returns `None` if the row's `kind` byte is
/// invalid (corrupt row — caller should skip).
fn compute_decayed(meta: &MemoryMetadata, now_unix_nanos: u64) -> Option<f32> {
    let kind = meta.kind().ok()?;
    let age = now_unix_nanos.saturating_sub(meta.created_at_unix_nanos);
    Some(decayed_salience(meta.salience_initial, age, kind))
}

// ---------------------------------------------------------------------------
// DecayWorker.
// ---------------------------------------------------------------------------

/// In-process decay cursor. `None` means "start from the beginning of
/// MEMORIES_TABLE." Spec §5: a full pass resets back to `None`. Lost
/// on restart — spec §11/00 §10 allows this since decay is
/// idempotent.
pub struct DecayWorker {
    config: WorkerConfig,
    cursor: Mutex<Option<MemoryId>>,
}

impl DecayWorker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::Decay),
            cursor: Mutex::new(None),
        }
    }

    /// Override the default config. Tests use this to set very small
    /// batch_sizes / max_runtimes; operators can use it to tune.
    #[must_use]
    pub fn with_config(mut self, config: WorkerConfig) -> Self {
        self.config = config;
        self
    }
}

impl Default for DecayWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for DecayWorker {
    fn name(&self) -> &'static str {
        WorkerKind::Decay.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::Decay
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + Send + 'a>> {
        Box::pin(do_decay_cycle(self, ctx))
    }
}

async fn do_decay_cycle(worker: &DecayWorker, ctx: &WorkerContext) -> Result<usize, WorkerError> {
    let cfg = worker.config.clone();
    if cfg.batch_size == 0 {
        return Ok(0);
    }
    let now_nanos = now_unix_nanos();
    let start_cursor: Option<MemoryId> = *worker.cursor.lock();
    let metadata = ctx.ops.executor.metadata.clone();
    let started = Instant::now();

    // ── Read phase: snapshot the batch. ──────────────────────────
    let mut updates: Vec<(MemoryId, f32)> = Vec::with_capacity(cfg.batch_size.min(1024));
    let mut last_scanned: Option<MemoryId> = start_cursor;
    let mut scanned = 0usize;
    let mut scanned_to_end = true;
    {
        let db = metadata.lock();
        let rtxn = db
            .read_txn()
            .map_err(|e| WorkerError::Ops(format!("decay read_txn: {e:?}")))?;
        let table = rtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| WorkerError::Ops(format!("decay open MEMORIES: {e:?}")))?;

        // We want strictly > cursor; redb's range API is half-open by
        // default. We approximate "> last" by bumping the cursor's
        // last byte (cheap because keys are u128 big-endian).
        let from_key: [u8; 16] = match start_cursor {
            Some(id) => bump_be_u128(id.to_be_bytes()),
            None => [0u8; 16],
        };

        // CLAUDE.md anti-pattern: don't hold a lock across `.await`.
        // The read txn holds `db` (a parking_lot MutexGuard); we keep
        // the entire scan synchronous inside this block. max_runtime
        // still bounds wall-clock; yields happen between phases.
        for entry in table
            .range(from_key..)
            .map_err(|e| WorkerError::Ops(format!("decay range: {e:?}")))?
        {
            let (key, value) = entry.map_err(|e| WorkerError::Ops(format!("decay row: {e:?}")))?;
            let id_bytes: [u8; 16] = key.value();
            let meta = value.value();

            last_scanned = Some(MemoryId::from_be_bytes(id_bytes));
            scanned += 1;

            if let Some(new_sal) = compute_decayed(&meta, now_nanos) {
                if (new_sal - meta.salience).abs() >= MIN_DELTA_FOR_WRITE {
                    updates.push((MemoryId::from_be_bytes(id_bytes), new_sal));
                }
            }

            if scanned >= cfg.batch_size {
                scanned_to_end = false;
                break;
            }
            if started.elapsed() >= cfg.max_runtime {
                scanned_to_end = false;
                break;
            }
            if ctx.is_shutdown() {
                scanned_to_end = false;
                break;
            }
        }
    }
    // Yield between read and write phases (spec §11/01 §6 yield
    // discipline). We can't yield mid-scan because the read txn holds
    // a parking_lot MutexGuard; the scan is bounded by max_runtime.
    tokio::task::yield_now().await;

    // ── Write phase: apply updates atomically in one wtxn. ───────
    let n_updates = updates.len();
    if !updates.is_empty() {
        let mut db = metadata.lock();
        let wtxn = db
            .write_txn()
            .map_err(|e| WorkerError::Ops(format!("decay write_txn: {e:?}")))?;
        {
            let mut table = wtxn
                .open_table(MEMORIES_TABLE)
                .map_err(|e| WorkerError::Ops(format!("decay open MEMORIES (w): {e:?}")))?;
            for (id, new_sal) in &updates {
                let key = id.to_be_bytes();
                let prior = table
                    .get(key)
                    .map_err(|e| WorkerError::Ops(format!("decay get: {e:?}")))?
                    .map(|access| access.value());
                if let Some(mut meta) = prior {
                    meta.salience = *new_sal;
                    table
                        .insert(key, meta)
                        .map_err(|e| WorkerError::Ops(format!("decay insert: {e:?}")))?;
                }
            }
        }
        wtxn.commit()
            .map_err(|e| WorkerError::Ops(format!("decay commit: {e:?}")))?;
    }

    // ── Cursor advance. ──────────────────────────────────────────
    let mut cursor = worker.cursor.lock();
    *cursor = if scanned_to_end {
        // Wrap to start of table next cycle.
        None
    } else {
        last_scanned
    };
    drop(cursor);

    trace!(
        scanned,
        updated = n_updates,
        cycle_ms = started.elapsed().as_millis() as u64,
        "decay cycle"
    );

    Ok(n_updates)
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Big-endian increment by 1. Saturates at all-1s (no wraparound —
/// callers will simply iterate over an empty range, which is what we
/// want at the top of the table).
fn bump_be_u128(mut bytes: [u8; 16]) -> [u8; 16] {
    for i in (0..16).rev() {
        let (v, overflow) = bytes[i].overflowing_add(1);
        bytes[i] = v;
        if !overflow {
            return bytes;
        }
    }
    [0xFF; 16]
}

// Compile-time Send + Sync guard.
const _: fn() = || {
    fn require<T: Send + Sync>() {}
    require::<DecayWorker>();
};

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn bump_be_u128_increments() {
        assert_eq!(bump_be_u128([0; 16])[15], 1);
        let mut b = [0u8; 16];
        b[15] = 0xFF;
        let r = bump_be_u128(b);
        assert_eq!(r[14], 1);
        assert_eq!(r[15], 0);
    }

    #[test]
    fn bump_be_u128_saturates_at_max() {
        let r = bump_be_u128([0xFF; 16]);
        assert_eq!(r, [0xFF; 16]);
    }
}
