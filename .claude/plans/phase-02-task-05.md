# Phase 2 — Task 2.5: Slot allocator with free list and version bumping

**Classification:** moderate. Pure-Rust bookkeeping over the arena, but the semantics are subtle (when does the version bump happen? what's a fresh-slot starting version? saturation handling).

**Spec:** `spec/05_storage_arena_wal/01_arena_overview.md` §10 (free list), `02_arena_layout.md` §3.2 (flags) + §8 (version saturation), `07_write_path.md` §§2 (slot allocation), `03_arena_growth.md` §2 (grow trigger), `02_data_model/03_identifiers.md` §2 (MemoryId structure).

## 1. Scope

This task delivers `SlotAllocator` — the per-shard structure that decides which arena slot the next ENCODE writes into, and which slots become reusable after FORGET + reclaim.

In:

- `SlotAllocator` struct: `free_list: Vec<u64>`, `next_fresh: u64`, `capacity: u64`.
- `empty(capacity)` constructor for fresh arenas.
- `rebuild_from_arena(arena)` for reopen/recovery — scans every slot, classifies each as occupied / freed / never-used.
- `alloc(arena)` — pops free list else takes `next_fresh`; returns `(idx, new_version)`; sets `PENDING_WRITE` on disk per spec §05/07 §48.
- `free(arena, idx)` — clears the slot's flags on disk (per spec §05/02 §3.2 "After reclaim, both bits become 0"), pushes onto free list. Does *not* bump version (spec §05/07 §56 says version-bump is on alloc, not on free).
- `version_of(arena, idx)` — read on-disk version.
- `on_capacity_grow(new_capacity)` — bookkeeping notification when `ArenaFile::grow_to` succeeds.

Out:

- The grow-trigger (allocator returns `Exhausted`; the writer task in 2.5+ decides to call `arena.grow_to`).
- Setting `OCCUPIED` flag — the encoder does that after writing the slot's data.
- Tombstone semantics — FORGET sets `TOMBSTONED`; later reclaim worker calls `allocator.free`.
- Persistence of the free list — spec §05/01 §10: "The free list is not persisted as a separate structure. The arena's slot flags are the source of truth." Rebuild on reopen is the only path.

## 2. Spec quotes that bind the design

> §05/01 §10 (free list): "Free slots are tracked via a per-shard in-memory free list. The list is rebuilt at startup by scanning the arena's metadata bytes (looking for slots with bit 0 == 0 in flags)."
>
> §05/01 §10 (no separate persistence): "The free list is not persisted as a separate structure. The arena's slot flags are the source of truth."
>
> §05/02 §3.2 (flags): bit 0 = OCCUPIED, bit 1 = TOMBSTONED, bit 2 = PENDING_WRITE, bit 3 = HARD_FORGOTTEN. "After reclaim, both bits [0 and 1] become 0 (slot free) until the next encode flips bit 0 back."
>
> §05/02 §8 (saturation): "`slot_version` is 32-bit. It increments each time the slot is reclaimed. Saturation at 2^32 retires the slot permanently."
>
> §05/07 §§40–48 (allocator state and alloc flow):
> > "The slot allocator is a per-shard structure. It maintains:
> >   - A free-list of slot IDs that are free (after FORGET reclamation).
> >   - A 'next new slot ID' counter for never-used slots.
> > When allocation is requested:
> >   1. If free-list is non-empty: pop the head; check the slot's flags to confirm it's still free; if so, use it.
> >   2. Otherwise: take the next new slot ID. If this exceeds the arena's capacity, trigger arena growth.
> > The allocated slot is marked as 'pending-write' (flags bit 2) before any data is written."
>
> §05/07 §56 (version semantics): "`slot_version_new` is `current_version + 1` for reclaimed slots, or **1** for never-used slots."

## 3. Spec ambiguities surfaced

### 3.1 Phase doc says `free()` bumps version; spec §05/07 says alloc bumps

Phase doc 2.5 prescribes:
> `fn free(&mut self, idx: SlotIndex)` — pushes onto free list and **bumps the slot's version**.

But spec §05/07 §56 says: *"slot_version_new is current_version + 1 for reclaimed slots"* — i.e. the version on the *new* MemoryId is `current + 1`. This is the version assigned at *alloc* time.

Implementation difference is small but observable:

| | Phase doc reading | Spec reading (chosen) |
|---|---|---|
| free() | `arena.slot[idx].version += 1; flags = 0; refresh_crc` | `arena.slot[idx].flags = 0; refresh_crc` (version unchanged) |
| alloc() | new_version = `arena.slot[idx].version` | new_version = `arena.slot[idx].version + 1` |
| Disk after alloc | unchanged (version was bumped at free) | needs to be written with the new version when the encoder writes the slot |

Consequences of choosing the spec:
- After `alloc → free → alloc`, the same idx returns with `new_version` = (last encoded version) + 1. Same as the phase doc's "version+1" done-when criterion. ✓
- Crashed encodes don't leak versions: if alloc returns `(idx, v)` but the encoder dies before writing the slot, the on-disk version is still the previous v-1. The next alloc returns `(idx, v)` again — same MemoryId. No silent skip.
- The encoder must write `slot.metadata.slot_version = new_version` as part of its write path. (Caller-side detail; not 2.5's concern.)

**Plan: follow the spec.** Phase doc 2.5 description gets corrected in the same commit.

### 3.2 "Never-used" version vs zeroed slot bytes

Spec §05/07 §56 says the version of a never-used slot's MemoryId is **1**. A freshly-zeroed slot has `slot_version = 0` on disk. The naive read `current_version + 1` gives `0 + 1 = 1`. ✓ — no special case needed; alloc's `current + 1` is correct for both never-used and reclaimed slots.

### 3.3 Spec §05/07 §1 "check the slot's flags to confirm it's still free"

The spec says alloc, after popping from the free list, should re-check the slot's flags to confirm `OCCUPIED == 0`. If the flag is set (somehow), the slot was double-allocated; treat as a corruption and skip / fail. Plan: implement the check; fail with a clear error rather than silently skipping (silent skip would mask a bug elsewhere).

## 4. Architecture

### 4.1 The struct

```rust
pub struct SlotAllocator {
    /// Slots that were used and freed; reused LIFO for cache locality.
    free_list: Vec<u64>,
    /// Lowest never-used slot index.
    next_fresh: u64,
    /// Mirrors arena.capacity_slots(); kept in sync via on_capacity_grow.
    capacity: u64,
}
```

Why LIFO over FIFO: better cache locality (last-freed slot's pages may still be hot). Spec doesn't mandate either; LIFO matches the §05/07 §1 phrasing "pop the head".

### 4.2 Constructors

```rust
impl SlotAllocator {
    /// Allocator for a freshly-created arena: free_list empty, next_fresh = 0.
    pub fn empty(capacity: u64) -> Self;

    /// Rebuild from an existing arena per spec §05/01 §10.
    ///
    /// Scans every slot, classifying each:
    /// - OCCUPIED set → live (or tombstoned). next_fresh = max(next_fresh, idx + 1).
    /// - OCCUPIED clear AND version > 0 → previously used, freed. Add to free_list. next_fresh = max(..., idx + 1).
    /// - OCCUPIED clear AND version == 0 → never used. (next_fresh unchanged.)
    /// - PENDING_WRITE set → treat as free per spec §05/07 §48 (a crashed encode);
    ///   add to free_list (spec recovery semantics in §05/08; this is the
    ///   conservative reading and matches what 2.10 will need).
    pub fn rebuild_from_arena(arena: &ArenaFile) -> Self;
}
```

`rebuild_from_arena` is O(N). For a 1M-slot arena that's 1M flag-byte reads (linear) ~ a few ms in practice. Spec §05/01 §10 budgets ~200 ms for 10M slots.

### 4.3 Allocation and free

```rust
impl SlotAllocator {
    pub fn alloc(
        &mut self,
        arena: &mut ArenaFile,
    ) -> Result<(u64, SlotVersion), AllocError> {
        // 1. Pop from free_list, else take next_fresh.
        let idx = if let Some(idx) = self.free_list.pop() {
            // Spec §05/07 §1: re-verify the slot is still free. If something
            // else flipped OCCUPIED on us, that's a bug — fail loudly.
            let s = arena.slot(idx);
            if s.is_occupied() {
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

        // 2. Compute new version: current + 1 (spec §05/07 §56).
        //    Saturation retires the slot permanently (spec §05/02 §8).
        let current = arena.slot(idx).metadata.slot_version;
        let new_version = current.checked_add(1).ok_or(AllocError::SlotRetired { idx })?;

        // 3. Set PENDING_WRITE flag on disk (spec §05/07 §48).
        let s = arena.slot_mut(idx);
        s.set_flag(flags::PENDING_WRITE, true);
        s.refresh_crc();

        Ok((idx, new_version))
    }

    pub fn free(
        &mut self,
        arena: &mut ArenaFile,
        idx: u64,
    ) -> Result<(), FreeError> {
        if idx >= self.capacity {
            return Err(FreeError::OutOfRange { idx, capacity: self.capacity });
        }
        let s = arena.slot_mut(idx);
        if s.metadata.slot_version == u32::MAX {
            return Err(FreeError::AlreadyRetired { idx });
        }
        // Spec §05/02 §3.2: "After reclaim, both bits become 0".
        // We clear all flags — TOMBSTONED, OCCUPIED, PENDING_WRITE,
        // HARD_FORGOTTEN. Vector bytes are *not* zeroed here; HARD_FORGOTTEN
        // is the path that zeros them, set explicitly by the caller before
        // calling free.
        s.metadata.flags = 0;
        s.refresh_crc();
        self.free_list.push(idx);
        Ok(())
    }
}
```

### 4.4 Errors

```rust
#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum AllocError {
    #[error("arena exhausted: next_fresh ({next_fresh}) == capacity ({capacity}); caller must grow")]
    Exhausted { next_fresh: u64, capacity: u64 },

    #[error("slot {idx} popped from free_list but is OCCUPIED — likely a bug elsewhere")]
    FreeListSlotOccupied { idx: u64 },

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
```

### 4.5 Observability helpers

```rust
impl SlotAllocator {
    pub fn capacity(&self) -> u64;
    pub fn next_fresh(&self) -> u64;
    pub fn free_count(&self) -> usize;
    pub fn used_count(&self) -> u64 {
        self.next_fresh - self.free_list.len() as u64
    }
    pub fn on_capacity_grow(&mut self, new_capacity: u64);   // panics on shrink
}
```

`used_count()` is the structural invariant: every slot ≤ next_fresh is either currently used by a memory or sitting on the free list.

### 4.6 Files

- `crates/brain-storage/src/arena/allocator.rs` (new, ~280 lines including tests).
- `crates/brain-storage/src/arena/mod.rs` (re-export `SlotAllocator`, `AllocError`, `FreeError`).
- `docs/phases/phase-02-storage.md` (correct §Task 2.5, check the boxes).

No new deps.

## 5. Trade-offs

| Option | Verdict | Why |
|---|---|---|
| **A. Allocator takes `&mut ArenaFile` (chosen)** | ✓ | Lets alloc/free touch the slot's flags + CRC + version. Matches spec's "the allocated slot is marked as pending-write before any data is written". Ergonomic: caller doesn't have to remember a separate refresh_crc. |
| B. Allocator is bookkeeping-only (no arena ref) | ✗ | Forces every caller to manually set PENDING_WRITE / clear flags / refresh CRC. Easier to misuse; fragments the spec invariant across the codebase. |
| C. Allocator owns `Arc<ArenaFile>` | ✗ | Premature: the arc-swap concurrency layer hasn't landed. Adding it here would force shape decisions on the writer task that should be a separate sub-task. |
| **D. Bump version on alloc (spec)** | ✓ | Matches spec §05/07 §56. Crashed encodes don't burn versions. |
| E. Bump version on free (phase doc) | ✗ | Phase doc-as-written; deviates from spec; surfaced as ambiguity §3.1. |
| **F. LIFO free list (chosen)** | ✓ | Cache locality; spec §05/07 §1 says "pop the head". |
| G. FIFO free list | ✗ | Better wear leveling but no observable benefit at the page-cache level. Spec doesn't ask for it. |
| **H. `Vec<u64>` for free list** | ✓ | Simplest; spec doesn't mandate a structure. |
| I. Bitmap for free list | ✗ | O(capacity) memory regardless of fragmentation; complex to maintain version metadata alongside; not a v1 concern. |

## 6. Risks

- **Spec ambiguity on version-bump location** (§3.1). Plan implements the spec reading; phase doc gets corrected.
- **Crashed-encode handling.** If `alloc` returns then the writer crashes before WAL sync, on restart `rebuild_from_arena` sees a slot with `PENDING_WRITE` set. We add it to the free list (treat as free; the version on disk is the pre-alloc value, so the next alloc will produce the same MemoryId). This is the conservative reading; full recovery semantics live in 2.10.
- **`SlotAllocator` requires `&mut ArenaFile`.** This sequentializes allocations against any reads of the same arena. Since the writer task is single-threaded per shard (Glommio), this is fine; no contention. The arc-swap layer that lets readers snapshot the arena while the writer mutates is a higher-level concern (separate sub-task).
- **Free list at scale.** Worst case: every slot freed → free_list grows to O(capacity). For 268M-slot max arena that's ~2 GB. In practice, churn is low and the list stays small (KB to MB). Out-of-scope optimizations (compressed bitmap, segmented list) for a v1.x.
- **PENDING_WRITE bit not cleared by free.** Setting `flags = 0` clears it. Caller clearing it on success is also fine; both reach 0.

## 7. Test plan

### Construction (3)

1. `empty(capacity)`: `capacity()`, `next_fresh()`, `free_count()`, `used_count()` all sensible.
2. `rebuild_from_arena` on a fresh arena: free_list empty, next_fresh = 0.
3. `rebuild_from_arena` on a populated arena (mix of occupied / tombstoned / freed-with-version / never-used slots crafted manually) reproduces exactly the right state.

### Alloc (5)

4. First alloc on fresh arena returns `(0, 1)` — spec §05/07 §56 (never-used → version 1).
5. Sequential allocs return 0, 1, 2, ... with versions all 1; PENDING_WRITE flag set on each slot; on-disk slot.metadata.slot_version *not* yet updated (encoder's job).
6. Alloc until exhausted returns `Exhausted` with the right next_fresh + capacity.
7. Alloc on a free-list slot returns the same idx with version+1.
8. Allocator hits `FreeListSlotOccupied` if a slot popped from the free list has OCCUPIED set on disk (build by hand-corrupting).

### Free (4)

9. Free out-of-range returns `OutOfRange`.
10. Free clears all flags on the slot and refreshes CRC; pushes to free_list.
11. Free on a slot at version u32::MAX returns `AlreadyRetired`; slot stays unmodified; not added to free_list.
12. Free a never-allocated idx is allowed (it sets flags=0 again and pushes to free_list — defensible v1 behavior; document).

### Round-trip (2 — phase doc done-when)

13. **`alloc → free → alloc` returns same idx with version+1** (the phase doc's done-when criterion).
14. After `alloc`, write slot.slot_version = returned version + clear PENDING_WRITE + set OCCUPIED + refresh_crc; then alloc/free pattern preserves the on-disk version progression: 0 → 1 → 1 → 2 → 2 → 3 → ... (each pair is alloc=N+1, free=stays at N+1).

### Capacity (2)

15. `on_capacity_grow(N)` updates `capacity()` and allows further allocs up to N.
16. `on_capacity_grow(N)` with `N < current` panics with a clear message (allocator capacity should only grow per spec §05/03 §8).

### Property (1)

17. `proptest!`: a sequence of arbitrary alloc/free operations preserves
    `used_count() == next_fresh - free_list.len()` at every step. Equivalent to "every slot < next_fresh is either currently used or in the free list."

**Total: 17 tests + 1 property test.**

## 8. Estimated commit shape

One commit on `feature/brain-storage`:

> `feat(brain-storage): slot allocator with free list and version bumping (sub-task 2.5)`

Body:
- Free-list + next-fresh primitive per spec §05/07 §§40–48.
- Version-bump-on-alloc (spec §05/07 §56) — deviation from phase doc, rationale captured in plan §3.1.
- PENDING_WRITE flag set in alloc per spec §05/07 §48.
- rebuild_from_arena classifier with the version > 0 / OCCUPIED / PENDING_WRITE rules.
- Saturation handling (spec §05/02 §8) — both alloc and free return `*Retired` errors at u32::MAX.
- Phase doc 2.5 entry corrected.
- Test count.

Files: as listed in §4.6. No new deps.

Verify gate: `cargo fmt --all -- --check && ./scripts/check-skills.sh && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-storage --all-targets` inside the dev container.

---

PLAN READY: see `.claude/plans/phase-02-task-05.md` — confirm to proceed.
