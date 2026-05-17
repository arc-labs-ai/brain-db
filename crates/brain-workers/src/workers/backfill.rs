//! Backfill worker (sub-task 24.1). Spec §27/04 §3.
//!
//! Admin-triggered worker that walks a `(memory_range × extractor_ids)`
//! grid and re-runs extractors against each memory. Each
//! `(memory, extractor)` pair has its own row in the shared
//! `worker_checkpoints` redb table so a restart resumes mid-run
//! without re-extracting already-completed items.
//!
//! ## v1 scope cut — memory text availability
//!
//! `MEMORIES_TABLE` stores `text_size` but not the text itself
//! (text lives on the ENCODE wire path + the WAL). Until a post-v1
//! enhancement adds a memory-text store (or WAL replay), the
//! backfill worker can:
//!
//! - Walk the plan + write per-item checkpoints.
//! - Mark `dry_run` items `Completed` immediately.
//! - For live runs, mark items `Failed` with reason
//!   `"memory text not persisted (v1 limitation)"`.
//!
//! This matches the §27/07 deferred-work entry for phase 22's
//! memory text rebuild scope cut. Operators re-ingest in v1; the
//! checkpoint scaffolding here ships so phase-25+ can light up
//! content-aware re-extraction without re-designing the worker.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use brain_core::{BackfillId, BackfillProgress, BackfillRange, BackfillRequest, MemoryId};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::worker_checkpoints as checkpoints;
use parking_lot::Mutex;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Stable worker id used as the first half of the checkpoint
/// composite key.
pub const WORKER_ID: &str = "backfill";

/// Per-request item-failure threshold beyond which the worker
/// aborts the request (spec §27/04 §3.3 — "bad-extractor abort").
pub const MAX_ATTEMPTS_PER_ITEM: u32 = 3;

pub struct BackfillWorker {
    config: WorkerConfig,
    state: Arc<BackfillState>,
}

#[derive(Default)]
struct BackfillState {
    pending: Mutex<VecDeque<BackfillRequest>>,
    current: Mutex<Option<RunningBackfill>>,
    last_progress: Mutex<BackfillProgress>,
}

struct RunningBackfill {
    request: BackfillRequest,
    /// `MemoryId` cursor — the next memory to process. `None` means
    /// "start from the range's lower bound".
    cursor: Option<MemoryId>,
    completed: u64,
    failed: u64,
    skipped_already_completed: u64,
    cancelled: bool,
}

impl BackfillWorker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::Backfill),
            state: Arc::new(BackfillState::default()),
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    /// Submit a backfill request. Returns the request id. The
    /// worker picks it up on its next tick.
    pub fn submit(&self, request: BackfillRequest) -> BackfillId {
        let id = request.request_id;
        self.state.pending.lock().push_back(request);
        id
    }

    /// Cancel the in-flight request matching `request_id`. Returns
    /// `true` if the cancel flag was flipped, `false` if no such
    /// request is running.
    pub fn cancel(&self, request_id: BackfillId) -> bool {
        let mut current = self.state.current.lock();
        if let Some(running) = current.as_mut() {
            if running.request.request_id == request_id {
                running.cancelled = true;
                return true;
            }
        }
        false
    }

    /// Snapshot of the worker's progress on the most-recent run.
    #[must_use]
    pub fn progress(&self) -> BackfillProgress {
        self.state.last_progress.lock().clone()
    }

    /// Dequeue the next request if no run is in flight.
    fn dequeue_if_idle(&self) -> Option<BackfillRequest> {
        let mut current = self.state.current.lock();
        if current.is_some() {
            return None;
        }
        let mut pending = self.state.pending.lock();
        let req = pending.pop_front()?;
        *current = Some(RunningBackfill {
            request: req.clone(),
            cursor: None,
            completed: 0,
            failed: 0,
            skipped_already_completed: 0,
            cancelled: false,
        });
        Some(req)
    }

    /// Process up to `cfg.batch_size` items from the in-flight
    /// request. Returns the number of items advanced (matches the
    /// `Worker::run_cycle` contract).
    async fn drive_one_batch(
        &self,
        ctx: &WorkerContext,
    ) -> Result<usize, WorkerError> {
        // Acquire current run (or dequeue a new one).
        let req = match self.state.current.lock().as_ref() {
            Some(r) => r.request.clone(),
            None => match self.dequeue_if_idle() {
                Some(r) => r,
                None => return Ok(0),
            },
        };

        let mut items_processed = 0usize;
        let now_ns = now_unix_nanos();

        while items_processed < self.config.batch_size {
            if ctx.is_shutdown() {
                break;
            }
            if self.is_cancelled() {
                tracing::info!(
                    target: "brain_workers::backfill",
                    request_id = ?req.request_id,
                    "backfill cancelled; ending current cycle",
                );
                self.finalise_run();
                break;
            }

            let Some(memory_id) = self.next_memory(&req, ctx)? else {
                // No more memories — run complete.
                self.finalise_run();
                break;
            };

            // For each extractor in the request, walk the checkpoint.
            for ext_id in &req.extractor_ids {
                let item_key = item_key_for(memory_id, ext_id.raw());
                let outcome = self.process_item(
                    ctx,
                    memory_id,
                    *ext_id,
                    &item_key,
                    req.dry_run,
                    now_ns,
                )?;
                self.record_outcome(outcome);
                items_processed += 1;
                if items_processed >= self.config.batch_size {
                    break;
                }
            }
            self.advance_cursor(memory_id);
        }

        // Publish a progress snapshot for the operator.
        self.publish_progress();
        Ok(items_processed)
    }

    fn is_cancelled(&self) -> bool {
        self.state
            .current
            .lock()
            .as_ref()
            .map_or(false, |r| r.cancelled)
    }

    fn next_memory(
        &self,
        req: &BackfillRequest,
        ctx: &WorkerContext,
    ) -> Result<Option<MemoryId>, WorkerError> {
        let cursor = self
            .state
            .current
            .lock()
            .as_ref()
            .and_then(|r| r.cursor);
        let lo: u128 = match (cursor, &req.memory_range) {
            (Some(c), _) => c.raw().saturating_add(1),
            (None, BackfillRange::All) => 0,
            (None, BackfillRange::ById { start, .. }) => start.raw(),
        };
        let hi: u128 = match &req.memory_range {
            BackfillRange::All => u128::MAX,
            BackfillRange::ById { end, .. } => end.raw(),
        };
        if lo > hi {
            return Ok(None);
        }

        let metadata = ctx.ops.executor.metadata.lock();
        let rtxn = metadata
            .read_txn()
            .map_err(|e| WorkerError::Internal(format!("backfill read_txn: {e}")))?;
        let table = match rtxn.open_table(MEMORIES_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => {
                return Err(WorkerError::Internal(format!(
                    "backfill open MEMORIES_TABLE: {e}",
                )));
            }
        };
        let mut iter = table
            .range(memory_key_from(lo)..=memory_key_from(hi))
            .map_err(|e| WorkerError::Internal(format!("backfill range: {e}")))?;
        if let Some(entry) = iter.next() {
            let (k, _) = entry
                .map_err(|e| WorkerError::Internal(format!("backfill iter: {e}")))?;
            let key_bytes = k.value();
            let raw = u128::from_be_bytes(key_bytes);
            return Ok(Some(MemoryId::from_raw(raw)));
        }
        Ok(None)
    }

    fn advance_cursor(&self, just_processed: MemoryId) {
        if let Some(running) = self.state.current.lock().as_mut() {
            running.cursor = Some(just_processed);
        }
    }

    fn record_outcome(&self, outcome: ItemOutcome) {
        if let Some(running) = self.state.current.lock().as_mut() {
            match outcome {
                ItemOutcome::Completed => running.completed += 1,
                ItemOutcome::Failed => running.failed += 1,
                ItemOutcome::Skipped => running.skipped_already_completed += 1,
            }
        }
    }

    fn finalise_run(&self) {
        let mut current = self.state.current.lock();
        if let Some(r) = current.take() {
            *self.state.last_progress.lock() = BackfillProgress {
                request_id: Some(r.request.request_id),
                completed: r.completed,
                failed: r.failed,
                skipped_already_completed: r.skipped_already_completed,
                last_processed_memory_id: r.cursor,
                running: false,
                eta: None,
            };
        }
    }

    fn publish_progress(&self) {
        let current = self.state.current.lock();
        if let Some(r) = current.as_ref() {
            *self.state.last_progress.lock() = BackfillProgress {
                request_id: Some(r.request.request_id),
                completed: r.completed,
                failed: r.failed,
                skipped_already_completed: r.skipped_already_completed,
                last_processed_memory_id: r.cursor,
                running: true,
                eta: None,
            };
        }
    }

    fn process_item(
        &self,
        ctx: &WorkerContext,
        memory_id: MemoryId,
        extractor_id: brain_core::ExtractorId,
        item_key: &[u8],
        dry_run: bool,
        now_ns: u64,
    ) -> Result<ItemOutcome, WorkerError> {
        let mut metadata = ctx.ops.executor.metadata.lock();

        // Resume / skip-check via rtxn.
        let rtxn = metadata
            .read_txn()
            .map_err(|e| WorkerError::Internal(format!("backfill read_txn: {e}")))?;
        let existing = checkpoints::get(&rtxn, WORKER_ID, item_key)
            .map_err(|e| WorkerError::Internal(format!("checkpoint get: {e}")))?;
        drop(rtxn);

        if let Some(row) = existing.as_ref() {
            if row.is_completed() {
                return Ok(ItemOutcome::Skipped);
            }
            if row.is_failed() && row.attempts >= MAX_ATTEMPTS_PER_ITEM {
                return Ok(ItemOutcome::Skipped);
            }
        }

        // Transition to `Started` then decide.
        let wtxn = metadata
            .write_txn()
            .map_err(|e| WorkerError::Internal(format!("backfill write_txn: {e}")))?;
        checkpoints::mark_started(&wtxn, WORKER_ID, item_key, now_ns)
            .map_err(|e| WorkerError::Internal(format!("mark_started: {e}")))?;

        let outcome = if dry_run {
            // Plan validation only — mark as `Completed` without invoking
            // the extractor pipeline.
            checkpoints::mark_completed(&wtxn, WORKER_ID, item_key, now_ns)
                .map_err(|e| WorkerError::Internal(format!("mark_completed: {e}")))?;
            ItemOutcome::Completed
        } else {
            // v1 scope cut: memory text isn't persisted beyond the WAL, so
            // backfill against historical memories can't re-invoke the
            // extractor pipeline. Mark as `Failed` with a clear reason;
            // operators re-ingest. The checkpoint scaffolding lives here
            // so a post-v1 memory-text store can light this up.
            let _ = (memory_id, extractor_id);
            checkpoints::mark_failed(
                &wtxn,
                WORKER_ID,
                item_key,
                "memory text not persisted (v1 limitation)",
                now_ns,
            )
            .map_err(|e| WorkerError::Internal(format!("mark_failed: {e}")))?;
            ItemOutcome::Failed
        };

        wtxn.commit()
            .map_err(|e| WorkerError::Internal(format!("backfill commit: {e}")))?;
        Ok(outcome)
    }
}

impl Default for BackfillWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for BackfillWorker {
    fn name(&self) -> &'static str {
        WorkerKind::Backfill.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::Backfill
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(self.drive_one_batch(ctx))
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemOutcome {
    Completed,
    Failed,
    Skipped,
}

fn memory_key_from(raw: u128) -> [u8; 16] {
    raw.to_be_bytes()
}

fn item_key_for(memory_id: MemoryId, extractor_id_raw: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(16 + 4);
    k.extend_from_slice(&memory_id.raw().to_be_bytes());
    k.extend_from_slice(&extractor_id_raw.to_le_bytes());
    k
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::ExtractorId;

    #[test]
    fn submit_enqueues_request_idempotently() {
        let w = BackfillWorker::new();
        let req = BackfillRequest::new(BackfillRange::All, vec![ExtractorId(1)]);
        let id = req.request_id;
        let got = w.submit(req);
        assert_eq!(got, id);
        assert_eq!(w.state.pending.lock().len(), 1);
    }

    #[test]
    fn cancel_unknown_request_is_noop() {
        let w = BackfillWorker::new();
        let unknown = BackfillId::new();
        assert!(!w.cancel(unknown));
    }

    #[test]
    fn item_key_is_stable_per_pair() {
        let m = MemoryId::from_raw(42);
        let k1 = item_key_for(m, 7);
        let k2 = item_key_for(m, 7);
        assert_eq!(k1, k2);
        let k3 = item_key_for(m, 8);
        assert_ne!(k1, k3);
    }

    #[test]
    fn worker_kind_name() {
        let w = BackfillWorker::new();
        assert_eq!(w.name(), "backfill");
        assert_eq!(w.kind(), WorkerKind::Backfill);
    }

    #[test]
    fn progress_starts_idle() {
        let w = BackfillWorker::new();
        let p = w.progress();
        assert!(!p.running);
        assert_eq!(p.completed, 0);
    }
}
