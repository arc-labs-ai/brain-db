# 18 — Storage and durability

This chapter is the systems-level version of "what happens
when you call `encode`." Specifically: what makes Brain
*durable*, how it survives crashes, and what specific
operating-system primitives it uses to do so.

If you've never thought about durability in databases
before, this is the chapter that lays the foundation. The
next chapters in Part 5 (mmap'd arenas, indexes,
concurrency, sharding) build on this one.

---

## What "durable" actually means

When `encode` returns, the memory is **durable**: a power
loss in the next millisecond will not lose it.

That's a stronger claim than "the data was written." Writing
isn't the same as durability:

- **Buffered write**: the program calls `write()`. Bytes
  land in *the operating system's page cache* (RAM). A
  crash here loses the data.
- **Page-cache write back**: the OS eventually writes the
  page cache to disk. Could be seconds to minutes later.
- **Disk-cache write**: the disk itself often buffers
  recent writes in its own RAM. A drive with no
  power-loss protection can lose its cache on power
  failure.
- **Stable storage**: the bytes have been *physically
  committed* to the storage device's non-volatile media
  and would survive a power loss.

Brain promises that by the time `encode` returns, the
memory has reached *stable storage*. This requires
explicit cooperation with the OS — you have to *ask* for
each level of write to be flushed, and on consumer SSDs
the last level (disk-cache → media) requires either FUA
(Force Unit Access) instructions or a power-loss-protected
device.

> **What's FUA?**
>
> Force Unit Access — a flag on SCSI/NVMe write commands
> that tells the drive "don't lie to me about durability;
> only acknowledge once the bytes are physically on the
> media." Enterprise SSDs implement it correctly; some
> consumer SSDs ignore it (drive cache is volatile but
> the drive reports back fast).
>
> See [Wikipedia: Force Unit Access](https://en.wikipedia.org/wiki/Hard_disk_drive#Disk_cache).

Brain runs the right kernel calls (next sections); a
properly-configured filesystem and enterprise SSD make
those calls actually durable. On a misconfigured deployment
(consumer SSD without power-loss protection, filesystem
mounted with caching options that lie), Brain's durability
guarantee can degrade to "durable on a normal kernel
crash, possibly not durable on a hardware power loss."

This isn't Brain-specific; it's the same caveat for every
database that needs durability. Production deployments
should use enterprise-grade storage.

---

## The crash model

What can go wrong:

| Failure | What happens to memory | What happens to disk |
|---|---|---|
| Process crash (panic, SIGSEGV) | RAM contents lost. | Last fsync'd state intact. |
| OS crash (kernel panic) | All RAM lost. | Last fsync'd state intact. |
| Power loss | All RAM lost. | Depends on disk: enterprise SSDs preserve last fsync; consumer SSDs may lose recent writes if they were in volatile cache. |
| Disk corruption | RAM intact. | Bytes are wrong; need recovery from snapshot. |
| Network partition | RAM intact. | Disk intact, but the client may have to retry or give up. |

Brain's strategy: write everything to disk in an
explicitly-durable way, and structure the disk format so
that *partial* writes (caught mid-operation by a crash) are
detectable and recoverable.

The mechanism: a **write-ahead log**.

---

## What a write-ahead log is

A **write-ahead log** (WAL) is a sequential file that records
every state-changing operation *before* the operation is
applied to the in-memory state.

The pattern is universal in databases:

1. Client asks the database to do something.
2. Database writes a record describing the operation to the
   WAL.
3. Database **fsyncs the WAL** to make the record durable.
4. Database applies the operation to the main data
   structures (which may not themselves be fsync'd yet).
5. Database acknowledges the operation to the client.

After step 5, the client thinks the operation is done. If
the system crashes after step 3 but before steps 4-5
complete, the WAL still has the record. On recovery, the
database replays the WAL — re-applying the operation to
the main data structures — and the state is identical to
what it would have been if the crash hadn't happened.

> **Why "write-ahead"?**
>
> Because the log is written *ahead of* the actual change
> to the main data. The order matters: WAL first, then
> apply. If the order were reversed, a crash between
> "apply" and "log" would leave you with a change in the
> data and no record of it — unrecoverable.
>
> See [Wikipedia: Write-ahead logging](https://en.wikipedia.org/wiki/Write-ahead_logging).

Postgres has a WAL. SQLite has a WAL. MySQL/InnoDB has a
WAL. The pattern is well-trodden.

Brain has one too. Every state-changing operation —
`encode`, `forget`, `link`, salience updates, statement
creation, all of it — gets a WAL record before the actual
mutation is applied to the arena and indexes.

---

## Brain's WAL, concretely

A shard's WAL is a sequence of records grouped into
**segments** — append-only files in
`<data_dir>/<shard_id>/wal/`:

```
wal/
├── seg-0000000001.wal     (256 MB)
├── seg-0000000002.wal     (256 MB)
└── seg-0000000003.wal     (current, growing)
```

Records carry the operation type and its inputs. A typical
record:

```
WalRecord {
    lsn:           42178            (log sequence number)
    record_type:   ENCODE
    timestamp:     2024-09-12T14:31:22.123Z
    payload:       EncodePayload {
                       memory_id, agent_id, context_id,
                       kind, text, vector, edges, …
                   }
    crc32c:        0x8f3a2b1d        (checksum)
}
```

Every record carries a **CRC32C checksum** so the recovery
path can detect torn writes (when a crash happens *while*
a record is being written; some bytes land, others don't).

> **What's CRC32C?**
>
> A 32-bit cyclic redundancy check using the Castagnoli
> polynomial. Modern CPUs have hardware instructions for
> it (Intel `crc32` opcode). It's the standard checksum
> for file-level integrity in ext4, BTRFS, SCTP, and many
> other systems.
>
> See [Wikipedia: Cyclic redundancy check](https://en.wikipedia.org/wiki/Cyclic_redundancy_check).

### fsync and fdatasync

The kernel call that makes a write durable is **fsync**:

```
write(fd, data, len)    # writes to page cache
fsync(fd)               # forces page cache → stable storage
```

`fsync` returns when the OS has flushed all dirty pages of
the file to disk (and, on cooperating hardware, the disk
has flushed its cache to the media).

Brain uses **fdatasync** specifically — a variant that
skips syncing the file's metadata (size, modification time)
when the data write doesn't need them. WAL appends fit this
pattern: the file size update is metadata, but if recovery
sees a record beyond the "logical end" of the file, the
CRC check catches it and treats it as a torn write. We
don't need the metadata to be durable to get the data
durable.

> **fsync vs fdatasync**
>
> `fsync(fd)` syncs the file's data **and** its metadata.
> `fdatasync(fd)` syncs only the data (and metadata that
> affects subsequent reads). For append-heavy workloads,
> fdatasync is roughly the same durability for fewer
> kernel-level operations.
>
> See [fsync(2) man page](https://man7.org/linux/man-pages/man2/fsync.2.html).

---

## Group commit

The naive WAL pattern (one record per fsync) is slow under
load. fsync itself takes ~50–200 µs on a fast NVMe SSD,
which limits throughput to ~5K–20K operations per second
per shard.

Brain uses **group commit**: a single fsync covers
*multiple* WAL records that arrived within a small time
window.

The pattern:

1. Many operations queue their WAL records into a shared
   buffer.
2. Periodically (default every 100 µs) or when the buffer
   reaches a threshold (default 60 KB), a single
   write+fsync flushes everything in the buffer.
3. Every operation in the batch is acknowledged together.

The win: 50 operations sharing one fsync cost ~50 µs each
of buffer-build time + one ~100 µs fsync, instead of 50 ×
100 µs individual fsyncs.

The trade-off: tail latency rises slightly (operations
that arrive just after a batch starts wait the full 100 µs
window). For Brain's workload — many concurrent operations
per shard — this is the right call.

---

## What the arena and metadata files look like

The WAL is the *durability ground truth*. The other files
in a shard — `arena.bin`, `metadata.redb` — are *derived*
from the WAL. They're updated *after* the WAL fsync, in
the background (relatively speaking — same task, just
after the durability barrier).

- **`arena.bin`** holds memory vectors. It's
  memory-mapped (chapter 19). Writes don't fsync the
  arena directly; the kernel writes it back lazily. If
  the system crashes between WAL fsync and arena
  writeback, the WAL still has the record, and recovery
  re-applies it.
- **`metadata.redb`** holds the row data — memory
  metadata, idempotency keys, edges, knowledge-layer
  rows. redb (chapter 05 in the architecture tier) is a
  copy-on-write B-tree with its own internal commit
  story; Brain treats its commit as part of the operation
  but the WAL fsync is what *acknowledges* to the client.

The order of writes is:

```
1. Append a WAL record.
2. fsync the WAL.               ← durability barrier
3. Acknowledge to the client.
4. Apply the change to the arena.
5. Apply the change to the metadata store.
6. Apply the change to the index (HNSW).
7. Salience workers / cascades catch up in the background.
```

Steps 1-2-3 must complete before the client sees an ack.
Steps 4-7 happen behind the ack; if any of them fails or
gets interrupted, recovery re-runs them from the WAL.

This is the "WAL-before-ack" invariant. It's one of seven
core invariants Brain promises (chapter 24).

---

## Recovery

When a shard boots, it runs the **recovery path**:

```
recover():
    open the metadata store
    determine durable_lsn         (the latest LSN safely applied)
    for each WAL segment, starting from durable_lsn + 1:
        for each record:
            verify CRC
            if CRC fails:
                this is the truncation point; stop here
            apply the record to the arena and metadata
    re-index: rebuild the in-RAM HNSW from the arena
    mark the shard ready
```

A few things to notice:

- **Recovery is idempotent.** Re-running it on the same
  WAL produces the same state. Each record's effect
  depends only on the record itself, not on what happened
  during a previous failed recovery.
- **The truncation point is the last good CRC.** A torn
  record marks the end of the durable log; recovery treats
  everything beyond it as never-happened.
- **The HNSW index is rebuilt.** It's RAM-only; no
  persistence; recovery scans the metadata store and
  rebuilds it. Takes seconds for ~10K memories, longer
  for larger shards. Snapshots speed this up (next
  section).

A clean shutdown takes ~milliseconds per shard. A crashed
shard takes proportional time to the WAL since the last
snapshot — typically seconds.

---

## Snapshots

A **snapshot** is a consistent point-in-time copy of the
shard's data files. Brain's snapshot worker can take one
periodically (or on operator demand). What it captures:

- A copy of `arena.bin` (or a reflinked copy on supporting
  filesystems — chapter 19).
- A copy of `metadata.redb` (redb's copy-on-write semantics
  make this near-free).
- The LSN at the time of the snapshot.

Why snapshots matter:

- **Fast cold start.** A shard with a recent snapshot only
  has to replay the WAL records *after* the snapshot LSN,
  not the entire WAL from time zero. Recovery of a
  long-running shard would be very slow without
  snapshots.
- **Disaster recovery.** Off-site copies of snapshots let
  you restore a shard's state after total disk loss. The
  snapshot is the basis for backups.

The snapshot worker runs on a configurable cadence (default
hourly, off by default — operators turn it on by setting
the config flag). The mechanism is fast — a few hundred
milliseconds for the file copies, atomic with respect to
ongoing writes.

---

## "Log is truth"

A subtle but important consequence of the WAL-first
design: **the WAL is the authoritative state**. The arena
and metadata files are *derived* from it. If they ever
disagree, the WAL wins.

This shows up most concretely in disaster scenarios:

- **The arena file is corrupted but the WAL is intact.**
  Recovery replays the WAL, rebuilds the arena, and you're
  fine.
- **The metadata store is corrupted but the WAL is
  intact.** Same — replay rebuilds the metadata.
- **The WAL is corrupted but the arena is intact.** You've
  lost the ability to recover. The arena's contents are
  the latest applied state, but you can't extend it
  safely; you have to restore from snapshot.

This is why Brain takes such care with the WAL specifically
— the per-record CRC, the careful fsync order, the
group-commit batching. If any of those slip, recovery
might not work.

It's also why the trust model (chapter 24) treats the WAL
as the foundation. Everything that derives from it can be
recomputed; the WAL itself cannot.

---

## What happens on SIGTERM vs SIGKILL

Brain handles graceful shutdown (SIGTERM) and hard kill
(SIGKILL) differently:

- **SIGTERM**: graceful. The shard stops accepting new
  requests, drains in-flight ones, fsyncs any pending WAL
  batches, closes the files cleanly. Recovery on next
  boot is fast.
- **SIGKILL** (or power loss): hard stop. The shard's
  process is gone instantly. In-flight WAL writes that
  hadn't yet been fsync'd are lost. Recovery on next boot
  scans the WAL, finds the last good record, treats the
  rest as torn writes, and rebuilds.

The two recovery paths produce the same outcome: the
state reflects every acknowledged operation up to the
crash. Anything that was in flight but not yet
acknowledged is lost — which is correct, because the
client never got an ack and (with idempotency) can retry.

---

## What happens on disk loss

If the storage device dies entirely — no shard files
recoverable — your only option is restoring from a backup
snapshot. The substrate is **fail-stop**: it doesn't
attempt to operate with missing files. The shard refuses to
spawn; the server logs an error; the operator restores from
snapshot and restarts.

For multi-shard deployments, individual shard failures
don't take the whole server down. The healthy shards keep
serving; the dead one is offline until restored.

For deployments that need *high availability* beyond
this — automatic failover, replica reads, cross-region
durability — v1 doesn't ship that. You'd run Brain
alongside an external replication system that copies
WAL segments and snapshots to a secondary location.
Native replication is on the future roadmap.

---

## Recap

- **Durable** = on stable storage; survives a power loss.
- Brain writes a **write-ahead log** record before applying
  every state change, then **fsync**s it.
- **Group commit** batches multiple records into one fsync
  for throughput.
- The arena and metadata are *derived* from the WAL;
  recovery replays the WAL to reconstruct them.
- Per-record **CRC32C** checksums catch torn writes; the
  last good CRC is the truncation point.
- **Snapshots** make recovery of long-running shards fast
  and provide a disaster-recovery anchor.
- The substrate is **fail-stop**: corrupt files cause a
  shard to refuse to spawn rather than serve potentially-
  wrong data.

---

## Where to go next

- **The mmap'd arena, in detail:** [chapter 19](19-mmap-and-arenas.md).
- **The architecture-tier version:**
  [`../architecture/03-arena-and-wal.md`](../architecture/03-arena-and-wal.md).
- **The seven invariants Brain promises:**
  [chapter 24](24-invariants-and-trust.md).
- **Idempotent retries on the client side:**
  [chapter 25](25-determinism-idempotency-replay.md).
- **Operating Brain in production:**
  [`../guides/deployment/`](../guides/deployment/).
