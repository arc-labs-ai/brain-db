# 05.00 Purpose

This document specifies the storage layer's vector arena and write-ahead log. Together they implement durable storage for memory vectors and the durability barrier for all state-mutating operations.

## What this document covers

- The vector arena: a memory-mapped flat file holding all of a shard's vectors at fixed-size slots.
- The WAL: a per-shard append-only log of every state-mutating operation.
- The interaction between arena and WAL during writes.
- The recovery procedure on crash.
- Snapshot creation via reflink-based file copies.

## What this document does not cover

- **The metadata store.** Defined in [07. Metadata + Graph Store](../07_metadata_graph/).
- **The HNSW index structure.** Defined in [06. ANN Index](../06_ann_index/).
- **The wire-protocol shape of operations.** Defined in [03. Wire Protocol](../03_wire_protocol/).
- **The concurrency model.** Defined in [10. Concurrency + Epoch Model](../10_concurrency_epochs/).

## 1. The role of the storage layer

Three responsibilities:

1. **Persist vectors** for fast access during search. The arena holds them in mmap'd memory; reads are zero-copy.
2. **Persist mutations** durably before acknowledging operations. The WAL provides this barrier; once an operation's WAL record is fsync'd, the operation is durable.
3. **Coordinate consistency** across the arena and metadata store. After a crash, the WAL is the source of truth; arena and metadata are reconstructed from it.

## 2. Per-shard isolation

Each shard has its own arena and its own WAL. Different shards' files are independent — different directories, different file descriptors, different fsyncs.

This design choice is consequential:

- No cross-shard fsync coupling: a slow disk write for shard A doesn't delay shard B's writes.
- Shard rebalancing copies whole files (arena.bin, WAL segments).
- Backups are per-shard.
- Concurrent writes scale with shard count.

## 3. The on-disk layout

For a single shard:

```
data/
└── <shard_uuid>/
    ├── arena.bin              # Vector arena, mmap'd
    ├── arena.header           # Arena metadata (4096 bytes)
    ├── wal/
    │   ├── 0000000000.wal     # WAL segment 0
    │   ├── 0000000001.wal     # WAL segment 1
    │   └── ...
    ├── metadata.redb          # redb metadata store ([07. Metadata + Graph Store])
    └── checkpoints/
        ├── 0000000003.ckpt    # Most recent checkpoint
        └── ...
```

The exact paths and naming conventions are part of the storage format. They MUST be stable within a format version.

## 4. The "log is truth" invariant

After any state-mutating operation:

- If the WAL record is fsync'd, the operation is durable. Recovery will replay it.
- If the WAL record is not fsync'd, the operation is treated as never having happened.

The arena and metadata stores are eventually-consistent with the WAL. They lag the WAL slightly (writes to them happen after the WAL fsync). On a crash, recovery replays WAL records to bring the arena and metadata back into sync.

This is the standard write-ahead-log invariant. The substrate uses it ruthlessly: at no point does the substrate consider an operation durable just because it's reflected in the arena or metadata. The WAL fsync is the durability barrier; everything else is bookkeeping.

## 5. The latency budget

Storage-layer latency targets per operation:

| Operation | Target |
|---|---|
| WAL append (group-committed) | 50–500 µs |
| WAL fsync (with RWF_DSYNC) | bound by NVMe write latency |
| Arena slot read (page cache hit) | < 100 ns |
| Arena slot read (page cache miss, NVMe) | 50–200 µs |
| Arena slot write | < 1 µs (memcpy into mmap region) |
| Recovery replay (per WAL record) | < 100 µs |

Recovery throughput target: at least 100K records/second (sustained), so a 1 GiB WAL recovers in ~30 seconds.

## 6. Why mmap for the arena

The arena could have used direct I/O reads. We chose mmap because:

- **Zero-copy reads.** ANN search reads vectors as `&[f32]` slices directly from the mmap'd region. No copy from kernel buffers.
- **OS-managed working set.** The kernel's page cache decides what's hot and what's cold. Better than any application-level cache.
- **Simple growth.** Extend the file with `fallocate`; remap with `mremap` (or via re-mapping in the substrate's address space).
- **Snapshot-friendly.** A point-in-time view of the arena is just the bytes of the file at that instant. Reflink-based snapshots are a single ioctl.

The cost: TLB pressure on very large arenas. Discussed in [01.05 Hardware](../01_system_architecture/05_hardware.md) §3.3.

## 7. Why O_DIRECT for the WAL

The WAL has the opposite access pattern from the arena:

- Always written sequentially.
- Never read after fsync (except during recovery).
- Doesn't benefit from page cache (no future re-reads).

`O_DIRECT` bypasses the page cache, performing direct DMA between user-space buffers and the storage device. For our WAL, this:

- Eliminates double-buffering (kernel buffer + our buffer).
- Reduces page-cache pollution from data we won't re-read.
- Gives more predictable latency (we control buffer ownership).

## 8. The two parts of this spec

The arena and the WAL are different beasts. The arena is read-heavy, mmap-friendly, no-fsync. The WAL is write-heavy, sequential-only, fsync-critical.

We document them separately in this spec but their interaction during a write is what makes the substrate durable. The write-path file ([`07_write_path.md`](07_write_path.md)) shows them coordinated.

---

*Continue to [`01_arena_overview.md`](01_arena_overview.md) for the arena.*
