//! WAL retention worker (sub-task 8.8). Spec §11/07.
//!
//! Deletes WAL segments whose entire LSN range is covered by the
//! latest checkpoint, minus a configurable retention buffer.
//!
//! ## v1 deviations (documented)
//!
//! brain-storage already ships the WAL substrate (segments, group
//! commit, checkpoint writer), but:
//! - There's no public `Wal::list_segments` / `delete_segment` API.
//! - brain-ops's `RealWriterHandle` doesn't hold a `Wal` instance yet.
//!
//! Both land in Phase 9. v1 therefore exposes:
//! - A pure [`decide_deletions`] function that matches spec §3.
//! - A pluggable [`WalRetentionSource`] trait where Phase 9 wires
//!   the real WAL.
//! - A [`DisabledWalRetentionSource`] default that makes the worker
//!   a no-op until the source is replaced.
//!
//! Same shape as the HNSW maintenance worker (sub-task 8.5).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use thiserror::Error;
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

// ---------------------------------------------------------------------------
// Descriptor types (kept dependency-free so brain-storage doesn't need to
// know about them).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentDesc {
    pub segment_id: u64,
    pub first_lsn: u64,
    pub last_lsn: u64,
    pub size_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointDesc {
    pub durable_lsn: u64,
}

// ---------------------------------------------------------------------------
// Pure decision logic — spec §11/07 §3.
// ---------------------------------------------------------------------------

/// Return the ids of segments fully covered by the checkpoint, minus
/// the retention buffer. A segment is deletable iff `last_lsn <
/// (durable_lsn - retention_extra_lsns)`. Spec §1: "A segment is
/// covered when its highest LSN is less than the checkpoint's
/// `durable_lsn`."
///
/// The cutoff saturates at 0 when the buffer exceeds the checkpoint
/// (early life of a shard), so nothing is deleted.
#[must_use]
pub fn decide_deletions(
    segments: &[SegmentDesc],
    checkpoint: CheckpointDesc,
    retention_extra_lsns: u64,
) -> Vec<u64> {
    let safe_cutoff = checkpoint.durable_lsn.saturating_sub(retention_extra_lsns);
    segments
        .iter()
        .filter(|s| s.last_lsn < safe_cutoff)
        .map(|s| s.segment_id)
        .collect()
}

// ---------------------------------------------------------------------------
// Source trait — Phase 9 injects a brain-storage-backed impl.
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum WalRetentionSourceError {
    /// v1 default — no WAL hookup yet.
    #[error("WAL retention source disabled")]
    Disabled,
    /// Spec §9 safety check denied this operation; the worker should
    /// skip it and try again next cycle.
    #[error("WAL retention source rejected operation: {0}")]
    Rejected(String),
    /// Underlying I/O / WAL error. Surfaces to the worker as a real
    /// failure (`WorkerError::Ops`).
    #[error("WAL retention source failed: {0}")]
    Failed(String),
}

pub type CheckpointFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CheckpointDesc, WalRetentionSourceError>> + 'a>>;

pub type SegmentListFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<SegmentDesc>, WalRetentionSourceError>> + 'a>>;

pub type DeleteFuture<'a> = Pin<Box<dyn Future<Output = Result<(), WalRetentionSourceError>> + 'a>>;

/// Pluggable seam for the WAL retention worker. Production
/// deployments inject an impl backed by `brain_storage::Wal` (Phase
/// 9.8 `WalDirRetentionSource`). Same `Pin<Box<Future>>` pattern as
/// `Summarizer` / `RebuildSource`.
///
/// Post-9.8 the trait is `!Send + !Sync`: the per-shard Glommio
/// executor is single-threaded, so the trait only needs `'static`
/// for `Arc<dyn …>` storage inside the worker.
pub trait WalRetentionSource: 'static {
    fn current_checkpoint(&self) -> CheckpointFuture<'_>;
    fn list_segments(&self) -> SegmentListFuture<'_>;
    fn delete_segment(&self, segment_id: u64) -> DeleteFuture<'_>;
}

/// Default no-op source. Every method returns `Disabled`; the worker
/// reports zero deletions and moves on.
pub struct DisabledWalRetentionSource;

impl WalRetentionSource for DisabledWalRetentionSource {
    fn current_checkpoint(&self) -> CheckpointFuture<'_> {
        Box::pin(async { Err(WalRetentionSourceError::Disabled) })
    }
    fn list_segments(&self) -> SegmentListFuture<'_> {
        Box::pin(async { Err(WalRetentionSourceError::Disabled) })
    }
    fn delete_segment(&self, _segment_id: u64) -> DeleteFuture<'_> {
        Box::pin(async { Err(WalRetentionSourceError::Disabled) })
    }
}

// ---------------------------------------------------------------------------
// WalRetentionWorker.
// ---------------------------------------------------------------------------

pub struct WalRetentionWorker {
    config: WorkerConfig,
    retention_extra_lsns: u64,
    source: Arc<dyn WalRetentionSource>,
}

impl WalRetentionWorker {
    #[must_use]
    pub fn new(source: Arc<dyn WalRetentionSource>) -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::WalRetention),
            retention_extra_lsns: 0,
            source,
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    /// Override the LSN retention buffer. v1 default is 0 — spec §7
    /// talks bytes ("256 MiB"); LSN/byte ratio depends on record
    /// size, so Phase 9's source impl will convert from
    /// `wal.segment_size`.
    #[must_use]
    pub fn with_retention_extra_lsns(mut self, n: u64) -> Self {
        self.retention_extra_lsns = n;
        self
    }
}

impl Worker for WalRetentionWorker {
    fn name(&self) -> &'static str {
        WorkerKind::WalRetention.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::WalRetention
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_retention_cycle(self, ctx))
    }
}

async fn do_retention_cycle(
    worker: &WalRetentionWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    let cfg = worker.config.clone();
    if cfg.batch_size == 0 {
        return Ok(0);
    }

    // 1. Latest checkpoint.
    let checkpoint = match worker.source.current_checkpoint().await {
        Ok(c) => c,
        Err(WalRetentionSourceError::Disabled) | Err(WalRetentionSourceError::Rejected(_)) => {
            return Ok(0);
        }
        Err(WalRetentionSourceError::Failed(e)) => {
            return Err(WorkerError::Ops(format!("wal checkpoint: {e}")));
        }
    };

    // 2. Segment list.
    let segments = match worker.source.list_segments().await {
        Ok(s) => s,
        Err(WalRetentionSourceError::Disabled) | Err(WalRetentionSourceError::Rejected(_)) => {
            return Ok(0);
        }
        Err(WalRetentionSourceError::Failed(e)) => {
            return Err(WorkerError::Ops(format!("wal list: {e}")));
        }
    };

    // 3. Decide.
    let candidates = decide_deletions(&segments, checkpoint, worker.retention_extra_lsns);
    if candidates.is_empty() {
        return Ok(0);
    }

    // 4. Delete, bounded by batch_size + max_runtime + shutdown.
    let started = Instant::now();
    let mut deleted = 0usize;
    for id in candidates.into_iter().take(cfg.batch_size) {
        if started.elapsed() >= cfg.max_runtime {
            break;
        }
        if ctx.is_shutdown() {
            break;
        }
        match worker.source.delete_segment(id).await {
            Ok(()) => deleted += 1,
            Err(WalRetentionSourceError::Rejected(_)) => {
                // Spec §9 — safety check denied; try again next cycle.
                continue;
            }
            Err(WalRetentionSourceError::Disabled) => break,
            Err(WalRetentionSourceError::Failed(e)) => {
                return Err(WorkerError::Ops(format!("wal delete: {e}")));
            }
        }
        glommio::executor().yield_if_needed().await;
    }

    trace!(
        deleted,
        durable_lsn = checkpoint.durable_lsn,
        cycle_ms = started.elapsed().as_millis() as u64,
        "wal retention cycle"
    );
    Ok(deleted)
}
