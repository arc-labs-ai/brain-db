# 05.01 Arena Overview

The vector arena is a memory-mapped flat file holding all of a shard's vectors. It is the substrate's bulk storage for the high-dimensional content that ANN search and attractor dynamics need.

## 1. The arena's purpose

The arena exists because vectors need a home that is:

- **Persistent.** Vectors must survive process restarts.
- **Fast to read.** ANN search visits hundreds of vectors per query; per-vector read latency matters.
- **Densely packed.** A shard with 1M memories has 1.5 GB of vectors; layout affects cache and TLB behavior.
- **Crash-consistent with the WAL.** After a crash, the arena's contents must be recoverable by replaying the WAL.

The arena meets these by being a contiguous file mmapped into the substrate's address space. Vectors are written via memcpy; vectors are read via direct pointer access. The kernel's page cache transparently caches hot regions.

## 2. Slots, not records

The arena is organized as an array of fixed-size **slots**, not as a sequence of variable-length records.

```
arena.bin:
  [slot 0]  [slot 1]  [slot 2]  [slot 3]  ...  [slot N]
  ^         ^                    ^
  4096-byte aligned slots        empty (tombstoned or never used)
```

Each slot is 1600 bytes:
- 1536 bytes — the 384-dim f32 vector.
- 64 bytes — slot metadata (flags, version, padding).

The fixed slot size means:
- Slot ID → byte offset is `slot_id * 1600`. No indirection table.
- Allocation is "find the next free slot" — a per-shard free list.
- Reuse is direct — overwrite the slot.

We don't pack arbitrary lengths into a flat file. Variable-length data (text, edges) lives in the metadata store; the arena is for fixed-size vectors only.

## 3. Why 1600 bytes per slot

The slot size is dictated by:

- Vector size: 384 × 4 = 1536 bytes.
- Metadata: enough to hold version, flags, fingerprint reference, alignment padding.

We chose 64 bytes of metadata to:
- Make the slot a multiple of 64 (a cache line). Vectors are SIMD-loaded; alignment matters.
- Pack the version, flags, and small reference fields without overflow.

Total: 1600 bytes = 25 × 64-byte cache lines.

The next-larger natural choice is 2048 bytes (32 cache lines). We rejected it: 28% wasted space, with no operational benefit.

## 4. Slot metadata layout (64 bytes)

```
offset  size  field
─────   ───   ─────
0       4     version: u32
4       4     flags: u32
8       16    embedding_model_fp_short: [u8; 16]   (truncated; full version in metadata)
24      8     created_at: u64                     (unix nanoseconds)
32      8     last_modified_at: u64
40      24    reserved (zeroed)
```

The `flags` field carries:
- `bit 0` — slot occupied (1) or free (0).
- `bit 1` — tombstoned (set after FORGET, before reclaim).
- `bit 2` — being-written (transient; set during WAL→arena window).
- bits 3–31 — reserved.

The truncated fingerprint (16 bytes) lets ANN search filter by model fingerprint without consulting the metadata store. The full fingerprint (also 16 bytes in our scheme) is stored, but the field carries the same value here for fast access.

## 5. The arena header

The first 4096 bytes of `arena.bin` are a header, not a slot. It carries:

```
offset  size  field
─────   ───   ─────
0       4     magic = "BARN"  (Brain ARena)
4       4     format_version: u32
8       16    shard_uuid: [u8; 16]
24      4     vector_dim: u32
28      4     slot_size: u32
32      8     slot_count_capacity: u64
40      8     slot_count_in_use: u64                    (advisory)
48      16    embedding_model_fp_active: [u8; 16]
64      4032  reserved (zeroed)
```

The header is read at startup. `slot_count_in_use` is advisory; the authoritative count comes from the metadata store and free-list reconstruction during recovery.

The arena header MUST be a multiple of the system page size (4096 on Linux x86_64) so that subsequent slots are page-aligned.

## 6. Initial sizing

A new shard's arena starts at:
- 4096 bytes (header) + 1600 × 1024 bytes (1024 slots) = ~1.6 MB.

This is small enough to allocate without ceremony. As the shard grows, the file is extended (see [`03_arena_growth.md`](03_arena_growth.md)).

A practical operational target is ~10M slots per shard — at 1600 bytes per slot, that's 16 GB of arena per shard. With ~64 shards per node (a typical configuration), 1 TB of arena per node.

## 7. Mapping into the substrate's address space

At startup, the substrate:

1. Opens `arena.bin` for read+write.
2. Parses the header.
3. Calls `mmap(NULL, file_size, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0)`.
4. Stores the resulting pointer.

The mmap is `MAP_SHARED` — writes through the pointer are persisted to the file. We do **not** use `MAP_PRIVATE`; that would create a private copy on write.

The page size for mmap on Linux x86_64 is 4096 bytes. Larger huge-page mappings (`MAP_HUGETLB`) are not used by the substrate for the arena, because:

- HugeTLB requires explicit kernel reservation.
- Transparent Huge Pages (THP) do not apply to file-backed mmaps on regular filesystems.

We accept regular 4 KB pages. For arena sizes up to ~64 GB, the TLB overhead is acceptable; beyond that, larger nodes should split into more shards rather than relying on huge pages.

## 8. Writes via memcpy

Writing a vector to a slot:

```rust
let slot_ptr: *mut f32 = arena_base.add(slot_offset(slot_id));
unsafe {
    std::ptr::copy_nonoverlapping(
        vector.as_ptr(),
        slot_ptr,
        VECTOR_DIM,
    );
}
```

The metadata bytes are written similarly, in a separate memcpy after the vector.

The kernel marks the affected pages as dirty. The pages are eventually written back to disk by the kernel's writeback mechanism. For durability, we do not rely on this — the WAL is the durability mechanism. Arena writes are not synchronously fsync'd in the hot path.

## 9. Reads via pointer access

Reading a vector from a slot:

```rust
let slot_ptr: *const f32 = arena_base.add(slot_offset(slot_id));
let vector: &[f32] = unsafe {
    std::slice::from_raw_parts(slot_ptr, VECTOR_DIM)
};
```

This is a zero-copy borrow into the mmap'd region. The borrowed slice is valid as long as the slot's content is still committed (concurrency rules in [10. Concurrency + Epoch Model](../10_concurrency_epochs/) §Read Path).

For SIMD-friendly access, the slot pointer is cache-line-aligned (slots start at multiples of 1600, which we round up to 1664 in some configurations to preserve cache-line alignment — see [`02_arena_layout.md`](02_arena_layout.md) for the exact byte layout decision).

## 10. The free list

Free slots are tracked via a per-shard in-memory free list. The list is rebuilt at startup by scanning the arena's metadata bytes (looking for slots with `bit 0 == 0` in `flags`).

For a 10M-slot arena, the rebuild scans 10M × 64 bytes = 640 MB of metadata bytes. At sequential read speeds (~3 GB/s on NVMe), this takes ~200 ms. Acceptable for startup; not a hot path.

The free list is not persisted as a separate structure. The arena's slot flags are the source of truth.

## 11. The arena is not the source of truth

As stated in [`00_purpose.md`](00_purpose.md) §4: the WAL is the source of truth, not the arena.

If the arena is corrupted (bit flips on disk, partial write before fsync), the substrate detects it via:
- Slot version mismatches with metadata.
- Norm checks on read.
- Periodic background scrubbing.

Corruptions are repaired by replaying the WAL or restoring from snapshot.

## 12. Single arena per shard

Each shard has exactly one arena file. No sharded sub-files, no rotation, no segment-style splits.

This simplifies things:
- One file descriptor per shard's arena.
- One mmap call.
- One contiguous address range.

The drawback: a single arena can grow to many GBs. Truncating an arena (removing the trailing portion after a large eviction) is supported via `fallocate(FALLOC_FL_PUNCH_HOLE)` but is not currently in the active design — the arena grows monotonically in v1.

Future versions may add arena rotation if operationally desirable (e.g., for snapshot-friendly partitioning).

---

*Continue to [`02_arena_layout.md`](02_arena_layout.md) for the byte-level layout.*
