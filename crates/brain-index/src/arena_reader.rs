//! Arena-read trait injected into [`crate::SharedHnsw`].
//!
//! PQ re-rank needs the
//! full-precision vector for every ADC-approximate candidate. Brain-
//! index doesn't own the arena (closed-leaf rule), so the caller
//! injects an [`ArenaReader`] at construction; the shared index calls
//! into it on every search.
//!
//! The trait stays minimal — one read method — so impls in
//! `brain-ops` / `brain-server` / tests can be ~30 LOC each.

use std::sync::Arc;

use brain_core::MemoryId;

use crate::params::VECTOR_DIM;

/// Resolve a [`MemoryId`] to its full-precision arena vector. Returns
/// `None` if the slot has been reclaimed (tombstoned + arena reclaim
/// cycle completed) between an HNSW traversal and the re-rank read.
/// `None` is fail-soft: the candidate is silently dropped and the
/// returned search list may be shorter than `k`.
pub trait ArenaReader: Send + Sync {
    fn read(&self, memory_id: MemoryId) -> Option<[f32; VECTOR_DIM]>;
}

/// No-op arena reader for tests and bootstrap-mode shards that
/// haven't wired a real arena yet. Always returns `None`; PQ search
/// degenerates to "no re-rank performed", which means the returned
/// candidates are in ADC order (approximate, not exact-cosine).
///
/// Production [`crate::SharedHnsw`] construction must inject a real
/// reader.
#[derive(Debug, Default)]
pub struct NullArenaReader;

impl ArenaReader for NullArenaReader {
    fn read(&self, _memory_id: MemoryId) -> Option<[f32; VECTOR_DIM]> {
        None
    }
}

/// Convenience: wrap a [`NullArenaReader`] in the [`Arc<dyn _>`] shape
/// [`crate::SharedHnsw::from_index`] expects.
#[must_use]
pub fn null_arena_reader() -> Arc<dyn ArenaReader> {
    Arc::new(NullArenaReader)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_arena_reader_always_returns_none() {
        let reader = NullArenaReader;
        let id = MemoryId::pack(1, 42, 1);
        assert!(reader.read(id).is_none());
    }

    #[test]
    fn null_arena_reader_helper_returns_arc() {
        let arc = null_arena_reader();
        let id = MemoryId::pack(1, 7, 1);
        assert!(arc.read(id).is_none());
    }
}
