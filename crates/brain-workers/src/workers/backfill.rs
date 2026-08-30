//! Backfill worker.
//!
//! Admin-triggered worker that walks a `(memory_range × extractor_ids)`
//! grid and re-runs extractors against each memory. Each
//! `(memory, extractor)` pair has its own row in the shared
//! `worker_checkpoints` redb table so a restart resumes mid-run
//! without re-extracting already-completed items.
//!
//! ## How a live backfill re-extracts
//!
//! Memory text is durably persisted in `TEXTS_TABLE` (written inside
//! the ENCODE apply txn alongside the memory row, and reconstructed on
//! recovery), so a backfill does not need the original ENCODE frame to
//! re-extract. For each live item the worker re-enqueues the memory on
//! the durable `extraction_queue` — the very trigger the live ENCODE
//! path uses — inside the same write txn as the per-item checkpoint.
//! The per-shard `ExtractorWorker` drains that queue on its next cycle
//! and re-runs the full tier pipeline; re-derived statements/relations
//! flow through the normal supersession path.
//!
//! - `dry_run` items are marked `Completed` without enqueueing (plan
//!   validation only).
//! - Live items enqueue + checkpoint `Completed` atomically.
//! - A memory already extracted under the current schema is re-run only
//!   when the operator clears the `ExtractorWorker`'s
//!   `skip_already_extracted` gate; otherwise the live worker
//!   no-op-skips it on re-drain. That gate is the forced-re-extraction
//!   knob — backfill itself never deletes prior derivations.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use brain_core::{BackfillId, BackfillProgress, BackfillRange, BackfillRequest, MemoryId};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::worker_checkpoints;
use parking_lot::Mutex;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Stable worker id used as the first half of the checkpoint
/// composite key.
pub const WORKER_ID: &str = "backfill";

/// Per-item attempt cap. An item whose checkpoint is `Failed` with
/// `attempts >= MAX_ATTEMPTS_PER_ITEM` is treated as permanently
/// failed: it is counted as `failed` in the run's progress and not
/// retried again, while the rest of the run continues (per-item
/// resilience — one bad item never aborts the whole backfill). The
/// worker does **not** abort the request on hitting the cap; the
/// earlier "bad-extractor abort" doc claim was never wired and has
/// been dropped in favour of this per-item-failure accounting.
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
    ///
    /// Idempotent by request id: submitting a `BackfillId` that is
    /// already in flight (the current run) or already queued (pending)
    /// is a no-op — the id is returned but nothing is enqueued a second
    /// time. This keeps the admin fan-out safe (one id is submitted to
    /// every shard, and a retry of that fan-out never double-queues a
    /// run) without ever blocking a genuine resume: once a run has
    /// finalised and left `current`, re-submitting its id enqueues a
    /// fresh pass that resumes from the durable checkpoints.
    pub fn submit(&self, request: BackfillRequest) -> BackfillId {
        let id = request.request_id;
        // Lock order matches `dequeue_if_idle` (current before pending)
        // so the two can never deadlock.
        let current = self.state.current.lock();
        let mut pending = self.state.pending.lock();
        let already_present = current
            .as_ref()
            .is_some_and(|r| r.request.request_id == id)
            || pending.iter().any(|r| r.request_id == id);
        if !already_present {
            pending.push_back(request);
        }
        id
    }

    /// Cancel the request matching `request_id`. Returns `true` if the
    /// cancel took effect on this shard — either the in-flight run's
    /// cancel flag was flipped, or a not-yet-started run was removed
    /// from the pending queue — and `false` if no such request is
    /// known here.
    ///
    /// A `DELETE` issued before the worker's next tick lands while the
    /// run is still only pending; dropping it from the queue there
    /// stops it from ever being promoted and executed. Cancelling a
    /// pending run is not a durable veto: it removes the queued request
    /// but records no permanent block, so a later re-submit of the same
    /// id is honoured (resuming from checkpoints, as after an in-flight
    /// cancel).
    pub fn cancel(&self, request_id: BackfillId) -> bool {
        // Lock order matches `dequeue_if_idle`/`submit` (current before
        // pending).
        let mut current = self.state.current.lock();
        let mut acted = false;
        if let Some(running) = current.as_mut() {
            if running.request.request_id == request_id {
                running.cancelled = true;
                acted = true;
            }
        }
        let mut pending = self.state.pending.lock();
        let before = pending.len();
        pending.retain(|req| req.request_id != request_id);
        if pending.len() != before {
            acted = true;
        }
        acted
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
    async fn drive_one_batch(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
        // Acquire current run (or dequeue a new one). Clone the request out
        // and drop the `current` guard *before* calling `dequeue_if_idle` —
        // the guard must not be held across that call, which re-locks
        // `current` (parking_lot mutexes are non-reentrant, so holding it
        // across the match would deadlock).
        let existing = self
            .state
            .current
            .lock()
            .as_ref()
            .map(|r| r.request.clone());
        let req = match existing {
            Some(r) => r,
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

            // Process EVERY extractor for this memory before advancing the
            // cursor. `extractor_ids` is capped at
            // `MAX_EXTRACTORS_PER_BACKFILL` (4 today), so a single memory is
            // at most a handful of items; finishing it whole keeps the batch
            // bound (checked between memories, above) while guaranteeing no
            // `(memory, extractor)` pair is ever split across a cycle
            // boundary and left behind — the batch-boundary skip (bug #6).
            //
            // Advancing the cursor only after all extractors are checkpointed
            // also means the in-memory cursor and the durable per-pair
            // checkpoints never disagree: a mid-memory crash leaves the cursor
            // un-advanced, so the next run re-visits the memory and the
            // already-`Completed` pairs short-circuit to `Skipped` (no
            // duplicate enqueue) while the unfinished ones run.
            for ext_id in &req.extractor_ids {
                let item_key = item_key_for(memory_id, ext_id.raw());
                let outcome =
                    self.process_item(ctx, memory_id, *ext_id, &item_key, req.dry_run, now_ns)?;
                self.record_outcome(outcome);
                items_processed += 1;
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
            .is_some_and(|r| r.cancelled)
    }

    fn next_memory(
        &self,
        req: &BackfillRequest,
        ctx: &WorkerContext,
    ) -> Result<Option<MemoryId>, WorkerError> {
        let cursor = self.state.current.lock().as_ref().and_then(|r| r.cursor);
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

        let metadata = ctx.ops.executor.metadata.as_ref();
        let rtxn = metadata
            .read_txn()
            .map_err(|e| WorkerError::Internal(format!("backfill read_txn: {e}")))?;
        let table = rtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| WorkerError::Internal(format!("backfill open MEMORIES_TABLE: {e}")))?;
        let mut iter = table
            .range(memory_key_from(lo)..=memory_key_from(hi))
            .map_err(|e| WorkerError::Internal(format!("backfill range: {e}")))?;
        if let Some(entry) = iter.next() {
            let (k, _) = entry.map_err(|e| WorkerError::Internal(format!("backfill iter: {e}")))?;
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
        let metadata = ctx.ops.executor.metadata.as_ref();

        // Resume / skip-check via rtxn.
        let rtxn = metadata
            .read_txn()
            .map_err(|e| WorkerError::Internal(format!("backfill read_txn: {e}")))?;
        let existing = worker_checkpoints::get(&rtxn, WORKER_ID, item_key)
            .map_err(|e| WorkerError::Internal(format!("checkpoint get: {e}")))?;
        drop(rtxn);

        if let Some(row) = existing.as_ref() {
            if row.is_completed() {
                return Ok(ItemOutcome::Skipped);
            }
            if row.is_failed() && row.attempts >= MAX_ATTEMPTS_PER_ITEM {
                // Permanently failed (hit the attempt cap on a prior run):
                // count it as `failed`, not `skipped_already_completed`, so
                // progress doesn't mask a bad item as a resume-skip. The
                // run continues; this item is never retried.
                return Ok(ItemOutcome::Failed);
            }
        }

        // Transition to `Started` then decide.
        let wtxn = metadata
            .write_txn()
            .map_err(|e| WorkerError::Internal(format!("backfill write_txn: {e}")))?;
        worker_checkpoints::mark_started(&wtxn, WORKER_ID, item_key, now_ns)
            .map_err(|e| WorkerError::Internal(format!("mark_started: {e}")))?;

        let outcome = if dry_run {
            // Plan validation only — mark as `Completed` without invoking
            // the extractor pipeline.
            worker_checkpoints::mark_completed(&wtxn, WORKER_ID, item_key, now_ns)
                .map_err(|e| WorkerError::Internal(format!("mark_completed: {e}")))?;
            ItemOutcome::Completed
        } else {
            // Re-run extraction by re-enqueueing the memory on the durable
            // extraction queue — the same trigger the live ENCODE path
            // uses (`brain_metadata::extraction_queue_enqueue`). Memory
            // text is durably persisted in `TEXTS_TABLE` (written in the
            // ENCODE apply txn and rebuilt on recovery), so the per-shard
            // ExtractorWorker can re-read it on its next cycle and re-run
            // the full tier pipeline; re-derivation flows through the
            // normal supersession path. The enqueue happens inside this
            // same wtxn as the checkpoint write, so the trigger commits
            // atomically with the checkpoint — a crash can never leave a
            // checkpoint `Completed` without the matching queue row.
            //
            // Memories not yet extracted under the current schema are
            // (re)processed; already-extracted memories are re-run only
            // when the operator has turned off the ExtractorWorker's
            // `skip_already_extracted` gate (the forced-re-extraction
            // knob), otherwise the live worker no-op-skips them on
            // re-drain — the safe default. `extractor_id` is the grid
            // coordinate that selected this memory; the pipeline re-runs
            // every enabled tier rather than one extractor, so it isn't
            // threaded further.
            let _ = extractor_id;
            match brain_metadata::extraction_queue_enqueue(&wtxn, memory_id, now_ns) {
                Ok(()) => {
                    worker_checkpoints::mark_completed(&wtxn, WORKER_ID, item_key, now_ns)
                        .map_err(|e| WorkerError::Internal(format!("mark_completed: {e}")))?;
                    ItemOutcome::Completed
                }
                Err(e) => {
                    // Per-item resilience: a failed enqueue marks only this
                    // item `Failed` and lets the run continue (Failed items
                    // retry on a later cycle up to MAX_ATTEMPTS_PER_ITEM),
                    // rather than `?`-aborting the whole backfill.
                    worker_checkpoints::mark_failed(
                        &wtxn,
                        WORKER_ID,
                        item_key,
                        format!("enqueue: {e}"),
                        now_ns,
                    )
                    .map_err(|e| WorkerError::Internal(format!("mark_failed: {e}")))?;
                    ItemOutcome::Failed
                }
            }
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

/// Bridge the worker's inherent submit/cancel/progress onto the
/// `brain-planner` control trait so the dispatch path can drive it
/// through `ExecutorContext::backfill_handle` without a back-dependency
/// on this crate.
impl brain_planner::BackfillControl for BackfillWorker {
    fn submit(&self, request: BackfillRequest) -> BackfillId {
        BackfillWorker::submit(self, request)
    }
    fn cancel(&self, request_id: BackfillId) -> bool {
        BackfillWorker::cancel(self, request_id)
    }
    fn progress(&self) -> BackfillProgress {
        BackfillWorker::progress(self)
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
    fn cancel_removes_pending_run_before_first_tick() {
        // A DELETE issued before the worker's next tick must drop the
        // still-pending run so it never starts.
        let w = BackfillWorker::new();
        let req = BackfillRequest::new(BackfillRange::All, vec![ExtractorId(1)]);
        let id = w.submit(req);
        assert_eq!(w.state.pending.lock().len(), 1);

        assert!(w.cancel(id), "cancel of a pending run reports it acted");
        assert_eq!(
            w.state.pending.lock().len(),
            0,
            "the pending run is dropped from the queue"
        );

        // Promotion finds nothing to run.
        assert!(w.dequeue_if_idle().is_none());
        assert!(w.state.current.lock().is_none());
    }

    #[test]
    fn cancel_pending_leaves_other_runs_queued() {
        let w = BackfillWorker::new();
        let keep = w.submit(BackfillRequest::new(BackfillRange::All, vec![ExtractorId(1)]));
        let drop_id = w.submit(BackfillRequest::new(BackfillRange::All, vec![ExtractorId(2)]));
        assert_eq!(w.state.pending.lock().len(), 2);

        assert!(w.cancel(drop_id));
        let pending = w.state.pending.lock();
        assert_eq!(pending.len(), 1, "only the cancelled run is removed");
        assert_eq!(pending.front().map(|r| r.request_id), Some(keep));
    }

    #[test]
    fn cancel_pending_does_not_block_resubmit() {
        // Cancelling a pending run records no durable veto: the same id
        // can be re-submitted afterwards.
        let w = BackfillWorker::new();
        let req = BackfillRequest::new(BackfillRange::All, vec![ExtractorId(1)]);
        let id = req.request_id;
        w.submit(req.clone());
        assert!(w.cancel(id));
        assert_eq!(w.state.pending.lock().len(), 0);

        w.submit(req);
        assert_eq!(
            w.state.pending.lock().len(),
            1,
            "the same id can be re-queued after a pending cancel"
        );
    }

    #[test]
    fn submit_is_idempotent_by_request_id_while_pending() {
        // Submitting the same BackfillId twice while it is still queued
        // is a harmless no-op — the fan-out to every shard is safe to
        // retry.
        let w = BackfillWorker::new();
        let req = BackfillRequest::new(BackfillRange::All, vec![ExtractorId(1)]);
        let id = req.request_id;
        let first = w.submit(req.clone());
        let second = w.submit(req);
        assert_eq!(first, id);
        assert_eq!(second, id);
        assert_eq!(
            w.state.pending.lock().len(),
            1,
            "the duplicate submit did not enqueue a second copy"
        );
    }

    #[test]
    fn submit_distinct_ids_both_queue() {
        let w = BackfillWorker::new();
        let a = w.submit(BackfillRequest::new(BackfillRange::All, vec![ExtractorId(1)]));
        let b = w.submit(BackfillRequest::new(BackfillRange::All, vec![ExtractorId(2)]));
        assert_ne!(a, b);
        assert_eq!(w.state.pending.lock().len(), 2);
    }

    #[test]
    fn submit_while_running_same_id_is_noop() {
        // A run already promoted to `current` must not be re-queued by a
        // duplicate submit of its id.
        let w = BackfillWorker::new();
        let req = BackfillRequest::new(BackfillRange::All, vec![ExtractorId(1)]);
        w.submit(req.clone());
        // Promote it to the in-flight slot.
        assert!(w.dequeue_if_idle().is_some());
        assert!(w.state.current.lock().is_some());
        assert_eq!(w.state.pending.lock().len(), 0);

        // Re-submitting the in-flight id does nothing.
        w.submit(req);
        assert_eq!(
            w.state.pending.lock().len(),
            0,
            "an in-flight id is not re-queued"
        );
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
    fn progress_starts_idle() {
        let w = BackfillWorker::new();
        let p = w.progress();
        assert!(!p.running);
        assert_eq!(p.completed, 0);
    }
}
