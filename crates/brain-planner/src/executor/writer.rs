//! Per-shard write surface: a channel-fed writer task that batches
//! encodes, group-commits to the WAL, and acks via a return channel.
//! This module ships only the **trait** — the real writer lands with
//! the workers and server.
//!
//! Tests use a `FakeWriterHandle` that drives the test `MetadataDb` and
//! `SharedHnsw` synchronously without WAL — enough to exercise the
//! interface but not the durability story.

use std::future::Future;
use std::pin::Pin;

use brain_core::{ContextId, EdgeKind, MemoryId, MemoryKind, RequestId};
use brain_protocol::envelope::request::ForgetMode;
use thiserror::Error;

/// Per-shard write surface.
///
/// Object-safe via `Pin<Box<dyn Future>>` — `Rc<dyn WriterHandle>`
/// is what the per-shard executor holds. Bare `async fn` in traits in
/// Rust 1.95 can't yet be used through `dyn`, so we hand-roll the return
/// type.
///
/// **`!Send + !Sync`.** Single-writer-per-shard is enforced by living
/// on one Glommio executor — no cross-thread sharing — so `Send + Sync` on
/// the trait would be misleading + over-constraining for concrete impls.
pub trait WriterHandle {
    /// Reserve a fresh `MemoryId` without writing anything. The
    /// returned id may be used by the caller (e.g., a transaction's
    /// pending buffer); if the caller never commits, the slot is
    /// silently skipped — `next_slot` keeps advancing.
    fn reserve_memory_id<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<MemoryId, WriterError>> + 'a>>;

    /// Agent the writer stamps on every memory it creates. Surfaced
    /// to handlers so the wire response can echo the bound agent
    /// without threading it through the request. Default is `nil`
    /// (`AgentId::default`) for impls that don't bind an agent.
    fn agent_id(&self) -> brain_core::AgentId {
        brain_core::AgentId::default()
    }

    /// Push `(memory_id, text)` onto the per-shard ExtractorWorker
    /// channel for out-of-band re-extraction. Returns `true` iff the
    /// enqueue landed (writer has a wired extractor channel and the
    /// queue accepted the payload), `false` otherwise.
    ///
    /// Used by the admin `EXTRACT_BACKFILL` op — operators replay
    /// existing memories through the three-tier extractor pipeline
    /// after enabling the worker or uploading a new schema. The
    /// default implementation drops on the floor so test fakes and
    /// writers without an extractor channel wired keep working
    /// without overriding.
    fn enqueue_for_extraction(&self, _memory_id: MemoryId, _text: &str) -> bool {
        false
    }

    /// Downcast hook for the unified write path. Handlers that want
    /// to call the concrete `RealWriterHandle::submit` (which takes
    /// the brain-ops `Write` value type that brain-planner cannot
    /// import without a dep cycle) do so via
    /// `ctx.executor.writer.as_any().downcast_ref::<RealWriterHandle>()`.
    ///
    /// Default implementation is a no-op (`&()`) — sufficient for
    /// test fakes that don't exercise the unified path. The real
    /// production impl returns `self`.
    fn as_any(&self) -> &dyn std::any::Any {
        &()
    }
}

/// Encode operation payload submitted to the writer. Carries
/// everything the writer needs to:
///
/// 1. Look up idempotency by `request_id`.
/// 2. Allocate a slot, append a WAL record, fsync.
/// 3. Write vector to arena, metadata row to redb, vector to HNSW
/// 4. Insert edge rows.
/// 5. Cache the response in the idempotency table (same write txn).
#[derive(Debug, Clone)]
pub struct EncodeOp {
    pub request_id: RequestId,
    pub context_id: ContextId,
    pub kind: MemoryKind,
    pub text: String,
    pub vector: [f32; brain_embed::VECTOR_DIM],
    pub salience_initial: f32,
    /// Embedding-model fingerprint stamped on the stored row. Wired
    /// from the live dispatcher later; for now the executor passes
    /// `Dispatcher::fingerprint()` through.
    pub fingerprint: [u8; 16],
    pub edges: Vec<EncodeOpEdge>,
    /// When `true`, the writer consults the per-shard `fingerprints`
    /// table keyed by `(agent_id, context_id, content_hash)` and, on a
    /// hit, returns the existing `MemoryId` without allocating a new
    /// slot.
    pub deduplicate: bool,
    /// BLAKE3 over the canonical UTF-8 text. Always computed by
    /// the executor (cheap); the writer only reads it when
    /// `deduplicate` is set.
    pub content_hash: [u8; 32],
    /// **The caller's authenticated agent.** Stamped by the
    /// dispatcher from `ConnPhase::Established.agent` — not from
    /// the wire request. Used by the writer to populate the
    /// memory row, the WAL payload, and the published event so the
    /// subscribe `agents` filter can isolate per-tenant on a
    /// shared shard. Defaults to `AgentId::default()` in tests
    /// that bypass the dispatcher.
    pub agent_id: brain_core::AgentId,
}

#[derive(Debug, Clone, Copy)]
pub struct EncodeOpEdge {
    pub target: MemoryId,
    pub kind: EdgeKind,
    pub weight: f32,
}

/// Per-edge outcome: edges with missing targets are
/// rejected; the encode proceeds without them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeOutcome {
    Inserted,
    TargetMissing,
}

#[derive(Debug, Error)]
pub enum WriterError {
    /// queue over its max length → reject + retry.
    #[error("writer queue overloaded")]
    Overloaded,
    /// Duplicate `request_id` with a different `request_hash`. Client
    /// retries should carry the same params; a hash mismatch indicates
    /// a client bug or RequestId reuse.
    #[error("idempotency conflict: {0}")]
    Conflict(String),
    #[error("writer internal error: {0}")]
    Internal(String),
}

/// Forget operation payload.
#[derive(Debug, Clone, Copy)]
pub struct ForgetOp {
    pub request_id: RequestId,
    pub memory_id: MemoryId,
    pub mode: ForgetMode,
    /// Caller's authenticated agent (see [`EncodeOp::agent_id`]).
    pub agent_id: brain_core::AgentId,
}

/// Per-memory outcome's per-memory error tolerance:
/// missing / already-tombstoned memories aren't errors — they're
/// reported and life goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgetOutcome {
    /// The memory was active; we set the tombstone.
    Tombstoned,
    /// The memory was already tombstoned by an earlier FORGET. No-op.
    AlreadyTombstoned,
    /// The memory_id has no live row in metadata
    /// logs and returns this; not an error.
    MemoryNotFound,
}

/// LINK operation payload.
#[derive(Debug, Clone, Copy)]
pub struct LinkOp {
    pub request_id: RequestId,
    pub source: MemoryId,
    pub target: MemoryId,
    pub kind: EdgeKind,
    /// `[0, 1]` for most kinds; `[-1, 1]` for `Contradicts`.
    pub weight: f32,
    /// Caller's authenticated agent (see [`EncodeOp::agent_id`]).
    pub agent_id: brain_core::AgentId,
}

/// UNLINK operation payload.
#[derive(Debug, Clone, Copy)]
pub struct UnlinkOp {
    pub request_id: RequestId,
    pub source: MemoryId,
    pub target: MemoryId,
    pub kind: EdgeKind,
    /// Caller's authenticated agent (see [`EncodeOp::agent_id`]).
    pub agent_id: brain_core::AgentId,
}
