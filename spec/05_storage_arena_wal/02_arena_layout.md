# 05.02 Arena Byte-Level Layout

The exact bytes in `arena.bin`. Implementers MUST produce this layout; clients of the file (recovery, snapshots, debugging tools) parse it.

## 1. Overall structure

```
[header: 4096 bytes]
[slot 0:  1600 bytes (or 1664 with cache-line padding — see § 4)]
[slot 1:  1600 bytes]
[slot 2:  1600 bytes]
...
[slot N-1: 1600 bytes]
```

The file's total size is `4096 + (slot_count × slot_size)` bytes. The substrate maintains a `slot_count_capacity` in the header that may exceed the currently-allocated file size; the difference is a region the substrate has reserved (via `fallocate`) for future growth.

## 2. The header (4096 bytes)

| Offset | Size (bytes) | Field | Type |
|---|---|---|---|
| 0 | 4 | magic | ASCII "BARN" (0x42 0x41 0x52 0x4E) |
| 4 | 4 | format_version | u32, little-endian |
| 8 | 16 | shard_uuid | UUIDv7 bytes |
| 24 | 4 | vector_dim | u32 LE (must be 384 for v1) |
| 28 | 4 | slot_size | u32 LE (must be 1600 for v1) |
| 32 | 8 | slot_count_capacity | u64 LE |
| 40 | 8 | slot_count_in_use | u64 LE (advisory) |
| 48 | 16 | embedding_model_fp_active | [u8; 16] |
| 64 | 8 | created_at | u64 LE, unix nanoseconds |
| 72 | 8 | last_grow_at | u64 LE |
| 80 | 4 | header_crc32c | u32 LE, computed over bytes [0..76] |
| 84 | 4012 | reserved | zero |

Endianness is **little-endian** for storage. (The wire protocol uses big-endian; storage uses LE because it matches modern x86_64 and ARM native order.)

The `header_crc32c` is computed over bytes 0–75 (i.e., excluding the CRC field itself and the reserved region). Validating the header on startup catches accidental header corruption before anything depends on the file.

## 3. The slot (1600 bytes)

```
+------+------+------+------+ -+
|     vector (1536 bytes)    |  |
|   384 × f32 little-endian  |  |
|                            |  |
+----------------------------+  |  1600 bytes total
|   slot metadata (64 bytes) |  |
+----------------------------+ -+
```

### 3.1 Vector (1536 bytes)

384 f32 values, little-endian, contiguous. Element 0 is at byte offset 0 within the slot; element 383 is at byte offset 1532.

The values must be finite and form an L2-normalized vector (norm in `[1.0 - 1e-3, 1.0 + 1e-3]`). The substrate validates norms on input and during periodic scrubbing.

### 3.2 Slot metadata (64 bytes)

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

The `metadata_crc32c` covers the slot's vector and most of its metadata, so a corruption check spans the whole slot. Computing this CRC on every read would slow the hot path; the substrate computes it only during periodic scrubbing or when a recovery suspects a slot.

The flags layout:

| Bit | Meaning |
|---|---|
| 0 | Slot occupied (1 = has a memory; 0 = free) |
| 1 | Tombstoned (1 = forgotten, awaiting reclaim) |
| 2 | Pending-write (1 = write in progress; transient) |
| 3 | Hard-forgotten (1 = vector was zeroed; informational) |
| 4–31 | Reserved (zero) |

The combination `bit 0 = 1, bit 1 = 1` is "active but tombstoned" — the slot still has its vector and metadata, but the memory is no longer queryable. After reclaim, both bits become 0 (slot free) until the next encode flips bit 0 back.

## 4. Cache-line padding consideration

Slot size 1600 is a multiple of 64 (cache line size). Slot offsets `4096 + n × 1600` are also multiples of 64 because 4096 is, and 1600 is. So slots are naturally cache-line-aligned.

Each slot's vector starts at offset 0 of the slot, which is cache-line-aligned. Each slot's metadata starts at offset 1536, also cache-line-aligned (1536 = 24 × 64).

We considered padding slots to 1664 (26 cache lines) to align with 4 KB page boundaries every 16 slots. Rejected:

- Wastes 64 bytes per slot (4% overhead at 1.5 GB scale = 60 MB of padding for 1M slots).
- Doesn't help TLB utilization meaningfully — the working set during search isn't 16 sequential slots.
- Adds complexity (slot offset computation becomes more error-prone).

We stick with 1600.

## 5. Page boundaries and slots

A slot's bytes may straddle a 4 KB page boundary. With slot size 1600:

- Slot 0 starts at offset 4096 (page-aligned).
- Slot 1 starts at offset 5696 (within the same page).
- Slot 2 starts at offset 7296 (in the next page; specifically, page 8192 starts within slot 2).

This means a single slot may span two pages. For random access patterns (which ANN search has), this is fine — the page cache loads both pages, and access patterns aren't sequential anyway.

For sequential scans (during recovery, scrubbing), reading slots in order accesses pages in order, which the kernel readahead handles efficiently.

## 6. Vector storage format

Each f32 is 4 bytes, little-endian IEEE 754. NaN, ±Inf, and subnormals are technically representable; the substrate validates that vectors contain only finite values.

A normalized 384-dim vector has typical f32 element magnitudes of ~0.05 (since 1/sqrt(384) ≈ 0.051). The full IEEE 754 dynamic range is wildly more than needed; we use f32 for SIMD efficiency, not range.

We considered f16 (half precision):
- 2× storage savings (768 bytes per vector instead of 1536).
- Sufficient precision for cosine similarity at 384 dim.
- But: poorer SIMD support on x86 (gather/scatter required), and the model itself outputs f32.

f32 is the right choice for v1. f16 is a possible future optimization (tracked in [`12_open_questions.md`](12_open_questions.md)).

## 7. Slot ID to offset

```rust
fn slot_offset(slot_id: u64) -> usize {
    HEADER_SIZE as usize + (slot_id as usize) * SLOT_SIZE
}

const HEADER_SIZE: u32 = 4096;
const SLOT_SIZE: u32 = 1600;
const VECTOR_DIM: usize = 384;
```

Slot ID 0 is the first slot at file offset 4096. Slot ID `slot_count - 1` is the last slot at file offset `4096 + (slot_count - 1) × 1600`.

## 8. The MemoryId encodes the slot

A `MemoryId` is 16 bytes laid out as ([02.03 Identifiers](../02_data_model/03_identifiers.md)):

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

## 9. The empty-arena case

A freshly-initialized arena (no memories yet) has:

- Header populated with zeros except for the magic, format version, shard UUID, and dimensions.
- All slots zeroed (their flags bit 0 = 0, indicating free).
- `slot_count_capacity = 1024` (default initial capacity).
- File on disk: 4096 + 1024 × 1600 = 1,642,496 bytes ≈ 1.6 MB, sparse if the filesystem supports it.

A newly-allocated slot (the first encode) goes to slot ID 0 (the lowest free slot). Subsequent encodes use slots 1, 2, 3, ... in order, populating the arena densely.

## 10. Reading slots that haven't been written

A slot in the file's reserved-but-not-written region returns zeros when read. The flags' bit 0 is 0 (free), so the substrate correctly identifies it as not occupied. The vector bytes are zero, but the substrate never reads vectors from free slots.

This depends on the filesystem honoring the sparse-file convention (or initializing zeroes on allocation, which the substrate does explicitly via `fallocate` followed by no writes).

## 11. Verification on load

At startup, after the file is mmap'd, the substrate optionally:

1. Reads the header.
2. Verifies the magic bytes (`"BARN"`).
3. Verifies the header CRC32C.
4. Verifies the shard UUID matches the configured shard.
5. Verifies the format version is supported.
6. Verifies dim and slot_size match expectations.

Any mismatch is a startup failure; the substrate refuses to operate with a malformed arena.

The full slot-level CRC verification is a separate background-job; the startup path is fast and only verifies the header.

---

*Continue to [`03_arena_growth.md`](03_arena_growth.md) for arena growth.*
