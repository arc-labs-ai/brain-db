---
name: brain-arena-audit
description: Audit arena discipline — 1600-byte slots, slot-version stamping, per-slot CRC, mmap safety. Fires on diffs in crates/brain-storage/arena/. Spec §05/02.
when-to-use: |
  Triggers:
    - Diff in crates/brain-storage/arena/**/*.rs or near slot read/write paths
    - User says "review arena" / "slot layout" / "mmap"
    - Touching slot allocation, reclamation, or the slot header
    - Changing the on-disk slot byte layout
trigger-files:
  - crates/brain-storage/**/*.rs
spec-refs:
  - spec/05_storage_arena_wal/02_arena_layout.md
  - spec/02_data_model/03_identifiers.md
---

# Arena Audit

## When to use

Any change to the arena (memory-mapped slot store): slot layout, slot read/write, slot allocation/reclamation, mmap setup, slot CRC.

## What this enforces

### From CLAUDE.md §4

> Linux server. Connection layer (Tokio) accepts TCP; each request dispatches to one of N **shards**. Each shard runs a **Glommio** executor (thread-per-core, io_uring) and owns its data: a memory-mapped **arena** for vectors, ...

> Note on slot size: the arena slot is **1600 bytes** (1536 vector capacity + 64 metadata/padding) for forward compatibility with larger embedding models. BGE-small uses 384 dims = 1536 bytes; the rest is reserved.

### From CLAUDE.md §5 (invariants)

- **#3 CRC everywhere.** Every arena slot has a CRC32C; reads verify.
- **#4 Slot version on `MemoryId`.** Encoded in the ID. Stale references → `NotFound`.
- **#7 No silent corruption.** Mismatch → halt + alert.

### From spec §05/02 (arena layout)

- Slot size: **1600 bytes** (verify in spec §05/02 before assuming).
- Slot header carries: slot version, kind, salience, timestamps, CRC of the rest of the slot.
- Vector portion: **1536 bytes** at the end of the slot (384 × f32 little-endian).
- Slots are allocated from a free-list per shard; reclaimed slots increment the slot version.

### From spec §02/03 (identifiers)

- `MemoryId` is `u128` packed: `[0..56]` slot index, `[56..72]` slot version, `[72..80]` shard, `[80..128]` reserved.
- A `MemoryId` resolves to a (shard, slot, version) triple. Stale slot version → `NotFound`.

## Workflow

1. **Locate arena touchpoints.** `grep -nE 'Arena|Slot|SlotHeader|mmap|slot_version|reclaim|free_list' <files>`.

2. **Slot size.** Confirm `SLOT_BYTES == 1600`. If the diff changes it, this is a wire/format change requiring a spec change first.

3. **Vector layout.** The vector portion is **little-endian f32** (spec §03/04 §4.2). The header is the first 64 bytes; the vector is the trailing 1536. `bytemuck::cast_slice<u8, f32>` over the trailing slice must yield 384 elements.

4. **Slot CRC.** Computed over header (excluding the CRC field) + vector bytes. On read: verify. On mismatch: halt the shard with `tracing::error!`. Never overwrite the stored CRC with a recomputed one (invariant #7).

5. **Slot version stamping.** Every reclamation increments the slot version (`u16` in the header). New writers stamp the new version. Reads that mismatch the requested `MemoryId.version()` return `NotFound`, never wrong data.

6. **mmap safety.** The arena is `unsafe`-allowed (CLAUDE.md §7). Verify:
   - Every `unsafe` block has a `// SAFETY:` comment.
   - The mapping size matches `SLOT_BYTES * capacity` exactly.
   - Slot indices are bounds-checked before pointer arithmetic.
   - Page-aligned access — slots are 1600 bytes which is *not* a multiple of 4 KiB; aligned access is at the slot index level, not byte level.
   - `madvise(WILLNEED)` / `RANDOM` hints used appropriately for hot/cold reads.

7. **Free list.** The reclaim path returns the slot to the free list and bumps slot version. Two reclaims of the same slot are a bug (use-after-free).

## Common errors → fixes

| Pattern | Why bad | Fix |
|---|---|---|
| `MemoryId::pack(shard, slot, 0)` | Slot version always 0 → stale-detection broken | Use the slot's current version |
| Vector stored big-endian | Spec §03/04 §4.2 says LE | `to_le_bytes` / `from_le_bytes` |
| CRC over slot data only (excluding header) | Doesn't catch header corruption | CRC over header (minus CRC field) + vector |
| Reclaim doesn't bump version | Stale `MemoryId` reads new data | `slot.header.version += 1; ...` |
| `unsafe { *ptr.add(idx) }` without bounds check | OOB read | Check `idx < self.capacity()` first |
| Repair-on-mismatch | Invariant #7 violation | Halt; surface |

## Test coverage required

- **Pod cast round-trip:** `Slot` ↔ `[u8; 1600]` via bytemuck.
- **CRC catches header corruption.** Flip a header byte → read returns `Corruption`.
- **CRC catches vector corruption.** Flip a vector byte → read returns `Corruption`.
- **Slot version on reclaim:** reclaim a slot, write new data → reading via the *old* `MemoryId` returns `NotFound`.
- **Bounds check.** OOB slot index → returns `InvalidArgument`, never panics.
- **mmap teardown.** Drop the arena → no segfault on subsequent reads (other shards unaffected).

## Cross-references

- `brain-invariants` — invariants #3, #4, #7.
- `brain-wal-audit` — companion for WAL durability.
- `rust-unsafe-checker` — for the `unsafe` blocks in mmap code.
- spec §05/02, §02/03.

## Source / Adaptations

Project-local. Operationalizes spec §05/02.
