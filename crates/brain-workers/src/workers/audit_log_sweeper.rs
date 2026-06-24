//! Audit log sweeper.
//!
//! Periodic low-priority worker that hard-deletes audit rows
//! past `retention_seconds` (default 90 d). v1 sweeps the
//! `EXTRACTOR_AUDIT_TABLE`. Merge / Unmerge audit rows (kept forever)
//! live on a different table and are untouched.

use std::future::Future;
use std::pin::Pin;
use std::time::SystemTime;

use brain_metadata::extractor::sweep::{sweep_audit_log, SweepSummary};

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// 90 days in seconds.
pub const DEFAULT_AUDIT_RETENTION_SECONDS: u64 = 90 * 24 * 60 * 60;

pub struct AuditLogSweeper {
    config: WorkerConfig,
    retention_seconds: u64,
    dry_run: bool,
}

impl AuditLogSweeper {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::AuditLogSweeper),
            retention_seconds: DEFAULT_AUDIT_RETENTION_SECONDS,
            dry_run: false,
        }
    }

    #[must_use]
    pub fn with_retention_seconds(mut self, seconds: u64) -> Self {
        self.retention_seconds = seconds;
        self
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    #[must_use]
    pub fn dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }

    async fn sweep_once(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
        if self.retention_seconds == 0 {
            return Ok(0);
        }
        let now_ns = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let metadata = ctx.ops.executor.metadata.as_ref();
        let wtxn = metadata
            .write_txn()
            .map_err(|e| WorkerError::Internal(format!("audit sweeper wtxn: {e}")))?;
        let summary: SweepSummary = sweep_audit_log(
            &wtxn,
            self.retention_seconds,
            now_ns,
            self.config.batch_size,
            self.dry_run,
        )
        .map_err(|e| WorkerError::Internal(format!("audit sweeper: {e}")))?;
        wtxn.commit()
            .map_err(|e| WorkerError::Internal(format!("audit sweeper commit: {e}")))?;
        tracing::debug!(
            target: "brain_workers::audit_log_sweeper",
            scanned = summary.scanned,
            deleted = summary.deleted,
            "audit log sweep complete",
        );
        Ok(summary.deleted as usize)
    }
}

impl Default for AuditLogSweeper {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for AuditLogSweeper {
    fn name(&self) -> &'static str {
        WorkerKind::AuditLogSweeper.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::AuditLogSweeper
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(self.sweep_once(ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_retention_is_90d() {
        let w = AuditLogSweeper::new();
        assert_eq!(w.retention_seconds, 90 * 24 * 60 * 60);
    }
}
