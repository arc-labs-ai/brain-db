//! Schema migration runner (sub-task 24.8). Spec §27/04 §5.
//!
//! Background worker triggered by a successful SCHEMA_UPLOAD
//! whose new version invalidates extracted state. Walks the
//! migration plan one (memory, extractor) item at a time,
//! checkpointing each via the shared `worker_checkpoints`
//! table; re-extracts each item under the new schema version.
//!
//! ## v1 scope
//!
//! Like the backfill worker (24.1), the actual re-extraction
//! step is gated by the same v1 limitation: `MEMORIES_TABLE`
//! doesn't carry `memory.text`. v1 scaffolds the queue, walk,
//! and checkpoint discipline; live re-extraction marks items
//! Failed with reason "memory text not persisted (v1 limitation)".
//! Dry-run is fully functional for plan preview.
//!
//! The `MigrationPlan` types ship in `brain-core::migration` so
//! the SCHEMA_UPLOAD handler can preview a plan in its response
//! without taking a dep on this worker module.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use brain_core::{MigrationId, MigrationPlan};
use brain_metadata::tables::worker_checkpoints as checkpoints;
use parking_lot::Mutex;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

pub const WORKER_ID: &str = "schema_migration";
pub const MAX_ATTEMPTS_PER_ITEM: u32 = 3;

pub struct SchemaMigrationWorker {
    config: WorkerConfig,
    state: Arc<MigrationState>,
}

#[derive(Default)]
struct MigrationState {
    pending: Mutex<VecDeque<MigrationPlan>>,
    current: Mutex<Option<RunningMigration>>,
}

struct RunningMigration {
    plan: MigrationPlan,
    cursor: usize,
    completed: u64,
    failed: u64,
    cancelled: bool,
}

impl SchemaMigrationWorker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::SchemaMigration),
            state: Arc::new(MigrationState::default()),
        }
    }

    pub fn submit(&self, plan: MigrationPlan) -> MigrationId {
        let id = plan.request_id;
        self.state.pending.lock().push_back(plan);
        id
    }

    pub fn cancel(&self, request_id: MigrationId) -> bool {
        let mut cur = self.state.current.lock();
        if let Some(r) = cur.as_mut() {
            if r.plan.request_id == request_id {
                r.cancelled = true;
                return true;
            }
        }
        false
    }

    async fn drive_one_batch(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
        // Dequeue if idle.
        {
            let mut current = self.state.current.lock();
            if current.is_none() {
                if let Some(plan) = self.state.pending.lock().pop_front() {
                    *current = Some(RunningMigration {
                        plan,
                        cursor: 0,
                        completed: 0,
                        failed: 0,
                        cancelled: false,
                    });
                } else {
                    return Ok(0);
                }
            }
        }

        let mut processed = 0usize;
        let now_ns = now_unix_nanos();

        while processed < self.config.batch_size {
            if ctx.is_shutdown() {
                break;
            }
            let item_info = {
                let current = self.state.current.lock();
                let Some(r) = current.as_ref() else {
                    break;
                };
                if r.cancelled || r.cursor >= r.plan.items.len() {
                    None
                } else {
                    Some((r.plan.items[r.cursor], r.cursor))
                }
            };

            let Some((item, _cursor)) = item_info else {
                self.finalise_run();
                break;
            };

            let item_key = migration_item_key(item.memory_id, item.extractor_id.raw());
            let outcome = self.process_item(ctx, &item_key, now_ns)?;
            self.advance_cursor(outcome);
            processed += 1;
        }
        Ok(processed)
    }

    fn advance_cursor(&self, outcome: ItemOutcome) {
        if let Some(r) = self.state.current.lock().as_mut() {
            r.cursor += 1;
            match outcome {
                ItemOutcome::Completed => r.completed += 1,
                ItemOutcome::Failed => r.failed += 1,
                ItemOutcome::Skipped => {}
            }
        }
    }

    fn finalise_run(&self) {
        *self.state.current.lock() = None;
    }

    fn process_item(
        &self,
        ctx: &WorkerContext,
        item_key: &[u8],
        now_ns: u64,
    ) -> Result<ItemOutcome, WorkerError> {
        let mut metadata = ctx.ops.executor.metadata.lock();

        let rtxn = metadata
            .read_txn()
            .map_err(|e| WorkerError::Internal(format!("migration rtxn: {e}")))?;
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

        let wtxn = metadata
            .write_txn()
            .map_err(|e| WorkerError::Internal(format!("migration wtxn: {e}")))?;
        checkpoints::mark_started(&wtxn, WORKER_ID, item_key, now_ns)
            .map_err(|e| WorkerError::Internal(format!("mark_started: {e}")))?;
        // v1 scope cut — same as 24.1 backfill. Mark Failed with a
        // clear reason; full re-extract logic lands once memory text
        // is persisted beyond the WAL.
        checkpoints::mark_failed(
            &wtxn,
            WORKER_ID,
            item_key,
            "memory text not persisted (v1 limitation)",
            now_ns,
        )
        .map_err(|e| WorkerError::Internal(format!("mark_failed: {e}")))?;
        wtxn.commit()
            .map_err(|e| WorkerError::Internal(format!("migration commit: {e}")))?;
        Ok(ItemOutcome::Failed)
    }
}

impl Default for SchemaMigrationWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for SchemaMigrationWorker {
    fn name(&self) -> &'static str {
        WorkerKind::SchemaMigration.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::SchemaMigration
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Completed variant used once memory text storage lands.
enum ItemOutcome {
    Completed,
    Failed,
    Skipped,
}

fn migration_item_key(memory_id: brain_core::MemoryId, extractor_raw: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(16 + 4);
    k.extend_from_slice(&memory_id.raw().to_be_bytes());
    k.extend_from_slice(&extractor_raw.to_le_bytes());
    k
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
    fn submit_queues_plan() {
        let w = SchemaMigrationWorker::new();
        let plan = MigrationPlan {
            request_id: MigrationId::new(),
            from_version: 1,
            to_version: 2,
            namespace: "acme".into(),
            items: Vec::new(),
        };
        let id = plan.request_id;
        let got = w.submit(plan);
        assert_eq!(got, id);
        assert_eq!(w.state.pending.lock().len(), 1);
    }

    #[test]
    fn worker_kind_name() {
        let w = SchemaMigrationWorker::new();
        assert_eq!(w.name(), "schema_migration");
    }
}
