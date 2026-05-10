# 05.04 Write-Ahead Log Overview

The write-ahead log (WAL) is the substrate's durability mechanism. Every state-mutating operation is appended to the WAL and fsync'd before the operation is acknowledged. After a crash, the WAL is replayed to reconstruct the substrate's state.

## 1. The WAL's purpose

The WAL achieves three things:

1. **Durability barrier.** Operations are durable iff their WAL record is fsync'd. The arena and metadata stores are eventually-consistent with the WAL.
2. **Crash recovery.** On startup, the substrate replays WAL records to bring its in-memory state and on-disk derived state in sync.
3. **Stream source for SUBSCRIBE.** The WAL's log structure lets clients subscribe to a stream of mutations from a starting LSN ([log sequence number](#3-log-sequence-numbers)).

These three uses share the same underlying append-only log.

## 2. Per-shard WAL

Each shard has its own WAL. WAL records are appended to per-shard segments; recovery operates per-shard.

Per-shard isolation matters because:

- A slow disk on shard A doesn't delay shard B's writes (different files, different fsyncs).
- Recovery can parallelize across shards (each shard's recovery is independent).
- Snapshot/backup is per-shard (atomic snapshots span only one shard's files).

## 3. Log sequence numbers

Every WAL record has a 64-bit unsigned integer **LSN** (Log Sequence Number). LSNs are:

- **Monotonically increasing** within a shard. Each new record gets `previous_lsn + 1`.
- **Unique** within a shard.
- **Not** globally unique across shards (different shards have independent LSN spaces).

LSN 0 is reserved (never used). The first WAL record after a fresh shard creation is LSN 1.

LSNs persist across restarts. The recovery process determines the highest LSN seen and continues from there.

## 4. Segments

The WAL is split into fixed-size **segments**: 256 MiB by default. Each segment is a separate file:

```
wal/
├── 0000000000.wal       # Contains records LSN 1 to (~1M, depending on record sizes)
├── 0000000001.wal       # Contains records LSN ~1M+1 to ~2M
└── 0000000002.wal       # Currently-active segment
```

Segment names are 10-digit zero-padded sequence numbers. The `.wal` extension is for tools; the substrate identifies segments by the name pattern.

When the active segment fills (reaches ~256 MiB), a new segment is started. The previous segment is closed and made read-only (logically; the substrate just stops appending).

256 MiB is a balance:
- Larger segments → fewer files, less overhead per fsync.
- Smaller segments → faster checkpointing and easier deletion of old data.

## 5. Append-only

The WAL is strictly append-only:
- Records are appended at the tail.
- Records are never modified after writing.
- Records are never moved.
- Old records can only be deleted by deleting their containing segment (after a checkpoint covers them).

This simplicity is key. An append-only structure has no concurrency issues for readers (older offsets are immutable) and is friendly to fsync (sequential writes, no random I/O).

## 6. Record format

Each WAL record carries:

```
[record_header: 32 bytes]
[record_payload: variable]
[record_footer: 8 bytes (CRC32C of header + payload)]
```

The record header includes:
- LSN (8 bytes)
- record_type (1 byte) — encode, forget, link, etc.
- payload_length (4 bytes)
- timestamp (8 bytes, unix nanoseconds)
- agent_id (16 bytes; for routing-aware filtering during SUBSCRIBE)
- ...

Detailed format is in [`05_wal_records.md`](05_wal_records.md).

## 7. Synchronization

A WAL append happens through the per-shard writer task. The writer:

1. Receives the record (from the request handler).
2. Buffers it in the active segment's append buffer.
3. Decides when to fsync (group commit window, see below).
4. Submits a `pwritev2` with `RWF_DSYNC` via io_uring.
5. Once the kernel signals completion, the record is durable.
6. Acknowledges to the request handler.

The single-writer-per-shard discipline means there's no lock contention; the writer task is the only producer of WAL records for that shard.

## 8. Group commit

Instead of fsync-per-record (slow), the WAL uses **group commit**: many records share a single fsync.

A group commit window is small (default: 100 µs). All records that arrive within the window are fsync'd together.

The trade-off:
- Smaller window → lower latency per record, less batching, more fsync overhead.
- Larger window → higher latency per record (waiting for window to close), but better fsync amortization.

100 µs is short enough that p50 latency isn't dominated by waiting; long enough to gather meaningful batches under load.

Detailed group-commit protocol is in [`06_wal_durability.md`](06_wal_durability.md).

## 9. The active segment's lifecycle

```
[empty]
   ↓ (first append)
[active, growing]
   ↓ (size threshold)
[full, sealed → read-only]
   ↓ (referenced by recent state)
[old, retained]
   ↓ (covered by checkpoint)
[old, eligible for deletion]
   ↓ (deletion sweep)
[deleted]
```

The substrate keeps recent old segments around even after they're checkpointed, in case SUBSCRIBE clients are still consuming them. The retention policy is in [`09_checkpointing.md`](09_checkpointing.md).

## 10. WAL on cold start

On a fresh shard's creation:

1. The substrate creates `wal/0000000000.wal` with a 4 KB header.
2. The first append (LSN 1) goes into this segment.
3. Subsequent appends extend the segment.

The WAL's segment header carries:

- Magic bytes ("BWAL" — Brain WAL).
- Format version.
- Shard UUID.
- Starting LSN of this segment.
- A CRC32C over the header.

## 11. WAL on recovery

On restart, the substrate:

1. Lists all `*.wal` segments.
2. Sorts them by name.
3. For each segment in order, reads records and applies them to in-memory state.
4. Stops at the first record that fails CRC validation (assumed truncated due to crash).
5. Computes the next LSN from the last successfully-read record.

Detailed recovery procedure in [`08_recovery.md`](08_recovery.md).

## 12. The fsync barrier and what it means

When we say "fsync'd", we mean the kernel has confirmed the data is on stable storage. For NVMe SSDs:

- A successful fsync (specifically `pwritev2` with `RWF_DSYNC`) means the data is in the device's write buffer or beyond (depending on device-level flush behavior).
- For most NVMe devices configured with FUA (Force Unit Access), the data has reached non-volatile media.
- For consumer-grade SSDs without FUA, a power loss may still lose buffered data; this is a property of the device, not of the substrate.

The substrate trusts the kernel's fsync semantics. Operators are responsible for using storage that honors fsync correctly. Most enterprise storage does; cheap consumer hardware sometimes lies.

## 13. WAL throughput

A single WAL writer can sustain:

- ~50K records/second on commodity NVMe (with group commit).
- ~200K records/second on enterprise NVMe with high IOPS.
- Higher on PMEM/Optane (close to a million).

These numbers assume:
- Group commit window of 100 µs.
- Records of typical size (1-2 KB for ENCODE).
- Modern Linux kernel (5.8+) with io_uring.

The write rate is per-shard. A node with 32 shards can sustain ~1.6M records/second aggregate.

## 14. The WAL's relationship with the metadata store

The metadata store (redb) has its own internal log/journal that ensures atomicity of redb transactions. The Brain WAL is at a higher level: it logs operations that may span multiple redb transactions (e.g., an encode that updates the metadata table and the edge table).

The interaction:

1. Brain writes its own WAL record.
2. Brain fsyncs the WAL.
3. Brain begins a redb transaction.
4. Brain modifies redb tables (with redb's internal journaling).
5. Brain commits the redb transaction (which syncs internally).
6. Brain may proceed to update the arena and HNSW.

If the system crashes after step 2 but before step 5, recovery replays the Brain WAL record, which redoes step 3-5.

Detailed write-path protocol is in [`07_write_path.md`](07_write_path.md).

---

*Continue to [`05_wal_records.md`](05_wal_records.md) for record formats.*
