//! Per-shard slot allocator.
//!
//! Owns the free list and the "next fresh slot" pointer per
//! `spec/05_storage_arena_wal/07_write_path.md` §2. Together with
//! `ArenaFile`, it decides which slot the next ENCODE writes into and
//! which slots become reusable after FORGET + reclaim.
//!
//! ## Version bump location
//!
//! Spec §05/07 §56 says `slot_version_new = current_version + 1` — the
//! version increment lives at *alloc* time, not at free time. Concretely:
//!
//! - `alloc(arena)` reads `arena.slot[idx].metadata.slot_version`, returns
//!   `current + 1` as the new version. The encoder is responsible for
//!   writing that new version into the slot's metadata along with the
//!   vector and other fields.
//! - `free(arena, idx)` clears the slot's flags and pushes onto the free
//!   list; it does *not* touch the version.
//!
//! Consequence: if an encoder crashes between `alloc` and the WAL fsync,
//! the on-disk version is still the pre-alloc value. The next `alloc`
//! returns the same `(idx, version)` pair, producing the same MemoryId —
//! no silent version skip.
//!
//! The phase doc 2.5 sketch describes the opposite policy
//! (free-bumps-version). Spec wins; phase doc was corrected in the same
//! commit that introduced this module.
//!
//! ## PENDING_WRITE flag
//!
//! Spec §05/07 §48: "The allocated slot is marked as 'pending-write'
//! (flags bit 2) before any data is written." `alloc` sets this on disk
//! so a crashed encode is detectable on the next startup (the recovery
//! path in 2.10 treats slots with `PENDING_WRITE` as free).

use crate::arena::file::ArenaFile;
use crate::arena::slot::flags;
use brain_core::SlotVersion;

/// Per-shard slot allocator.
#[derive(Debug)]
pub struct SlotAllocator {
    /// Slots that were used and freed. LIFO for cache locality and to match
    /// spec §05/07 §1 ("pop the head").
    free_list: Vec<u64>,
    /// Lowest never-used slot index. Slots at `[next_fresh, capacity)` are
    /// guaranteed zeroed (sparse-file region).
    next_fresh: u64,
    /// Mirrors `ArenaFile::capacity_slots()`; updated via `on_capacity_grow`.
    capacity: u64,
}

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum AllocError {
    /// Free list is empty *and* `next_fresh == capacity`. Caller must
    /// `ArenaFile::grow_to` and then retry.
    #[error(
        "arena exhausted: next_fresh ({next_fresh}) == capacity ({capacity}); caller must grow"
    )]
    Exhausted { next_fresh: u64, capacity: u64 },

    /// A slot popped from the free list has `OCCUPIED` set on disk. This
    /// shouldn't happen under normal use; surfaces as a loud error rather
    /// than a silent skip (spec §05/07 §1: "check the slot's flags to
    /// confirm it's still free").
    #[error("slot {idx} popped from free_list but is OCCUPIED — likely a bug elsewhere")]
    FreeListSlotOccupied { idx: u64 },

    /// Version saturated at `u32::MAX`; the slot is permanently retired
    /// per spec §05/02 §8.
    #[error("slot {idx} version saturated at u32::MAX; permanently retired per spec §05/02 §8")]
    SlotRetired { idx: u64 },
}

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum FreeError {
    #[error("free({idx}) but capacity is {capacity}")]
    OutOfRange { idx: u64, capacity: u64 },

    #[error("free({idx}): slot already retired (version saturated)")]
    AlreadyRetired { idx: u64 },
}

impl SlotAllocator {
    /// Build an empty allocator for a freshly-created arena.
    #[must_use]
    pub fn empty(capacity: u64) -> Self {
        Self {
            free_list: Vec::new(),
            next_fresh: 0,
            capacity,
        }
    }

    /// Rebuild the allocator state by scanning the arena's slot metadata
    /// (spec §05/01 §10). O(capacity). Used on reopen and recovery.
    ///
    /// Two-pass classifier:
    ///
    /// 1. Find `next_fresh` = `last_used_idx + 1`, where a slot counts as
    ///    "used" if any of `OCCUPIED`, `PENDING_WRITE`, or `slot_version > 0`
    ///    is true. (`slot_version > 0` covers freed-after-reuse slots; the
    ///    free-list must include them so they get reused before the
    ///    capacity is exhausted.)
    /// 2. Add every non-`OCCUPIED` slot in `[0, next_fresh)` to the free
    ///    list — including any never-used slots that happen to sit below
    ///    the highest used index. Without this, those slots would be
    ///    permanently lost.
    pub fn rebuild_from_arena(arena: &ArenaFile) -> Self {
        let capacity = arena.capacity_slots();

        // Pass 1: find the boundary.
        let mut next_fresh: u64 = 0;
        for idx in 0..capacity {
            let s = arena.slot(idx);
            let used = s.is_occupied() || s.is_pending_write() || s.metadata.slot_version > 0;
            if used {
                next_fresh = idx + 1;
            }
        }

        // Pass 2: every non-OCCUPIED slot below the boundary is free.
        let mut free_list = Vec::new();
        for idx in 0..next_fresh {
            if !arena.slot(idx).is_occupied() {
                free_list.push(idx);
            }
        }

        Self {
            free_list,
            next_fresh,
            capacity,
        }
    }

    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    #[must_use]
    pub fn next_fresh(&self) -> u64 {
        self.next_fresh
    }

    #[must_use]
    pub fn free_count(&self) -> usize {
        self.free_list.len()
    }

    /// Slots that are currently allocated (occupied or tombstoned).
    /// Invariant: `used_count() + free_count() == next_fresh`.
    #[must_use]
    pub fn used_count(&self) -> u64 {
        self.next_fresh - self.free_list.len() as u64
    }

    /// Allocate a slot for a new ENCODE.
    ///
    /// Returns `(slot_idx, new_version)`. The caller is responsible for
    /// writing `slot.metadata.slot_version = new_version` along with the
    /// vector, setting `OCCUPIED`, clearing `PENDING_WRITE`, and refreshing
    /// the CRC.
    ///
    /// Sets `PENDING_WRITE` on the slot before returning (spec §05/07 §48).
    pub fn alloc(&mut self, arena: &mut ArenaFile) -> Result<(u64, SlotVersion), AllocError> {
        // 1. Pick a slot.
        let idx = if let Some(idx) = self.free_list.pop() {
            // Spec §05/07 §1: re-verify the slot is still free.
            if arena.slot(idx).is_occupied() {
                return Err(AllocError::FreeListSlotOccupied { idx });
            }
            idx
        } else if self.next_fresh < self.capacity {
            let idx = self.next_fresh;
            self.next_fresh += 1;
            idx
        } else {
            return Err(AllocError::Exhausted {
                next_fresh: self.next_fresh,
                capacity: self.capacity,
            });
        };

        // 2. Compute the new version: current + 1 (spec §05/07 §56).
        let current = arena.slot(idx).metadata.slot_version;
        let new_version = match current.checked_add(1) {
            Some(v) => v,
            None => return Err(AllocError::SlotRetired { idx }),
        };

        // 3. Set PENDING_WRITE on disk (spec §05/07 §48). Encoder will clear
        //    it (and set OCCUPIED) when the slot's data is fully written.
        let s = arena.slot_mut(idx);
        s.set_flag(flags::PENDING_WRITE, true);
        s.refresh_crc();

        Ok((idx, new_version))
    }

    /// Free a previously-allocated slot. Clears flags on disk (per spec
    /// §05/02 §3.2: "After reclaim, both bits become 0") and pushes the
    /// slot onto the free list. Does *not* bump the version — alloc does
    /// that.
    ///
    /// The vector bytes are *not* zeroed here. Hard-forget (which zeros
    /// the vector and sets the HARD_FORGOTTEN flag) is the caller's
    /// concern; this method only reclaims the slot for reuse.
    pub fn free(&mut self, arena: &mut ArenaFile, idx: u64) -> Result<(), FreeError> {
        if idx >= self.capacity {
            return Err(FreeError::OutOfRange {
                idx,
                capacity: self.capacity,
            });
        }
        let s = arena.slot_mut(idx);
        if s.metadata.slot_version == u32::MAX {
            return Err(FreeError::AlreadyRetired { idx });
        }
        s.metadata.flags = 0;
        s.refresh_crc();
        self.free_list.push(idx);
        Ok(())
    }

    /// Read the on-disk slot version for `idx`. Convenience for callers
    /// validating MemoryIds.
    ///
    /// # Panics
    /// Panics if `idx >= arena.capacity_slots()`.
    #[must_use]
    pub fn version_of(arena: &ArenaFile, idx: u64) -> SlotVersion {
        arena.slot(idx).metadata.slot_version
    }

    /// Notify the allocator that the arena's capacity grew. Call after a
    /// successful `ArenaFile::grow_to`.
    ///
    /// # Panics
    /// Panics if `new_capacity < self.capacity` — the arena does not
    /// shrink in v1 (spec §05/03 §8).
    pub fn on_capacity_grow(&mut self, new_capacity: u64) {
        assert!(
            new_capacity >= self.capacity,
            "allocator capacity cannot shrink: current = {}, new = {}",
            self.capacity,
            new_capacity,
        );
        self.capacity = new_capacity;
    }
}

// Tests open an `ArenaFile` (syscalls). Gated behind `not(miri)`; see
// `.claude/plans/phase-02-miri.md`.
#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::arena::slot::flags;
    use proptest::prelude::*;

    fn uuid(byte: u8) -> [u8; 16] {
        [byte; 16]
    }

    fn open_fresh(dir: &tempfile::TempDir, capacity: u64) -> ArenaFile {
        ArenaFile::open(dir.path().join("arena.bin"), uuid(1), capacity).expect("open fresh")
    }

    fn invariant_holds(alloc: &SlotAllocator) -> bool {
        alloc.used_count() + alloc.free_count() as u64 == alloc.next_fresh()
    }

    /// Helper to simulate what an encoder does after `alloc`: write the
    /// new version, set OCCUPIED, clear PENDING_WRITE, refresh CRC.
    fn finalize_encode(arena: &mut ArenaFile, idx: u64, new_version: SlotVersion) {
        let s = arena.slot_mut(idx);
        s.metadata.slot_version = new_version;
        s.metadata.flags = flags::OCCUPIED;
        s.refresh_crc();
    }

    // ----- Construction --------------------------------------------------

    #[test]
    fn empty_allocator_has_zero_used_and_free() {
        let alloc = SlotAllocator::empty(1024);
        assert_eq!(alloc.capacity(), 1024);
        assert_eq!(alloc.next_fresh(), 0);
        assert_eq!(alloc.free_count(), 0);
        assert_eq!(alloc.used_count(), 0);
        assert!(invariant_holds(&alloc));
    }

    #[test]
    fn rebuild_from_fresh_arena_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let arena = open_fresh(&dir, 16);
        let alloc = SlotAllocator::rebuild_from_arena(&arena);
        assert_eq!(alloc.capacity(), 16);
        assert_eq!(alloc.next_fresh(), 0);
        assert_eq!(alloc.free_count(), 0);
    }

    #[test]
    fn rebuild_from_populated_arena_classifies_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 16);

        // Hand-craft a mix:
        //   idx 0: occupied (version 1)         -> used, advances next_fresh
        //   idx 1: never used (version 0, no flag)
        //   idx 2: freed (version 3, no flags)  -> free_list, advances next_fresh
        //   idx 3: tombstoned (version 2, OCCUPIED|TOMBSTONED) -> used, advances next_fresh
        //   idx 4: pending-write crash (version 0, PENDING_WRITE) -> free_list, advances next_fresh
        //   idx 5..16: never used.
        {
            let s = arena.slot_mut(0);
            s.metadata.slot_version = 1;
            s.metadata.flags = flags::OCCUPIED;
            s.refresh_crc();
        }
        {
            let s = arena.slot_mut(2);
            s.metadata.slot_version = 3;
            s.metadata.flags = 0;
            s.refresh_crc();
        }
        {
            let s = arena.slot_mut(3);
            s.metadata.slot_version = 2;
            s.metadata.flags = flags::OCCUPIED | flags::TOMBSTONED;
            s.refresh_crc();
        }
        {
            let s = arena.slot_mut(4);
            s.metadata.slot_version = 0;
            s.metadata.flags = flags::PENDING_WRITE;
            s.refresh_crc();
        }

        let alloc = SlotAllocator::rebuild_from_arena(&arena);
        assert_eq!(alloc.capacity(), 16);
        // next_fresh advances past the highest used-or-freed slot (idx 4).
        assert_eq!(alloc.next_fresh(), 5);
        // free_list holds:
        //   - idx 1 (never used, but sits below next_fresh),
        //   - idx 2 (freed-after-reuse),
        //   - idx 4 (pending-write crash).
        // idx 0 and 3 are OCCUPIED (idx 3 OCCUPIED|TOMBSTONED still counts).
        assert_eq!(alloc.free_count(), 3);
        // used_count = 5 - 3 = 2, matching the 2 OCCUPIED slots.
        assert_eq!(alloc.used_count(), 2);
        assert!(invariant_holds(&alloc));
    }

    #[test]
    fn rebuild_includes_skipped_never_used_slots_in_free_list() {
        // The classifier must add slot 1 to the free list (it sits below
        // next_fresh but isn't occupied) so that the invariant
        // used_count + free_count == next_fresh holds and the encoder
        // doesn't leak slot 1.
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 8);
        // Only idx 3 is occupied; idx 0, 1, 2, 4..7 are never used.
        {
            let s = arena.slot_mut(3);
            s.metadata.slot_version = 1;
            s.metadata.flags = flags::OCCUPIED;
            s.refresh_crc();
        }
        let alloc = SlotAllocator::rebuild_from_arena(&arena);
        assert_eq!(
            alloc.next_fresh(),
            4,
            "next_fresh should be just past idx 3"
        );
        // idx 0, 1, 2 are below next_fresh and never used → must be on the
        // free list so they aren't permanently lost.
        assert_eq!(alloc.free_count(), 3);
        assert!(invariant_holds(&alloc));
    }

    // ----- Alloc ---------------------------------------------------------

    #[test]
    fn first_alloc_returns_idx_0_version_1() {
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 4);
        let mut alloc = SlotAllocator::empty(arena.capacity_slots());
        let (idx, ver) = alloc.alloc(&mut arena).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(ver, 1);
        // PENDING_WRITE set on the slot (spec §05/07 §48); OCCUPIED not yet.
        assert!(arena.slot(0).is_pending_write());
        assert!(!arena.slot(0).is_occupied());
        // On-disk version is *unchanged* — encoder writes it later.
        assert_eq!(arena.slot(0).metadata.slot_version, 0);
        assert!(invariant_holds(&alloc));
    }

    #[test]
    fn sequential_allocs_advance_next_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 4);
        let mut alloc = SlotAllocator::empty(arena.capacity_slots());
        for expected in 0..4 {
            let (idx, ver) = alloc.alloc(&mut arena).unwrap();
            assert_eq!(idx, expected);
            assert_eq!(ver, 1);
            assert!(arena.slot(idx).is_pending_write());
            finalize_encode(&mut arena, idx, ver);
        }
        assert_eq!(alloc.next_fresh(), 4);
        assert_eq!(alloc.used_count(), 4);
    }

    #[test]
    fn alloc_at_capacity_returns_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 2);
        let mut alloc = SlotAllocator::empty(arena.capacity_slots());
        let _ = alloc.alloc(&mut arena).unwrap();
        let _ = alloc.alloc(&mut arena).unwrap();
        let err = alloc.alloc(&mut arena).unwrap_err();
        assert!(
            matches!(
                err,
                AllocError::Exhausted {
                    next_fresh: 2,
                    capacity: 2
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn alloc_from_free_list_returns_same_idx_with_higher_version() {
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 4);
        let mut alloc = SlotAllocator::empty(arena.capacity_slots());

        // First-encode round-trip for idx 0.
        let (idx, ver) = alloc.alloc(&mut arena).unwrap();
        assert_eq!((idx, ver), (0, 1));
        finalize_encode(&mut arena, idx, ver);

        // Free it.
        alloc.free(&mut arena, idx).unwrap();

        // Re-alloc — must return the same idx, version+1.
        let (idx2, ver2) = alloc.alloc(&mut arena).unwrap();
        assert_eq!(idx2, 0);
        assert_eq!(ver2, 2, "version must increment on alloc, not on free");
    }

    #[test]
    fn alloc_detects_corrupt_free_list_slot() {
        // Set up: alloc, finalize, free → free_list has idx 0 with OCCUPIED clear.
        // Then sneak OCCUPIED back on; alloc should refuse.
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 4);
        let mut alloc = SlotAllocator::empty(arena.capacity_slots());
        let (idx, ver) = alloc.alloc(&mut arena).unwrap();
        finalize_encode(&mut arena, idx, ver);
        alloc.free(&mut arena, idx).unwrap();

        // Corruption: flip OCCUPIED back on without the allocator knowing.
        arena.slot_mut(idx).metadata.flags = flags::OCCUPIED;
        arena.slot_mut(idx).refresh_crc();

        let err = alloc.alloc(&mut arena).unwrap_err();
        assert!(
            matches!(err, AllocError::FreeListSlotOccupied { idx: 0 }),
            "got {err:?}"
        );
    }

    // ----- Free ----------------------------------------------------------

    #[test]
    fn free_out_of_range_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 2);
        let mut alloc = SlotAllocator::empty(arena.capacity_slots());
        let err = alloc.free(&mut arena, 5).unwrap_err();
        assert!(
            matches!(
                err,
                FreeError::OutOfRange {
                    idx: 5,
                    capacity: 2
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn free_clears_flags_and_pushes_to_free_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 2);
        let mut alloc = SlotAllocator::empty(arena.capacity_slots());
        let (idx, ver) = alloc.alloc(&mut arena).unwrap();
        finalize_encode(&mut arena, idx, ver);
        // Add TOMBSTONED + HARD_FORGOTTEN to make sure free clears
        // every bit, not just OCCUPIED.
        arena.slot_mut(idx).metadata.flags |= flags::TOMBSTONED | flags::HARD_FORGOTTEN;
        arena.slot_mut(idx).refresh_crc();

        alloc.free(&mut arena, idx).unwrap();
        assert_eq!(arena.slot(idx).metadata.flags, 0);
        assert!(
            arena.slot(idx).is_valid(),
            "CRC must be refreshed after free"
        );
        assert_eq!(alloc.free_count(), 1);
        assert_eq!(alloc.used_count(), 0);
    }

    #[test]
    fn free_at_max_version_returns_already_retired() {
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 2);
        let mut alloc = SlotAllocator::empty(arena.capacity_slots());
        let (idx, _) = alloc.alloc(&mut arena).unwrap();
        // Hand-craft saturation.
        arena.slot_mut(idx).metadata.slot_version = u32::MAX;
        arena.slot_mut(idx).metadata.flags = flags::OCCUPIED;
        arena.slot_mut(idx).refresh_crc();

        let err = alloc.free(&mut arena, idx).unwrap_err();
        assert!(
            matches!(err, FreeError::AlreadyRetired { idx: 0 }),
            "got {err:?}"
        );
        // Slot not added to free list.
        assert_eq!(alloc.free_count(), 0);
    }

    #[test]
    fn alloc_at_max_version_returns_slot_retired() {
        // Pop a slot whose on-disk version is u32::MAX. The realistic
        // path: alloc → finalize → free (version on disk = 1) → external
        // corruption pushes the on-disk version to u32::MAX → next alloc
        // hits saturation when it tries `current + 1`.
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 4);
        let mut alloc = SlotAllocator::empty(arena.capacity_slots());

        let (idx, ver) = alloc.alloc(&mut arena).unwrap();
        finalize_encode(&mut arena, idx, ver);
        alloc.free(&mut arena, idx).unwrap();
        // free_list = [0], on-disk version = 1, flags = 0.

        // Inject saturation directly (simulates a slot whose on-disk
        // version reached u32::MAX through some external path — e.g. a
        // future operator tool or recovered state).
        arena.slot_mut(idx).metadata.slot_version = u32::MAX;
        arena.slot_mut(idx).refresh_crc();

        let err = alloc.alloc(&mut arena).unwrap_err();
        assert!(
            matches!(err, AllocError::SlotRetired { idx: 0 }),
            "got {err:?}"
        );
    }

    // ----- Round-trip (phase doc done-when) ------------------------------

    #[test]
    fn alloc_free_alloc_returns_same_idx_with_version_plus_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 1);
        let mut alloc = SlotAllocator::empty(arena.capacity_slots());

        let (idx1, ver1) = alloc.alloc(&mut arena).unwrap();
        finalize_encode(&mut arena, idx1, ver1);
        alloc.free(&mut arena, idx1).unwrap();
        let (idx2, ver2) = alloc.alloc(&mut arena).unwrap();

        assert_eq!(idx2, idx1);
        assert_eq!(ver2, ver1 + 1);
    }

    #[test]
    fn version_progression_across_many_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 1);
        let mut alloc = SlotAllocator::empty(arena.capacity_slots());
        let mut expected_version = 0u32;
        for _ in 0..10 {
            let (idx, ver) = alloc.alloc(&mut arena).unwrap();
            assert_eq!(idx, 0);
            expected_version += 1;
            assert_eq!(ver, expected_version);
            finalize_encode(&mut arena, idx, ver);
            alloc.free(&mut arena, idx).unwrap();
        }
    }

    // ----- Capacity ------------------------------------------------------

    #[test]
    fn on_capacity_grow_extends_available_slots() {
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 2);
        let mut alloc = SlotAllocator::empty(arena.capacity_slots());
        let _ = alloc.alloc(&mut arena).unwrap();
        let _ = alloc.alloc(&mut arena).unwrap();
        assert!(matches!(
            alloc.alloc(&mut arena).unwrap_err(),
            AllocError::Exhausted { .. }
        ));

        arena.grow_to(8).unwrap();
        alloc.on_capacity_grow(arena.capacity_slots());
        assert_eq!(alloc.capacity(), 8);

        // Further allocs now succeed up to the new capacity.
        for expected in 2..8 {
            let (idx, _) = alloc.alloc(&mut arena).unwrap();
            assert_eq!(idx, expected);
        }
    }

    #[test]
    #[should_panic(expected = "allocator capacity cannot shrink")]
    fn on_capacity_shrink_panics() {
        let mut alloc = SlotAllocator::empty(16);
        alloc.on_capacity_grow(8);
    }

    // ----- Property ------------------------------------------------------

    proptest! {
        #[test]
        fn invariant_holds_across_arbitrary_op_sequences(
            ops in prop::collection::vec(any::<bool>(), 0..200),
        ) {
            // Each op: true = alloc, false = free a random previously-used slot.
            let dir = tempfile::tempdir().unwrap();
            let mut arena = ArenaFile::open(
                dir.path().join("arena.bin"),
                uuid(1),
                256,
            ).unwrap();
            let mut alloc = SlotAllocator::empty(arena.capacity_slots());
            let mut live: Vec<u64> = Vec::new();

            // Deterministic pseudo-RNG seeded from ops length so that
            // proptest's shrinking remains useful.
            let mut rng_state = ops.len() as u64;
            let mut next_rand = || {
                rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                rng_state
            };

            for op in ops {
                if op || live.is_empty() {
                    match alloc.alloc(&mut arena) {
                        Ok((idx, ver)) => {
                            finalize_encode(&mut arena, idx, ver);
                            live.push(idx);
                        }
                        Err(AllocError::Exhausted { .. }) => {
                            // Capacity reached; skip. Not a test failure.
                        }
                        Err(other) => panic!("unexpected alloc error: {other:?}"),
                    }
                } else {
                    let pick = (next_rand() as usize) % live.len();
                    let idx = live.swap_remove(pick);
                    alloc.free(&mut arena, idx).unwrap();
                }
                prop_assert!(invariant_holds(&alloc));
            }
        }
    }
}
