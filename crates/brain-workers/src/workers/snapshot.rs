//! Snapshot worker (sub-task 8.13). Spec §11/08 §6.
//!
//! Periodic snapshot trigger with retention policy. Spec §6 marks
//! this worker **off by default** ("many deployments prefer
//! external backup tooling. The substrate's built-in snapshot worker
//! is a convenience").
//!
//! ## v1 deviation (documented)
//!
//! No full-shard snapshot orchestration exists yet:
//! - `SharedHnsw::save_snapshot` exists but no arena / metadata
//!   wrappers do.
//! - No `Wal` instance hangs off the writer, so the spec §1 "trigger
//!   checkpoint first" sequencing is Phase 9.
//!
//! v1 ships the **worker shape + retention policy** as a pluggable
//! seam (same pattern as 8.5/8.8/8.12). [`DisabledSnapshotSource`]
//! is the default. Phase 9 plugs in a real source.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

// ---------------------------------------------------------------------------
// Descriptors.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SnapshotId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotDesc {
    pub id: SnapshotId,
    pub taken_at_unix_nanos: u64,
    pub size_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Keep at most this many snapshots. Spec §6.2 default = 7.
    pub max_count: usize,
    /// Drop snapshots older than this age. Spec §6.2 default = 30d.
    pub max_age: Duration,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_count: 7,
            max_age: Duration::from_secs(30 * 24 * 3600),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure retention logic.
// ---------------------------------------------------------------------------

/// Return ids of snapshots to delete given the current set + policy.
/// A snapshot is deletable if **either**:
///   - its age >= `max_age` (oldness rule), or
///   - it's outside the newest `max_count` (count rule).
///
/// Spec §6.2 leaves the combination unspecified; v1 uses "either".
#[must_use]
pub fn decide_retention(
    snapshots: &[SnapshotDesc],
    now_unix_nanos: u64,
    policy: RetentionPolicy,
) -> Vec<SnapshotId> {
    if snapshots.is_empty() {
        return Vec::new();
    }
    let max_age_nanos = u64::try_from(policy.max_age.as_nanos()).unwrap_or(u64::MAX);

    // Sort newest-first by taken_at.
    let mut by_age: Vec<&SnapshotDesc> = snapshots.iter().collect();
    by_age.sort_by_key(|s| std::cmp::Reverse(s.taken_at_unix_nanos));

    let mut out = Vec::new();
    for (idx, snap) in by_age.iter().enumerate() {
        let age = now_unix_nanos.saturating_sub(snap.taken_at_unix_nanos);
        let too_old = age >= max_age_nanos;
        let excess = idx >= policy.max_count;
        if too_old || excess {
            out.push(snap.id);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Source trait — Phase 9 injects an impl.
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum SnapshotSourceError {
    #[error("snapshot source disabled")]
    Disabled,
    #[error("snapshot source failed: {0}")]
    Failed(String),
}

pub type TakeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SnapshotId, SnapshotSourceError>> + 'a>>;
pub type ListFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<SnapshotDesc>, SnapshotSourceError>> + 'a>>;
pub type DeleteFuture<'a> = Pin<Box<dyn Future<Output = Result<(), SnapshotSourceError>> + 'a>>;

/// Pluggable seam for the snapshot worker. Production deployments
/// inject an impl backed by the per-shard arena + WAL + metadata
/// (Phase 9.8 `ShardSnapshotSource`).
///
/// Post-9.8 the trait is `!Send + !Sync`: real adapters hold
/// `Rc<RefCell<…>>` per-shard state and run on the per-shard Glommio
/// executor.
pub trait SnapshotSource: 'static {
    fn take_snapshot(&self) -> TakeFuture<'_>;
    fn list_snapshots(&self) -> ListFuture<'_>;
    fn delete_snapshot(&self, id: SnapshotId) -> DeleteFuture<'_>;
}

pub struct DisabledSnapshotSource;

impl SnapshotSource for DisabledSnapshotSource {
    fn take_snapshot(&self) -> TakeFuture<'_> {
        Box::pin(async { Err(SnapshotSourceError::Disabled) })
    }
    fn list_snapshots(&self) -> ListFuture<'_> {
        Box::pin(async { Err(SnapshotSourceError::Disabled) })
    }
    fn delete_snapshot(&self, _id: SnapshotId) -> DeleteFuture<'_> {
        Box::pin(async { Err(SnapshotSourceError::Disabled) })
    }
}

// ---------------------------------------------------------------------------
// SnapshotWorker.
// ---------------------------------------------------------------------------

pub struct SnapshotWorker {
    config: WorkerConfig,
    retention: RetentionPolicy,
    source: Arc<dyn SnapshotSource>,
}

impl SnapshotWorker {
    #[must_use]
    pub fn new(source: Arc<dyn SnapshotSource>) -> Self {
        Self {
            // WorkerKind::Snapshot defaults enabled=false per spec §6.2.
            config: WorkerConfig::defaults_for(WorkerKind::Snapshot),
            retention: RetentionPolicy::default(),
            source,
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    #[must_use]
    pub fn with_retention(mut self, p: RetentionPolicy) -> Self {
        self.retention = p;
        self
    }

    #[must_use]
    pub fn retention(&self) -> RetentionPolicy {
        self.retention
    }
}

impl Worker for SnapshotWorker {
    fn name(&self) -> &'static str {
        WorkerKind::Snapshot.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::Snapshot
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_snapshot_cycle(self, ctx))
    }
}

async fn do_snapshot_cycle(
    worker: &SnapshotWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    // Take the snapshot.
    let new_id = match worker.source.take_snapshot().await {
        Ok(id) => id,
        Err(SnapshotSourceError::Disabled) => return Ok(0),
        Err(SnapshotSourceError::Failed(e)) => {
            return Err(WorkerError::Ops(format!("snapshot take: {e}")));
        }
    };

    // List + apply retention.
    let snapshots = match worker.source.list_snapshots().await {
        Ok(v) => v,
        Err(SnapshotSourceError::Disabled) => {
            // Took one but can't enumerate; report the single unit
            // of work.
            return Ok(1);
        }
        Err(SnapshotSourceError::Failed(e)) => {
            return Err(WorkerError::Ops(format!("snapshot list: {e}")));
        }
    };
    let now_nanos = now_unix_nanos();
    let to_delete = decide_retention(&snapshots, now_nanos, worker.retention);

    let mut deleted = 0usize;
    for id in to_delete {
        if ctx.is_shutdown() {
            break;
        }
        match worker.source.delete_snapshot(id).await {
            Ok(()) => deleted += 1,
            Err(SnapshotSourceError::Disabled) => break,
            Err(SnapshotSourceError::Failed(e)) => {
                return Err(WorkerError::Ops(format!("snapshot delete: {e}")));
            }
        }
    }

    trace!(
        new_id = new_id.0,
        deleted,
        retained = snapshots.len() + 1 - deleted,
        "snapshot cycle"
    );
    Ok(1 + deleted)
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
