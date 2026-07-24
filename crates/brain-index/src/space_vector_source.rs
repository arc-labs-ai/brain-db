//! Borrowed by-slot vector source for the per-space brute-force lane.
//!
//! Single-space RECALL over a small tenant is served by an exact cosine
//! scan of only that space's vectors instead of a filtered walk of the
//! shared HNSW graph (which misses a sparse tenant at high selectivity).
//! The scan needs raw full-precision vectors by arena slot — `hnsw_rs`
//! exposes no by-id reconstruct, and re-embedding stored text is
//! catastrophic at brute-force cardinality — so the retriever reads the
//! arena directly.
//!
//! Layering: `brain-index` (and its consumer `brain-planner`) must not
//! depend on `brain-storage`, so the arena is reached through this
//! object-safe trait. The real impl lives in `brain-server` over the
//! per-shard `Rc<RefCell<ArenaFile>>`; it is `!Send` and passed to the
//! retriever as a borrowed `&dyn SpaceVectorSource` per call, never
//! stored — keeping [`crate::SemanticRetriever`] `Send + Sync`.

use brain_core::{SlotIndex, SlotVersion};

use crate::params::VECTOR_DIM;

/// Read a live memory's full-precision vector by arena slot.
///
/// Object-safe; the retriever holds it as `&dyn SpaceVectorSource`. The
/// production impl copies the vector out of the mmap'd slot under the
/// single-shard executor, so the value returned is owned and the borrow
/// of the arena does not escape the call.
pub trait SpaceVectorSource {
    /// Verified read of a live memory's vector by arena slot.
    ///
    /// Returns `None` when the slot is out of range, unoccupied,
    /// tombstoned, hard-forgotten, or its stored slot version does not
    /// equal `expected_version` (a stale id — invariant #4). `None` is
    /// fail-soft: the caller drops that candidate from the brute-force
    /// set.
    fn vector_at(
        &self,
        slot: SlotIndex,
        expected_version: SlotVersion,
    ) -> Option<[f32; VECTOR_DIM]>;
}
