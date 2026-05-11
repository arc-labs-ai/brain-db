//! Bidirectional `MemoryId ↔ u32` map for the HNSW index.
//!
//! See `spec/06_ann_index/03_insertion.md` §1–2 (id_map pattern), §10
//! (duplicate-MemoryId is a bug — we detect and reject rather than
//! letting hnsw_rs silently overwrite).
//!
//! ## Type choices
//!
//! - **Internal id is `u32`** (spec §06/03 §2). Saves ~80 MB at the
//!   spec's 10M-memory per-shard ceiling vs `usize`. Cast to `usize`
//!   at the hnsw_rs API boundary; overflow at `u32::MAX` returns
//!   [`MemoryIdAlreadyInserted`]'s sibling [`IdMapError::Exhausted`].
//! - **Forward key is `[u8; 16]`** (the MemoryId's wire-form bytes),
//!   matching brain-metadata's table-key convention. Avoids any
//!   coupling to MemoryId's internal `u128` hashing.
//! - **No atomic counter.** Brain's `HnswIndex::insert(&mut self, ...)`
//!   borrow discipline rules out concurrent inserts at compile time;
//!   spec §06/03 §1's `AtomicU32::fetch_add` example assumes a
//!   multi-writer path Brain doesn't have.

use brain_core::MemoryId;
use std::collections::HashMap;
use thiserror::Error;

/// `MemoryId ↔ u32` bidirectional map plus a sequential allocator.
///
/// Insert-only at this layer. Removal of stale entries (slot reclamation
/// per spec §06/05 §11) lives in the Phase 8 maintenance worker.
#[derive(Default)]
pub struct IdMap {
    forward: HashMap<[u8; 16], u32>,
    reverse: HashMap<u32, [u8; 16]>,
    next_id: u32,
}

/// Errors from [`IdMap::insert`].
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum IdMapError {
    /// `memory_id` was already inserted. Per spec §06/03 §10 this is a
    /// caller bug; the counter is not advanced.
    #[error("memory_id already inserted: {memory_id_bytes:?}")]
    AlreadyInserted { memory_id_bytes: [u8; 16] },

    /// `u32::MAX` internal IDs have been allocated. At the spec's 10M
    /// per-shard ceiling this is unreachable; the check is defensive.
    #[error("id_map exhausted: u32::MAX internal ids allocated")]
    Exhausted,
}

impl IdMap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Test-only constructor that seeds the next-id counter. Used to
    /// exercise the `u32::MAX` overflow guard without performing
    /// billions of inserts.
    #[cfg(test)]
    pub(crate) fn with_next_id(next_id: u32) -> Self {
        Self {
            forward: HashMap::new(),
            reverse: HashMap::new(),
            next_id,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.forward.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    /// Does `memory_id` have an internal id assigned?
    #[must_use]
    pub fn contains(&self, memory_id: MemoryId) -> bool {
        self.forward.contains_key(&memory_id.to_be_bytes())
    }

    /// Allocate the next internal id and bind it to `memory_id`.
    ///
    /// On duplicate, returns [`IdMapError::AlreadyInserted`] and **does
    /// not** advance the counter — a caller's mistake shouldn't burn
    /// IDs. Spec §06/03 §10.
    pub fn insert(&mut self, memory_id: MemoryId) -> Result<u32, IdMapError> {
        let key = memory_id.to_be_bytes();
        if self.forward.contains_key(&key) {
            return Err(IdMapError::AlreadyInserted {
                memory_id_bytes: key,
            });
        }
        if self.next_id == u32::MAX {
            return Err(IdMapError::Exhausted);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.forward.insert(key, id);
        self.reverse.insert(id, key);
        Ok(id)
    }

    #[must_use]
    pub fn lookup_forward(&self, memory_id: MemoryId) -> Option<u32> {
        self.forward.get(&memory_id.to_be_bytes()).copied()
    }

    #[must_use]
    pub fn lookup_reverse(&self, internal_id: u32) -> Option<MemoryId> {
        self.reverse
            .get(&internal_id)
            .map(|bytes| MemoryId::from_be_bytes(*bytes))
    }

    /// Iterate the forward mapping as `([u8; 16], u32)` pairs. Used by
    /// snapshot persistence (sub-task 4.5) to serialise the map.
    /// Order is unspecified — callers must not depend on it.
    pub fn iter_forward(&self) -> impl Iterator<Item = ([u8; 16], u32)> + '_ {
        self.forward.iter().map(|(k, v)| (*k, *v))
    }

    /// The internal allocator's next-id value. Used by snapshot
    /// persistence to restore the counter on load.
    #[must_use]
    pub fn next_id(&self) -> u32 {
        self.next_id
    }

    /// Re-construct an `IdMap` from a parsed snapshot's forward
    /// entries + the recorded `next_id`. Reverse direction is rebuilt
    /// here in O(N) so we don't have to serialise both directions.
    pub fn from_snapshot(entries: Vec<([u8; 16], u32)>, next_id: u32) -> Self {
        let mut forward = std::collections::HashMap::with_capacity(entries.len());
        let mut reverse = std::collections::HashMap::with_capacity(entries.len());
        for (key, id) in entries {
            forward.insert(key, id);
            reverse.insert(id, key);
        }
        Self {
            forward,
            reverse,
            next_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid(slot: u64) -> MemoryId {
        MemoryId::pack(1, slot, 1)
    }

    #[test]
    fn new_is_empty() {
        let m = IdMap::new();
        assert_eq!(m.len(), 0);
        assert!(m.is_empty());
    }

    #[test]
    fn insert_allocates_sequential_ids() {
        let mut m = IdMap::new();
        assert_eq!(m.insert(mid(1)).unwrap(), 0);
        assert_eq!(m.insert(mid(2)).unwrap(), 1);
        assert_eq!(m.insert(mid(3)).unwrap(), 2);
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn insert_populates_both_directions() {
        let mut m = IdMap::new();
        let id = m.insert(mid(42)).unwrap();
        assert_eq!(m.lookup_forward(mid(42)), Some(id));
        assert_eq!(m.lookup_reverse(id), Some(mid(42)));
        // A different MemoryId not present.
        assert_eq!(m.lookup_forward(mid(43)), None);
        assert_eq!(m.lookup_reverse(id + 1), None);
    }

    #[test]
    fn duplicate_insert_rejects_and_does_not_advance_id() {
        let mut m = IdMap::new();
        assert_eq!(m.insert(mid(1)).unwrap(), 0);
        // Second insert of the same MemoryId rejects.
        match m.insert(mid(1)) {
            Err(IdMapError::AlreadyInserted { memory_id_bytes }) => {
                assert_eq!(memory_id_bytes, mid(1).to_be_bytes());
            }
            other => panic!("expected AlreadyInserted, got {other:?}"),
        }
        assert_eq!(m.len(), 1);
        // Counter was not burned — next insert is id=1, not id=2.
        assert_eq!(m.insert(mid(2)).unwrap(), 1);
    }

    #[test]
    fn contains_pin() {
        let mut m = IdMap::new();
        assert!(!m.contains(mid(7)));
        m.insert(mid(7)).unwrap();
        assert!(m.contains(mid(7)));
        assert!(!m.contains(mid(8)));
    }

    #[test]
    fn u32_overflow_returns_exhausted() {
        let mut m = IdMap::with_next_id(u32::MAX);
        match m.insert(mid(1)) {
            Err(IdMapError::Exhausted) => {}
            other => panic!("expected Exhausted, got {other:?}"),
        }
        assert_eq!(m.len(), 0);
    }
}
