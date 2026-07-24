//! Types shared across the per-request plan modules.

use brain_core::{EdgeKind, MemoryId, MemoryKind, SessionId};

/// Logical shard reference: the plan is at a level of abstraction
/// above transport. The executor maps shard references to actual
/// destinations. We re-export the type alias from `brain_core`
/// (`= u16`) so the planner and the storage layers share one shard-id
/// encoding; sharding wires the routing later.
pub use brain_core::ShardId;

/// Whether a filter rule runs before or after ANN candidate
/// gathering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterStage {
    PreFilter,
    PostFilter,
}

/// Merge-step sort key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Score,
    Salience,
    InsertedAt,
}

/// Concrete predicate the executor applies to candidates. Categories
/// are expanded into typed variants so the executor can dispatch
/// without re-parsing.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterRule {
    KindIn(Vec<MemoryKind>),
    SessionIn(Vec<SessionId>),
    SalienceFloor(f32),
    AgeBound { not_older_than_unix_nanos: u64 },
    ConfidenceFloor(f32),
}

/// Planner-side edge spec. Distinct from `brain_protocol::EdgeRequest`
/// which is the wire representation; this one uses the resolved
/// `MemoryId` + `EdgeKind` types from `brain-core`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeSpec {
    pub target: MemoryId,
    pub kind: EdgeKind,
    pub weight: f32,
}

/// A sub-RECALL embedded inside a higher-level plan (PLAN, REASON).
/// Named `RecallSubStep` to keep it distinct from the top-level
/// `RecallPlan` (which is the shape returned by `plan_recall`).
///
/// Mirrors the recall plan's shape but flattened to one shard (the
/// only shape v1 needs; cross-shard fan-out arrives later).
#[derive(Debug, Clone)]
pub struct RecallSubStep {
    pub embedding: super::recall::EmbeddingStep,
    pub shard: super::recall::ShardSearchStep,
    pub merge: super::recall::MergeStep,
}
