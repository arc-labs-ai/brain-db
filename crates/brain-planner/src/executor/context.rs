//! The handle bag passed to every `execute_*` function.
//!
//! Spec §08/08 §7: "handles are cheap to clone (Arc-based). Each
//! executor task gets its own handles; no contention." We use the
//! same pattern: every field is shareable across tasks (Send + Sync).
//!
//! Phase 6.4 ships embedder + index + metadata (read side) + writer
//! (write side). Future sub-tasks may add `arena: Arc<Arena>` if a
//! caller needs raw arena access — current executors don't.

use std::sync::Arc;

use brain_embed::Dispatcher;
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

/// Executor-side context. Cheap to clone (every field is `Arc` or
/// already cheap-clone like `SharedHnsw`).
#[derive(Clone)]
pub struct ExecutorContext {
    pub embedder: Arc<dyn Dispatcher>,
    pub index: SharedHnsw<384>,
    pub metadata: SharedMetadataDb,
    pub writer: Arc<dyn WriterHandle>,
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
        }
    }
}

// Compile-time guard: the context must be Send + Sync so executor
// tasks can carry it across .await boundaries.
const _: fn() = || {
    fn require<T: Send + Sync>() {}
    require::<ExecutorContext>();
};
