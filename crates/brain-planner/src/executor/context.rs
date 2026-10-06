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
use std::rc::Rc;
use std::sync::Arc;

use brain_core::{
    BackfillId, BackfillProgress, BackfillRequest, EdgeKind, MemoryId, MemoryKind, SessionId,
};
use brain_embed::{Dispatcher, VECTOR_DIM};
use brain_index::{SharedHnsw, SpaceVectorSource};
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
    pub session_id: SessionId,
    pub created_at_unix_nanos: u64,
}

/// Control handle onto the per-shard backfill worker.
///
/// `brain_workers::BackfillWorker` lives above this crate in the
/// dependency graph, so the concrete worker can't be named here. The
/// shard registers one worker in its scheduler and threads the *same*
/// `Arc` onto the executor context as this trait object, giving the
/// dispatch path (`ADMIN_BACKFILL` / `ADMIN_BACKFILL_CANCEL`) a way to
/// submit and cancel resumable runs without a back-dependency on
/// `brain-workers`.
///
/// The worker's `submit` / `cancel` / `progress` are all `&self` over
/// interior state (single-writer-per-shard; no new lock on a hot
/// path), so a shared `Arc` suffices — no `&mut` and no `Mutex` wrapper.
pub trait BackfillControl {
    /// Enqueue a backfill run; returns its id (the idempotency key).
    fn submit(&self, request: BackfillRequest) -> BackfillId;
    /// Flag the in-flight run matching `request_id` for cancellation.
    /// Returns `true` if a matching run was flagged.
    fn cancel(&self, request_id: BackfillId) -> bool;
    /// Snapshot the most-recent run's progress.
    fn progress(&self) -> BackfillProgress;
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
    /// scopes results to it. Defaults to [`brain_core::NamespaceId::SYSTEM`].
    pub caller_namespace: brain_core::NamespaceId,
    /// Authenticated caller's human-readable space string for **this
    /// request only** — the structured selector the wire `act_as` carried
    /// (empty for a raw key-bound space). Handlers stamp it onto space
    /// registry writes so `SPACE_LIST` surfaces the original string; the
    /// 16-byte `caller_space` is a non-invertible UUIDv5 of it.
    pub caller_space_string: String,
    /// Per-shard by-slot vector source, wired at shard construction over
    /// the arena. `Some` on the live shard read path; `None` in tests and
    /// non-arena callers. Feeds the single-space brute-force retrieval
    /// lane — an exact cosine scan of a small tenant's own vectors, which
    /// the filtered shared-HNSW walk misses at high selectivity. Held as
    /// `Rc<dyn _>` (the arena is `!Send`), which makes this context
    /// `!Send` — already true of the whole dispatch path (`OpsContext`),
    /// so no new constraint. `Clone` still holds (`Rc: Clone`).
    pub space_vectors: Option<Rc<dyn SpaceVectorSource>>,
    /// Per-shard backfill worker handle, wired at shard construction
    /// from the registered `BackfillWorker` `Arc`. `None` on contexts
    /// that didn't provision the worker (unit tests, non-shard callers);
    /// the `ADMIN_BACKFILL` dispatch arm returns a clean "backfill worker
    /// not provisioned" error rather than panicking in that case. Held
    /// as `Arc<dyn BackfillControl>` (not the concrete type) to avoid a
    /// back-dependency on `brain-workers`.
    pub backfill_handle: Option<Arc<dyn BackfillControl>>,
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
            caller_space_string: String::new(),
            space_vectors: None,
            backfill_handle: None,
        }
    }

    /// Wire the per-shard backfill worker handle. Called once at shard
    /// construction with the same `Arc` registered in the scheduler.
    #[must_use]
    pub fn with_backfill_handle(mut self, handle: Arc<dyn BackfillControl>) -> Self {
        self.backfill_handle = Some(handle);
        self
    }

    /// Wire the per-shard by-slot vector source (the arena) for the
    /// single-space brute-force lane. Called once at shard construction.
    #[must_use]
    pub fn with_space_vectors(mut self, src: Rc<dyn SpaceVectorSource>) -> Self {
        self.space_vectors = Some(src);
        self
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

    /// Stamp the per-request human-readable space string. Called by
    /// `brain-ops::dispatch` alongside [`Self::with_caller_space`].
    #[must_use]
    pub fn with_caller_space_string(mut self, space_string: String) -> Self {
        self.caller_space_string = space_string;
        self
    }

    /// Does memory `id` belong to the caller's `(namespace, space)`?
    ///
    /// The per-shard memory-edge graph and HNSW are keyed by id alone (tenant-
    /// blind), so any traversal/recall path that seeds or projects a raw memory
    /// id must re-verify its owner scope here — otherwise a caller could reach
    /// another tenant's memory by id. Reads the owner scope from
    /// `MEMORIES_TABLE` and compares BOTH halves. Fail-closed: a missing row or
    /// any read error returns `false` (deny), never a wrong-tenant true.
    #[must_use]
    pub fn memory_in_caller_scope(&self, id: MemoryId) -> bool {
        let Ok(rtxn) = self.metadata.read_txn() else {
            return false;
        };
        let Ok(table) = rtxn.open_table(brain_metadata::tables::memory::MEMORIES_TABLE) else {
            return false;
        };
        match table.get(&id.to_be_bytes()) {
            Ok(Some(guard)) => {
                let row = guard.value();
                row.namespace_id == self.caller_namespace.raw()
                    && row.space_id_bytes == <[u8; 16]>::from(self.caller_space)
            }
            _ => false,
        }
    }
}

// ExecutorContext is intentionally `!Send + !Sync`: WriterHandle is
// per-shard (single-writer-per-shard). The per-shard Glommio executor
// is the containment boundary; no cross-thread sharing is required.
