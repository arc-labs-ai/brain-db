//! The handle bag passed to every `execute_*` function.
//!
//! Spec §08/08 §7: "handles are cheap to clone (Arc-based). Each
//! executor task gets its own handles; no contention." We use the
//! same pattern: every field is shareable across tasks (Send + Sync).
//!
//! Phase 6.4 ships embedder + index + metadata (read side) + writer
//! (write side). Future sub-tasks may add `arena: Arc<Arena>` if a
//! caller needs raw arena access — current executors don't.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use brain_core::{ContextId, EdgeKind, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, VECTOR_DIM};
use brain_index::SharedHnsw;
use brain_metadata::MetadataDb;
use parking_lot::Mutex;

use super::writer::WriterHandle;

/// Shared handle to the per-shard `MetadataDb`. The `Mutex` enforces
/// the spec §07/08 §3 single-writer-per-shard discipline at runtime
/// (brain-metadata's `write_txn(&mut self)` does it at compile time;
/// the lock lets multiple threads share one DB handle without
/// fracturing into separate redb files). Reads acquire the lock
/// briefly; redb's MVCC means read txns don't block subsequent
/// writes once the lock is released.
pub type SharedMetadataDb = Arc<Mutex<MetadataDb>>;

/// Read-your-writes snapshot of an in-flight transaction. Spec
/// §09/08 §5: RECALL/PLAN/REASON within a txn must see the buffer's
/// pending writes layered on top of committed state. brain-ops
/// builds this from its `TxnBuffer` and attaches it to a cloned
/// `ExecutorContext` for the duration of the request.
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
    pub index: SharedHnsw<384>,
    pub metadata: SharedMetadataDb,
    pub writer: Arc<dyn WriterHandle>,
    /// `Some` only inside the request scope of a txn-flagged op. Carries
    /// the in-flight buffer so the executor's edge / memory lookups can
    /// layer pending state on committed state.
    pub txn: Option<Arc<TxnSnapshot>>,
}

impl ExecutorContext {
    #[must_use]
    pub fn new(
        embedder: Arc<dyn Dispatcher>,
        index: SharedHnsw<384>,
        metadata: SharedMetadataDb,
        writer: Arc<dyn WriterHandle>,
    ) -> Self {
        Self {
            embedder,
            index,
            metadata,
            writer,
            txn: None,
        }
    }

    #[must_use]
    pub fn with_txn(mut self, snapshot: Arc<TxnSnapshot>) -> Self {
        self.txn = Some(snapshot);
        self
    }
}

// After sub-task 9.7 (audit §4) ExecutorContext is intentionally
// `!Send + !Sync`: WriterHandle is per-shard (single-writer-per-shard
// per spec §10/02). Phase 9's per-shard Glommio executor is the
// containment boundary; no cross-thread sharing is required.
