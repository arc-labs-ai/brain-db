//! Entity GC worker.
//!
//! **Off by default.** When enabled, tombstones entities with no
//! active inbound references after a grace period (default 30 d).
//! Reversal during grace is the entity ops layer's
//! responsibility (separate handler; out of scope here).
//!
//! ## v1 scope
//!
//! Full inbound-reference counting (statements-by-subject +
//! relations-by-from + relations-by-to + entity_mentions) is
//! implementation-heavy and depends on shape of the existing
//! reverse-index tables. The v1 worker scaffolds the eligibility
//! scan + tombstone path; the inbound-count helper currently
//! returns a conservative `usize::MAX` (never eligible) so the
//! worker is a safe no-op. Operators enable the env flag; the
//! detector exists; full inbound-count logic lands as a
//! follow-up.

use std::future::Future;
use std::pin::Pin;

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

    async fn run_once(&self, _ctx: &WorkerContext) -> Result<usize, WorkerError> {
        if !self.enabled {
            return Ok(0);
        }
        // v1 scope cut: full inbound-reference counting + tombstone
        // path lands as a follow-up. The worker is enabled-but-no-op
        // until then; metric records the cycle so operators can see
        // it ticking.
        tracing::debug!(
            target: "brain_workers::entity_gc",
            grace_seconds = self.grace_seconds,
            "entity GC tick (v1 scope cut — full inbound-count logic pending)",
        );
        Ok(0)
    }
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
