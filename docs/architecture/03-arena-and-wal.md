# 03 — Arena and WAL

**Audience:** anyone who needs to reason about *what's actually on
disk*, how Brain stays durable across crashes, and why writes can
ack as fast as they do.

**Goal:** by the end of this chapter you should be able to point
at a 16-byte `MemoryId` and explain (a) which file it lives in,
(b) where in that file, (c) what guarantees its content is intact,
and (d) what happens if the machine loses power right now.

This chapter doesn't cover the HNSW index ([04](04-hnsw-index.md))
or the redb metadata store ([05](05-redb-metadata.md)); both
*derive from* the arena and WAL, and both can be rebuilt from
them. The arena and the WAL together are the durability boundary.

---

## What we're actually building

Each shard owns three durable artefacts on disk:

```
<data_dir>/<shard_id>/
├── shard.uuid          16 bytes — identity, written once
├── arena.bin           mmap'd vector store, one slot per memory
├── wal/                segments containing every state mutation
│   ├── seg-0000000001.wal
│   ├── seg-0000000002.wal
│   └── …
└── metadata.redb       derived from WAL replay (chapter 05)
```

Two of those — arena and WAL — are designed by hand for our access
pattern. `metadata.redb` is a B-tree we don't own (chapter 05).
The HNSW index lives only in RAM; on cold start we rebuild it from
the arena.

(Knowledge-layer files — `entity.hnsw`, `statement.hnsw`,
`*.tantivy/`, `llm_cache.redb` — live in the same directory but
are only populated when a schema is declared. See
[09 — knowledge layer](09-knowledge-layer.md).)

The path constants are centralised in
`crates/brain-storage/src/layout.rs:43`; `ensure_dirs` is the
idempotent `mkdir -p` used at shard spawn.

---

## The arena, byte by byte

`arena.bin` is a flat file consisting of a 4 KiB header followed
by an array of fixed-size *slots*. Total file size is
`4096 + slot_count × 1600` bytes. Each slot holds one memory's
embedding plus its metadata. The whole file is mapped into the
shard's address space with `mmap(MAP_SHARED, PROT_READ | PROT_WRITE)`
(`crates/brain-storage/src/arena/file.rs:603`), so reads are zero-copy
`&[f32]` views straight into the page cache.

### The 4 KiB header

```
+---------+------+-----------------------------+
| offset  | size | field                       |
+---------+------+-----------------------------+
|  0      |   4  | magic                "BARN" |
|  4      |   4  | format_version              |
|  8      |  16  | shard_uuid (UUIDv7)         |
| 24      |   4  | vector_dim       (384 v1)   |
| 28      |   4  | slot_size       (1600 v1)   |
| 32      |   8  | slot_count_capacity         |
| 40      |   8  | slot_count_in_use (advisory)|
| 48      |  16  | embedding_model_fp_active   |
| 64      |   8  | created_at_unix_nanos       |
| 72      |   8  | last_grow_at_unix_nanos     |
| 80      |   4  | header_crc32c (bytes 0..76) |
| 84      | 4012 | reserved (zero)             |
+---------+------+-----------------------------+
```

Constants are pinned in code:
`HEADER_LEN = 4096`, `HEADER_MAGIC = "BARN"`,
`FORMAT_VERSION_V1 = 1`, `VECTOR_DIM_V1 = 384`,
`SLOT_SIZE_V1 = 1600`
(`crates/brain-storage/src/arena/file.rs:44`).

On startup the header is validated end-to-end: magic, format
version, dimension, slot size, then CRC32C over bytes 0..80, then
the shard UUID against the one we expected
(`crates/brain-storage/src/arena/file.rs:340`). Any mismatch is a
hard refusal to open — half-good arenas don't get to serve.

### The 1600-byte slot

```
+----------------------------------+ +--+
|   vector: 384 × f32 = 1536 bytes |  |
|   little-endian, L2-normalized   |  |  1600 bytes
+----------------------------------+  |  total
|   metadata: 64 bytes             |  |
+----------------------------------+ +--+
```

Metadata layout inside the slot
(`crates/brain-storage/src/arena/slot.rs:49`):

| Offset (within slot) | Size | Field |
|---|---|---|
| 1536 | 4 | `slot_version` |
| 1540 | 4 | `flags` |
| 1544 | 16 | `embedding_model_fp_short` |
| 1560 | 8 | `created_at_unix_nanos` |
| 1568 | 8 | `last_modified_at_unix_nanos` |
| 1576 | 4 | `metadata_crc32c` (covers vector + meta[0..36]) |
| 1580 | 20 | reserved |

Flags (`crates/brain-storage/src/arena/slot.rs:76`):

| Bit | Constant | Meaning |
|---|---|---|
| 0 | `OCCUPIED` | Slot holds a live memory |
| 1 | `TOMBSTONED` | Memory is forgotten, awaiting reclaim |
| 2 | `PENDING_WRITE` | Allocation in progress (transient) |
| 3 | `HARD_FORGOTTEN` | Vector was zeroed; informational |

### Why 1600 and not 1664?

The slot is exactly 24 cache lines. The header is 64 cache lines.
Every slot's vector starts cache-line-aligned, and every slot's
metadata starts cache-line-aligned at offset 1536 = 24 × 64. We
considered padding slots to 1664 (26 cache lines) to keep 16 slots
inside a 4 KiB page. We rejected it: at 1M slots the padding costs
~60 MiB for no measurable working-set improvement on the random
access pattern HNSW search has anyway.

### Why 1536 bytes of vector when BGE only needs 1536?

It doesn't need more — 384 × 4 = 1536 — but a future model with
512 dimensions would also fit, leaving the slot layout the same.
The 64 metadata bytes are also fixed. A change to either is a
format-version bump (the header carries one for exactly this
reason).

### MemoryId → slot

`MemoryId` is a 16-byte handle laid out as `(shard_id : u16, slot_id
: u48, slot_version : u32, reserved : u32)`. Going from a `MemoryId`
to its bytes on disk is three operations and no allocations:

```rust
let slot_idx = memory_id.slot();
let off = HEADER_LEN + (slot_idx as usize) * SLOT_SIZE;
let slot_bytes = &arena_mmap[off..off + SLOT_SIZE];
// validate slot.metadata.slot_version == memory_id.version()
```

The `slot_version` check is what makes stale `MemoryId`s safe.
After a forget + reclaim, the slot's version counter bumps; any
`MemoryId` minted before that bump still has the old version and
gets a `NotFound` instead of someone else's memory
(`crates/brain-storage/src/arena/file.rs:470`).

`slot_version` is `u32`. Saturation at `2^32` retires the slot
permanently — `AllocError::SlotRetired`
(`crates/brain-storage/src/arena/allocator.rs:188`). Roughly four
billion lifecycles before a slot is taken out of service forever.

---

## Slot allocation

The allocator (`crates/brain-storage/src/arena/allocator.rs`) is a
simple two-piece structure: a free-list of slot indices that came
back from `FORGET` reclamation, and a `next_fresh` cursor for
never-used slots.

```
alloc():
  if free_list.pop().is_some():
    re-check the slot is still flag-free   (corruption guard)
    use it
  else if next_fresh < capacity:
    use next_fresh; next_fresh += 1
  else:
    return AllocError::Exhausted → trigger arena growth
```

`crates/brain-storage/src/arena/allocator.rs:167`. The corruption
guard exists because the free-list is a runtime structure: if it
ever lists a slot that's also `OCCUPIED` on disk, something else
has gone wrong, and we'd rather error than overwrite. That branch
returns `AllocError::FreeListSlotOccupied` and the operation
fails.

A fresh allocation sets `PENDING_WRITE` immediately so that a
crash between allocation and `OCCUPIED` flip leaves the slot
clearly half-baked. Recovery treats `PENDING_WRITE` slots
appropriately (covered below).

**Rebuild from arena.** On startup we don't have an in-memory
free-list — the allocator is rebuilt by scanning the arena
(`crates/brain-storage/src/arena/allocator.rs:109`): find the
highest used index (occupancy + pending-write + nonzero version),
then list every slot below that high-water mark that isn't
occupied. This recovers slots that were freed before the crash,
including never-used slots that happen to sit below the watermark
because of a sparse historical fill.

---

## Arena growth

The arena is allocated sparse on first open. Default initial
capacity is 1024 slots (~1.6 MiB on disk including header,
`crates/brain-storage/src/arena/file.rs:67`). When the allocator's
`next_fresh` would exceed `capacity`, the shard grows the arena.

`ArenaFile::grow_to`
(`crates/brain-storage/src/arena/file.rs:510`) is a four-step
ceremony:

1. **`fallocate`** the file to the new size (zero-filled extents,
   no actual data writes).
2. **`mremap(MREMAP_MAYMOVE)`** to re-map the bigger file in
   place. The kernel may relocate the mapping; the
   `ArenaFile::base` pointer is updated.
3. **Update the header** (`slot_count_capacity`, `last_grow_at`,
   recompute CRC).
4. **`msync(MS_SYNC)`** the header page so the new capacity is
   durable.

The growth policy doubles capacity (typical), with a configurable
upper bound. Importantly, growth doesn't move existing slots —
`fallocate` extends the tail and `mremap` may or may not move the
mapping virtually, but slot-N's *file offset* is unchanged. That's
what lets us hand out `MemoryId`s with the slot index baked in
forever.

If `mremap` returns `MAP_FAILED` (typically out-of-address-space
on 32-bit, never seen on 64-bit), the grow returns an error and
the in-flight `ENCODE` surfaces `Exhausted`. The allocator stays
intact at the old capacity.

---

## The WAL

The arena is the *current state*. The WAL is *every change that
got us there*. The pair gives us crash-safety: if the arena is
ahead of the WAL, that's impossible (we WAL before we touch the
arena). If the WAL is ahead of the arena, recovery replays.

### Records and segments

The WAL is an ordered sequence of `WalRecord`s
(`crates/brain-storage/src/wal/record.rs`), each with:

```
+--------+-------------+---------+
| header | payload     | footer  |
| ...    | type-       | CRC32C  |
| LSN    | specific    |         |
+--------+-------------+---------+
```

Record types include `Encode`, `Forget`, `Reclaim`, `Consolidate`,
`MigrateEmbedding`, `Link`/`Unlink`, `UpdateSalience` / `UpdateKind`
/ `UpdateContext`, `CheckpointBegin` / `CheckpointEnd`, transaction
markers `TxnBegin` / `TxnCommit` / `TxnAbort`, and a
knowledge-layer envelope (`Knowledge`) for typed entity/statement
records (`crates/brain-storage/src/wal/payload.rs`,
`crates/brain-storage/src/recovery.rs:308`).

Records are grouped into 256 MiB segment files
(`crates/brain-storage/src/lib.rs:61`). Each segment has its own
4 KiB header — magic `"BWAL"`, format version, the same
`shard_uuid` as the arena, and the starting LSN of the segment
(`crates/brain-storage/src/wal/segment.rs:46`). The starting LSN
makes recovery's seek-to-checkpoint cheap: it doesn't have to
scan every segment to find the one containing
`durable_lsn + 1`.

When the active segment fills, a rollover creates the next one.
The rollover briefly stalls the writer task — `Wal::append`
detects the projected size overflow, drops every borrow, awaits
`rollover`, then proceeds
(`crates/brain-storage/src/wal/wal.rs:247`).

### Group commit

The hot path is `Wal::append`
(`crates/brain-storage/src/wal/wal.rs:238`):

1. Validate record size against the segment cap.
2. Pick an LSN (`inner.next_lsn`), atomically.
3. Decide whether to roll over.
4. Hand the record to the `GroupCommitter`
   (`crates/brain-storage/src/wal/group_commit.rs:142`).
5. Get an `AppendHandle`. `.await` it.

`GroupCommitter::start`
(`crates/brain-storage/src/wal/group_commit.rs:117`) spawns one
Glommio task per shard that owns the active segment. That task
runs `committer_loop`
(`crates/brain-storage/src/wal/group_commit.rs:208`):

```
loop:
  await first record on submission channel       (no batch in progress)
  start a 100 µs timer
  drain additional records until either:
    - the timer fires, or
    - the buffered batch reaches max_batch_bytes (60 KiB)
  call segment.flush_durable()    // one write_at + one fdatasync
  ack every pending appender with Ok(lsn)
```

Defaults:
`commit_window = 100 µs`, `max_batch_bytes = 60 KiB`
(`crates/brain-storage/src/wal/group_commit.rs:56`).

The shape of `flush_durable`
(`crates/brain-storage/src/wal/segment.rs:274`):

```rust
let got = self.file.write_at(buf, offset).await?;
self.file.fdatasync().await?;
self.bytes_on_disk += got;
```

Both syscalls go through io_uring courtesy of Glommio's
`BufferedFile`. One `write_at` puts every record from the batch
on disk; one `fdatasync` durabilises them. **The fsync is the
only thing that gates response.** Steps after it (arena write,
metadata commit, HNSW insert, epoch publish — see below) happen
behind the response, post-ack.

### Why `write_at` + `fdatasync` instead of `pwritev2(RWF_DSYNC)`

The combined `pwritev2(RWF_DSYNC)` call is one syscall instead of
two. Glommio's typed `BufferedFile` API doesn't expose
`RWF_DSYNC`, so we use the two-call equivalent. Same durability
guarantee — the kernel completes the write-back, then
`fdatasync` waits for the device to confirm — just one extra
syscall per batch. The batch amortises it.

### Batch sizing in practice

`max_batch_bytes = 60 KiB` (not 64) because typical record sizes
push the batch over a 4 KiB boundary; 60 leaves headroom to fit
one more record before the threshold without overshooting an
aligned write. The 100 µs window matches what NVMe takes to
complete an `fdatasync` (50–200 µs), so a write that arrives in
the middle of a batch waits at most ~100 µs to join, then ~100 µs
for the sync — `~200 µs` p50 for a quiet shard, much lower
amortised on a busy one.

### Group commit failure

Errors are sticky
(`crates/brain-storage/src/wal/group_commit.rs:28`): once a flush
fails, the `GroupCommitter` is "broken" and every subsequent
append, including ones in flight, sees `CommitError::WalBroken`.
The shard surfaces that as `WalUnavailable` to the client and
refuses further writes. Reads still work — they don't go through
the WAL. The fix is process restart (the shard recovers on
restart from whatever was durable up to the failure).

---

## The end-to-end write path for `ENCODE`

Putting the arena and the WAL together, here is exactly what
happens between an `ENCODE` request hitting the shard's executor
and the response leaving the shard.

```
┌──────────────────────────────┐
│ embed text                   │   (in-shard; cache hit or BGE inference)
└──────────────┬───────────────┘
               │
   ┌───────────▼───────────┐
   │ 1. allocator.alloc()  │   set PENDING_WRITE on the slot
   └───────────┬───────────┘
               │
   ┌───────────▼───────────┐
   │ 2. build WAL record   │   Encode payload + headers
   └───────────┬───────────┘
               │
   ┌───────────▼───────────┐
   │ 3. wal.append().await │   ━━━━━━ DURABILITY BARRIER ━━━━━━
   └───────────┬───────────┘          fdatasync returns here
               │
   ┌───────────▼───────────┐
   │ 4. write vector +     │   mmap'd; just a memcpy
   │    metadata to arena  │   set OCCUPIED, clear PENDING_WRITE
   │    refresh slot CRC   │
   └───────────┬───────────┘
               │
   ┌───────────▼───────────┐
   │ 5. redb txn:          │   substrate metadata
   │      MEMORIES.put     │   + idempotency entry
   │      IDEMPOTENCY.put  │
   │    txn.commit()       │
   └───────────┬───────────┘
               │
   ┌───────────▼───────────┐
   │ 6. HNSW insert        │   in-RAM; rebuildable on restart
   └───────────┬───────────┘
               │
   ┌───────────▼───────────┐
   │ 7. publish (epoch)    │   memory becomes visible to readers
   └───────────┬───────────┘
               │
            response
```

A few things to notice:

- **Step 3 is the only thing the client waits for.** The handler
  may emit the response immediately after the WAL ack; steps 4–7
  can happen on the same task before the response is written
  (current implementation), but the durability promise is
  satisfied at step 3.
- **No arena `fsync`.** Steps 4 (vector write) and the slot CRC
  refresh go to the mmap'd page cache. The kernel writes them
  back lazily. If we crash before write-back, recovery replays
  the WAL and re-applies — the arena ends up correct.
- **No HNSW persistence on the hot path.** Step 6 is in-RAM.
  Restart rebuilds it from the arena. See
  [04 — HNSW index](04-hnsw-index.md).
- **`PENDING_WRITE` is set before the WAL record exists.** If we
  crash between step 1 and step 3, the slot is allocated but
  has no WAL record. Recovery sees no `Encode` record for that
  slot and the allocator's `rebuild_from_arena` does not list it
  as occupied — the slot quietly becomes free again on next boot
  (`crates/brain-storage/src/arena/allocator.rs:122`).

### `FORGET` and friends

Other writes follow the same shape. `FORGET` writes a `Forget`
WAL record, sets `TOMBSTONED` on the slot, marks the memory as
forgotten in redb, removes it from the HNSW. Hard-mode `FORGET`
additionally zeroes the vector bytes (and sets `HARD_FORGOTTEN`)
so the data is unrecoverable from the arena.

`LINK` / `UNLINK` are metadata-only: WAL record, redb txn,
publish. No arena traffic.

`CONSOLIDATE` (run by the consolidation worker, chapter 07)
allocates a new slot for the consolidated memory and writes a
`Consolidate` WAL record with `DERIVED_FROM` edges to the
sources. Same shape as `ENCODE` after that.

`MIGRATE_EMBEDDING` (model upgrade) re-embeds an existing
memory's text and overwrites the slot's vector in place, with a
`MigrateEmbedding` WAL record. The `slot_version` doesn't bump —
it's the same memory, new vector.

`RECLAIM` (run by the slot reclamation worker once the tombstone
grace expires) writes a `Reclaim` WAL record bumping
`slot_version`, then clears `OCCUPIED` and `TOMBSTONED` so the
slot can be re-allocated. The grace period (default 7 days) is
what keeps stale `MemoryId`s from accidentally hitting a fresh
allocation: any client still holding the old `MemoryId` gets
`NotFound` because the on-disk `slot_version` no longer matches.

### Transactions

Multi-record transactions (`TxnBegin` / `TxnCommit` / `TxnAbort`)
let a handler group several WAL records under one durability
event. The committer flushes them all in one batch; recovery
treats them atomically. A `TxnBegin` with no matching `TxnCommit`
at end-of-WAL is discarded (covered next).

---

## Recovery

The recovery driver is `brain_storage::recover`
(`crates/brain-storage/src/recovery.rs:203`). It runs at shard
startup, before the executor accepts requests.

```
recover(arena, wal_dir, shard_uuid, metadata_sink):
  durable_lsn = sink.durable_lsn()                  // 0 for a fresh shard
  for record in WalReader::open(wal_dir, shard_uuid):
    if record.lsn <= durable_lsn:   records_skipped += 1; continue
    if record is TxnBegin:           start buffering
    elif record is TxnCommit(id):    apply buffered batch, clear
    elif record is TxnAbort(id):     drop buffered, clear
    elif inside a transaction:       buffer this record
    else:                            apply(arena, sink, record)
  if a transaction is still open at EOF:
    records_discarded += buffered.len()             // dropped
  allocator = SlotAllocator::rebuild_from_arena(arena)
```

The return is a `RecoveryReport`
(`crates/brain-storage/src/recovery.rs:278`) — replayed / skipped /
discarded counts, plus the next LSN to hand out. The shard logs
it (`crates/brain-server/src/shard/mod.rs:769`) so you can see
exactly what woke up.

### `apply()` per record

Each record type maps to one of a handful of arena edits, in
`apply_to_arena`
(`crates/brain-storage/src/recovery.rs:308`):

- `Encode` → write vector, set `OCCUPIED`, stamp version, refresh
  slot CRC.
- `Forget` → set `TOMBSTONED`, stamp `last_modified_at`, refresh
  CRC. (Hard-forget vector zeroing is deferred to the reclamation
  worker.)
- `Reclaim` → clear flags, bump `slot_version`, refresh CRC.
- `Consolidate` → like `Encode`, into a freshly-allocated slot.
- `MigrateEmbedding` → overwrite the slot's vector, no version
  bump.
- `Link` / `Unlink` / `Update*` / `Checkpoint*` / `Txn*` → no
  arena work; the metadata sink handles them.
- `Knowledge` → no substrate arena work; the knowledge sink
  hydrates entity/statement tables independently.

`apply_to_arena` is idempotent: each handler stamps absolute
values rather than deltas. Re-running recovery on the same WAL
twice yields the same arena.

### Where recovery stops

A `WalReader` iteration ends when it hits one of:

- **End of segments.** Clean termination; the last record's LSN
  becomes `next_lsn - 1`.
- **A torn record.** The CRC check inside the reader fails. The
  reader treats this as the truncation point — every record
  before it is committed, the torn record and everything after
  is gone. Tearing is only ever expected at the *end* of the WAL
  (the active segment, the last record before the crash). Torn
  records in the middle of a segment are a corruption signal —
  the reader logs the LSN and refuses to proceed.
- **Out-of-order LSN.** Records should be strictly monotonic. If
  the reader sees a gap or a backwards jump, it stops with a
  corruption error.

The "log is truth" invariant lives here. A *durable* record
(written + fdatasync'd) is replayed and reflected; a *not-yet-
durable* record (in the page cache when the crash happened) is
absent because `fdatasync` never returned. There is no
intermediate state.

### Recovery and partial slot writes

If a crash happens between the WAL fdatasync (step 3 above) and
the arena memcpy (step 4), the arena has a `PENDING_WRITE` slot
with no vector. Recovery replays the `Encode` record, which
*does* write the vector and stamps `OCCUPIED`. The slot ends up
correct.

If the crash happens between arena memcpy and metadata commit
(step 5), the arena's vector is there but redb doesn't know. Same
fix: recovery replays the `Encode` against both the arena and the
metadata sink. The redb sink is idempotent — calling `apply` with
the same LSN twice has no effect after the first.

### Recovery and transactions

A `TxnBegin` followed by `n` records followed by no `TxnCommit`
is treated as `TxnAbort`: the buffered records are discarded
(`crates/brain-storage/src/recovery.rs:273`). The client never
saw an ack — `wal.append().await` for the commit record didn't
return — so the operation never happened from a client's
perspective. The discarded count is reported.

### Recovery is fast on a checkpointed shard

The sink's `durable_lsn` is the LSN through which previous
recoveries (and previous successful checkpoints — see chapter
07's checkpoint worker) already populated redb. Recovery skips
records below that. In practice a healthy shard has a recent
checkpoint, so each restart replays only the records since the
last checkpoint, not the whole WAL.

HNSW rebuild happens *after* recovery and runs in parallel for
each shard — covered in [04 — HNSW index](04-hnsw-index.md).

---

## Failure modes

What can go wrong, and what the operator sees.

**Torn last record.** Expected. Recovery treats it as the
truncation point, logs `records_discarded`, the shard comes up
clean.

**Torn record mid-segment.** Unexpected. Recovery refuses to
proceed; the shard fails to spawn. The operator must investigate —
disk corruption, partial restore from backup, or a bug. There is
no "skip the bad record" mode by default because it could
silently drop committed operations.

**Segment file missing in the middle.** Same handling — refuse
to start. The shard layout is a contiguous sequence; gaps are
not a legitimate state.

**Header CRC mismatch on `arena.bin`.** Refuse to open. The
header is small and we re-msync it on every header-touching
operation; a CRC mismatch implies catastrophic damage.

**`shard.uuid` doesn't match the arena's `shard_uuid` field.**
Refuse to open. The shard's identity is part of every WAL
segment's header too; a mismatch usually means the data
directory was assembled from two shards.

**WAL device fills up.** `fallocate` of the next segment fails
at rollover; the `GroupCommitter` flips to broken; subsequent
writes return `WalUnavailable`. Reads keep working. The operator
needs to free disk and restart.

**Disk briefly slow.** Latency rises, group commit batches grow.
No data is lost; the system stays correct. p99 latency on writes
tracks `fdatasync` latency directly.

**SIGKILL.** Same as power loss. The durability cut is at the
last completed `fdatasync`. Recovery replays from there.

**A second writer accidentally opens the same arena.** Two
processes mmap'ing the same file is a misconfiguration we can't
prevent at this layer; on Linux nothing stops it. The
single-writer-per-shard discipline (see
[08 — Tokio/Glommio boundary](08-tokio-glommio-boundary.md))
relies on operators not running two `brain-server` processes
against the same `data_dir`. If you do, both will silently
corrupt each other.

---

## Configuration & tuning

Defaults that matter:

| Field | Default | Notes |
|---|---|---|
| `shard.arena_capacity_bytes` | `1 GiB` | Initial sparse mmap (slot count = bytes ÷ 1600 less 4 KiB). Grows. |
| `shard.wal_segment_size_bytes` | `256 MiB` | One active segment at a time. Rollover stalls writer briefly. |
| `shard.wal_retention_segments` | `4` | The retention worker (chapter 07) deletes older segments. |
| WAL `commit_window` | `100 µs` | In code, not exposed in TOML yet. |
| WAL `max_batch_bytes` | `60 KiB` | Same. |
| Tombstone grace | 7 days | Reclamation worker waits this long before reusing a forgotten slot. |
| Arena initial capacity | 1024 slots | Used when `arena_capacity_bytes` is not specified; ~1.6 MiB on disk. |

Tuning rules of thumb:

- **Don't shrink `wal_segment_size_bytes` below 64 MiB.** Smaller
  segments mean more rollovers, and rollover briefly stalls the
  writer. The default 256 MiB stalls for ~1 ms on NVMe.
- **`wal_retention_segments = 4` keeps roughly 1 GiB of replay
  log per shard.** That's enough for any practical recovery
  scenario as long as checkpoints are happening. Crank it higher
  if you're disabling checkpoints for some reason (you shouldn't).
- **`arena_capacity_bytes` is initial, not max.** The arena
  grows; pick a starting value that fits your expected one-day
  workload without growing once, then let it grow on its own.
- **Use NVMe with FUA.** Group commit's batched `fdatasync`
  buys nothing if the device lies about durability. Enterprise
  SSDs with power-loss protection are the right hardware here.

---

## Where it lives in the code

| Topic | Path |
|---|---|
| Path layout, `ensure_dirs` | `crates/brain-storage/src/layout.rs` |
| Arena header, mmap, open/grow | `crates/brain-storage/src/arena/file.rs` |
| Slot layout, flags, CRC | `crates/brain-storage/src/arena/slot.rs` |
| Slot allocator, rebuild-from-arena | `crates/brain-storage/src/arena/allocator.rs` |
| `WalRecord` and payloads | `crates/brain-storage/src/wal/record.rs`, `payload.rs` |
| Segment file format, `flush_durable` | `crates/brain-storage/src/wal/segment.rs` |
| Group commit task | `crates/brain-storage/src/wal/group_commit.rs` |
| `Wal::append` (writer entry) | `crates/brain-storage/src/wal/wal.rs` |
| `WalReader` (recovery iteration) | `crates/brain-storage/src/wal/reader.rs` |
| Recovery driver | `crates/brain-storage/src/recovery.rs` |
| Segment size constant | `crates/brain-storage/src/lib.rs:61` |

---

## Further reading

- [01 — System architecture](01-system-architecture.md) for who
  *calls* the WAL and how the per-shard executor is set up.
- [02 — Wire protocol](02-wire-protocol.md) for how a `MemoryId`
  appears on the wire after `ENCODE` returns.
- [05 — redb metadata](05-redb-metadata.md) for what step 5
  (metadata commit) actually does and which tables it touches.
- [07 — Background workers](07-background-workers.md) for
  checkpointing, retention, reclamation, and scrubbing — the
  background processes that keep recovery fast and arenas tidy.
