# 19 — Memory-mapped files and arenas

Brain stores memory vectors in a special file called the
**arena**. It's not opened with normal `read()` and `write()`
calls — instead it's **memory-mapped**, which means reading
the file looks exactly like reading from an array in RAM.

This chapter explains what memory mapping is, why Brain
uses it specifically for the arena, and what the on-disk
layout of the arena looks like.

---

## What memory mapping is

The standard way to read a file is:

```
fd = open("data.bin")
read(fd, buffer, 1024)         # copy bytes from disk to buffer
process(buffer)
close(fd)
```

The OS reads disk blocks into the kernel's page cache,
copies them from the kernel cache into your `buffer`, and
your program reads from the buffer. Three copies, plus
syscalls.

The memory-mapped version:

```
fd = open("data.bin")
data = mmap(fd, length)        # map the file into your address space
process(data[0:1024])           # just read from the array
close(fd)
```

`mmap` makes the *file's bytes* look like an array in your
program's memory. There's no `read()` call; you just index
the array. The OS does the disk I/O behind the scenes —
when you access an address that hasn't been read yet, the
hardware traps, the OS loads the disk block into RAM, and
your access proceeds.

> **What is mmap?**
>
> mmap is a Unix system call that maps a file directly into
> a process's virtual address space. After mmap, reading
> from a memory address looks like a normal pointer
> dereference; the OS transparently loads disk blocks as
> needed.
>
> See [Wikipedia: Memory-mapped file](https://en.wikipedia.org/wiki/Memory-mapped_file)
> and the [mmap(2) man page](https://man7.org/linux/man-pages/man2/mmap.2.html).

Why this is fast for the right use case:

- **No copy** from kernel cache to user buffer. You're
  reading from the kernel's page cache directly.
- **OS handles caching.** Frequently-accessed pages stay
  in RAM; rarely-accessed ones get evicted. You don't have
  to write a cache manager.
- **OS handles I/O.** No explicit `read` calls;
  page-faults trigger disk loads.

Where mmap is *not* the right tool:

- Large sequential reads that you do once. The cost of
  setting up the mapping isn't worth it; just use `read()`.
- Writes you need to know are durable. mmap'd writes go to
  the page cache; `fsync` or `msync` is still needed to
  force them to disk (chapter 18).
- Random-access workloads where most of the file is *cold*.
  Page-faulting on every access is expensive; explicit
  reads with a tighter prefetch strategy can be faster.

The arena's workload is exactly the right shape for mmap:
random-access reads of small fixed-size records, with a
hot subset that fits comfortably in the page cache.

---

## The kernel page cache

> **What's the page cache?**
>
> The kernel maintains a pool of in-RAM copies of recently-
> accessed disk pages. When a program reads a file, the
> kernel first checks if the page is already cached; if so,
> it serves the read from RAM. When the cache fills, the
> kernel evicts the least-recently-used pages back to disk.
>
> See [Wikipedia: Page cache](https://en.wikipedia.org/wiki/Page_cache).

mmap'd files participate in the page cache like any other
file — and *with* the page cache, mmap'd reads are
effectively in-RAM reads as long as the relevant page is
hot. The first access to a page is a disk read (slow); the
hundredth access is a memory read (~ns).

For Brain's arena, this means:

- The arena file might be 5 GB on disk.
- The actively-recalled subset (the "working set") might
  be 500 MB.
- The kernel keeps the 500 MB hot in RAM; the rest sits
  on disk.
- Recall operations hit the hot subset; the kernel
  doesn't have to be told what to cache.

You can tune this somewhat with `madvise` — telling the
kernel "this region will be sequentially scanned" or "this
region is random-access" so it can pre-load pages or set
eviction policy. Brain uses `madvise(MADV_RANDOM)` on the
arena because vector search is, by nature, random access.

---

## What the arena actually holds

`<data_dir>/<shard_id>/arena.bin` is a flat binary file:

```
arena.bin layout
─────────────────────────────────────────────────────
[ 4096-byte header                                ]
[ slot 0:  1600 bytes (vector + metadata)         ]
[ slot 1:  1600 bytes                             ]
[ slot 2:  1600 bytes                             ]
[ ...                                             ]
[ slot N-1: 1600 bytes                            ]
─────────────────────────────────────────────────────
```

The file's size on disk is `4096 + slot_count × 1600`
bytes. For 1 million memories, that's ~1.5 GB.

---

## The header (4096 bytes)

The first 4 KiB are a header describing the file:

| Offset | Size | Field | Purpose |
|---|---|---|---|
| 0 | 4 | magic = `"BARN"` | File-type identifier. |
| 4 | 4 | format_version | Bump for incompatible layout changes. |
| 8 | 16 | shard_uuid | Identifies the shard owning this file. |
| 24 | 4 | vector_dim | 384 in v1. |
| 28 | 4 | slot_size | 1600 in v1. |
| 32 | 8 | slot_count_capacity | Total slots this file can hold. |
| 40 | 8 | slot_count_in_use | Advisory; not authoritative. |
| 48 | 16 | embedding_model_fp_active | Current model fingerprint. |
| 64 | 8 | created_at_unix_nanos | When the arena was first created. |
| 72 | 8 | last_grow_at_unix_nanos | When the file last grew. |
| 80 | 4 | header_crc32c | CRC over bytes 0..76. |
| 84 | 4012 | reserved | Zero. |

The header CRC is verified at open. A header that doesn't
look right (wrong magic, wrong shard_uuid, bad CRC) makes
the shard refuse to spawn — fail-stop, chapter 24.

---

## The slot (1600 bytes)

Each slot is 1600 bytes:

```
slot N
[ vector: 1536 bytes (384 × f32 = 1536 bytes)     ]
[ metadata: 64 bytes                              ]
```

The vector is `384` `f32` values, little-endian,
contiguous. It's the embedding (chapter 08).

The 64 bytes of metadata at the end of each slot are
*not* the full memory metadata — that lives in
`metadata.redb`. The slot-local metadata is what the
substrate needs at the arena level:

- **`slot_version`** (4 bytes) — bumps on reuse (next
  section).
- **`flags`** (4 bytes) — occupied / tombstoned /
  pending-write / hard-forgotten bits.
- **`embedding_model_fp_short`** (16 bytes) — model
  fingerprint that produced this vector.
- **`created_at`** (8 bytes), **`last_modified_at`**
  (8 bytes).
- **`metadata_crc32c`** (4 bytes) — covers the vector +
  slot metadata.
- **`reserved`** (20 bytes) — zero.

The slot CRC is computed on every write and verified
on suspicious reads (typically during periodic scrubbing
rather than on the hot recall path).

Why 1600 bytes and not a tidier number like 2048?

- The vector is 1536 bytes (384 × 4).
- The metadata is 64 bytes.
- Total: 1600.
- 1600 is a clean multiple of 64 (cache line size), so
  slots are naturally cache-aligned.
- We considered padding to 1664 to fit 16 slots per
  4 KiB page; rejected because it wastes ~60 MB at 1M
  slots for negligible gain.

---

## Why fixed-size slots

Every slot is the same size, no exceptions. This isn't a
small detail — fixed sizes make several things much easier:

1. **O(1) lookup by slot index.** Given a slot index `i`,
   the slot's bytes are at offset `4096 + i * 1600`. No
   tree-walk, no hash table.
2. **In-place updates.** A vector update is a memcpy into
   a known location; no allocation, no rebalancing.
3. **Allocation is simple.** Pick the next free slot,
   either from a free-list or by extending the file.
4. **mmap behaves predictably.** The OS can prefetch /
   evict pages without surprises about variable-size
   structures.

The cost is that you can't store a variable-length text
in the arena — Brain doesn't, the text lives in
`metadata.redb`. The arena holds vectors, which are
already fixed-size by the model's design.

---

## Slot allocation

A shard's `SlotAllocator` is a small in-RAM structure:

```
SlotAllocator {
    free_list:    Vec<u64>     # slots that came back from forget
    next_fresh:   u64           # next never-used slot index
    capacity:     u64           # total slots the arena can hold
}
```

When `encode` arrives and needs a slot:

```
1. If free_list is non-empty: pop a slot, return it.
2. Else if next_fresh < capacity: use next_fresh, return it,
   increment next_fresh.
3. Else: trigger arena growth (next section).
```

The allocator's state is rebuilt at shard boot by scanning
the arena's slot flags — every non-occupied, non-pending
slot below the high-water mark goes back into the free
list. (Architecture chapter 03 has the details.)

The free-list isn't persisted directly. It's a runtime
optimisation rebuilt on every boot.

---

## Slot versions and the safe-reuse story

When `forget` runs and the grace period expires, the
slot's bytes get reused by a future `encode`. To make this
safe — to prevent a client's stale `MemoryId` from
accidentally reading the new occupant's data — the slot
carries a **version counter**:

```
slot.metadata.slot_version = 5    # has been used 5 times
```

The `MemoryId` (chapter 05) encodes the slot's version at
issue time:

```
MemoryId  (16 bytes)
[ shard_id ][ slot_id ][ slot_version ][ reserved ]
```

When a client does anything with a `MemoryId`, Brain:

1. Looks up the slot at the encoded index.
2. Compares the encoded version to the slot's current
   version.
3. **Match** → the `MemoryId` is current; proceed.
4. **Mismatch** → return `NotFound`. The slot was reused
   since this `MemoryId` was issued.

Every reclamation (post-grace-period forget cleanup)
bumps the slot's version. So a `MemoryId` from before the
reclamation has a version of `N`; after reclamation the
slot's version is `N+1`; the old `MemoryId` no longer
finds a valid memory.

This is what makes the substrate's grace period actually
matter. The grace period bounds the window between forget
and reclamation; the version check makes the cutover safe.

> **What would happen without slot versions?**
>
> A client holds `MemoryId = (shard=0, slot=42, version=3)`.
> They forget the memory; reclamation happens. A new
> encode lands in slot 42. The client retries the original
> operation with the old `MemoryId`. **Without versions,
> they'd accidentally operate on the new memory.** With
> versions, they get `NotFound`.

---

## Sparse storage

When the arena is created, it's allocated *sparsely*: the
filesystem reserves the space but doesn't actually write
1.5 GB of zeros. Reading an unwritten region returns zeros
without ever touching disk.

> **What's a sparse file?**
>
> A file that the filesystem records as having a certain
> *logical* size but where unwritten regions don't consume
> disk blocks. Most modern filesystems (ext4, XFS, BTRFS,
> APFS) support sparse files transparently.
>
> See [Wikipedia: Sparse file](https://en.wikipedia.org/wiki/Sparse_file).

This matters because the arena is sized for *capacity*, not
*current population*. A fresh arena might be 1.5 GB
logically but only kilobytes on disk. As memories
accumulate, the actual disk usage grows; the file's logical
size doesn't change.

The substrate uses `fallocate` to allocate space *up to*
the capacity. `fallocate(0, 0, size)` tells the filesystem
"reserve this much space for me," which lets the substrate
detect "the disk is going to run out" at allocation time
rather than mid-write.

---

## Growing the arena

When `next_fresh == capacity`, the substrate grows the
file:

1. Call `fallocate(fd, 0, 0, new_size)` to extend the
   reserved space.
2. Call `mremap(MREMAP_MAYMOVE, old_base, old_size,
   new_size)` to extend the memory mapping. The kernel
   may move the mapping if the next region of address
   space isn't free; the substrate updates its base
   pointer.
3. Update the header (`slot_count_capacity`,
   `last_grow_at_unix_nanos`, recompute the CRC).
4. `msync` the header so the capacity bump is durable
   even before any new slot writes happen.

Growth doubles the capacity each time. So a shard that
started at 1024 slots becomes 2048, then 4096, etc. The
amortised cost per slot is constant, and growth is rare
(O(log N) growths per N slots inserted).

Existing slot indexes **never change** during growth. Slot
42 is still slot 42 after the file doubles. This is what
lets `MemoryId`s encode the slot index directly: they
remain valid across growths.

---

## Why mmap, not regular reads

A few alternatives that the substrate considered and
rejected:

- **`read()` with explicit caching in user space.** Brain
  would manage its own cache of vector pages. Equivalent
  in capability, but Brain's cache would compete with the
  kernel's page cache, leading to double-caching and
  worse memory utilisation.
- **`pread()` / `pwrite()` for every vector access.**
  Reasonable for explicit-I/O databases (Postgres works
  this way for its data files). The cost is the syscall
  per access; for vector search that touches many slots
  per query, this adds up.
- **A custom block-storage layer.** Worth it at a different
  scale. For Brain's "100 thousand to 10 million memories
  per shard" scale, mmap is fine and much simpler.

mmap also makes recovery cleaner: opening the file is
~free (`open` + `mmap`), and the substrate can immediately
start reading slots without having to populate any cache.

---

## Recap

- **mmap** maps a file directly into memory; reads look
  like array accesses, OS handles the disk I/O.
- The **kernel page cache** makes mmap'd reads
  RAM-speed for hot pages.
- The arena (`arena.bin`) is a flat file with a 4 KiB
  header and an array of fixed 1600-byte slots.
- Each slot holds a 1536-byte vector + 64 bytes of
  slot-local metadata.
- **Slot versions** make slot reuse safe: a stale
  `MemoryId` gets `NotFound` instead of accidentally
  reading the new occupant's bytes.
- The arena is **sparse** — capacity is reserved but
  not written until slots are filled.
- The file **grows by doubling** when full; existing slot
  indexes are unchanged across growth.

---

## Where to go next

- **What durability builds on top of all this:**
  [chapter 18](18-storage-and-durability.md).
- **How indexes (HNSW) live alongside the arena:**
  [chapter 20](20-indexes-exact-vs-approximate.md).
- **The architecture-tier deep dive:**
  [`../architecture/03-arena-and-wal.md`](../architecture/03-arena-and-wal.md).
- **The redb metadata file that holds the rest of a
  memory's row:**
  [`../architecture/05-redb-metadata.md`](../architecture/05-redb-metadata.md).
