# 05.03 Arena Growth

The arena starts small and grows over time as the shard's memories accumulate. This file specifies how growth happens — the system calls used, the mmap remapping, and the policies for when to grow.

## 1. The growth model

The arena grows in **doubling steps**: each growth doubles the slot capacity. So the sequence of capacities is:

```
1024 → 2048 → 4096 → ... → 1M → 2M → 4M → ... → 268M
```

Doubling is the standard choice for amortized-O(1) growth. Linear growth (add a fixed number of slots) would have O(n²) total cost across n growths; doubling is O(n).

Initial capacity: 1024 slots (1.6 MB). Maximum capacity: 2^48 slots (effectively unbounded; bounded by the operator's disk).

## 2. The growth procedure

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

## 3. fallocate

`fallocate(fd, 0, offset, length)` extends the file. Mode 0 (the default) means "ensure the requested range is allocated"; if the filesystem supports sparse files, this may not actually write blocks until they're modified.

We use mode 0 (not `FALLOC_FL_KEEP_SIZE`) because we want the file's reported size to grow. This is what tools like `du` and `ls -l` report.

For `XFS` and `ext4`, `fallocate` is fast — it allocates extents without zeroing them. For older filesystems or those without extent support, `fallocate` may fall back to writing zeros, which is much slower; we accept this rare case.

The Linux kernel header for fallocate flags is at [`include/uapi/linux/falloc.h`](https://github.com/torvalds/linux/blob/master/include/uapi/linux/falloc.h).

## 4. mremap

After extending the file, we need to extend the mmap region.

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

`MREMAP_MAYMOVE` lets the kernel relocate the mapping if there's no contiguous space at the old address. The substrate handles the relocation by atomically swapping the mmap pointer.

The relocation is a concurrency event:

- Readers that obtained the old pointer continue using it (still valid until they release it).
- New reads acquire the new pointer.
- The old mapping is unmapped after no readers reference it.

Coordination uses `arc-swap`: the mmap pointer is wrapped in `Arc<MmapRegion>`, swapped atomically on growth. Readers use `arc-swap`'s load to get the current Arc; growth updates with `store`.

See [10. Concurrency + Epoch Model](../10_concurrency_epochs/) §arena_remap for the full coordination protocol.

## 5. The fallback: re-mmap

If `mremap` is not available or fails, the substrate falls back to:

1. `mmap` a new region at `new_size`.
2. Copy data... no wait, we don't copy. The new mapping points at the same file. The kernel maps the same file pages.
3. Atomically swap.
4. `munmap` the old region.

This works because both regions point at the same file; the kernel doesn't duplicate page-cache pages.

## 6. When growth fails

`fallocate` can fail with `ENOSPC` (out of disk space) or `EFBIG` (file too large for the filesystem).

Response:
- The encode operation that triggered growth fails with `OutOfStorage`.
- The substrate logs the failure with current capacity and disk free-space stats.
- The substrate continues operating with the existing arena.
- The operator must address the underlying issue (add disk, evict memories, split shards).

The substrate does **not** automatically delete or evict to make room. Eviction happens only via the consolidation worker on its own schedule, not as a response to growth failure.

## 7. Write amplification

Growth uses `fallocate`, which doesn't write data — it just reserves blocks. The actual writing happens lazily as new slots are populated.

So growth itself has minimal write cost (some metadata in the filesystem). The cost is paid as slots fill up, distributed over time.

For NVMe SSDs, this is a non-issue. For older storage with significant block-allocation overhead, the cost is bounded.

## 8. Shrinking?

The arena does not shrink in v1. Slots that become free (after FORGET + reclaim) are reused, not returned to the OS.

Why no shrinking:
- The mmap region's address range can't be partially unmapped without disrupting the file's logical layout.
- Slot IDs are monotonic; "shrinking" would require reorganizing live slots, breaking MemoryId stability.
- The cost of unused slots is small (1600 bytes each, on cheap storage).

A shrinking operation would be a tool-level offline procedure: snapshot, copy live slots to a new compact arena, atomically swap. Useful but not in v1.

## 9. Pre-allocation

Operators expecting a known shard size can pre-allocate via configuration:

```
[shard.<shard_uuid>]
initial_arena_slots = 1000000
```

At startup, the substrate sizes the arena to 1M slots (1.6 GB) immediately. This avoids growth events during the first ~1M encodes.

Pre-allocation is an optimization, not a requirement. Default is the doubling sequence starting at 1024.

## 10. Growth concurrency

A grow event holds the per-shard writer lock briefly:

1. Lock acquired (shared with all writes).
2. Compute new size, call `fallocate`.
3. Call `mremap` and atomic-swap the pointer.
4. Update header's `slot_count_capacity` (one cache-line write to mmap'd region).
5. Lock released.

Total wall time: typically < 1 ms on NVMe. The lock is held for the duration; readers continue without contention; new writes wait briefly.

For very large arenas (10 GB → 20 GB grow), `fallocate` may take longer; typically still under 50 ms but operator-visible.

## 11. The scheme for very large shards

Some operators may want shards larger than the 16 GB-class arena (say, 100 GB). At slot size 1600, this is 64M slots. Mapping a 100 GB region is fine on modern Linux (64-bit address space, ample RAM); the question is whether it makes operational sense.

We recommend keeping individual shards at ≤ 32 GB arenas (≈ 20M memories). Beyond that:

- TLB pressure increases (no huge pages for file-backed mmaps on regular FS).
- Recovery times grow.
- Backup sizes become unwieldy.

If a deployment needs more, add more shards (the cluster-level horizontal split) rather than growing individual shards.

## 12. Fragmentation

Slot reuse means the arena's logical layout becomes "swiss cheese" over time — interleaved live and free slots. This is fine for random-access ANN search; it doesn't affect correctness or performance meaningfully.

For sequential scans (recovery, scrubbing), the substrate handles free slots in stride; no cost beyond the time to look at the flag.

## 13. Header sync on growth

After `fallocate` and `mremap`, the substrate updates the header to reflect the new capacity. This update is to the mmap'd region; the kernel will writeback eventually.

To make the new capacity durable across a crash, the substrate `msync`s the first 4 KB (the header page):

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

If the crash happens before the header sync completes, recovery sees the old capacity. The newly-allocated file region is lost (the file is truncated back to the old size during recovery). The substrate logs this as a recoverable inconsistency; no data is lost because no slots in the new region were actually populated yet.

---

*Continue to [`04_wal_overview.md`](04_wal_overview.md) for the WAL.*
