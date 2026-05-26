# 08.04 Crash Recovery

The procedure for bringing a shard back to a consistent state after a crash. The "log is truth" invariant ([`00_purpose.md`](00_purpose.md) §4) makes recovery straightforward: replay the WAL.

## 1. Recovery goals

After recovery, the shard's state satisfies:

- All WAL records that were durably written before the crash are reflected in the in-memory state, arena, metadata, and HNSW indexes.
- All WAL records that weren't durably written are absent from all derived state (as if they never happened).
- The shard is ready to accept new operations.

## 2. The high-level procedure

```
1. Open the metadata store (redb).
2. Read the most recent checkpoint marker.
3. Open WAL segments from the checkpoint's durable_lsn forward.
4. Replay records in LSN order:
   a. For each record, validate CRC.
   b. If CRC fails, this is the truncation point — stop here.
   c. If CRC succeeds, apply the record's effect.
5. Verify integrity (slot versions match metadata, arena norms reasonable).
6. Restore the HNSW indexes (memory, entity, statement) per [09.06 §2 + §6](../09_indexing/06_persistence.md). The memory HNSW tries the most-recent snapshot first and replays only the WAL tail past `taken_at_lsn`; on any snapshot failure, falls through to a full rebuild from the arena. Entity / statement HNSW reseed from durable source-of-truth (entity vectors persisted at write time; statement embed queue).
7. Open the WAL for new appends.
8. Mark the shard ready.
```

Steps 1–7 typically take seconds to tens of seconds for a fresh-checkpointed shard.

## 3. Identifying the start point

Brain starts replay from the `durable_lsn` of the most recent valid checkpoint. The checkpoint is in the metadata store ([`05_checkpointing.md`](05_checkpointing.md)) and includes:

- `durable_lsn` — all records up to and including this LSN are reflected in the arena and metadata.
- `arena_capacity_at_checkpoint` — for cross-validation.

If no checkpoint exists (a fresh shard or one whose checkpoint is corrupted), recovery starts from LSN 1 (the very beginning of the WAL).

## 4. Reading WAL segments

Brain enumerates `wal/*.wal` segments. The segment containing `durable_lsn + 1` is identified by its starting LSN (in the segment header).

For each segment, recovery reads sequentially:

```rust
let mut segment = open_wal_segment(path);
let header = read_segment_header(&mut segment)?;
validate_segment_header(&header, expected_shard_uuid)?;

let mut current_lsn = header.starting_lsn;
loop {
    let record_header = match read_record_header(&mut segment) {
        Ok(h) => h,
        Err(EOF) => break,  // End of segment
        Err(_) => return Err(...),
    };

    // Validate
    if record_header.lsn != current_lsn {
        // Out-of-order LSN: corruption
        return Err(...);
    }

    let payload = read_record_payload(&mut segment, record_header.payload_length)?;
    let footer = read_record_footer(&mut segment)?;

    let computed_crc = crc32c(&record_header) ^ crc32c(&payload);
    if computed_crc != footer.payload_crc32c {
        // CRC mismatch: assume truncation
        log::info!("WAL truncation detected at LSN {}", current_lsn);
        return Ok(current_lsn - 1);  // Last valid LSN
    }

    apply_record(record_header, payload)?;
    current_lsn += 1;
}
```

## 5. Applying records

For each valid record, recovery applies its effect:

| Record type | Recovery action |
|---|---|
| ENCODE | Allocate slot if not already (look up MemoryId), write vector to arena, insert metadata |
| FORGET | Mark slot tombstoned, update metadata, set forgot_at |
| LINK | Insert edge into metadata's edge table |
| UNLINK | Delete edge from metadata |
| UPDATE_SALIENCE | Update memory's salience in metadata |
| RECLAIM | Mark slot as free, increment version |
| CONSOLIDATE | Allocate slot for new memory, write vector and metadata, add DERIVED_FROM edges |
| UPDATE_KIND | Update kind field in metadata |
| UPDATE_CONTEXT | Update context_id in metadata |
| MIGRATE_EMBEDDING | Update slot's vector and fingerprint |
| TXN_BEGIN | Note the txn_id; queue subsequent records |
| TXN_COMMIT | Apply queued records atomically |
| TXN_ABORT | Discard queued records |
| CHECKPOINT_BEGIN | Note the checkpoint started |
| CHECKPOINT_END | Update durable_lsn marker |

Each apply is idempotent — if recovery is re-run on the same WAL, the result is the same. This is important because:

- Recovery may itself crash and be retried.
- Brain may detect inconsistency and re-run recovery on a subset.

Idempotency is achieved by checking the current state before applying each record:

```rust
fn apply_encode(record: EncodeRecord) {
    if metadata.get_memory(record.memory_id).is_some() {
        return;  // Already applied
    }
    // ... otherwise apply
}
```

## 6. Transactional record handling

When recovery encounters a TXN_BEGIN, it buffers subsequent records until TXN_COMMIT or TXN_ABORT:

```
state = Normal
buffer = []

for record in records:
    if record.is_txn_begin:
        state = InTxn(record.txn_id)
        buffer = []
    elif record.is_txn_commit and matches(state):
        apply(buffer)
        state = Normal
    elif record.is_txn_abort and matches(state):
        buffer = []
        state = Normal
    elif state == InTxn:
        buffer.append(record)
    else:
        apply(record)

# At end of WAL:
if state == InTxn:
    log::info!("Discarding partial transaction at end of WAL")
    # buffer is discarded
```

A partial transaction at the end of the WAL is treated as TXN_ABORT — the operation didn't complete, so Brain does not apply it.

## 7. Verifying integrity

After replay completes, Brain optionally runs integrity checks:

- Each slot's metadata `slot_version` matches the metadata store's record.
- Each occupied slot has a non-zero vector with norm in the expected range.
- Each metadata-store memory entry references a valid arena slot.
- Each edge references valid (or now-tombstoned) source and target slots.

These checks are configurable; default is "fast checks only" (slot version and metadata consistency). The full norm-and-vector check is opt-in for paranoid environments.

If a check fails, Brain logs the inconsistency. Depending on severity:

- Minor: log and continue (e.g., a salience field that didn't get its update applied; recovery will re-apply on next checkpoint).
- Major: refuse to start (e.g., a slot's metadata is internally inconsistent; the operator must investigate).

## 8. Restoring the HNSW indexes

The three HNSW indexes (memory, statement, entity) are derived state. See [09.06 Index Persistence & Recovery](../09_indexing/06_persistence.md) for the per-index recovery model. Summary:

- **Memory HNSW** — try snapshot-load first (`SharedHnsw::load_snapshot` against the most-recent checkpoint directory). On success, replay any arena entries whose `encoded_at_lsn > snapshot.taken_at_lsn` into the pending buffer (so writes between the snapshot and the crash are not lost). On any snapshot failure (missing, CRC / version / shard_uuid mismatch), fall through to a **full rebuild** from the arena: iterate occupied, non-tombstoned arena slots and insert each `(MemoryId, vector)`. The fallback path matches the original v1 behavior.
- **Entity HNSW** — rebuild from `ENTITIES_TABLE`. Each row's vector is read from `ENTITY_VECTORS_TABLE` (persisted at write time, [09.06 §6](../09_indexing/06_persistence.md)); rows without a stored vector re-embed `canonical_name` as the fallback.
- **Statement HNSW** — seed the embed queue with every live statement; the per-shard `StatementEmbedWorker` repopulates the index in the background.

Memory and entity restores complete before the shard is marked ready (step 8); the statement restore proceeds asynchronously after ready.

For a 1M-memory shard with a fresh snapshot: load is dominated by the on-disk graph read (~1–5 seconds); WAL-tail replay is bounded by the snapshot cadence. The full-rebuild fallback takes ~30 seconds (single-threaded) or ~5 seconds (parallel). Each shard's indexes are owned by that shard's executor; multiple shards restore concurrently.

**No silent corruption.** A snapshot whose header CRC, footer hash, file-level BLAKE3, or shard_uuid fails verification is refused with a warn-level log; the shard then runs the full-rebuild fallback rather than loading a possibly-wrong graph (CLAUDE invariant 7).

## 9. Recovery time

For a fresh-checkpointed shard:

- WAL replay: 100K records/sec → 1M records in 10 sec.
- HNSW rebuild: 1M memories in 5–30 sec depending on parallelization.

Total: 15–40 sec for a 1M-memory shard.

For a shard that hasn't checkpointed in a long time, recovery time scales with the volume of un-checkpointed records. Operators should ensure regular checkpoints (every 10–30 minutes by default) to bound recovery time.

## 10. Recovery failures

### 10.1 Segment file missing

If a segment file is missing in the middle of the segment sequence (e.g., segment 5 is missing while segment 6 exists), recovery refuses to start. The operator must investigate (filesystem corruption, accidental deletion, restore from backup).

### 10.2 Segment header invalid

If a segment's header magic, format version, or shard UUID is wrong, recovery refuses to start.

### 10.3 Mid-segment corruption

If a record's CRC fails in the middle of a segment (not at the end), this is unusual — typically only the last record of the WAL is truncated. Mid-segment corruption suggests:

- Disk corruption.
- A bug.
- Restoration from a partial backup.

Brain logs the LSN of the corrupted record and refuses to proceed. Operators must investigate.

A future enhancement would be a recovery mode that skips the corrupted record and continues — but this risks silently dropping operations, so Brain does not enable it by default.

### 10.4 Metadata store corruption

redb has its own corruption detection. If redb fails to open the metadata store, recovery cannot proceed. The operator restores from backup.

### 10.5 Arena corruption

The arena's header CRC is verified at load time. Mismatch refuses startup.

Slot-level corruption is detected lazily (on access or scrubbing) — Brain doesn't full-scan the arena at startup (too expensive).

## 11. Recovery and snapshot restore

Restoring from a snapshot is a separate path from crash recovery, but the two interact:

1. Operator restores arena.bin, metadata.redb, and the relevant WAL segments from snapshot.
2. Brain starts up, finds the restored files, and runs recovery.
3. Recovery replays WAL records that were taken in the snapshot but possibly not yet reflected in arena/metadata.

The snapshot mechanism ensures the snapshot represents a consistent point-in-time view. Recovery is the same as a normal crash recovery in this case.

## 12. Recovery progress reporting

Brain exposes recovery progress via:

- A startup log line per checkpoint encountered, per WAL segment opened, per N records replayed.
- A separate admin endpoint (`ADMIN_RECOVERY_STATUS`) that returns current state during recovery.

For very long recoveries (multi-shard or slow disks), this lets operators monitor progress and estimate completion.

## 13. Post-recovery state

When recovery completes, Brain is in a state equivalent to one that:

- Was up and running normally.
- Had received exactly the operations whose WAL records were durably written.

The shard is ready to accept new operations. The next LSN is `last_replayed_lsn + 1`. The arena, metadata, and HNSW are consistent with the WAL.

---

*Continue to [`05_checkpointing.md`](05_checkpointing.md) for checkpoints.*
