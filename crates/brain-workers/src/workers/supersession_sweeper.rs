//! Supersession sweeper.
//!
//! Periodic low-priority worker that hard-deletes superseded
//! statements past the configured retention. **Off by default**
//! (`retention_seconds == 0`) retains superseded
//! statements forever and lets operators opt in to sweeping.

use std::future::Future;
use std::pin::Pin;
use std::time::SystemTime;

use brain_metadata::extractor::sweep::{sweep_superseded_statements, SweepSummary};

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Default sweep cadence in seconds (1 day) for an operator who opts in
/// by setting a non-zero retention. Mirrors the statement-reclaim
/// worker's daily cadence — both are low-priority physical-reclamation
/// passes where once-a-day is ample.
pub const DEFAULT_PERIOD_SECONDS: u64 = 86_400;

pub struct SupersessionSweeper {
    config: WorkerConfig,
    retention_seconds: u64,
    dry_run: bool,
}

impl SupersessionSweeper {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::SupersessionSweeper),
            retention_seconds: 0, // disabled by default.
            dry_run: false,
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    #[must_use]
    pub fn with_retention_seconds(mut self, seconds: u64) -> Self {
        self.retention_seconds = seconds;
        self
    }

    /// Set the sweep cadence (the scheduler reads this off `config`).
    /// Clamped to a minimum of one second so a misconfigured `0` can't
    /// busy-loop the scheduler.
    #[must_use]
    pub fn with_period_seconds(mut self, seconds: u64) -> Self {
        self.config.interval = std::time::Duration::from_secs(seconds.max(1));
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
            .map_err(|e| WorkerError::Internal(format!("supersession sweeper wtxn: {e}")))?;
        let summary: SweepSummary = sweep_superseded_statements(
            &wtxn,
            self.retention_seconds,
            now_ns,
            self.config.batch_size,
            self.dry_run,
        )
        .map_err(|e| WorkerError::Internal(format!("supersession sweeper: {e}")))?;
        wtxn.commit()
            .map_err(|e| WorkerError::Internal(format!("supersession commit: {e}")))?;
        tracing::debug!(
            target: "brain_workers::supersession_sweeper",
            scanned = summary.scanned,
            deleted = summary.deleted,
            dry_run_would_delete = summary.dry_run_would_delete,
            "supersession sweep complete",
        );
        Ok(summary.deleted as usize)
    }
}

impl Default for SupersessionSweeper {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for SupersessionSweeper {
    fn name(&self) -> &'static str {
        WorkerKind::SupersessionSweeper.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::SupersessionSweeper
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
    fn default_retention_is_disabled() {
        let w = SupersessionSweeper::new();
        assert_eq!(w.retention_seconds, 0);
    }

    #[test]
    fn with_period_seconds_sets_scheduler_interval() {
        let w = SupersessionSweeper::new().with_period_seconds(3600);
        assert_eq!(w.config().interval, std::time::Duration::from_secs(3600));
    }

    #[test]
    fn with_period_seconds_clamps_zero_to_one() {
        // A misconfigured 0 must not become a zero-interval busy loop.
        let w = SupersessionSweeper::new().with_period_seconds(0);
        assert_eq!(w.config().interval, std::time::Duration::from_secs(1));
    }
}
