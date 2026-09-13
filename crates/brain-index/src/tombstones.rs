//! Bit-packed tombstone bitmap.
//!
//! HNSW doesn't support efficient
//! eager deletion, so forgotten memories are *tombstoned*: their node
//! stays in the graph and is filtered out at search time. This module
//! owns the filter state; the search wiring lives in `hnsw.rs`.
//!
//! ## Representation
//!
//! `Vec<u64>` — 64 bits per word. At the per-shard ceiling of
//! 10M memories, this is ~1.25 MB; a `Vec<bool>` would be 10 MB.
//! Grows lazily — fresh bitmaps allocate nothing.
//!
//! Operates on internal `u32` ids (the type hnsw_rs returns). The
//! `MemoryId → u32` translation lives on `HnswIndex` via the id_map;
//! this module stays MemoryId-free.
//!
//! ## Count is tracked incrementally
//!
//! `count()` returns an O(1) running counter — set/clear update it
//! only on actual bit transitions's `tombstone_ratio`
//! metric is exposed at the request handler; a per-call O(N/64) sum
//! would be wasteful.

/// Bit-packed tombstone state.
///
/// All operations are idempotent: setting an already-set bit (or
/// clearing an already-clear one) is a no-op and doesn't perturb the
/// count.
#[derive(Default, Debug)]
pub struct TombstoneBitmap {
    bits: Vec<u64>,
    count: usize,
}

impl TombstoneBitmap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Tombstone `id`. No-op if already tombstoned. Grows the underlying
    /// `Vec<u64>` on demand.
    pub fn set(&mut self, id: u32) {
        let (word, bit) = word_bit(id);
        if word >= self.bits.len() {
            self.bits.resize(word + 1, 0);
        }
        let mask = 1u64 << bit;
        if self.bits[word] & mask == 0 {
            self.bits[word] |= mask;
            self.count += 1;
        }
    }

    /// Un-tombstone `id`. No-op if not currently tombstoned.
    pub fn clear_one(&mut self, id: u32) {
        let (word, bit) = word_bit(id);
        if word >= self.bits.len() {
            return;
        }
        let mask = 1u64 << bit;
        if self.bits[word] & mask != 0 {
            self.bits[word] &= !mask;
            self.count -= 1;
        }
    }

    /// Clear all tombstones. Used by [`crate::hnsw::HnswIndex`] during
    /// rebuild: a fresh post-rebuild index skips the
    /// previously-tombstoned memories, so the new bitmap starts empty.
    pub fn clear(&mut self) {
        self.bits.fill(0);
        self.count = 0;
    }

    /// Is `id` currently tombstoned?
    #[must_use]
    pub fn is_set(&self, id: u32) -> bool {
        let (word, bit) = word_bit(id);
        match self.bits.get(word) {
            Some(w) => (w & (1u64 << bit)) != 0,
            None => false,
        }
    }

    /// Running count of tombstoned ids. O(1).
    #[must_use]
    pub fn count(&self) -> usize {
        self.count
    }

    /// Raw bitmap words. Used by snapshot persistence
    /// to serialise the bitmap byte-for-byte.
    #[must_use]
    pub fn raw_words(&self) -> &[u64] {
        &self.bits
    }

    /// Re-construct a `TombstoneBitmap` from a parsed snapshot. The
    /// `count` is taken verbatim from the snapshot rather than
    /// re-summed; this is correct because the caller validated the
    /// snapshot's BLAKE3 footer.
    #[must_use]
    pub fn from_snapshot(words: Vec<u64>, count: usize) -> Self {
        Self { bits: words, count }
    }
}

/// Split `id` into `(word_index, bit_index_within_word)`.
const fn word_bit(id: u32) -> (usize, u32) {
    ((id / 64) as usize, id % 64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_empty() {
        let b = TombstoneBitmap::new();
        assert_eq!(b.count(), 0);
        assert!(!b.is_set(0));
        assert!(!b.is_set(100));
    }

    #[test]
    fn set_and_query() {
        let mut b = TombstoneBitmap::new();
        b.set(5);
        assert!(b.is_set(5));
        assert_eq!(b.count(), 1);
    }

    #[test]
    fn unmarked_returns_false() {
        let mut b = TombstoneBitmap::new();
        b.set(5);
        assert!(!b.is_set(100));
        assert!(!b.is_set(4));
        assert!(!b.is_set(6));
    }

    #[test]
    fn set_grows_lazily() {
        let mut b = TombstoneBitmap::new();
        // Fresh bitmap allocates nothing.
        assert_eq!(b.bits.len(), 0);
        b.set(1000);
        // 1000 / 64 = 15, so we need word index 15 → length 16.
        assert_eq!(b.bits.len(), 16);
        assert!(b.is_set(1000));
        assert_eq!(b.count(), 1);
    }

    #[test]
    fn set_is_idempotent() {
        let mut b = TombstoneBitmap::new();
        b.set(5);
        b.set(5);
        b.set(5);
        assert_eq!(b.count(), 1);
        assert!(b.is_set(5));
    }

    #[test]
    fn clear_one_resets_bit() {
        let mut b = TombstoneBitmap::new();
        b.set(5);
        assert_eq!(b.count(), 1);
        b.clear_one(5);
        assert!(!b.is_set(5));
        assert_eq!(b.count(), 0);
    }

    #[test]
    fn clear_one_idempotent_on_unset() {
        // Both clearing a bit that was never set AND clearing one that
        // was already cleared must not underflow the count.
        let mut b = TombstoneBitmap::new();
        b.clear_one(5);
        assert_eq!(b.count(), 0);

        b.set(7);
        b.clear_one(7);
        b.clear_one(7);
        assert_eq!(b.count(), 0);

        // Clear far above current bitmap length — should not allocate
        // or panic.
        b.clear_one(10_000);
        assert_eq!(b.count(), 0);
    }

    #[test]
    fn clear_resets_all() {
        let mut b = TombstoneBitmap::new();
        b.set(1);
        b.set(63);
        b.set(64);
        b.set(1000);
        b.set(10_000);
        assert_eq!(b.count(), 5);

        b.clear();
        assert_eq!(b.count(), 0);
        for id in [1u32, 63, 64, 1000, 10_000] {
            assert!(!b.is_set(id), "bit {id} should be cleared");
        }
    }
}
