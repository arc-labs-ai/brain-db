# 08. Storage: Arena & WAL

> **TL;DR.** Per-shard durable storage. The arena is a memory-mapped flat file of fixed-size 1600-byte slots holding vectors, accessed zero-copy via `MAP_SHARED`. The WAL is an O_DIRECT append-only log fsynced via `pwritev2(RWF_DSYNC)` group commit; no operation acks until its record is durable. After a crash, the WAL is replayed to rebuild arena and metadata. The WAL is the source of truth; everything else is derived. Reflink-based snapshots provide point-in-time backups.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Storage-layer implementers; operators planning capacity |
| Voice | Hybrid (rationale + normative byte-level requirements) |
| Depends on | [01. System Architecture](../01_architecture/00_purpose.md), [02. Data Model](../02_data_model/00_purpose.md), [07. Embedding Layer](../07_embedding/00_purpose.md) |
| Referenced by | [09. ANN Index](../09_indexing/00_purpose.md), [14. Concurrency](../14_concurrency/00_purpose.md), [15. Background Workers](../15_background_workers/00_purpose.md), [18. Failure Recovery](../18_failure_recovery/00_purpose.md) |

## What this spec defines

Layer L5 of the architecture — the storage layer. It defines:

- The **vector arena**: a memory-mapped flat file that holds all of a shard's vectors. Slot-based, fixed-size, alignment-preserving.
- The **write-ahead log (WAL)**: an append-only durable log of every state-mutating operation. Brain's source of truth for crash recovery.
- The coordination between arena and WAL during writes.
- The recovery procedure on startup.
- The retention and checkpointing policies.

The metadata store (redb-backed B-tree) is a separate spec ([10. Metadata + Graph Store](../10_metadata/00_purpose.md)). This spec covers the parts of the storage layer that hold vectors and the durability log.

This document specifies the storage layer's vector arena and write-ahead log. Together they implement durable storage for memory vectors and the durability barrier for all state-mutating operations.

## What this document covers

- The vector arena: a memory-mapped flat file holding all of a shard's vectors at fixed-size slots.
- The WAL: a per-shard append-only log of every state-mutating operation.
- The interaction between arena and WAL during writes.
- The recovery procedure on crash.
- Snapshot creation via reflink-based file copies.

## What this document does not cover

- **The metadata store.** Defined in [10. Metadata + Graph Store](../10_metadata/00_purpose.md).
- **The HNSW index structure.** Defined in [09. Indexing](../09_indexing/00_purpose.md).
- **The wire-protocol shape of operations.** Defined in [04. Wire Protocol](../04_wire_protocol/00_purpose.md).
- **The concurrency model.** Defined in [14. Concurrency](../14_concurrency/00_purpose.md).

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
├── llm_credentials.redb       # Tenant LLM provider keys, sealed; shared by every shard
├── llm_credential_kek         # Generated sealing key, unless [llm.credentials] kek is set
└── <shard_uuid>/
    ├── arena.bin              # Vector arena, mmap'd
    ├── arena.header           # Arena metadata (4096 bytes)
    ├── wal/
    │   ├── 0000000000.wal     # WAL segment 0
    │   ├── 0000000001.wal     # WAL segment 1
    │   └── ...
    ├── metadata.redb          # redb metadata store ([10. Metadata + Graph Store])
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

This is the standard write-ahead-log invariant. Brain uses it ruthlessly: at no point does Brain consider an operation durable just because it's reflected in the arena or metadata. The WAL fsync is the durability barrier; everything else is bookkeeping.

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

The arena could have used direct I/O reads. Brain uses mmap because:

- **Zero-copy reads.** ANN search reads vectors as `&[f32]` slices directly from the mmap'd region. No copy from kernel buffers.
- **OS-managed working set.** The kernel's page cache decides what's hot and what's cold. Better than any application-level cache.
- **Simple growth.** Extend the file with `fallocate`; remap with `mremap` (or via re-mapping in Brain's address space).
- **Snapshot-friendly.** A point-in-time view of the arena is just the bytes of the file at that instant. Reflink-based snapshots are a single ioctl.

The cost: TLB pressure on very large arenas. Discussed in [01.05 Hardware and Targets](../01_architecture/05_hardware_and_targets.md) §3.3.

## 7. Why O_DIRECT for the WAL

The WAL has the opposite access pattern from the arena:

- Always written sequentially.
- Never read after fsync (except during recovery).
- Doesn't benefit from page cache (no future re-reads).

`O_DIRECT` bypasses the page cache, performing direct DMA between user-space buffers and the storage device. For Brain's WAL, this:

- Eliminates double-buffering (kernel buffer + Brain's buffer).
- Reduces page-cache pollution from data Brain does not re-read.
- Gives more predictable latency (Brain controls buffer ownership).

## 8. The two parts of this spec

The arena and the WAL are different beasts. The arena is read-heavy, mmap-friendly, no-fsync. The WAL is write-heavy, sequential-only, fsync-critical.

They are documented separately in this spec but their interaction during a write is what makes Brain durable. The write-path file ([`03_write_path.md`](03_write_path.md)) shows them coordinated.

---

*Continue to [`01_arena.md`](01_arena.md) for the arena.*
