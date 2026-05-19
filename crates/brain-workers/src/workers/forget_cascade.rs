//! FORGET cascade worker (sub-task 24.2). Spec §27/04 §4.
//!
//! Triggered by `handle_forget` post-commit (wiring lands as
//! a follow-up; see "v1 scope" below). The worker dequeues
//! `ForgetCascadeJob`s and walks dependent statements via
//! [`brain_metadata::cascade_ops::cascade_forget_to_statements`].
//!
//! ## v1 scope
//!
//! - The cascade engine is implemented and unit-tested in
//!   `brain_metadata::cascade_ops`. This worker is the queue
//!   driver.
//! - `handle_forget` does NOT yet enqueue jobs post-commit;
//!   that integration is a follow-up. Operators / tests can
//!   call [`ForgetCascadeWorker::enqueue`] directly until the
//!   hook lands. Spec §25/00 contract ("FORGET returns
//!   immediately; cascade processes in background") is
//!   preserved either way — the cascade is decoupled from the
//!   FORGET response path.
//! - Soft-cascade revert (the cascade rollback when a soft
//!   FORGET is reverted within grace) is post-v1. The job
//!   shape carries the `CascadeKind` discriminant so the
//!   revert path can be added without a wire-level change.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use brain_core::MemoryId;
use brain_metadata::cascade_ops::{
    cascade_forget_to_edges, cascade_forget_to_statements, DEFAULT_CASCADE_CONFIDENCE_THRESHOLD,
};
use parking_lot::Mutex;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Worker id for the shared `worker_checkpoints` table.
pub const WORKER_ID: &str = "forget_cascade";

/// Per-cascade-job wall-time cap. Heavily-referenced memories
/// produce continuation jobs (post-v1).
pub const PER_JOB_BATCH_CAP: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgetMode {
    Soft,
    Hard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadeKind {
    Apply,
    Revert,
}

/// One cascade job. Spec §27/04 §4.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForgetCascadeJob {
    pub memory_id: MemoryId,
    pub mode: ForgetMode,
    pub kind: CascadeKind,
    pub forgot_at_unix_nanos: u64,
}

pub struct ForgetCascadeWorker {
    config: WorkerConfig,
    state: Arc<CascadeState>,
    confidence_threshold: f32,
}

#[derive(Default)]
struct CascadeState {
    queue: Mutex<VecDeque<ForgetCascadeJob>>,
}

impl ForgetCascadeWorker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::ForgetCascade),
            state: Arc::new(CascadeState::default()),
            confidence_threshold: DEFAULT_CASCADE_CONFIDENCE_THRESHOLD,
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    #[must_use]
    pub fn with_confidence_threshold(mut self, threshold: f32) -> Self {
        self.confidence_threshold = threshold;
        self
    }

    /// Enqueue a cascade job. Callers: post-commit FORGET hook
    /// (when wired); tests; admin manual triggers.
    pub fn enqueue(&self, job: ForgetCascadeJob) {
        self.state.queue.lock().push_back(job);
    }

    /// Current queue depth — surfaces in metrics.
    pub fn queue_depth(&self) -> usize {
        self.state.queue.lock().len()
    }

    async fn drive_one_batch(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
        let mut processed = 0usize;
        while processed < self.config.batch_size {
            if ctx.is_shutdown() {
                break;
            }
            let Some(job) = self.state.queue.lock().pop_front() else {
                break;
            };
            if matches!(job.kind, CascadeKind::Revert) {
                // v1 scope cut: soft-cascade revert not implemented.
                // The job is still consumed (queue doesn't back up)
                // and a warn log surfaces the gap.
                tracing::warn!(
                    target: "brain_workers::forget_cascade",
                    memory_id = ?job.memory_id,
                    "cascade revert requested; v1 implementation pending — job dropped",
                );
                processed += 1;
                continue;
            }
            let now_ns = now_unix_nanos();
            let mut metadata = ctx.ops.executor.metadata.lock();
            let wtxn = metadata
                .write_txn()
                .map_err(|e| WorkerError::Internal(format!("cascade write_txn: {e}")))?;
            let summary = cascade_forget_to_statements(
                &wtxn,
                job.memory_id,
                self.confidence_threshold,
                PER_JOB_BATCH_CAP,
                now_ns,
            )
            .map_err(|e| WorkerError::Internal(format!("cascade: {e}")))?;
            // Phase C cascade: drop substrate / mention edges anchored
            // at the forgotten memory and clean up typed-relation
            // evidence in the same write txn. A single wtxn keeps the
            // edge sweep, statement evidence rewrite, and relation
            // tombstone atomic — a partial cascade would leak dangling
            // edges past the FORGET visibility boundary.
            let edge_summary = cascade_forget_to_edges(&wtxn, job.memory_id, now_ns)
                .map_err(|e| WorkerError::Internal(format!("cascade edges: {e}")))?;
            wtxn.commit()
                .map_err(|e| WorkerError::Internal(format!("cascade commit: {e}")))?;
            tracing::debug!(
                target: "brain_workers::forget_cascade",
                memory_id = ?job.memory_id,
                scanned = summary.scanned,
                evidence_dropped = summary.evidence_dropped,
                kept_stale = summary.kept_stale,
                tombstoned = summary.tombstoned,
                substrate_unlinked = edge_summary.substrate_unlinked,
                relations_tombstoned = edge_summary.relations_tombstoned,
                relations_evidence_dropped = edge_summary.relations_evidence_dropped,
                "cascade applied",
            );
            processed += 1;
            let _ = job.forgot_at_unix_nanos;
            let _ = job.mode;
        }
        Ok(processed)
    }
}

impl Default for ForgetCascadeWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for ForgetCascadeWorker {
    fn name(&self) -> &'static str {
        WorkerKind::ForgetCascade.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::ForgetCascade
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

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_grows_queue_depth() {
        let w = ForgetCascadeWorker::new();
        assert_eq!(w.queue_depth(), 0);
        w.enqueue(ForgetCascadeJob {
            memory_id: MemoryId::from_raw(1),
            mode: ForgetMode::Soft,
            kind: CascadeKind::Apply,
            forgot_at_unix_nanos: 0,
        });
        assert_eq!(w.queue_depth(), 1);
    }

    #[test]
    fn worker_kind_name() {
        let w = ForgetCascadeWorker::new();
        assert_eq!(w.name(), "forget_cascade");
    }
}
