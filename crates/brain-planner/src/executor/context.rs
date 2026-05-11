//! The handle bag passed to every `execute_*` function.
//!
//! Spec §08/08 §7: "handles are cheap to clone (Arc-based). Each
//! executor task gets its own handles; no contention." We use the
//! same pattern: every field is shareable across tasks (Send + Sync).
//!
//! Phase 6.3 ships only the read-side handles — Dispatcher, index,
//! metadata. 6.4 adds `writer: Arc<dyn WriterHandle>` + `arena:
//! Arc<Arena>` when encode lands. Struct-additive change; existing
//! callers keep working.

use std::sync::Arc;

use brain_embed::Dispatcher;
use brain_index::SharedHnsw;
use brain_metadata::MetadataDb;

/// Executor-side context. Cheap to clone (every field is `Arc` or
/// already cheap-clone like `SharedHnsw`).
#[derive(Clone)]
pub struct ExecutorContext {
    pub embedder: Arc<dyn Dispatcher>,
    pub index: SharedHnsw<384>,
    pub metadata: Arc<MetadataDb>,
}

impl ExecutorContext {
    #[must_use]
    pub fn new(
        embedder: Arc<dyn Dispatcher>,
        index: SharedHnsw<384>,
        metadata: Arc<MetadataDb>,
    ) -> Self {
        Self {
            embedder,
            index,
            metadata,
        }
    }
}

// Compile-time guard: the context must be Send + Sync so executor
// tasks can carry it across .await boundaries.
const _: fn() = || {
    fn require<T: Send + Sync>() {}
    require::<ExecutorContext>();
};
