# 08.02 Write-Ahead Log

The WAL: overview, byte-level record formats, and the durability mechanism (O_DIRECT, RWF_DSYNC, group commit). The WAL is Brain's durability mechanism — every state-mutating operation is appended to the WAL and fsync'd before the operation is acknowledged. After a crash, the WAL is replayed to reconstruct Brain's state.

## Overview

The write-ahead log (WAL) is Brain's durability mechanism. Every state-mutating operation is appended to the WAL and fsync'd before the operation is acknowledged. After a crash, the WAL is replayed to reconstruct Brain's state.

### 1. The WAL's purpose

The WAL achieves three things:

1. **Durability barrier.** Operations are durable iff their WAL record is fsync'd. The arena and metadata stores are eventually-consistent with the WAL.
2. **Crash recovery.** On startup, Brain replays WAL records to bring its in-memory state and on-disk derived state in sync.
3. **Stream source for SUBSCRIBE.** The WAL's log structure lets clients subscribe to a stream of mutations from a starting LSN ([log sequence number](#3-log-sequence-numbers)).

These three uses share the same underlying append-only log.

### 2. Per-shard WAL

Each shard has its own WAL. WAL records are appended to per-shard segments; recovery operates per-shard.

Per-shard isolation matters because:

- A slow disk on shard A doesn't delay shard B's writes (different files, different fsyncs).
- Recovery can parallelize across shards (each shard's recovery is independent).
- Snapshot/backup is per-shard (atomic snapshots span only one shard's files).

### 3. Log sequence numbers

Every WAL record has a 64-bit unsigned integer **LSN** (Log Sequence Number). LSNs are:

- **Monotonically increasing** within a shard. Each new record gets `previous_lsn + 1`.
- **Unique** within a shard.
- **Not** globally unique across shards (different shards have independent LSN spaces).

LSN 0 is reserved (never used). The first WAL record after a fresh shard creation is LSN 1.

LSNs persist across restarts. The recovery process determines the highest LSN seen and continues from there.

### 4. Segments

The WAL is split into fixed-size **segments**: 256 MiB by default. Each segment is a separate file:

```
wal/
├── 0000000000.wal       # Contains records LSN 1 to (~1M, depending on record sizes)
├── 0000000001.wal       # Contains records LSN ~1M+1 to ~2M
└── 0000000002.wal       # Currently-active segment
```

Segment names are 10-digit zero-padded sequence numbers. The `.wal` extension is for tools; Brain identifies segments by the name pattern.

When the active segment fills (reaches ~256 MiB), a new segment is started. The previous segment is closed and made read-only (logically; Brain just stops appending).

256 MiB is a balance:
- Larger segments → fewer files, less overhead per fsync.
- Smaller segments → faster checkpointing and easier deletion of old data.

### 5. Append-only

The WAL is strictly append-only:
- Records are appended at the tail.
- Records are never modified after writing.
- Records are never moved.
- Old records can only be deleted by deleting their containing segment (after a checkpoint covers them).

This simplicity is key. An append-only structure has no concurrency issues for readers (older offsets are immutable) and is friendly to fsync (sequential writes, no random I/O).

### 6. Record format

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
- space_id (16 bytes; for routing-aware filtering during SUBSCRIBE)
- ...

Detailed format is in § Record formats below.

### 7. Synchronization

A WAL append happens through the per-shard writer task. The writer:

1. Receives the record (from the request handler).
2. Buffers it in the active segment's append buffer.
3. Decides when to fsync (group commit window, see below).
4. Submits a `pwritev2` with `RWF_DSYNC` via io_uring.
5. Once the kernel signals completion, the record is durable.
6. Acknowledges to the request handler.

The single-writer-per-shard discipline means there's no lock contention; the writer task is the only producer of WAL records for that shard.

### 8. Group commit

Instead of fsync-per-record (slow), the WAL uses **group commit**: many records share a single fsync.

A group commit window is small (default: 100 µs). All records that arrive within the window are fsync'd together.

The trade-off:
- Smaller window → lower latency per record, less batching, more fsync overhead.
- Larger window → higher latency per record (waiting for window to close), but better fsync amortization.

100 µs is short enough that p50 latency isn't dominated by waiting; long enough to gather meaningful batches under load.

Detailed group-commit protocol is in § Durability below.

### 9. The active segment's lifecycle

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

Brain keeps recent old segments around even after they're checkpointed, in case SUBSCRIBE clients are still consuming them. The retention policy is in [`05_checkpointing.md`](05_checkpointing.md).

### 10. WAL on cold start

On a fresh shard's creation:

1. Brain creates `wal/0000000000.wal` with a 4 KB header.
2. The first append (LSN 1) goes into this segment.
3. Subsequent appends extend the segment.

The WAL's segment header carries:

- Magic bytes ("BWAL" — Brain WAL).
- Format version.
- Shard UUID.
- Starting LSN of this segment.
- A CRC32C over the header.

### 11. WAL on recovery

On restart, Brain:

1. Lists all `*.wal` segments.
2. Sorts them by name.
3. For each segment in order, reads records and applies them to in-memory state.
4. Stops at the first record that fails CRC validation (assumed truncated due to crash).
5. Computes the next LSN from the last successfully-read record.

Detailed recovery procedure in [`04_recovery.md`](04_recovery.md).

### 12. The fsync barrier and what it means

When "fsync'd" is used, it means the kernel has confirmed the data is on stable storage. For NVMe SSDs:

- A successful fsync (specifically `pwritev2` with `RWF_DSYNC`) means the data is in the device's write buffer or beyond (depending on device-level flush behavior).
- For most NVMe devices configured with FUA (Force Unit Access), the data has reached non-volatile media.
- For consumer-grade SSDs without FUA, a power loss may still lose buffered data; this is a property of the device, not of Brain.

Brain trusts the kernel's fsync semantics. Operators are responsible for using storage that honors fsync correctly. Most enterprise storage does; cheap consumer hardware sometimes lies.

### 13. WAL throughput

A single WAL writer can sustain:

- ~50K records/second on commodity NVMe (with group commit).
- ~200K records/second on enterprise NVMe with high IOPS.
- Higher on PMEM/Optane (close to a million).

These numbers assume:
- Group commit window of 100 µs.
- Records of typical size (1-2 KB for ENCODE).
- Modern Linux kernel (5.8+) with io_uring.

The write rate is per-shard. A node with 32 shards can sustain ~1.6M records/second aggregate.

### 14. The WAL's relationship with the metadata store

The metadata store (redb) has its own internal log/journal that ensures atomicity of redb transactions. The Brain WAL is at a higher level: it logs operations that may span multiple redb transactions (e.g., an encode that updates the metadata table and the edge table).

The interaction:

1. Brain writes its own WAL record.
2. Brain fsyncs the WAL.
3. Brain begins a redb transaction.
4. Brain modifies redb tables (with redb's internal journaling).
5. Brain commits the redb transaction (which syncs internally).
6. Brain may proceed to update the arena and HNSW.

If the system crashes after step 2 but before step 5, recovery replays the Brain WAL record, which redoes step 3-5.

Detailed write-path protocol is in [`03_write_path.md`](03_write_path.md).

## Record Formats

The byte-level format of WAL records. Implementers MUST produce these layouts; recovery and SUBSCRIBE consume them.

### 1. The segment header

Each segment file begins with a 4 KB header:

| Offset | Size | Field | Type |
|---|---|---|---|
| 0 | 4 | magic | "BWAL" (0x42 0x57 0x41 0x4C) |
| 4 | 4 | format_version | u32 LE |
| 8 | 16 | shard_uuid | UUIDv7 |
| 24 | 8 | segment_seq | u64 LE (matches the file name) |
| 32 | 8 | starting_lsn | u64 LE (first LSN in this segment) |
| 40 | 8 | created_at | u64 LE, unix nanoseconds |
| 48 | 4 | header_crc32c | u32 LE |
| 52 | 4044 | reserved | zero |

After the header (offset 4096), records begin.

### 2. The record header (32 bytes)

Each record starts with a 32-byte header:

| Offset | Size | Field | Type |
|---|---|---|---|
| 0 | 8 | lsn | u64 LE |
| 8 | 1 | record_type | u8 |
| 9 | 1 | flags | u8 |
| 10 | 2 | reserved | zero |
| 12 | 4 | payload_length | u32 LE |
| 16 | 8 | timestamp | u64 LE, unix nanoseconds |
| 24 | 8 | space_id_lo64 | u64 LE (low 8 bytes of the space UUID) |

The full `space_id` is 16 bytes; only the low 64 bits sit in the header, for cheap filtering during a scan. The full id is in the payload where it is needed. The header deliberately carries no `namespace_id`: the namespace is implied by the shard's own scope, and a 16-byte key in a 32-byte header would leave no room for the LSN and length fields a scan needs first.

After the record header comes the payload (variable length, exactly `payload_length` bytes), followed by an 8-byte footer:

| Offset (relative to footer) | Size | Field | Type |
|---|---|---|---|
| 0 | 4 | payload_crc32c | u32 LE (CRC32C over the entire record header + payload) |
| 4 | 4 | reserved | zero |

Total record size: `32 + payload_length + 8`.

### 3. Record types

The `record_type` byte:

| Value | Type | Description |
|---|---|---|
| 0 | Reserved | Never used |
| 1 | ENCODE | A new memory was created |
| 2 | FORGET | A memory was forgotten |
| 3 | LINK | An edge was added |
| 4 | UNLINK | An edge was removed |
| 5 | UPDATE_SALIENCE | A memory's salience was updated |
| 6 | RECLAIM | A tombstoned slot was reclaimed |
| 7 | CONSOLIDATE | The consolidation worker created a Consolidated memory |
| 8 | UPDATE_KIND | A memory's kind was changed |
| 9 | UPDATE_SESSION | A memory's session was changed |
| 10 | CHECKPOINT_BEGIN | A checkpoint started |
| 11 | CHECKPOINT_END | A checkpoint completed |
| 12 | TXN_BEGIN | A transaction started |
| 13 | TXN_COMMIT | A transaction committed |
| 14 | TXN_ABORT | A transaction was aborted |
| 15 | MIGRATE_EMBEDDING | A memory was re-embedded with a new model |
| 16-127 | Typed-graph, registry and lifecycle records (§21) | |
| 128-255 | Reserved for future major version | |

Each type's payload format is specified below.

### 4. Flags byte

The `flags` byte:

| Bit | Meaning |
|---|---|
| 0 | Part of a transaction (see TxnId in payload) |
| 1 | Coalesced (this record represents multiple logical operations of the same type) |
| 2 | Replayed (set during recovery; helps idempotency in re-recovery scenarios) |
| 3-7 | Reserved |

### 5. ENCODE record payload

```
struct EncodeRecord {
    memory_id: MemoryId,             // 16 bytes (slot_id + version assigned by allocator)
    request_id: RequestId,           // 16 bytes (UUIDv7)
    space_id: SpaceId,               // 16 bytes (full UUID; matches header low64)
    session_id: SessionId,           // 8 bytes
    kind: u8,                        // 1 byte (Episodic/Semantic/Consolidated)
    salience_initial: f32,           // 4 bytes
    embedding_model_fp: [u8; 16],    // 16 bytes
    text_length: u32,                // 4 bytes
    text: [u8; text_length],         // UTF-8
    vector: [f32; 384],              // 1536 bytes (only if FLAG_INCLUDE_VECTOR)
    // edges follow only if FLAG_INCLUDE_EDGES
    edge_count: u16,
    edges: [EdgeRecord; edge_count],
}
```

The vector is included in ENCODE records by default. This makes the WAL self-sufficient: replay can reconstruct the arena without consulting the metadata store. The cost is ~1.5 KB per encode in the WAL.

For deployments that prefer smaller WAL records, a configuration option excludes the vector (the WAL just records "this memory was encoded"; the arena and metadata store the vector). Recovery is still correct as long as both the arena and WAL survive together.

Default: include the vector.

### 6. FORGET record payload

```
struct ForgetRecord {
    memory_id: MemoryId,             // 16 bytes
    request_id: RequestId,           // 16 bytes
    mode: u8,                        // 0 = soft, 1 = hard
    reason: u8,                      // 0 = client request, 1 = eviction, ...
}
```

Total payload: 34 bytes. Plus header + footer = 74 bytes per FORGET record.

### 7. LINK record payload

```
struct LinkRecord {
    source: MemoryId,                // 16 bytes
    target: MemoryId,                // 16 bytes
    edge_kind: u8,                   // 1 byte (one of the 8 edge types)
    weight: f32,                     // 4 bytes
    origin: u8,                      // 0 = explicit, 1 = auto-derived
}
```

### 8. UNLINK record payload

```
struct UnlinkRecord {
    source: MemoryId,
    target: MemoryId,
    edge_kind: u8,
    edge_seq: u32,                   // For multi-edges
}
```

### 9. UPDATE_SALIENCE record payload

```
struct UpdateSalienceRecord {
    memory_id: MemoryId,
    new_salience: f32,
    reason: u8,                      // 0 = access, 1 = decay, 2 = explicit
}
```

These records are common (every access boost generates one). They're typically coalesced — one UPDATE_SALIENCE record may cover multiple logical updates within a small time window. The `flags` bit 1 (coalesced) is set when this happens; the payload then carries multiple `(memory_id, new_salience, reason)` tuples.

For typical workloads, salience updates are the most numerous WAL records. Coalescing brings them under control.

### 10. RECLAIM record payload

```
struct ReclaimRecord {
    slot_id: u64,                    // 6 effective bytes
    old_version: u32,
    new_version: u32,
}
```

A RECLAIM record indicates a slot was reused. It's the WAL signal that the old MemoryId becomes invalid.

### 11. CONSOLIDATE record payload

```
struct ConsolidateRecord {
    new_memory_id: MemoryId,         // The new Consolidated memory
    source_memory_ids: Vec<MemoryId>, // The episodic memories consolidated
    text_length: u32,
    text: [u8; text_length],
    vector: [f32; 384],
    embedding_model_fp: [u8; 16],
}
```

Plus the implied edges: `DERIVED_FROM` from the new memory to each source. These edges get their own LINK records, written as part of the same transaction (TXN_BEGIN/TXN_COMMIT bracket).

### 12. UPDATE_KIND record payload

```
struct UpdateKindRecord {
    memory_id: MemoryId,
    new_kind: u8,
}
```

### 13. UPDATE_SESSION record payload

```
struct UpdateSessionRecord {
    memory_id: MemoryId,
    new_session_id: SessionId,
}
```

### 14. Transaction records

```
struct TxnBeginRecord {
    txn_id: TxnId,                   // 16 bytes
    expected_record_count: u32,      // Hint for recovery; not strict
}

struct TxnCommitRecord {
    txn_id: TxnId,
}

struct TxnAbortRecord {
    txn_id: TxnId,
    reason_code: u32,
}
```

Records within a transaction carry the `txn_id` in their payload (in addition to having `flags` bit 0 set).

Recovery treats transactional records as a unit:
- TXN_BEGIN seen, TXN_COMMIT seen: apply all records in between.
- TXN_BEGIN seen, TXN_ABORT seen: discard all records in between.
- TXN_BEGIN seen, neither commit nor abort (partial transaction at end of WAL): discard.

### 15. CHECKPOINT records

Checkpoint records are detailed in [`05_checkpointing.md`](05_checkpointing.md). Briefly:

```
struct CheckpointBeginRecord {
    checkpoint_id: u64,
    started_at: u64,
}

struct CheckpointEndRecord {
    checkpoint_id: u64,
    durable_lsn: u64,                // All records up to this LSN are reflected in the checkpoint
    arena_capacity: u64,
}
```

### 16. MIGRATE_EMBEDDING record payload

```
struct MigrateEmbeddingRecord {
    memory_id: MemoryId,
    old_fingerprint: [u8; 16],
    new_fingerprint: [u8; 16],
    new_vector: [f32; 384],
}
```

This is what the migration worker writes when it re-embeds a memory.

### 17. Record alignment

Records are not padded to any alignment within a segment. They're packed back-to-back. The segment grows by appending records; the file's tail is the next free byte.

For O_DIRECT writes (see § Durability below), the writes themselves must be aligned to the device's block size (typically 4 KB). Brain buffers records in an aligned page-sized buffer until full, then writes the buffer.

### 18. CRC32C semantics

The footer's `payload_crc32c` covers:
- The 32-byte record header.
- The variable-length payload.

It does **not** cover itself. The CRC is computed last; it's the receiver's check on the rest.

CRC mismatches during recovery indicate truncation (last record was being written when the crash happened) or corruption (rare). Recovery treats any CRC failure as "truncate here; everything after this point is lost" — which is correct for the truncation case.

### 19. Maximum record size

A record's `payload_length` field is u32 — supports up to 4 GiB payloads. In practice:

- ENCODE records are typically 2-3 KiB (with the included vector).
- CONSOLIDATE records may be 5-10 KiB (multiple source IDs, text, vector).
- Other record types are much smaller.

Brain enforces a configurable max record size (default: 16 MiB) to prevent pathologically-large records from causing problems. Records larger than the limit are rejected at the request validation layer.

### 20. Record indexing

The WAL is sequential; finding a specific LSN requires reading from the segment that contains it. The starting LSN of each segment is in the segment header, so binary-searching across segments is fast.

Within a segment, records are scanned linearly. There's no per-record index; for SUBSCRIBE clients consuming forward, this is the natural access pattern.

For random-access patterns (recovery only needs sequential), no index is needed.

### 21. Typed-graph record types

Entity / statement / relation / schema / audit mutations go through the same WAL. These occupy the reserved range above `0x0F`; the segment header, 32-byte record header, footer CRC, and transaction bracketing rules in §1–§4 and §10 are unchanged. The frame body selects its parser by `record_type`:

| Value | Record type | Body |
|---|---|---|
| 0x10 | ENTITY_CREATE | entity record |
| 0x11 | ENTITY_UPDATE | entity attribute / alias delta |
| 0x12 | ENTITY_MERGE | merge of one entity into another |
| 0x13 | ENTITY_TOMBSTONE | tombstone mark |
| 0x14 | ENTITY_RENAME | canonical-name rename (alias-trail policy) |
| 0x15 | ENTITY_UNMERGE | reverse of a prior merge |
| 0x20 | STATEMENT_CREATE | statement record |
| 0x21 | STATEMENT_SUPERSEDE | (old, new) supersession |
| 0x22 | STATEMENT_TOMBSTONE | tombstone |
| 0x30 | RELATION_CREATE | relation record |
| 0x31 | RELATION_SUPERSEDE | supersession |
| 0x32 | RELATION_TOMBSTONE | tombstone |
| 0x40 | SCHEMA_UPDATE | schema document |
| 0x50 | AUDIT | extractor / resolution audit entry |
| 0x51 | STAGE_COMPLETED | background-stage completion — **change feed only**, see below |
| 0x60 | SPACE_CREATE | space registry row provisioned |
| 0x61 | SPACE_DELETE | space scope-prefix delete (registry + cascade) |
| 0x62 | SESSION_CREATE | session registry row provisioned |
| 0x63 | SESSION_DELETE | session registry row deleted |
| 0x64 | RESTORE_MEMORY | un-tombstone a soft-forgotten memory |

Most of these travel as an **opaque body**: the framing layer transports the bytes unchanged and the typed schema is layered above. Three do not, and are first-class typed payloads instead — relation create / supersede / tombstone, so recovery can rebuild the unified edge row and its sidecar atomically, and `RESTORE_MEMORY`, which is the exact mirror of `FORGET` and re-activates the row, clears `tombstoned_at`, re-inserts the timeline entry and restores the dedup fingerprint, idempotently.

**`STAGE_COMPLETED` (0x51) is not state.** It is a subscribe-replay change-feed record — the durable backing for a `StageCompleted` event — and there is no table it hydrates, unlike every typed-graph write which has a durable counterpart. It is always flagged `FLAG_SUBSCRIBE_EVENT` and **recovery skips it**, exactly like the flagged typed-graph event records. A replay that applied it would be replaying a notification.

Recovery: on startup, the WAL is replayed. Memory frames (§3) hydrate memory state; entity / statement / relation / registry frames hydrate the rest. Derived indexes (tantivy, entity / statement HNSW) are rebuilt from the authoritative redb tables (see [`../10_metadata/02_table_layout.md`](../10_metadata/02_table_layout.md) §14) if missing or corrupt.

A replay failure caused by the **record's own illegality** — a create the API already refused — is a skip, not corruption; see [`04_recovery.md`](04_recovery.md) §5a. The distinction is what keeps one legitimately-refused request from making a shard permanently unbootable.

## Durability

The WAL's durability mechanism: O_DIRECT, RWF_DSYNC, group commit. This is where Linux kernel primitives meet Brain's per-record guarantees.

### 1. The durability commitment

When Brain acknowledges a state-mutating operation, it has guaranteed:

1. The WAL record is on stable storage (fsync semantics).
2. All earlier records are also on stable storage (no out-of-order durability).

Properly speaking, the second guarantee is implied by the first (the WAL is sequential), but it's worth stating explicitly.

After acknowledgment, Brain can crash and the operation will be recovered.

### 2. The kernel primitives

Three kernel primitives form Brain's durability machinery:

#### 2.1 O_DIRECT

When opening WAL segment files, Brain uses `O_DIRECT`:

```rust
let fd = unsafe {
    libc::open(
        path.as_ptr(),
        libc::O_WRONLY | libc::O_CREAT | libc::O_DIRECT,
        0o600,
    )
};
```

`O_DIRECT` semantics:
- Writes go through the kernel directly to the device, bypassing the page cache.
- Buffers must be aligned to the device's block size (typically 4 KB).
- Buffer lengths must be multiples of the block size.
- No double-buffering (kernel page cache + user buffer).

For the WAL, this is appropriate: Brain never re-reads the bytes from the file (except during recovery, when the page cache being clean is fine). Avoiding the page cache means:
- No memory used for caching pages Brain does not re-read.
- More predictable latency (no page-cache writeback variance).

Reference: `O_DIRECT` is defined in `<fcntl.h>` and the kernel UAPI header [`include/uapi/asm-generic/fcntl.h`](https://github.com/torvalds/linux/blob/master/include/uapi/asm-generic/fcntl.h).

#### 2.2 RWF_DSYNC

When writing WAL records, Brain uses the `RWF_DSYNC` flag in `pwritev2`:

```rust
const RWF_DSYNC: u32 = 0x00000002;

unsafe {
    libc::syscall(
        libc::SYS_pwritev2,
        fd,
        iovecs.as_ptr(),
        iovecs.len(),
        offset,
        RWF_DSYNC,
    )
}
```

`RWF_DSYNC` semantics:
- The write is performed.
- The kernel ensures the data is on stable storage before returning.
- Equivalent to `pwritev` followed by `fdatasync`, but in a single syscall.

The numeric value `0x2` is from the Linux UAPI [`include/uapi/linux/fs.h`](https://github.com/torvalds/linux/blob/master/include/uapi/linux/fs.h).

`fdatasync` (as opposed to `fsync`) syncs only data, not metadata changes that don't affect data accessibility. For WAL appends to existing segments, this is appropriate — the file's size update is metadata that the recovery doesn't strictly need (CRC failures at the truncation boundary will detect the truncation).

For new segment files, Brain `fsync`s the parent directory after creating a new segment, ensuring the directory entry is durable.

#### 2.3 io_uring

Rather than calling `pwritev2` synchronously, Brain submits writes via io_uring:

```rust
let mut sqe = ring.next_submission_entry();
sqe.set_op(io_uring::opcode::Writev::CODE)
   .set_fd(fd)
   .set_addr(iovecs.as_ptr() as u64)
   .set_len(iovecs.len() as u32)
   .set_offset(offset)
   .set_rw_flags(RWF_DSYNC);
ring.submit();
```

The io_uring submission queues the write; a completion arrives later via the completion queue. The writer task awaits the completion before acknowledging.

io_uring's value here:
- Multiple writes can be in flight simultaneously (different shards' WALs).
- The syscall overhead is amortized across submissions.
- Modern kernels (5.8+) support `pwritev2` via io_uring with proper semantics.

The [`liburing`](https://github.com/axboe/liburing) library provides the userspace abstraction. The Glommio runtime wraps it.

### 3. The buffer

The WAL writer maintains an aligned buffer for accumulating records:

```rust
struct WalWriter {
    fd: RawFd,
    file_offset: u64,
    buffer: AlignedBuffer,           // 4 KB aligned, 64 KB capacity
    pending_records: Vec<PendingRecord>,
}
```

The buffer's size is configurable; default 64 KB. Larger buffers gather more records per flush but increase per-flush latency.

When a record is appended:
1. Serialize the record into the buffer at the current write offset.
2. Track the record in `pending_records` (LSN, awakener channel).
3. Schedule a group-commit flush (see § 4 below).

When a flush happens:
1. Round the buffer's used size up to the next 4 KB boundary (padding with zero bytes that recovery will interpret as a CRC-failed record and stop at).
2. Submit a `pwritev2` with `RWF_DSYNC` for the buffer's content.
3. Wait for completion.
4. Notify all pending records' awakeners.
5. Advance `file_offset` and reset the buffer for the next batch.

### 4. Group commit timing

Two triggers fire a group commit:

1. **Time-based:** A 100 µs timer (the group-commit window). When the timer fires, flush whatever's in the buffer.
2. **Size-based:** The buffer fills (reaches 60 KB out of 64 KB capacity). Flush immediately.

The 100 µs window:
- Short enough that p50 latency for a single-record write is dominated by fsync, not waiting.
- Long enough to gather meaningful batches under load (1000 records/sec → 1 record per 100 µs window typical; 10K rec/sec → 1-2 records per window).

For very low-rate workloads, every record fsyncs alone; group commit doesn't help, but the latency floor is fsync alone. For high-rate workloads, group commit amortizes fsync over many records.

### 5. The fsync latency floor

On modern NVMe, a single fsync (with FUA) takes ~50-200 µs. With group commit, each record's wait time is dominated by:

- Time waiting for the group-commit window to close: 0-100 µs (avg 50 µs).
- Time for the actual fsync: 50-200 µs.

Per-record wait time: 50-300 µs typical, depending on load and storage. For Brain's overall p99 < 25 ms target, the WAL is a small fraction.

### 6. Pre-allocation

When a new segment is created, Brain uses `fallocate` to pre-allocate its full size (256 MiB):

```rust
unsafe {
    libc::fallocate(
        fd,
        0,           // mode 0 = allocate
        0,           // offset
        SEGMENT_SIZE as i64,  // 256 MiB
    );
}
```

Pre-allocation:
- Reduces filesystem metadata churn during the segment's life (extent already allocated).
- Provides early warning of disk-full conditions (fails at segment creation, not mid-record).
- Improves write performance (writes overwrite already-allocated blocks rather than triggering extent allocation).

The pre-allocated file appears full to `ls -l` but is mostly zeros (sparse if the FS allows; explicit zero blocks otherwise).

### 7. Segment rollover

When the active segment fills (reaches ~256 MiB), a rollover occurs:

1. The current group commit completes (flushing the last records into the old segment).
2. A new segment file is created.
3. The new segment's header is written and fsync'd.
4. The directory containing segments is fsync'd (so the new file's directory entry is durable).
5. Subsequent records go to the new segment.

The rollover briefly stalls writes (the writer task is busy with the new-segment setup). On NVMe, this is < 5 ms.

### 8. The durability protocol

For a single record, the timeline:

```
T=0 µs:   Record is appended to in-memory buffer.
T=10 µs:  Group-commit window timer started (or extended).
T=110 µs: Window closes; flush triggered.
T=110 µs: pwritev2 with RWF_DSYNC submitted via io_uring.
T=160 µs: Kernel begins write to NVMe.
T=210 µs: NVMe acknowledges write.
T=210 µs: Kernel returns completion to io_uring.
T=215 µs: Brain notifies record's awaiter.
```

Plus or minus device variance. Typical p50 ~150 µs, p99 ~500 µs.

### 9. Failure of the underlying device

If `pwritev2` returns an error (device removed, OOM in the kernel, etc.), Brain:

1. Logs the error.
2. Marks the WAL as "broken"; no further records can be written.
3. Existing in-flight records receive errors.
4. Brain transitions to a degraded state where reads are still possible but writes fail with `WalUnavailable`.

Recovery from this state requires operator intervention (replace storage, check disk, restart Brain).

### 10. The fsync-on-data-only choice

Brain uses `fdatasync` semantics (via `RWF_DSYNC`) rather than full `fsync`. This means:

- Data writes are synchronized.
- File metadata changes (file size, modification time) might not be synchronized.

For the WAL, file size changes are not strictly needed for durability:

- After a crash following a write but before the size update is durable, recovery reads the file expecting the old size.
- The data already written is on disk (the data sync ensured it).
- Recovery will see records past the "logical" file end, validate their CRCs, and process them.

This works because the WAL is append-only and CRC-checked. Brain does not depend on the file size as the truth; recovery depends on CRC-validated records.

### 11. Cross-shard fsync independence

Each shard has its own WAL. Different shards' fsyncs are independent:

- Different file descriptors.
- Different io_uring submissions.
- Different kernel-side queues.

A slow disk write on shard A doesn't delay shard B's fsyncs. This matters for latency tail at scale: per-shard isolation prevents one slow shard from poisoning the cluster.

### 12. Battery-backed write caches and the SSD's role

On enterprise SSDs with FUA (Force Unit Access), `RWF_DSYNC` semantics include flushing to non-volatile media. On consumer SSDs without FUA, the data may sit in a volatile DRAM cache that survives only as long as the device's super-capacitor allows.

For deployments using consumer SSDs, Brain's durability is "best effort" — the kernel reports the write as durable, but a power loss may still lose recently-written records.

Enterprise SSDs with FUA are recommended for production. For development or test, consumer SSDs are fine.

### 13. The reverse case: too-frequent fsync

If the workload has many small writes and the group-commit window doesn't gather enough records to amortize, fsync overhead can dominate. Symptoms:

- High WAL write latency (p99 > 1 ms).
- Low write throughput (much less than the device's write IOPS suggest).

Mitigations:

- Increase the group-commit window (default 100 µs; up to ~500 µs).
- Increase the buffer size (default 64 KB; up to a few MB).
- Coalesce salience updates (already done).

These knobs are exposed in configuration; operators tune as needed.

---

*Continue to [`03_write_path.md`](03_write_path.md) for the full ENCODE write path.*
