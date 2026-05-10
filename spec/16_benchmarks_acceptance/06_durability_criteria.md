# 16.06 Durability Criteria

The criteria Brain v1 must meet for data durability and consistency.

## 1. The core promise

Once an operation receives a success response, its effect is durable:

- Survives substrate crash.
- Survives OS crash.
- Survives power loss.
- Recoverable upon restart.

This is the foundational property. Many criteria below derive from it.

## 2. Test: WAL durability

For each ENCODE that succeeds:

```
1. Client sends ENCODE.
2. Substrate appends WAL with RWF_DSYNC.
3. Substrate sends success.
4. (Crash here.)
5. After restart: the memory is present.
```

Tested: kill the substrate immediately after success. Verify on restart, the memory exists.

Run 1000 iterations; expect 100% success.

## 3. Test: Group commit durability

When ENCODE responses are batched:

```
1. Multiple clients send ENCODEs concurrently.
2. Substrate batches them; performs one fsync.
3. All clients get success.
4. (Crash here.)
5. After restart: all memories are present.
```

Tested: 100 concurrent ENCODEs; kill mid-batch. All that received success are durable.

## 4. Test: Crash before fsync

If crash happens between WAL write and fsync:

```
1. Operation in progress.
2. WAL append succeeded but no fsync yet.
3. (Crash.)
4. Client got no response or got error.
5. After restart: the operation may or may not be present.
```

This is acceptable: the client got no success, so it must retry.

Tested: verify that no client got "success" for an operation that wasn't durable.

## 5. Test: Atomicity

For each operation:
- Either fully applies, or doesn't apply at all.
- No partial state.

Tested: kill mid-operation; verify post-restart state is one or the other, not in between.

## 6. Test: Idempotency

For a duplicated request (same RequestId):
- Returns the original result.
- Doesn't create duplicate state.

Tested: send same ENCODE twice. Verify only one memory exists; both responses match.

## 7. Test: Read-after-write

After a successful ENCODE:
- A subsequent read sees the memory.
- This holds even immediately after.

Tested: ENCODE; immediately RECALL. Verify the encoded memory appears.

## 8. Test: Read-after-tombstone

After a successful FORGET:
- A subsequent read does not see the memory.

Tested: FORGET; immediately RECALL. Verify the memory is absent.

## 9. Test: Recovery completeness

After restart from any crash:
- All committed operations are present.
- No half-committed state.

Tested: apply 10K operations; kill at random points; restart; verify state matches expected.

## 10. Test: Recovery idempotency

If recovery is interrupted (crash during recovery):
- Re-running recovery is safe.
- Final state is correct.

Tested: kill the substrate during WAL replay; restart; verify normal operation resumes.

## 11. Test: WAL retention safety

The WAL retention worker:
- Never deletes records that haven't been checkpointed.
- Verified by invariant: all retained WAL has LSN > last_durable_lsn for that data.

Tested: trigger retention while operations are in flight; verify no data loss.

## 12. Test: Snapshot consistency

A snapshot represents a consistent point-in-time state:
- All operations before the snapshot point are present.
- No operations after are present.
- No partial states.

Tested: snapshot, then verify the snapshot's contents match a consistent moment.

## 13. Test: Backup-restore round-trip

A backup, restored to a fresh substrate:
- Has all the data the original had.
- Behaves the same as the original.

Tested: backup substrate with 100K memories; restore; query both for the same cues; verify identical results.

## 14. Test: Edge durability

Edges are as durable as memories:
- LINK persists across crash.
- UNLINK persists across crash.
- No "missing" edges.

Tested: LINK, kill, restart, verify edge present.

## 15. Test: Audit-log durability

The audit log:
- Each entry is fsynced.
- Survives crash.
- Hash chain remains valid after restart.

Tested: verify post-restart hash chain integrity.

## 16. Test: Tombstone durability

A FORGET:
- Marks memory as tombstoned in the WAL.
- Tombstone status persists across crash.

Tested: FORGET, kill, restart, verify memory still tombstoned.

## 17. Test: Slot reclamation safety

Slot reclamation:
- Only reclaims slots whose tombstone grace period has passed.
- Bumps slot version (to detect stale references).
- Persists the new state.

Tested: tombstone, advance time past grace, reclaim, restart, verify slot is in the free list.

## 18. Test: Concurrent operations

Many concurrent operations:
- All that succeed are durable.
- No race conditions cause data loss.

Tested: 1000 concurrent ENCODEs; verify all are present; verify count.

## 19. Test: Long-running stability

Continuous load for 48 hours:
- No memory leaks.
- No data corruption.
- All committed data is durable.

Tested: continuous load + periodic crashes + verifications.

## 20. The combined "no data loss" certification

Brain v1 is certified if:

```
Across all the tests above:
  - No data loss for committed operations.
  - No corruption of existing data.
  - No state machine bugs that produce inconsistent state.
```

Run the tests in CI on every release candidate. All must pass.

---

*Continue to [`07_benchmark_methodology.md`](07_benchmark_methodology.md) for methodology.*
