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
/// Object-safe via `Pin<Box<dyn Future>>` — `Rc<dyn WriterHandle>`
/// is what the per-shard executor holds. Bare `async fn` in traits in
/// Rust 1.95 can't yet be used through `dyn`, so we hand-roll the return
/// type.
///
/// **`!Send + !Sync`** per the audit (`docs/development/phases/phase-09-glommio-port.md`
/// §4). Phase 9 enforces single-writer-per-shard (spec §10/02) by living
/// on one Glommio executor — no cross-thread sharing — so `Send + Sync` on
/// the trait would be misleading + over-constraining for concrete impls.
pub trait WriterHandle {
    fn submit_encode<'a>(
        &'a self,
        op: EncodeOp,
    ) -> Pin<Box<dyn Future<Output = Result<EncodeAck, WriterError>> + 'a>>;

    fn submit_forget<'a>(
        &'a self,
        op: ForgetOp,
    ) -> Pin<Box<dyn Future<Output = Result<ForgetAck, WriterError>> + 'a>>;

    fn submit_link<'a>(
        &'a self,
        op: LinkOp,
    ) -> Pin<Box<dyn Future<Output = Result<LinkAck, WriterError>> + 'a>>;

    fn submit_unlink<'a>(
        &'a self,
        op: UnlinkOp,
    ) -> Pin<Box<dyn Future<Output = Result<UnlinkAck, WriterError>> + 'a>>;

    /// Reserve a fresh `MemoryId` without writing anything. The
    /// returned id may be used by the caller (e.g., a transaction's
    /// pending buffer); if the caller never commits, the slot is
    /// silently skipped — `next_slot` keeps advancing. Spec §09/08
    /// §10 caps txns at 1000 ops, bounding the leak.
    fn reserve_memory_id<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<MemoryId, WriterError>> + 'a>>;

    /// Apply a pre-built batch of buffered operations atomically.
    /// Used by `TXN_COMMIT` — one redb write txn, all-or-nothing.
    /// On any failure the wtxn is dropped (redb auto-rolls-back) and
    /// the buffer is reported as not applied.
    fn submit_batch<'a>(
        &'a self,
        batch: TxnBatch,
    ) -> Pin<Box<dyn Future<Output = Result<TxnBatchAck, WriterError>> + 'a>>;

    /// Agent the writer stamps on every memory it creates. Surfaced
    /// to handlers so the wire response can echo the bound agent
    /// without threading it through the request. Default is `nil`
    /// (`AgentId::default`) for impls that don't bind an agent.
    fn agent_id(&self) -> brain_core::AgentId {
        brain_core::AgentId::default()
    }
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
    /// Spec §07/07 §6 — when `true`, the writer consults the per-
    /// shard `fingerprints` table keyed by
    /// `(agent_id, context_id, content_hash)` and, on a hit, returns
    /// the existing `MemoryId` without allocating a new slot.
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

/// Writer's ack. Spec §08/04 §11.
#[derive(Debug, Clone)]
pub struct EncodeAck {
    pub memory_id: MemoryId,
    pub edge_results: Vec<EdgeOutcome>,
    /// `true` iff this ack came from a replayed idempotency entry
    /// (same `request_id` retried); `false` for a fresh write.
    /// Spec §08/04 §4. **Transparent to the caller** — never
    /// surfaced in the wire response.
    pub replayed: bool,
    /// `true` iff `op.deduplicate` was set AND the fingerprint
    /// lookup hit an existing Active memory; the returned
    /// `memory_id` is that prior memory's. Spec §07/07 §6.
    pub was_deduplicated: bool,
    /// WAL LSN this encode was recorded at. `Some(lsn)` when the
    /// shard has a wired WAL sink (production); `None` for the
    /// legacy in-memory test path that mints LSNs from the event
    /// bus. Surfaced to the client so they can chain
    /// `encode → subscribe --start-lsn lsn+1` to follow downstream
    /// events.
    pub lsn: Option<u64>,
    /// Outgoing edges actually inserted (`EdgeOutcome::Inserted`
    /// count). Reported back so clients can show "5 of 7 edges
    /// landed; 2 targets were missing."
    pub edges_out_count: u32,
    /// Server unix-nanos timestamp stamped on the memory row.
    /// Useful when the client clock drifts vs the server.
    pub created_at_unix_nanos: u64,
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
    /// Spec §07/06 §5 — duplicate `request_id` with a different
    /// `request_hash`. Client retries should carry the same params;
    /// a hash mismatch indicates a client bug or RequestId reuse.
    #[error("idempotency conflict: {0}")]
    Conflict(String),
    #[error("writer internal error: {0}")]
    Internal(String),
}

/// Forget operation payload. Spec §08/06 §1.
#[derive(Debug, Clone, Copy)]
pub struct ForgetOp {
    pub request_id: RequestId,
    pub memory_id: MemoryId,
    pub mode: ForgetMode,
    /// Caller's authenticated agent (see [`EncodeOp::agent_id`]).
    pub agent_id: brain_core::AgentId,
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

/// LINK operation payload. Spec §09/07.
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

/// Writer's ack for a LINK. Spec §09/07 §3.
#[derive(Debug, Clone, Copy)]
pub struct LinkAck {
    pub source: MemoryId,
    pub target: MemoryId,
    pub kind: EdgeKind,
    pub weight: f32,
    pub created_at_unix_nanos: u64,
    /// `true` when the edge already existed (LINK is overwriting weight);
    /// `false` for a brand-new edge.
    pub already_existed: bool,
    /// `true` iff this ack came from a replayed idempotency entry.
    pub replayed: bool,
}

/// UNLINK operation payload. Spec §09/07 §4-§5.
#[derive(Debug, Clone, Copy)]
pub struct UnlinkOp {
    pub request_id: RequestId,
    pub source: MemoryId,
    pub target: MemoryId,
    pub kind: EdgeKind,
    /// Caller's authenticated agent (see [`EncodeOp::agent_id`]).
    pub agent_id: brain_core::AgentId,
}

/// Writer's ack for an UNLINK. Spec §09/07 §5: non-existent edge →
/// `removed: false`, no error (idempotent).
#[derive(Debug, Clone, Copy)]
pub struct UnlinkAck {
    pub source: MemoryId,
    pub target: MemoryId,
    pub kind: EdgeKind,
    pub removed: bool,
    pub replayed: bool,
}

// ---------------------------------------------------------------------------
// TxnBatch — buffered transaction payload for atomic apply.
// ---------------------------------------------------------------------------

/// A buffered transaction's payload, handed to `submit_batch` by the
/// COMMIT path. Order matters: edges may reference memories created
/// earlier in the same batch.
#[derive(Debug, Clone, Default)]
pub struct TxnBatch {
    pub memories: Vec<TxnEncode>,
    pub links: Vec<TxnLink>,
    pub unlinks: Vec<TxnUnlink>,
    pub forgets: Vec<TxnForget>,
}

/// A pre-allocated memory destined for commit. `memory_id` was
/// returned by `reserve_memory_id` at buffer time; the apply path
/// writes the metadata row + HNSW vector + idempotency entry atomically.
#[derive(Debug, Clone)]
pub struct TxnEncode {
    pub memory_id: MemoryId,
    pub request_id: RequestId,
    pub request_hash: [u8; 32],
    pub context_id: ContextId,
    pub kind: MemoryKind,
    pub text: String,
    pub vector: [f32; brain_embed::VECTOR_DIM],
    pub salience_initial: f32,
    pub fingerprint: [u8; 16],
    pub edges: Vec<EncodeOpEdge>,
    pub created_at_unix_nanos: u64,
    /// Caller's authenticated agent (see [`EncodeOp::agent_id`]).
    pub agent_id: brain_core::AgentId,
}

#[derive(Debug, Clone, Copy)]
pub struct TxnLink {
    pub request_id: RequestId,
    pub request_hash: [u8; 32],
    pub source: MemoryId,
    pub target: MemoryId,
    pub kind: EdgeKind,
    pub weight: f32,
    pub created_at_unix_nanos: u64,
    /// Caller's authenticated agent (see [`EncodeOp::agent_id`]).
    pub agent_id: brain_core::AgentId,
}

#[derive(Debug, Clone, Copy)]
pub struct TxnUnlink {
    pub request_id: RequestId,
    pub request_hash: [u8; 32],
    pub source: MemoryId,
    pub target: MemoryId,
    pub kind: EdgeKind,
    pub created_at_unix_nanos: u64,
    /// Caller's authenticated agent (see [`EncodeOp::agent_id`]).
    pub agent_id: brain_core::AgentId,
}

#[derive(Debug, Clone, Copy)]
pub struct TxnForget {
    pub request_id: RequestId,
    pub request_hash: [u8; 32],
    pub memory_id: MemoryId,
    pub mode: brain_protocol::request::ForgetMode,
    pub created_at_unix_nanos: u64,
    /// Caller's authenticated agent (see [`EncodeOp::agent_id`]).
    pub agent_id: brain_core::AgentId,
}

/// Result of a successful `submit_batch`. Per-op acks come back in
/// the same order as the corresponding `TxnBatch` field.
#[derive(Debug, Clone)]
pub struct TxnBatchAck {
    pub encodes: Vec<EncodeAck>,
    pub links: Vec<LinkAck>,
    pub unlinks: Vec<UnlinkAck>,
    pub forgets: Vec<ForgetAck>,
}
