//! Entity GC worker.
//!
//! **Off by default.** When enabled, tombstones entities that have no
//! inbound references after a grace period (default 30 d). It only ever
//! sets the tombstone flag + writes an audit row; physical reclamation
//! rides Brain's existing tombstone-grace-then-reclaim flow. Reversal
//! (clearing the tombstone when a new inbound reference is created during
//! grace) is the entity-ops layer's contract, not this worker's.
//!
//! ## Sweep shape
//!
//! Each enabled tick is two phases, honouring the single-writer
//! discipline:
//! 1. **Collect** — under a read txn, scan live entities and keep those
//!    past grace with `entity_inbound_reference_count == 0`, up to
//!    `config.batch_size`.
//! 2. **Re-check + tombstone** — open one write txn, and (via a read txn
//!    that sees the latest committed state while we hold the writer lock)
//!    re-verify each candidate is still orphaned + past grace before
//!    tombstoning it. This catches an inbound write that landed between
//!    the two phases.

use std::future::Future;
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::AuditId;
use brain_metadata::tables::audit::{resolution_outcome, ResolutionAudit};
use brain_metadata::tables::entity::flags as entity_flags;
use brain_metadata::{
    entity_get, entity_inbound_reference_count, entity_iter_live_for_gc, entity_tombstone,
    resolution_audit_write,
};

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// 30 days in seconds.
pub const DEFAULT_ENTITY_GC_GRACE_SECONDS: u64 = 30 * 24 * 60 * 60;

pub struct EntityGcWorker {
    config: WorkerConfig,
    enabled: bool,
    grace_seconds: u64,
}

impl EntityGcWorker {
    /// New worker — **disabled** by default.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::EntityGc),
            enabled: false,
            grace_seconds: DEFAULT_ENTITY_GC_GRACE_SECONDS,
        }
    }

    #[must_use]
    pub fn enabled(mut self) -> Self {
        self.enabled = true;
        self
    }

    #[must_use]
    pub fn with_grace_seconds(mut self, seconds: u64) -> Self {
        self.grace_seconds = seconds;
        self
    }

    async fn run_once(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
        if !self.enabled {
            return Ok(0);
        }
        let now_ns = now_unix_nanos();
        let grace_ns = self.grace_seconds.saturating_mul(1_000_000_000);
        let batch = self.config.batch_size;
        let metadata = ctx.ops.executor.metadata.as_ref();

        // Phase 1 — collect eligible candidates under a read txn.
        let candidates: Vec<(brain_core::EntityId, brain_metadata::RowScope)> = {
            let rtxn = metadata
                .read_txn()
                .map_err(|e| WorkerError::Internal(format!("entity gc rtxn: {e}")))?;
            let live = entity_iter_live_for_gc(&rtxn)
                .map_err(|e| WorkerError::Internal(format!("entity gc scan: {e}")))?;
            let mut out = Vec::new();
            for (id, scope, created_at) in live {
                if now_ns.saturating_sub(created_at) < grace_ns {
                    continue;
                }
                let count = entity_inbound_reference_count(&rtxn, scope, id)
                    .map_err(|e| WorkerError::Internal(format!("entity gc inbound count: {e}")))?;
                if count == 0 {
                    out.push((id, scope));
                    if out.len() >= batch {
                        break;
                    }
                }
                // Cooperative yield during the O(N) scan so foreground ops
                // keep their latency. Safe on the glommio executor; the
                // read txn is `!Send` and stays on this task.
                glommio::executor().yield_if_needed().await;
            }
            out
        };

        if candidates.is_empty() {
            return Ok(0);
        }

        // Phase 2 — re-check + tombstone under a single write txn.
        let wtxn = metadata
            .write_txn()
            .map_err(|e| WorkerError::Internal(format!("entity gc wtxn: {e}")))?;
        // A read txn opened while we hold the writer lock observes the
        // latest committed state (redb has one writer at a time), so the
        // re-check sees any inbound write that committed after phase 1.
        let recheck = metadata
            .read_txn()
            .map_err(|e| WorkerError::Internal(format!("entity gc recheck rtxn: {e}")))?;
        let mut tombstoned = 0usize;
        for (id, scope) in candidates {
            let Some(entity) = entity_get(&recheck, id)
                .map_err(|e| WorkerError::Internal(format!("entity gc recheck get: {e}")))?
            else {
                continue; // vanished between the phases.
            };
            if entity.flags & entity_flags::TOMBSTONED != 0 {
                continue; // concurrently tombstoned.
            }
            if now_ns.saturating_sub(entity.created_at_unix_nanos) < grace_ns {
                continue; // defensive — created_at is immutable.
            }
            let count = entity_inbound_reference_count(&recheck, scope, id)
                .map_err(|e| WorkerError::Internal(format!("entity gc recheck count: {e}")))?;
            if count != 0 {
                continue; // gained a reference since phase 1.
            }

            entity_tombstone(&wtxn, id, now_ns)
                .map_err(|e| WorkerError::Internal(format!("entity gc tombstone: {e}")))?;

            // One audit row per swept entity: outcome TOMBSTONED_ENTITY_GC
            // (reason: no inbound references past grace — EntityGcEligible),
            // committed in the same wtxn as the tombstone.
            let mut audit = ResolutionAudit::new(
                AuditId::new(),
                entity.canonical_name.clone(),
                entity.entity_type.raw(),
                resolution_outcome::TOMBSTONED_ENTITY_GC,
                1.0,
                now_ns,
            );
            audit.resolved_entity_bytes = Some(id.to_bytes());
            resolution_audit_write(&wtxn, &audit)
                .map_err(|e| WorkerError::Internal(format!("entity gc audit: {e}")))?;
            tombstoned += 1;
        }
        drop(recheck);
        wtxn.commit()
            .map_err(|e| WorkerError::Internal(format!("entity gc commit: {e}")))?;

        tracing::debug!(
            target: "brain_workers::entity_gc",
            grace_seconds = self.grace_seconds,
            tombstoned,
            "entity GC sweep complete",
        );
        Ok(tombstoned)
    }
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl Default for EntityGcWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for EntityGcWorker {
    fn name(&self) -> &'static str {
        WorkerKind::EntityGc.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::EntityGc
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(self.run_once(ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default() {
        let w = EntityGcWorker::new();
        assert!(!w.enabled);
    }
}
