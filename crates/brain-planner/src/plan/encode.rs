//! `EncodePlan` and its step structs.
//!
//! Phase order:
//! 1. Idempotency check
//! 2. Embedding
//! 3. Context resolution
//! 4. Slot allocation
//! 5. WAL append + fsync — durability barrier
//! 6. Apply: arena + metadata + HNSW
//! 7. Edges
//! 8. Response
//!
//! Single-shard only for now; the `shard` field is always the local
//! shard.

use brain_core::{MemoryKind, RequestId, SessionId, SpaceId};

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
    /// When `true`, the executor computes a content hash and consults
    /// the per-shard `fingerprints` table; on a hit, the existing
    /// `MemoryId` is returned without allocating a new slot.
    pub deduplicate: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct IdempotencyCheckStep {
    pub request_id: RequestId,
}

/// Explicit `SessionId` short-circuits; named sessions are resolved
/// or created in `brain-metadata`.
#[derive(Debug, Clone)]
pub enum ContextResolutionStep {
    Explicit(SessionId),
    GetOrCreate { space_id: SpaceId, name: String },
}

/// The arena grows asynchronously if near full; this step doesn't
/// block on growth.
#[derive(Debug, Clone, Copy)]
pub struct SlotAllocationStep {
    pub arena_grow_if_needed: bool,
}

/// The WAL append is the durability barrier; after fsync, the encode
/// is durable. Group commit batches multiple in-flight encodes into a
/// single fsync.
#[derive(Debug, Clone, Copy)]
pub struct WalAppendStep {
    pub kind: MemoryKind,
    pub salience_initial: f32,
    pub fsync: bool,
}

/// Applied *after* the durability barrier.
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
