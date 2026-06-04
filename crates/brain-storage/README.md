# brain-storage

> Memory-mapped vector arena and write-ahead log for Brain.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

The durable storage layer for a shard. It owns a memory-mapped **vector arena**
(1600-byte slots = 1536 vector + 64 metadata, 64-byte aligned, per-slot CRC32C,
free-list allocator with version bumping on reclamation) and a per-shard
**write-ahead log** (256 MiB segments, group commit, CRC32C + LSN per record).
It also provides the **crash-recovery** replay engine that re-applies the WAL
into downstream sinks. This is the only crate in the workspace allowed to use
`unsafe`, and only for memory-mapping.

## Key modules

| Module | Purpose |
|---|---|
| `arena` | Memory-mapped slot store and free-list allocator with slot-version bumping. |
| `wal` | Per-shard write-ahead log: segments, group commit, CRC32C/LSN records. |
| `recovery` | WAL replay engine and the `MetadataSink` trait that downstream sinks implement. |
| `layout` | On-disk path layout (`ShardPaths`, `ensure_dirs`). |

## Where it fits

Depends on `brain-core` plus Linux-only I/O primitives (`glommio`, `libc`,
`crc32c`, `bytemuck`). Consumed by `brain-metadata` (which implements the
recovery sink) and the shard runtime in `brain-server`.

## Platform

Linux-only by design — relies on `mmap`/`mremap`, `O_DIRECT`,
`pwritev2(RWF_DSYNC)`, and `io_uring`. Building on non-Linux or big-endian
targets fails at compile time on purpose. Build inside the dev container.

## Spec

- Storage overview: [`../../spec/08_storage/00_purpose.md`](../../spec/08_storage/00_purpose.md)
- Arena: [`../../spec/08_storage/01_arena.md`](../../spec/08_storage/01_arena.md)
- WAL: [`../../spec/08_storage/02_wal.md`](../../spec/08_storage/02_wal.md)
- Recovery: [`../../spec/08_storage/04_recovery.md`](../../spec/08_storage/04_recovery.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
