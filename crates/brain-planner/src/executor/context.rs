//! The handle bag passed to every `execute_*` function.
//!
//! Handles are cheap to clone (Arc-based). Each executor task gets its
//! own handles; no contention. Every field is shareable across tasks
//! (Send + Sync).
//!
//! Ships embedder + index + metadata (read side) + writer (write
//! side). An `arena: Arc<Arena>` field may be added later if a caller
//! needs raw arena access — current executors don't.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use brain_core::{ContextId, EdgeKind, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, VECTOR_DIM};
use brain_index::SharedHnsw;
use brain_metadata::MetadataDb;

use super::writer::WriterHandle;

/// Shared handle to the per-shard `MetadataDb`. Both reads and writes
/// flow through `&self` — redb's MVCC lets unbounded readers share
/// the handle without locking, and redb itself serialises writes per
/// database. The single-writer-per-shard invariant lives in the
/// shard's writer task discipline, not in a mutex; wrapping the DB
/// in `Arc<Mutex<...>>` previously serialised readers against
/// readers for no real safety win.
pub type SharedMetadataDb = Arc<MetadataDb>;

/// Read-your-writes snapshot of an in-flight transaction.
/// RECALL/PLAN/REASON within a txn must see the buffer's pending
/// writes layered on top of committed state. brain-ops builds this
/// from its `TxnBuffer` and attaches it to a cloned `ExecutorContext`
/// for the duration of the request.
#[derive(Clone, Debug, Default)]
pub struct TxnSnapshot {
    /// Pending edges added by the txn: `(source, kind, target, weight)`.
    pub pending_links: Vec<(MemoryId, EdgeKind, MemoryId, f32)>,
    /// Edges the txn has removed (canonical triple).
    pub pending_unlinks: HashSet<(MemoryId, EdgeKind, MemoryId)>,
    /// Pending memories created in the txn: vector + salience + kind +
    /// context + created_at. Used for the RECALL lens (cosine over
    /// pending vectors) and for REASON's base-resolution (a base
    /// memory id might point at a pending row).
    pub pending_memories: HashMap<MemoryId, PendingMemorySnapshot>,
    /// Memories tombstoned by an in-txn FORGET. Dropped from lens
    /// outputs in RECALL/PLAN/REASON.
    pub tombstoned: HashSet<MemoryId>,
}

#[derive(Clone, Debug)]
pub struct PendingMemorySnapshot {
    pub vector: [f32; VECTOR_DIM],
    pub salience: f32,
    pub kind: MemoryKind,
    pub context_id: ContextId,
    pub created_at_unix_nanos: u64,
}

/// Executor-side context. Cheap to clone (every field is `Arc` or
/// already cheap-clone like `SharedHnsw`).
#[derive(Clone)]
pub struct ExecutorContext {
    pub embedder: Arc<dyn Dispatcher>,
    pub index: SharedHnsw,
    pub metadata: SharedMetadataDb,
    pub writer: Arc<dyn WriterHandle>,
    /// `Some` only inside the request scope of a txn-flagged op. Carries
    /// the in-flight buffer so the executor's edge / memory lookups can
    /// layer pending state on committed state.
    pub txn: Option<Arc<TxnSnapshot>>,
    /// Authenticated caller for **this request only**. The shared
    /// per-shard `ExecutorContext` carries the connection-less
    /// default; `brain-ops::dispatch` clones the ctx and stamps the
    /// per-request value via [`Self::with_caller_space`] before
    /// invoking handlers. The encode executor reads it to populate
    /// `EncodeOp.space_id`, which the writer then stamps onto the
    /// memory row + WAL payload + EventEnvelope so the subscribe
    /// `spaces` filter can isolate per-tenant.
    pub caller_space: brain_core::SpaceId,
    /// Authenticated caller's namespace (tenant) for **this request
    /// only**, the outer half of the `(namespace, space)` scope key.
    /// Stamped per-request by `brain-ops::dispatch` alongside
    /// `caller_space`; the encode executor passes it to the writer so
    /// every row is owned by the caller's tenant, and the read path
    /// scopes results to it. Defaults to [`NamespaceId::SYSTEM`].
    pub caller_namespace: brain_core::NamespaceId,
}

impl ExecutorContext {
    #[must_use]
    pub fn new(
        embedder: Arc<dyn Dispatcher>,
        index: SharedHnsw,
        metadata: SharedMetadataDb,
        writer: Arc<dyn WriterHandle>,
    ) -> Self {
        Self {
            embedder,
            index,
            metadata,
            writer,
            txn: None,
            caller_space: brain_core::SpaceId::default(),
            caller_namespace: brain_core::NamespaceId::SYSTEM,
        }
    }

    #[must_use]
    pub fn with_txn(mut self, snapshot: Arc<TxnSnapshot>) -> Self {
        self.txn = Some(snapshot);
        self
    }

    /// Stamp the per-request authenticated space. Called by
    /// `brain-ops::dispatch` after cloning the shared ctx so the
    /// per-request flow doesn't mutate shared state.
    #[must_use]
    pub fn with_caller_space(mut self, space: brain_core::SpaceId) -> Self {
        self.caller_space = space;
        self
    }

    /// Stamp the per-request authenticated namespace (tenant). Called by
    /// `brain-ops::dispatch` alongside [`Self::with_caller_space`].
    #[must_use]
    pub fn with_caller_namespace(mut self, namespace: brain_core::NamespaceId) -> Self {
        self.caller_namespace = namespace;
        self
    }
}

// ExecutorContext is intentionally `!Send + !Sync`: WriterHandle is
// per-shard (single-writer-per-shard). The per-shard Glommio executor
// is the containment boundary; no cross-thread sharing is required.
