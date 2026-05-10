# 07.10 Metadata Store Failure Modes

What can go wrong with the metadata store and how the substrate responds.

## 1. redb file corruption

**Failure mode.** The metadata.redb file is corrupted (bad bytes from a hardware fault, partial write during crash, etc.).

**Detection.** redb checks page checksums; corruption is detected on read. The substrate gets a `redb::Error::Corrupted`.

**Response.**
- The substrate logs the error and refuses to start (or marks the shard offline if running).
- An operator must restore from backup or rebuild from WAL.

**Operator action.** Restore the latest known-good snapshot ([05.10 Snapshots](../05_storage_arena_wal/10_snapshots.md)). After restore, replay WAL records to bring the metadata current.

## 2. Failed commit

**Failure mode.** A write transaction's `commit()` returns an error (disk full, permission lost, hardware error).

**Detection.** redb returns the error.

**Response.**
- The substrate aborts the in-progress operation.
- The WAL record was already written (it's the durability barrier); the metadata commit failure means the operation is partially-applied (WAL durable, redb not).
- The substrate retries the commit a few times.
- If retries fail, the substrate marks the shard degraded and stops accepting writes.

**Operator action.** Investigate disk health. Free space if full. Restart the shard, which will re-apply the WAL record on recovery.

## 3. Disk full

**Failure mode.** The disk has no free space; redb commits fail.

**Detection.** Commit returns `IoError(NoSpace)`.

**Response.**
- The substrate stops accepting writes.
- Reads continue to work.
- An alert is raised.

**Operator action.** Free disk space (delete old WAL segments, prune snapshots, expand the volume). After space is available, the shard resumes writes.

## 4. Inconsistency between WAL and metadata

**Failure mode.** After a crash, recovery finds that the metadata doesn't match what the WAL says it should.

**Possible causes:**
- redb commit succeeded but the substrate crashed before acknowledging.
- A bug in the recovery code.

**Detection.** Recovery validates: every WAL record after the metadata's checkpoint LSN should successfully apply. If applying a record finds the change is already present (idempotent re-apply), that's fine. If it finds a conflicting state, that's a bug.

**Response.**
- Idempotent re-apply: the recovery proceeds.
- Conflict: recovery logs the bug, attempts to resolve (typically by trusting the WAL), and proceeds. The substrate raises an alert.

**Operator action.** Report the bug. The substrate is usually still functional after a recovery anomaly.

## 5. Failed read

**Failure mode.** A read returns an error (disk read fault, redb internal error).

**Detection.** redb returns an error from `get()` or `range()`.

**Response.**
- The request handler returns an error to the client.
- The substrate logs the failure with details.
- Repeated failures suggest a hardware problem; the shard may be marked unhealthy.

**Operator action.** Investigate disk. May need to restore from backup if the corruption is widespread.

## 6. Schema version mismatch

**Failure mode.** The metadata store's table format version is newer or older than the substrate expects.

**Detection.** On open, the substrate reads the format version and compares to its expected version.

**Response.**
- If the file is older: run the registered migrations to bring it up to current.
- If the file is newer: the substrate refuses to open (the substrate is too old).

**Operator action.** For older files: nothing; migrations run automatically. For newer files: upgrade the substrate.

## 7. Migration failure

**Failure mode.** A schema migration fails partway through (e.g., disk error during a migration that rewrites all rows).

**Detection.** The migration's transaction commit fails.

**Response.**
- The migration's transaction aborts; the database is in the pre-migration state.
- The substrate logs the error and refuses to start.
- The migration can be retried; partial migrations don't leave the database in a half-state.

**Operator action.** Address the underlying error (disk space, etc.) and restart. The migration will retry.

## 8. Concurrent write failure (impossible by design)

**Failure mode.** Two writer tasks try to commit on the same shard.

**Detection.** redb's API ensures at most one write transaction at a time; second `begin_write()` blocks.

**Response.**
- This shouldn't happen due to single-writer-per-shard discipline.
- If it does (a bug), the second writer waits.
- An assertion in the writer task catches this case in debug builds.

**Operator action.** Report the bug.

## 9. Long-running read holds resources

**Failure mode.** A long-running read transaction (a misbehaving SUBSCRIBE client, a stuck maintenance worker) holds the redb snapshot, preventing page reclamation.

**Detection.** Disk usage grows beyond expected; the substrate exposes a metric `read_txn_oldest_age`.

**Response.**
- The substrate kills read transactions older than the configured max (default 1 hour).
- An alert is raised when transactions exceed the warning threshold (default 30 min).

**Operator action.** Investigate which client or worker is holding the long transaction. May indicate a stuck process.

## 10. Idempotency table grows unbounded

**Failure mode.** The TTL pruning worker fails or stalls; the idempotency table grows beyond expected size.

**Detection.** Table size metric exceeds threshold.

**Response.** Manual or scheduled retry of the pruning worker.

**Operator action.** Investigate why the worker stalled. Manually trigger pruning if needed.

## 11. Edge orphan

**Failure mode.** An edge references a memory that no longer exists (the memory was deleted but the edge wasn't).

**Detection.** Edge listing finds the orphan; verification queries flag it.

**Response.** A maintenance worker periodically scrubs orphan edges:
- For each edge in `edges_out`, verify both endpoints exist.
- If an endpoint is missing, delete the edge.

**Operator action.** None; the worker handles it.

## 12. Counter drift

**Failure mode.** The denormalized counters (edges_out_count, edges_in_count, memory_count) drift from the actual counts.

**Detection.** Periodic reconciliation by maintenance workers.

**Response.** Workers recompute the actual count and update the denormalized field.

**Operator action.** None.

## 13. Stale slot version

**Failure mode.** A MemoryId's slot_version doesn't match the slot's current version (the slot was reclaimed without the MemoryId being cleaned up elsewhere).

**Detection.** When following a MemoryId reference, the substrate compares its slot_version to the slot's current version.

**Response.**
- The reference is treated as dangling; the operation fails or returns "not found".
- A maintenance worker scrubs stale references in the HNSW.

**Operator action.** None; the substrate handles it.

## 14. Write transaction starvation

**Failure mode.** The writer task can't make progress because every commit hits a disk error or because cooperative yields don't return control.

**Detection.** The writer's `last_commit_at` doesn't advance; the WAL hasn't been written either (since they're paired).

**Response.**
- Latency rises sharply.
- Health checks mark the shard unhealthy.

**Operator action.** Investigate disk health, OS load, the writer task itself.

## 15. The "metadata-only" recovery

**Failure mode.** The arena is intact but the metadata store is corrupted/lost.

**Detection.** Arena exists, metadata.redb is missing or corrupt.

**Response.**
- Recovery rebuilds the metadata from the WAL.
- The arena is the source of vectors; the WAL is the source of metadata operations.
- Rebuild iterates the WAL from the start (or from the last known-good metadata snapshot).

**Operator action.** Trigger metadata rebuild via `ADMIN_RECOVER_METADATA`. The shard is offline during rebuild.

## 16. The "WAL-only" recovery

**Failure mode.** The metadata is intact but the WAL is partial/corrupt.

**Detection.** WAL records can't be read past a certain LSN.

**Response.**
- Recovery uses the metadata as it stands at the last consistent LSN.
- Records past the corruption are lost; this is a partial data loss event.

**Operator action.** Restore from backup if the data loss is unacceptable.

## 17. Total shard loss

**Failure mode.** All shard files (arena, WAL, metadata) are lost (e.g., disk failure with no replication).

**Detection.** Files don't exist on startup.

**Response.**
- The shard can't be opened.
- If the deployment uses replication or snapshots, the operator restores.
- Otherwise: data is lost.

**Operator action.** Restore from snapshot. Brain doesn't have built-in replication in v1.

## 18. The error-reporting discipline

For every failure mode above, the substrate:

- Logs the failure with structured fields (shard, operation, error code).
- Increments a counter metric.
- Raises an event for severity > Warning.

Operators monitor these signals; alerts fire on severity > Error.

---

*Continue to [`11_open_questions.md`](11_open_questions.md) for unresolved questions.*
