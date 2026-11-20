# 08.01 Arena

The vector arena: overview, byte-level layout, and growth. The arena is a memory-mapped flat file holding all of a shard's vectors — Brain's bulk storage for the high-dimensional content that ANN search and attractor dynamics need.

## Overview

The vector arena is a memory-mapped flat file holding all of a shard's vectors. It is Brain's bulk storage for the high-dimensional content that ANN search and attractor dynamics need.

### 1. The arena's purpose

The arena exists because vectors need a home that is:

- **Persistent.** Vectors must survive process restarts.
- **Fast to read.** ANN search visits hundreds of vectors per query; per-vector read latency matters.
- **Densely packed.** A shard with 1M memories has 1.5 GB of vectors; layout affects cache and TLB behavior.
- **Crash-consistent with the WAL.** After a crash, the arena's contents must be recoverable by replaying the WAL.

The arena meets these by being a contiguous file mmapped into Brain's address space. Vectors are written via memcpy; vectors are read via direct pointer access. The kernel's page cache transparently caches hot regions.

### 2. Slots, not records

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
- Slot indexes are issued from a monotonic per-shard counter; the free list serves the replay path (§10).
- Reuse is direct — overwrite the slot.

Brain does not pack arbitrary lengths into a flat file. Variable-length data (text, edges) lives in the metadata store; the arena is for fixed-size vectors only.

### 3. Why 1600 bytes per slot

The slot size is dictated by:

- Vector size: 384 × 4 = 1536 bytes.
- Metadata: enough to hold version, flags, fingerprint reference, alignment padding.

Brain uses 64 bytes of metadata to:
- Make the slot a multiple of 64 (a cache line). Vectors are SIMD-loaded; alignment matters.
- Pack the version, flags, and small reference fields without overflow.

Total: 1600 bytes = 25 × 64-byte cache lines.

The next-larger natural choice is 2048 bytes (32 cache lines). It was rejected: 28% wasted space, with no operational benefit.

### 4. Slot metadata layout (64 bytes)

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

The truncated fingerprint (16 bytes) lets ANN search filter by model fingerprint without consulting the metadata store. The full fingerprint (also 16 bytes in Brain's scheme) is stored, but the field carries the same value here for fast access.

### 5. The arena header

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

### 6. Initial sizing

A new shard's arena starts at:
- 4096 bytes (header) + 1600 × 1024 bytes (1024 slots) = ~1.6 MB.

This is small enough to allocate without ceremony. As the shard grows, the file is extended (see § Growth below).

A practical operational target is ~10M slots per shard — at 1600 bytes per slot, that's 16 GB of arena per shard. With ~64 shards per node (a typical configuration), 1 TB of arena per node.

### 7. Mapping into Brain's address space

At startup, Brain:

1. Opens `arena.bin` for read+write.
2. Parses the header.
3. Calls `mmap(NULL, file_size, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0)`.
4. Stores the resulting pointer.

The mmap is `MAP_SHARED` — writes through the pointer are persisted to the file. Brain does **not** use `MAP_PRIVATE`; that would create a private copy on write.

The page size for mmap on Linux x86_64 is 4096 bytes. Larger huge-page mappings (`MAP_HUGETLB`) are not used by Brain for the arena, because:

- HugeTLB requires explicit kernel reservation.
- Transparent Huge Pages (THP) do not apply to file-backed mmaps on regular filesystems.

Brain accepts regular 4 KB pages. For arena sizes up to ~64 GB, the TLB overhead is acceptable; beyond that, larger nodes should split into more shards rather than relying on huge pages.

### 8. Writes via memcpy

Vectors are written into slots by WAL replay, not by the live write path ([`03_write_path.md`](03_write_path.md) §5). The mechanism is a memcpy into the mapped region either way.

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

The kernel marks the affected pages as dirty. The pages are eventually written back to disk by the kernel's writeback mechanism. For durability, Brain does not rely on this — the WAL is the durability mechanism. Arena writes are not synchronously fsync'd in the hot path.

### 9. Reads via pointer access

Reading a vector from a slot:

```rust
let slot_ptr: *const f32 = arena_base.add(slot_offset(slot_id));
let vector: &[f32] = unsafe {
    std::slice::from_raw_parts(slot_ptr, VECTOR_DIM)
};
```

This is a zero-copy borrow into the mmap'd region. The borrowed slice is valid as long as the slot's content is still committed (concurrency rules in [14. Concurrency](../14_concurrency/00_purpose.md) §Read Path).

For SIMD-friendly access, the slot pointer is cache-line-aligned (slots start at multiples of 1600, which is itself a multiple of 64 — see § Byte-level layout below for the exact byte layout).

### 10. The free list

Free slots are tracked via a per-shard in-memory free list, rebuilt from the post-replay arena at startup by scanning the slot metadata bytes (looking for slots with `bit 0 == 0` in `flags`).

For a 10M-slot arena, the rebuild scans 10M × 64 bytes = 640 MB of metadata bytes. At sequential read speeds (~3 GB/s on NVMe), this takes ~200 ms. Acceptable for startup; not a hot path.

The free list belongs to the arena allocator, which serves the replay path. The live write path reserves slot indexes from a monotonic counter instead and does not consult the free list, so a reclaimed index is not reissued while the process is up ([`03_write_path.md`](03_write_path.md) §2).

The free list is not persisted as a separate structure. The arena's slot flags are the source of truth.

### 11. The arena is not the source of truth

As stated in [`00_purpose.md`](00_purpose.md) §4: the WAL is the source of truth, not the arena.

The arena is, more precisely, the **boot-time image** of every durable memory: replay fills it, and nothing on the live path adds to it. It is therefore authoritative for the boot-recovery index rebuild and only for that. A rebuild at runtime must read the durable write-time vectors from redb, because a runtime rebuild from the arena would silently drop every memory encoded since the process started.

If the arena is corrupted (bit flips on disk, partial write before fsync), Brain detects it via:
- Slot version mismatches with metadata.
- Norm checks on read.
- Periodic background scrubbing.

Corruptions are repaired by replaying the WAL or restoring from snapshot.

### 12. Single arena per shard

Each shard has exactly one arena file. No sharded sub-files, no rotation, no segment-style splits.

This simplifies things:
- One file descriptor per shard's arena.
- One mmap call.
- One contiguous address range.

The drawback: a single arena can grow to many GBs. Truncating an arena (removing the trailing portion after a large eviction) is supported via `fallocate(FALLOC_FL_PUNCH_HOLE)` but is not currently in the active design — the arena grows monotonically.

Future versions may add arena rotation if operationally desirable (e.g., for snapshot-friendly partitioning).

## Byte-Level Layout

The exact bytes in `arena.bin`. Implementers MUST produce this layout; clients of the file (recovery, snapshots, debugging tools) parse it.

### 1. Overall structure

```
[header: 4096 bytes]
[slot 0:  1600 bytes (or 1664 with cache-line padding — see § 4)]
[slot 1:  1600 bytes]
[slot 2:  1600 bytes]
...
[slot N-1: 1600 bytes]
```

The file's total size is `4096 + (slot_count × slot_size)` bytes. Brain maintains a `slot_count_capacity` in the header that may exceed the currently-allocated file size; the difference is a region Brain has reserved (via `fallocate`) for future growth.

### 2. The header (4096 bytes)

| Offset | Size (bytes) | Field | Type |
|---|---|---|---|
| 0 | 4 | magic | ASCII "BARN" (0x42 0x41 0x52 0x4E) |
| 4 | 4 | format_version | u32, little-endian |
| 8 | 16 | shard_uuid | UUIDv7 bytes |
| 24 | 4 | vector_dim | u32 LE (must be 384) |
| 28 | 4 | slot_size | u32 LE (must be 1600) |
| 32 | 8 | slot_count_capacity | u64 LE |
| 40 | 8 | slot_count_in_use | u64 LE (advisory) |
| 48 | 16 | embedding_model_fp_active | [u8; 16] |
| 64 | 8 | created_at | u64 LE, unix nanoseconds |
| 72 | 8 | last_grow_at | u64 LE |
| 80 | 4 | header_crc32c | u32 LE, computed over bytes [0..76] |
| 84 | 4012 | reserved | zero |

Endianness is **little-endian** for storage. (The wire protocol uses big-endian; storage uses LE because it matches modern x86_64 and ARM native order.)

The `header_crc32c` is computed over bytes 0–75 (i.e., excluding the CRC field itself and the reserved region). Validating the header on startup catches accidental header corruption before anything depends on the file.

### 3. The slot (1600 bytes)

```
+------+------+------+------+ -+
|     vector (1536 bytes)    |  |
|   384 × f32 little-endian  |  |
|                            |  |
+----------------------------+  |  1600 bytes total
|   slot metadata (64 bytes) |  |
+----------------------------+ -+
```

#### 3.1 Vector (1536 bytes)

384 f32 values, little-endian, contiguous. Element 0 is at byte offset 0 within the slot; element 383 is at byte offset 1532.

The values must be finite and form an L2-normalized vector (norm in `[1.0 - 1e-3, 1.0 + 1e-3]`). Brain validates norms on input and during periodic scrubbing.

#### 3.2 Slot metadata (64 bytes)

Slot metadata is at byte offset 1536–1599 within the slot.

| Offset within metadata | Size | Field | Type |
|---|---|---|---|
| 0 | 4 | slot_version | u32 LE |
| 4 | 4 | flags | u32 LE |
| 8 | 16 | embedding_model_fp_short | [u8; 16] |
| 24 | 8 | created_at | u64 LE, unix nanoseconds |
| 32 | 8 | last_modified_at | u64 LE |
| 40 | 4 | metadata_crc32c | u32 LE, computed over slot metadata bytes [0..36] and the vector bytes |
| 44 | 20 | reserved | zero |

The `metadata_crc32c` covers the slot's vector and most of its metadata, so a corruption check spans the whole slot. Computing this CRC on every read would slow the hot path; Brain computes it only during periodic scrubbing or when a recovery suspects a slot.

The flags layout:

| Bit | Meaning |
|---|---|
| 0 | Slot occupied (1 = has a memory; 0 = free) |
| 1 | Tombstoned (1 = forgotten, awaiting reclaim) |
| 2 | Pending-write (1 = write in progress; transient) |
| 3 | Hard-forgotten (1 = vector was zeroed; informational) |
| 4–31 | Reserved (zero) |

The combination `bit 0 = 1, bit 1 = 1` is "active but tombstoned" — the slot still has its vector and metadata, but the memory is no longer queryable. After reclaim, both bits become 0 (slot free) until the next encode flips bit 0 back.

### 4. Cache-line padding consideration

Slot size 1600 is a multiple of 64 (cache line size). Slot offsets `4096 + n × 1600` are also multiples of 64 because 4096 is, and 1600 is. So slots are naturally cache-line-aligned.

Each slot's vector starts at offset 0 of the slot, which is cache-line-aligned. Each slot's metadata starts at offset 1536, also cache-line-aligned (1536 = 24 × 64).

Padding slots to 1664 (26 cache lines) to align with 4 KB page boundaries every 16 slots was considered and rejected:

- Wastes 64 bytes per slot (4% overhead at 1.5 GB scale = 60 MB of padding for 1M slots).
- Doesn't help TLB utilization meaningfully — the working set during search isn't 16 sequential slots.
- Adds complexity (slot offset computation becomes more error-prone).

Brain sticks with 1600.

### 5. Page boundaries and slots

A slot's bytes may straddle a 4 KB page boundary. With slot size 1600:

- Slot 0 starts at offset 4096 (page-aligned).
- Slot 1 starts at offset 5696 (within the same page).
- Slot 2 starts at offset 7296 (in the next page; specifically, page 8192 starts within slot 2).

This means a single slot may span two pages. For random access patterns (which ANN search has), this is fine — the page cache loads both pages, and access patterns aren't sequential anyway.

For sequential scans (during recovery, scrubbing), reading slots in order accesses pages in order, which the kernel readahead handles efficiently.

### 6. Vector storage format

Each f32 is 4 bytes, little-endian IEEE 754. NaN, ±Inf, and subnormals are technically representable; Brain validates that vectors contain only finite values.

A normalized 384-dim vector has typical f32 element magnitudes of ~0.05 (since 1/sqrt(384) ≈ 0.051). The full IEEE 754 dynamic range is wildly more than needed; Brain uses f32 for SIMD efficiency, not range.

f16 (half precision) is a future possibility:
- 2× storage savings (768 bytes per vector instead of 1536).
- Sufficient precision for cosine similarity at 384 dim.
- But: poorer SIMD support on x86 (gather/scatter required), and the model itself outputs f32.

f32 is the choice: the poorer x86 SIMD support and the f32 model output outweigh the storage saving.

### 7. Slot ID to offset

```rust
fn slot_offset(slot_id: u64) -> usize {
    HEADER_SIZE as usize + (slot_id as usize) * SLOT_SIZE
}

const HEADER_SIZE: u32 = 4096;
const SLOT_SIZE: u32 = 1600;
const VECTOR_DIM: usize = 384;
```

Slot ID 0 is the first slot at file offset 4096. Slot ID `slot_count - 1` is the last slot at file offset `4096 + (slot_count - 1) × 1600`.

### 8. The MemoryId encodes the slot

A `MemoryId` is 16 bytes laid out as ([02.03 Identifiers](../02_data_model/02_memory.md)):

```
[shard_id_runtime: 2 bytes]
[slot_id: 6 bytes]
[slot_version: 4 bytes]
[reserved: 4 bytes]
```

The `slot_id` is 48-bit, allowing up to 2^48 ≈ 281 trillion slots per shard — far beyond any practical limit.

`slot_version` is 32-bit. It increments each time the slot is reclaimed. Saturation at 2^32 retires the slot permanently.

Validation when looking up a memory by its `MemoryId`:

1. Extract `slot_id` and `slot_version`.
2. Read the slot at `slot_offset(slot_id)`.
3. Compare the slot's stored `slot_version` (in metadata) to the `MemoryId`'s.
4. If they match, this is the right memory. If they don't, return `MemoryNotFound`.

### 9. The empty-arena case

A freshly-initialized arena (no memories yet) has:

- Header populated with zeros except for the magic, format version, shard UUID, and dimensions.
- All slots zeroed (their flags bit 0 = 0, indicating free).
- `slot_count_capacity = 1024` (default initial capacity).
- File on disk: 4096 + 1024 × 1600 = 1,642,496 bytes ≈ 1.6 MB, sparse if the filesystem supports it.

Slot indexes on the live path come from a monotonic counter that starts at 1, so the first encode is slot 1 and subsequent encodes take 2, 3, 4, … in order. Slot index 0 is never issued; the counter is seeded at boot with `fetch_max` from the persisted high-water mark and never moves backwards, so reclaimed indexes are not reissued by the live path and the arena fills densely in index order. ([`03_write_path.md`](03_write_path.md) §2.)

### 10. Reading slots that haven't been written

A slot in the file's reserved-but-not-written region returns zeros when read. The flags' bit 0 is 0 (free), so Brain correctly identifies it as not occupied. The vector bytes are zero, but Brain never reads vectors from free slots.

This depends on the filesystem honoring the sparse-file convention (or initializing zeroes on allocation, which Brain does explicitly via `fallocate` followed by no writes).

### 11. Verification on load

At startup, after the file is mmap'd, Brain optionally:

1. Reads the header.
2. Verifies the magic bytes (`"BARN"`).
3. Verifies the header CRC32C.
4. Verifies the shard UUID matches the configured shard.
5. Verifies the `format_version` is exactly 1. Any other value is refused — there is no supported range and no migration path.
6. Verifies dim and slot_size match expectations.

Any mismatch is a startup failure; Brain refuses to operate with a malformed arena.

The full slot-level CRC verification is a separate background-job; the startup path is fast and only verifies the header.

## Growth

The arena starts small and grows over time as the shard's memories accumulate. This section specifies how growth happens — the system calls used, the mmap remapping, and the policies for when to grow.

### 1. The growth model

The arena grows in **doubling steps**: each growth doubles the slot capacity. So the sequence of capacities is:

```
1024 → 2048 → 4096 → ... → 1M → 2M → 4M → ... → 268M
```

Doubling is the standard choice for amortized-O(1) growth. Linear growth (add a fixed number of slots) would have O(n²) total cost across n growths; doubling is O(n).

Initial capacity: 1024 slots (1.6 MB). Maximum capacity: 2^48 slots (effectively unbounded; bounded by the operator's disk).

### 2. The growth procedure

When the slot allocator can't find a free slot:

1. Compute the new capacity: `new_capacity = current_capacity × 2`.
2. Compute the new file size: `4096 + new_capacity × 1600`.
3. Extend the file via `fallocate(fd, 0, 0, new_file_size)`.
4. Re-map: either via `mremap` (Linux) or by `mmap`-ing additional pages.
5. Update the header's `slot_count_capacity` field.
6. Sync the header (single 4 KB page) to ensure the new capacity survives a crash.
7. Add the new slots to the free list.
8. Continue with the encode that triggered growth.

The growth happens in the writer task. Other readers continue using the existing mmap region; once growth completes, they see the new region on their next read.

### 3. fallocate

`fallocate(fd, 0, offset, length)` extends the file. Mode 0 (the default) means "ensure the requested range is allocated"; if the filesystem supports sparse files, this may not actually write blocks until they're modified.

Brain uses mode 0 (not `FALLOC_FL_KEEP_SIZE`) because Brain wants the file's reported size to grow. This is what tools like `du` and `ls -l` report.

For `XFS` and `ext4`, `fallocate` is fast — it allocates extents without zeroing them. For older filesystems or those without extent support, `fallocate` may fall back to writing zeros, which is much slower; Brain accepts this rare case.

The Linux kernel header for fallocate flags is at [`include/uapi/linux/falloc.h`](https://github.com/torvalds/linux/blob/master/include/uapi/linux/falloc.h).

### 4. mremap

After extending the file, Brain requires to extend the mmap region.

```rust
let new_addr = unsafe {
    libc::mremap(
        old_addr,
        old_size,
        new_size,
        libc::MREMAP_MAYMOVE,
    )
};
```

`MREMAP_MAYMOVE` lets the kernel relocate the mapping if there's no contiguous space at the old address. Brain handles the relocation by atomically swapping the mmap pointer.

The relocation is a concurrency event:

- Readers that obtained the old pointer continue using it (still valid until they release it).
- New reads acquire the new pointer.
- The old mapping is unmapped after no readers reference it.

Coordination uses `arc-swap`: the mmap pointer is wrapped in `Arc<MmapRegion>`, swapped atomically on growth. Readers use `arc-swap`'s load to get the current Arc; growth updates with `store`.

See [14. Concurrency](../14_concurrency/00_purpose.md) §arena_remap for the full coordination protocol.

### 5. The fallback: re-mmap

If `mremap` is not available or fails, Brain falls back to:

1. `mmap` a new region at `new_size`.
2. Copy data... no wait, Brain does not copy. The new mapping points at the same file. The kernel maps the same file pages.
3. Atomically swap.
4. `munmap` the old region.

This works because both regions point at the same file; the kernel doesn't duplicate page-cache pages.

### 6. When growth fails

`fallocate` can fail with `ENOSPC` (out of disk space) or `EFBIG` (file too large for the filesystem).

Response:
- The encode operation that triggered growth fails with `OutOfStorage`.
- Brain logs the failure with current capacity and disk free-space stats.
- Brain continues operating with the existing arena.
- The operator must address the underlying issue (add disk, evict memories, split shards).

Brain does **not** automatically delete or evict to make room. Eviction happens only via the consolidation worker on its own schedule, not as a response to growth failure.

### 7. Write amplification

Growth uses `fallocate`, which doesn't write data — it just reserves blocks. The actual writing happens lazily as new slots are populated.

So growth itself has minimal write cost (some metadata in the filesystem). The cost is paid as slots fill up, distributed over time.

For NVMe SSDs, this is a non-issue. For older storage with significant block-allocation overhead, the cost is bounded.

### 8. Shrinking?

The arena does not shrink. Slots that become free (after FORGET + reclaim) stay allocated in the file rather than being returned to the OS.

Why no shrinking:
- The mmap region's address range can't be partially unmapped without disrupting the file's logical layout.
- Slot IDs are monotonic; "shrinking" would require reorganizing live slots, breaking MemoryId stability.
- The cost of unused slots is small (1600 bytes each, on cheap storage).

A shrinking operation would be a tool-level offline procedure: snapshot, copy live slots to a new compact arena, atomically swap. Useful but not currently implemented.

### 9. Pre-allocation

Operators expecting a known shard size can pre-allocate via configuration:

```
[shard.<shard_uuid>]
initial_arena_slots = 1000000
```

At startup, Brain sizes the arena to 1M slots (1.6 GB) immediately. This avoids growth events during the first ~1M encodes.

Pre-allocation is an optimization, not a requirement. Default is the doubling sequence starting at 1024.

### 10. Growth concurrency

A grow event holds the per-shard writer lock briefly:

1. Lock acquired (shared with all writes).
2. Compute new size, call `fallocate`.
3. Call `mremap` and atomic-swap the pointer.
4. Update header's `slot_count_capacity` (one cache-line write to mmap'd region).
5. Lock released.

Total wall time: typically < 1 ms on NVMe. The lock is held for the duration; readers continue without contention; new writes wait briefly.

For very large arenas (10 GB → 20 GB grow), `fallocate` may take longer; typically still under 50 ms but operator-visible.

### 11. The scheme for very large shards

Some operators may want shards larger than the 16 GB-class arena (say, 100 GB). At slot size 1600, this is 64M slots. Mapping a 100 GB region is fine on modern Linux (64-bit address space, ample RAM); the question is whether it makes operational sense.

The recommendation is to keep individual shards at ≤ 32 GB arenas (≈ 20M memories). Beyond that:

- TLB pressure increases (no huge pages for file-backed mmaps on regular FS).
- Recovery times grow.
- Backup sizes become unwieldy.

If a deployment needs more, add more shards (the cluster-level horizontal split) rather than growing individual shards.

### 12. Fragmentation

Slot reuse means the arena's logical layout becomes "swiss cheese" over time — interleaved live and free slots. This is fine for random-access ANN search; it doesn't affect correctness or performance meaningfully.

For sequential scans (recovery, scrubbing), Brain handles free slots in stride; no cost beyond the time to look at the flag.

### 13. Header sync on growth

After `fallocate` and `mremap`, Brain updates the header to reflect the new capacity. This update is to the mmap'd region; the kernel will writeback eventually.

To make the new capacity durable across a crash, Brain `msync`s the first 4 KB (the header page):

```rust
unsafe {
    libc::msync(
        arena_base as *mut c_void,
        4096,  // header size
        libc::MS_SYNC,
    );
}
```

`MS_SYNC` blocks until the page is durably written. This is the only fsync the arena performs in the hot path; other arena writes are asynchronous (the WAL is the durability mechanism).

If the crash happens before the header sync completes, recovery sees the old capacity. The newly-allocated file region is lost (the file is truncated back to the old size during recovery). Brain logs this as a recoverable inconsistency; no data is lost because no slots in the new region were actually populated yet.

---

*Continue to [`02_wal.md`](02_wal.md) for the WAL.*
