//! Typed request / progress shapes for state-carrying workers.
//! Spec §27/04.
//!
//! These are pure value types shared across the CLI / admin HTTP
//! surfaces, brain-workers, and the per-shard scheduler. They have
//! no I/O and no async — handed to the worker which owns the
//! persistence + execution.

use std::time::Duration;

use uuid::Uuid;

use crate::knowledge::ExtractorId;
use crate::MemoryId;

// ---------------------------------------------------------------------------
// Identifiers.
// ---------------------------------------------------------------------------

/// UUIDv7 identifier for a single backfill / migration request.
/// Lets operators cancel + look up by id.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BackfillId(pub Uuid);

impl BackfillId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    #[must_use]
    pub const fn from_uuid(u: Uuid) -> Self {
        Self(u)
    }

    #[must_use]
    pub const fn to_bytes(self) -> [u8; 16] {
        *self.0.as_bytes()
    }

    #[must_use]
    pub const fn from_bytes(b: [u8; 16]) -> Self {
        Self(Uuid::from_bytes(b))
    }
}

impl Default for BackfillId {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// BackfillRange.
// ---------------------------------------------------------------------------

/// Which memories a backfill request covers. Spec §27/04 §3.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackfillRange {
    /// Every memory in the shard's metadata table.
    All,
    /// Inclusive id range, walked in sorted order. `start <= end`.
    ById { start: MemoryId, end: MemoryId },
}

// ---------------------------------------------------------------------------
// WorkerPriority.
// ---------------------------------------------------------------------------

/// Mirrors §27/00 §"Scheduling priorities and budgets". Backfill
/// runs in `Background` by default; operators can bump for one-off
/// rebuilds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WorkerPriority {
    Foreground,
    NearForeground,
    Background,
    Low,
}

impl WorkerPriority {
    /// The default priority for the backfill worker (spec §27/04 §3).
    #[must_use]
    pub const fn backfill_default() -> Self {
        Self::Background
    }
}

// ---------------------------------------------------------------------------
// BackfillRequest.
// ---------------------------------------------------------------------------

/// One end-to-end backfill request. Submitted by an admin
/// (CLI or HTTP); the worker walks `memory_range × extractor_ids`
/// under the per-(memory, extractor) checkpoint table.
///
/// Cancellation: an admin issues `CancelBackfill(request_id)`; the
/// worker checks the cancel flag between items.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackfillRequest {
    pub request_id: BackfillId,
    pub memory_range: BackfillRange,
    /// Up to 4 extractors per request — backfill against more is a
    /// new request (avoids unbounded per-memory item count and
    /// keeps progress reporting comprehensible).
    pub extractor_ids: Vec<ExtractorId>,
    pub priority: WorkerPriority,
    /// `true` = walk the plan, mark each item `Completed`, do not
    /// invoke extractors. Used to preview cost + plan shape.
    pub dry_run: bool,
}

impl BackfillRequest {
    /// Builder constructor with sensible defaults.
    #[must_use]
    pub fn new(memory_range: BackfillRange, extractor_ids: Vec<ExtractorId>) -> Self {
        Self {
            request_id: BackfillId::new(),
            memory_range,
            extractor_ids,
            priority: WorkerPriority::backfill_default(),
            dry_run: false,
        }
    }

    #[must_use]
    pub fn with_priority(mut self, priority: WorkerPriority) -> Self {
        self.priority = priority;
        self
    }

    #[must_use]
    pub fn dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }
}

// ---------------------------------------------------------------------------
// BackfillProgress.
// ---------------------------------------------------------------------------

/// Operator-visible progress snapshot. Returned by
/// `admin backfill status` (CLI) / equivalent HTTP route.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackfillProgress {
    pub request_id: Option<BackfillId>,
    /// Items marked `Completed` during this run (including dry-run).
    pub completed: u64,
    /// Items that hit `MAX_ATTEMPTS` failures.
    pub failed: u64,
    /// Items whose checkpoint was already `Completed` from a prior
    /// run (resume path).
    pub skipped_already_completed: u64,
    /// Last `MemoryId` processed — useful for showing a progress
    /// cursor in CLI.
    pub last_processed_memory_id: Option<MemoryId>,
    /// `true` iff the worker is mid-run.
    pub running: bool,
    /// Estimated wall-time remaining at the worker's current
    /// throughput. `None` until the worker has processed > 50 items.
    pub eta: Option<Duration>,
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backfill_id_round_trips_through_bytes() {
        let a = BackfillId::new();
        let b = BackfillId::from_bytes(a.to_bytes());
        assert_eq!(a, b);
    }

    #[test]
    fn backfill_request_builder_defaults() {
        let req = BackfillRequest::new(BackfillRange::All, vec![ExtractorId(1)]);
        assert!(matches!(req.memory_range, BackfillRange::All));
        assert_eq!(req.extractor_ids, vec![ExtractorId(1)]);
        assert_eq!(req.priority, WorkerPriority::Background);
        assert!(!req.dry_run);
    }

    #[test]
    fn backfill_request_dry_run_helper() {
        let req = BackfillRequest::new(BackfillRange::All, Vec::new()).dry_run();
        assert!(req.dry_run);
    }

    #[test]
    fn backfill_progress_default_is_idle() {
        let p = BackfillProgress::default();
        assert!(!p.running);
        assert_eq!(p.completed, 0);
        assert_eq!(p.last_processed_memory_id, None);
    }
}
