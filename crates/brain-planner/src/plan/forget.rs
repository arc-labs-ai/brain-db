//! `ForgetPlan` and its step structs.
//!
//! Spec §08/06 §2.1 — single-memory FORGET. The wire `ForgetRequest`
//! (Phase 1) carries only one `MemoryId`; batch + filter targets need
//! a wire bump and land later.
//!
//! Step ordering matches spec §08/06 §14: WAL fsync → arena tombstone
//! → metadata commit → HNSW mark removed. The plan describes each
//! step; the writer task enforces the order at execution time.

use brain_core::MemoryId;
use brain_protocol::request::ForgetMode;

use super::common::ShardId;
use super::encode::IdempotencyCheckStep;

#[derive(Debug, Clone, Copy)]
pub struct ForgetPlan {
    pub shard: ShardId,
    pub memory_id: MemoryId,
    pub mode: ForgetMode,
    pub idempotency_check: IdempotencyCheckStep,
    pub wal_append: ForgetWalStep,
    pub apply: ForgetApplyStep,
    pub response: ForgetResponseStep,
    pub estimated_cost_ms: f32,
}

/// WAL record for FORGET. Carries the mode so recovery knows whether
/// to apply zeroing (spec §08/06 §4).
#[derive(Debug, Clone, Copy)]
pub struct ForgetWalStep {
    pub fsync: bool,
    pub mode: ForgetMode,
}

/// What the apply phase does. Spec §08/06 §3 + §4 + §14.
#[derive(Debug, Clone, Copy)]
pub struct ForgetApplyStep {
    pub arena_tombstone: bool,
    pub metadata_commit: bool,
    pub hnsw_mark_removed: bool,
    /// Spec §08/06 §4 — hard forget only.
    pub arena_zero_vector: bool,
    /// Spec §08/06 §4 — hard forget only.
    pub text_zero: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ForgetResponseStep {
    /// Spec §08/06 §10 — the response indicates which memories were
    /// processed and how. Always `true` for v1's single-memory shape;
    /// the field exists so a future batch variant can carry richer
    /// per-id outcomes.
    pub include_outcome: bool,
}
