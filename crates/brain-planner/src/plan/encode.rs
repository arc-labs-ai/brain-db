//! `EncodePlan` and its step structs. Mirrors spec §08/04 §3's shape.
//!
//! Phase order in the spec:
//! 1. Idempotency check (§4)
//! 2. Embedding (§5)
//! 3. Context resolution (§6)
//! 4. Slot allocation (§7)
//! 5. WAL append + fsync — durability barrier (§8)
//! 6. Apply: arena + metadata + HNSW (§9)
//! 7. Edges (§10)
//! 8. Response (§11)
//!
//! Per orientation plan §4.7, Phase 6 ships single-shard only; the
//! `shard` field is always the local shard.

use brain_core::{AgentId, ContextId, MemoryKind, RequestId};

use super::common::{EdgeSpec, ShardId};
use super::recall::EmbeddingStep;

#[derive(Debug, Clone)]
pub struct EncodePlan {
    pub shard: ShardId,
    pub idempotency_check: IdempotencyCheckStep,
    pub embedding: EmbeddingStep,
    pub context_resolution: ContextResolutionStep,
    pub allocation: SlotAllocationStep,
    pub wal_append: WalAppendStep,
    pub apply: ApplyStep,
    pub edges: Vec<EdgeStep>,
    pub response: EncodeResponseStep,
    pub estimated_cost_ms: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct IdempotencyCheckStep {
    pub request_id: RequestId,
}

/// Spec §08/04 §6 — explicit `ContextId` short-circuits; named
/// contexts are resolved or created in `brain-metadata`.
#[derive(Debug, Clone)]
pub enum ContextResolutionStep {
    Explicit(ContextId),
    GetOrCreate { agent_id: AgentId, name: String },
}

/// Spec §08/04 §7 — the arena grows asynchronously if near full;
/// this step doesn't block on growth.
#[derive(Debug, Clone, Copy)]
pub struct SlotAllocationStep {
    pub arena_grow_if_needed: bool,
}

/// Spec §08/04 §8 — the WAL append is the durability barrier; after
/// fsync, the encode is durable. Group commit batches multiple
/// in-flight encodes into a single fsync (spec §08/04 §16).
#[derive(Debug, Clone, Copy)]
pub struct WalAppendStep {
    pub kind: MemoryKind,
    pub salience_initial: f32,
    pub fsync: bool,
}

/// Spec §08/04 §9 — applied *after* the durability barrier.
#[derive(Debug, Clone, Copy)]
pub struct ApplyStep {
    pub arena_write: bool,
    pub metadata_write: bool,
    pub hnsw_insert: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct EdgeStep {
    pub edge: EdgeSpec,
    pub insert_in_metadata: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct EncodeResponseStep {
    /// Always `true` in v1 — the response carries the persistent
    /// `MemoryId` the client uses for future references.
    pub persistent_id: bool,
}
