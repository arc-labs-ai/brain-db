//! Per-shard write surface. Spec §08/08 §10 describes a channel-fed
//! writer task that batches encodes, group-commits to the WAL, and
//! acks via a return channel. Phase 6 ships only the **trait** — the
//! real writer lands in Phase 8 (workers) / Phase 9 (server).
//!
//! Tests use a `FakeWriterHandle` that drives the test `MetadataDb` and
//! `SharedHnsw` synchronously without WAL — enough to exercise the
//! interface but not the durability story.

use std::future::Future;
use std::pin::Pin;

use brain_core::{ContextId, EdgeKind, MemoryId, MemoryKind, RequestId};
use brain_protocol::request::ForgetMode;
use thiserror::Error;

/// Per-shard write surface.
///
/// Object-safe via `Pin<Box<dyn Future>>` — `Arc<dyn WriterHandle>`
/// is what the executor holds. Bare `async fn` in traits in Rust 1.95
/// can't yet be used through `dyn`, so we hand-roll the return type.
pub trait WriterHandle: Send + Sync {
    fn submit_encode<'a>(
        &'a self,
        op: EncodeOp,
    ) -> Pin<Box<dyn Future<Output = Result<EncodeAck, WriterError>> + Send + 'a>>;

    fn submit_forget<'a>(
        &'a self,
        op: ForgetOp,
    ) -> Pin<Box<dyn Future<Output = Result<ForgetAck, WriterError>> + Send + 'a>>;
}

/// Encode operation payload submitted to the writer. Carries
/// everything the writer needs to:
///
/// 1. Look up idempotency by `request_id` (spec §08/04 §4).
/// 2. Allocate a slot, append a WAL record, fsync (spec §08/04 §7-§8).
/// 3. Write vector to arena, metadata row to redb, vector to HNSW
///    (spec §08/04 §9).
/// 4. Insert edge rows (spec §08/04 §10).
/// 5. Cache the response in the idempotency table (same write txn).
#[derive(Debug, Clone)]
pub struct EncodeOp {
    pub request_id: RequestId,
    pub context_id: ContextId,
    pub kind: MemoryKind,
    pub text: String,
    pub vector: [f32; brain_embed::VECTOR_DIM],
    pub salience_initial: f32,
    /// Embedding-model fingerprint stamped on the stored row. Phase 7
    /// wires this from the live dispatcher; for now the executor passes
    /// `Dispatcher::fingerprint()` through.
    pub fingerprint: [u8; 16],
    pub edges: Vec<EncodeOpEdge>,
}

#[derive(Debug, Clone, Copy)]
pub struct EncodeOpEdge {
    pub target: MemoryId,
    pub kind: EdgeKind,
    pub weight: f32,
}

/// Writer's ack. Spec §08/04 §11.
#[derive(Debug, Clone)]
pub struct EncodeAck {
    pub memory_id: MemoryId,
    pub edge_results: Vec<EdgeOutcome>,
    /// `true` iff this ack came from a replayed idempotency entry;
    /// `false` for a fresh write. Spec §08/04 §4.
    pub replayed: bool,
}

/// Per-edge outcome. Spec §08/04 §10: edges with missing targets are
/// rejected; the encode proceeds without them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeOutcome {
    Inserted,
    TargetMissing,
}

#[derive(Debug, Error)]
pub enum WriterError {
    /// Spec §08/08 §14: queue over its max length → reject + retry.
    #[error("writer queue overloaded")]
    Overloaded,
    #[error("writer internal error: {0}")]
    Internal(String),
}

/// Forget operation payload. Spec §08/06 §1.
#[derive(Debug, Clone, Copy)]
pub struct ForgetOp {
    pub request_id: RequestId,
    pub memory_id: MemoryId,
    pub mode: ForgetMode,
}

/// Per-memory outcome. Spec §08/06 §10's per-memory error tolerance:
/// missing / already-tombstoned memories aren't errors — they're
/// reported and life goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgetOutcome {
    /// The memory was active; we set the tombstone.
    Tombstoned,
    /// The memory was already tombstoned by an earlier FORGET. No-op.
    AlreadyTombstoned,
    /// The memory_id has no live row in metadata. Spec §08/06 §10
    /// logs and returns this; not an error.
    MemoryNotFound,
}

/// Writer's ack for a FORGET. Spec §08/06 §11.
#[derive(Debug, Clone, Copy)]
pub struct ForgetAck {
    pub memory_id: MemoryId,
    pub outcome: ForgetOutcome,
    /// `true` iff this ack came from a replayed idempotency entry.
    pub replayed: bool,
}
