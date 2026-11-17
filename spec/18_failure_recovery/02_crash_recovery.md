# 18.02 Crash Recovery

How Brain recovers from crashes — the WAL-based recovery procedure.

## 1. The crash scenario

A "crash" is any abrupt termination:

- Brain process panics.
- OS kills the process (OOM killer).
- Power loss.
- Hardware reset.

In all cases, in-memory state is lost; on-disk state is what persists.

## 2. The recovery contract

After crash + restart:

- All committed data is recovered.
- No half-applied operations are visible.
- No data appears that wasn't committed.

In other words: crash recovery is "as if the crash never happened", up to and including the latest WAL-fsynced commit.

## 3. The on-disk state

What persists across crash:

```
data/<shard>/
├── arena.bin                # Vectors (mmap'd, but on disk)
├── wal/
│   ├── segment-N.bin        # WAL records (durable per pwritev2 RWF_DSYNC)
│   └── ...
├── metadata.redb            # Metadata (redb's transactions are durable)
└── hnsw_snapshot.bin        # Optional periodic snapshot
```

The arena, WAL, and metadata are all durable. The HNSW snapshot may or may not exist (it's an optimization).

## 4. The recovery procedure

```
1. Open the arena file.
2. Open the metadata store.
3. Read the latest checkpoint marker; identify durable_lsn.
4. List WAL segments; identify the start of unprocessed records.
5. Replay each WAL record:
   a. Verify CRC.
   b. Apply to metadata if not already applied (idempotent via LSN).
   c. Apply to arena if not already applied.
6. After all records replayed:
   a. The metadata is now caught up.
   b. The arena is consistent.
   c. The next_lsn is set to last_replayed_lsn + 1.
7. Build the HNSW (from snapshot + WAL diff, or fresh from metadata).
8. Start workers and request handlers.
```

Brain is now ready to serve.

## 5. The checkpoint

A checkpoint marker is in the metadata store:

```rust
struct Checkpoint {
    durable_lsn: LSN,             // Highest contiguous prefix of WAL records applied to metadata
    timestamp: SystemTime,
    arena_size_bytes: u64,
    memory_count: u64,
}
```

The checkpoint is updated periodically (every minute or every N records).

Recovery starts from `durable_lsn + 1` and skips everything at or below `durable_lsn` unconditionally. The word that carries the weight is **contiguous**. A shard's writes commit one at a time but not in LSN order — several requests are in flight at once, so a write that appended earlier may commit later. A `durable_lsn` defined as the maximum committed LSN would therefore sit above a record whose metadata transaction had not run; recovery would skip it, and an acknowledged write would vanish on restart with nothing in any log to say so.

Defined as the contiguous prefix, the skip is sound: a record at or below `durable_lsn` either committed, or mutates no metadata at all (a checkpoint or a subscribe-event record), or belongs to a failed write whose records were abandoned beneath a later real commit. See [`../08_storage/04_recovery.md`](../08_storage/04_recovery.md) §3 and [`../14_concurrency/02_writer_model.md`](../14_concurrency/02_writer_model.md) §13a.

## 6. The WAL replay

```rust
async fn replay_wal(state: &mut ShardState, from_lsn: LSN) -> Result<LSN> {
    let mut current_lsn = from_lsn;
    let segments = list_wal_segments(&state.dir)?;
    
    for segment in segments {
        if segment.last_lsn < from_lsn { continue; }
        
        let mut reader = WalReader::open(&segment.path)?;
        while let Some(record) = reader.next_record()? {
            if record.lsn < from_lsn { continue; }
            verify_crc(&record)?;
            apply_record(state, record)?;
            current_lsn = record.lsn;
        }
    }
    
    Ok(current_lsn)
}
```

Each WAL record is processed in order. Records are applied to the metadata and arena.

## 7. The CRC verification

Each WAL record has a CRC32C. During replay, the CRC is verified:

- If valid: process the record.
- If invalid: log critical; depending on policy, halt or skip.

A bad CRC indicates corruption in the WAL — typically a hardware issue. Brain's default is to halt; recovery requires investigation and possibly a snapshot restore.

## 8. The "torn write" handling

A torn write is a partially-written record (e.g., crash mid-pwrite).

The wire framing detects torn writes:

- Each record has a length prefix.
- If the actual data ends short, the record is truncated.
- Brain ignores truncated records at the tail of the WAL.

Truncated records mean the operation didn't fully commit. Their data isn't visible. Subsequent ENCODEs / etc. are correct.

## 9. The replay performance

Replay speed depends on record types:

- ENCODE: arena write (~5 µs each), metadata write (~10 µs each). ~50 µs total.
- FORGET: tombstone update (~10 µs).
- Other: similar.

For 100K records: ~5-10 seconds. For 1M records: ~50-100 seconds.

The checkpoint cadence keeps the replay window bounded. Typical replay: 100K-1M records (1 hour's worth).

## 10. The HNSW reconstruction

Two paths:

### Path A: from snapshot + WAL diff

If a snapshot exists:

```
1. Load HNSW from snapshot file.
2. Apply WAL records since the snapshot.
3. Now caught up.
```

Fast (~10s of seconds).

### Path B: fresh from metadata

Without a snapshot:

```
1. Build HNSW from scratch.
2. For each active memory in metadata: add to HNSW.
```

Slower (~1-10 minutes for 1M memories). But simpler; no snapshot to maintain.

Brain prefers Path A when a snapshot is available; falls back to Path B otherwise.

## 11. The startup time bound

Recovery startup time:

| Memory count | With snapshot | Without snapshot |
|---|---|---|
| 100K | ~5 sec | ~10 sec |
| 1M | ~10-30 sec | ~1-2 min |
| 10M | ~30 sec - 2 min | ~5-15 min |

For latency-sensitive deployments, take regular HNSW snapshots.

## 12. The post-replay verification

After replay:

- The next_lsn matches the last replayed record + 1.
- The arena's expected slot count matches metadata.
- The HNSW node count matches active memory count.

Mismatches indicate corruption or a bug. Brain logs and may refuse to come up.

## 13. The "WAL gap" detection

If a record is missing (gap in LSN sequence):

```
Records ... 1000, 1001, 1003, 1004 ...
          missing 1002
```

This indicates:
- WAL retention deleted a needed record (bug).
- Disk corruption.
- Manual file manipulation.

Brain detects gaps via LSN continuity check. Refuses to come up; restoration from backup needed.

## 14. The "no committed data lost"

Assuming no corruption:

- All operations that received a success response are recovered.
- Operations that didn't get a success response may or may not be (the WAL might or might not have been fsynced, and the metadata commit might or might not have run).
- Operations that received an error are not applied.

Brain's contract: an operation that got "success" is durable. An operation that got "error" is not applied. Operations in between (no response yet) are ambiguous; the application should retry.

The success response is sent after the metadata commit, not merely after the WAL fsync, so "success" means the record is fsync'd *and* the row is committed. That is what makes the first bullet hold without qualification — and the contiguous durable watermark is what keeps it holding across a restart, since a watermark that could run ahead of an uncommitted record would let recovery skip a write that had already answered "success".

The "error" bullet is the mirror image. A write can fail after its WAL records are durable (a metadata commit failure, an index failure, a dropped future). Those records stay in the log, but the write's reserved LSN range is abandoned, so a later real commit closes the contiguous prefix across them and recovery skips them. If the failure was the last thing before the crash, recovery replays them instead and the apply re-derives the same refusal; see [`../08_storage/04_recovery.md`](../08_storage/04_recovery.md) §5a.

## 15. The retry-after-crash semantics

When a client retries an operation after Brain crashed:

- Brain is recovering or restored.
- The client's RequestId is in the idempotency table (from before crash, if applied).
- The retry hits the idempotency cache; returns the original response.

So clients don't see duplicates after recovery, as long as the WAL captured the original.

## 16. The "incomplete recovery"

If recovery encounters errors that prevent full success:

- Brain logs and refuses to come up.
- An operator decides:
  - Fix and retry.
  - Restore from backup.
  - Recover partial state (advanced; requires manual procedures).

Brain doesn't auto-degrade — it prefers fail-stop to silent partial recovery.

## 17. What recovery reports

**There is no `brain_recovery_*` metric family.** Nothing counts restarts,
times a replay, or records when the last one happened, and unlike the families
in [17.03](../17_observability/03_alerts.md) these are not declared deferred
either — a rule or dashboard written against them would simply never fire.
Recovery reports itself in two places instead.

**One log line per shard, at `info`**, emitted after `recover()` returns:

```text
WAL recovery complete
  shard_id, records_replayed, records_skipped, records_discarded,
  next_lsn, tail_segment_seq, tail_offset
```

That is the per-recovery record: `records_replayed` is what the spec above
describes, `records_discarded` is what the cut dropped, and the tail pair is
where the log resumes. Timing is the gap between this line and the shard's
spawn line, not a field.

**Two gauges on `/metrics`** describe the WAL afterwards rather than the
recovery itself: `brain_wal_size_bytes` and `brain_wal_segments`, per shard.
Bounding the replay window is what they are for — a WAL that keeps growing
across restarts is the thing an operator can act on.

One gap worth knowing: when boot takes the loss-free cut, `recover()` returns
a `hole_discard` report carrying the first discarded LSN, the bytes dropped and
the count of well-formed records found past the hole. The shard's log line does
not include those fields, so today the extent is available to a caller of the
library and not to an operator reading logs.

## 18. The "warm restart" optimization (future)

For planned restarts (operator-initiated):

- Brain can pre-emptively trigger a checkpoint.
- WAL replay window is minimal.
- Recovery is fast.

Brain does this on graceful shutdown. Crash recovery has the full WAL window to replay.

## 19. The recovery testing

In CI:

- Tests crash at random points; verify recovery is correct.
- Compare pre-crash state to post-recovery state.
- Verify no data loss for committed operations.

Brain's CI runs hundreds of crash scenarios per release.

---

*Continue to [`03_corruption_recovery.md`](03_corruption_recovery.md) for corruption recovery.*
