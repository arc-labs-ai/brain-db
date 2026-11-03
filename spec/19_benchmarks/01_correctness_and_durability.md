# 19.01 Correctness and Durability

> **TL;DR.** Correctness criteria (wire-protocol, semantic, recovery, idempotency) plus durability criteria (zero data loss on committed writes, RPO=0 / RTO per-shard, CRC + WAL invariants). Both gates are MUST for v1.0 release.

## Correctness Criteria

Brain behaves as specified. This section enumerates correctness requirements that MUST hold.


## 1. Wire-protocol correctness

**MUST**: every frame conforms to the protocol spec ([04. Wire Protocol](../04_wire_protocol/00_purpose.md)).

- Magic bytes "BRN0" present.
- Version byte matches.
- Header CRC32C valid.
- Payload CRC32C valid (when included).
- Length fields match actual payload sizes.

Tests:
- Round-trip every opcode through encode → decode → re-encode; expect equality.
- Malformed frames (bad CRC, wrong magic, length mismatch) are rejected with the correct error code.
- Fuzz testing: 10⁶ random byte sequences as input; the process doesn't crash; only valid frames are accepted.

## 2. ENCODE correctness

**MUST**: ENCODE creates a memory satisfying:

- The vector matches `embed(text, model_version)` (deterministic for a given model).
- The metadata fields match the request (agent, session, salience, kind, etc.).
- A new MemoryId is generated and returned.
- The memory is queryable in subsequent RECALLs.

Tests:
- Encode 1000 memories. Verify each is retrievable by exact-match (RECALL with the original text returns the memory with similarity ≈ 1.0).

## 3. RECALL correctness

**MUST**: RECALL returns memories ranked by relevance.

- The top-1 result is the closest in cosine similarity (modulo HNSW's approximation).
- Filters (agent, session, kind, salience, age) are honored.
- The result count ≤ K (the requested limit).
- Tombstoned memories don't appear unless `include_tombstoned=true`.

Tests:
- Encode 10K memories with known similarity structure; RECALL with a controlled cue. Verify top-K matches expectations within HNSW's recall bound.

## 4. PLAN correctness

**MUST**: PLAN returns a sequence of memories forming a chain.

- Edges in the chain are FOLLOWED_BY (or other temporally meaningful types).
- Memory order respects edge direction.
- The starting memory matches the request.

Tests:
- Construct a known graph; PLAN from a known starting point; verify the returned chain is correct.

## 5. REASON correctness

**MUST**: REASON returns memories along reasoning paths.

- Edge types (CAUSED, SUPPORTS, etc.) are respected.
- Multi-hop paths are explored within the depth limit.
- Cycles don't cause infinite loops.

Tests:
- Construct a small known graph; REASON returns the expected paths.

## 6. FORGET correctness

**MUST**: FORGET marks memories as tombstoned.

- Soft FORGET: memory is invisible to subsequent RECALLs but recoverable via UNFORGET.
- Hard FORGET: memory's vector and text are zeroed; not recoverable.
- The slot is reclaimable after the grace period.

Tests:
- Soft-FORGET, then RECALL: memory not returned. UNFORGET. RECALL: returned again.
- Hard-FORGET; verify vector is zero in arena.

## 7. LINK / UNLINK correctness

**MUST**: edges are created and removed correctly.

- LINK creates an edge between two memories.
- Bidirectional edges (when applicable) are stored both directions.
- UNLINK removes the edge.
- Edge types are respected (CAUSED, FOLLOWED_BY, etc.).

Tests:
- LINK m1 → m2 with type CAUSED. Query edges from m1: includes the edge.
- UNLINK. Query: edge gone.

## 8. Idempotency correctness

**MUST**: a repeated request with the same RequestId returns the same result.

- ENCODE with same RequestId returns the same MemoryId.
- Within the idempotency TTL (24h default), the cached response is served.
- After the TTL: a new operation; new MemoryId.

Tests:
- ENCODE with RequestId X. Returns MemoryId Y.
- ENCODE with RequestId X again (within TTL). Returns Y (not a new memory).
- Verify only one memory was actually created.

## 9. Transaction correctness (TXN_*)

**MUST**: transactions are atomic.

- All operations in a transaction commit together or none commit.
- Reads within a transaction are consistent.
- Aborted transactions leave no trace.

Tests:
- TXN_BEGIN. ENCODE within. ABORT. The encoded memory is not visible.
- TXN_BEGIN. ENCODE. COMMIT. Memory is visible.

## 10. Filter correctness

**MUST**: filters in RECALL / PLAN / REASON are honored.

- Space ID filter: only that space's memories.
- Session filter: only that session's memories.
- Kind filter: only that kind.
- Salience filter: only memories ≥ threshold.
- Time filter: only memories within window.

Tests:
- Encode memories across different agents. RECALL with space filter. Verify only the requested space's memories appear.

## 11. Edge-traversal correctness

**MUST**: graph traversals follow edges correctly.

- Only edges of requested types are traversed.
- Direction is honored (outgoing vs incoming).
- The traversal terminates within depth bound.

Tests: graph fixtures with known structure; verify traversal results match.

## 12. Tombstone correctness

**MUST**: tombstoned memories are correctly handled.

- Visibility: not returned unless explicitly requested.
- Slot reuse: only after grace period and tombstone reclamation.
- Edges referencing tombstoned memories: don't cause errors.

Tests:
- FORGET m. PLAN through a chain that includes m. Verify m is excluded.
- After grace period: slot is reusable.

## 13. Slot version correctness

**MUST**: stale MemoryIds (referring to reclaimed slots) return NotFound.

Tests:
- ENCODE m, get MemoryId M1.
- Hard FORGET m with `force_reclaim_now`.
- ENCODE another memory; it might land in m's slot.
- RECALL by the new memory's ID: works.
- RECALL by M1: returns NotFound (slot version mismatch).

## 14. Audit-log correctness

**MUST**: every state-mutating operation is audit-logged.

- Operation type, timestamp, actor, parameters, result.
- Hash chain integrity.

Tests:
- Perform 100 operations; verify each is in the audit log; verify hash chain.

## 15. Recovery correctness

**MUST**: after crash + recovery:

- All committed operations are durable.
- No half-committed state.
- All invariants hold.

Tests:
- Apply load, kill the process, restart. Verify final state matches expectation.
- Repeat 1000× at random kill points.

## 16. Configuration correctness

**MUST**: all configuration values are honored.

- Memory limits, retention windows, worker intervals — all do what they say.

Tests: change config, verify behavior changes accordingly.

## 17. Error-code correctness

**MUST**: errors are returned with the correct code.

- NotFound for missing data.
- PermissionDenied for unauthorized.
- InvalidArgument for malformed.
- Conflict for idempotency mismatch.
- Etc.

Tests: trigger each error condition; verify the correct code.

## 18. Schema versioning correctness

**MUST**: schema changes don't break existing data.

- New fields default appropriately.
- Old data reads correctly with new code.
- Migration (when needed) is correct.

Tests: load v1 data with v1.x code; verify operations work.

## 19. Determinism (where claimed)

**MUST**: where Brain claims determinism (e.g., for the same input, same output):

- Embeddings are deterministic for a given model version.
- Pure-function operations (like merging filter sets) are deterministic.

Tests: compute the same operation 100×; verify equal results.

## 20. The "no surprises" principle

**MUST**: no observable behavior outside what's specified.

- No undocumented side effects.
- No hidden caches that break consistency.
- No mode where errors are returned but operations still partially apply.

Tests: review the spec; for each statement, write a test verifying it. The full coverage is the union.

---

## Durability Criteria

The criteria Brain v1 must meet for data durability and consistency.

## 1. The core promise

Once an operation receives a success response, its effect is durable:

- Survives Brain process crash.
- Survives OS crash.
- Survives power loss.
- Recoverable upon restart.

This is the foundational property. Many criteria below derive from it.

## 2. Test: WAL durability

For each ENCODE that succeeds:

```
1. Client sends ENCODE.
2. Brain appends WAL with RWF_DSYNC.
3. Brain sends success.
4. (Crash here.)
5. After restart: the memory is present.
```

Tested: kill Brain immediately after success. Verify on restart, the memory exists.

Run 1000 iterations; expect 100% success.

## 3. Test: Group commit durability

When ENCODE responses are batched:

```
1. Multiple clients send ENCODEs concurrently.
2. Brain batches them; performs one fsync.
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

Tested: kill Brain during WAL replay; restart; verify normal operation resumes.

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

A backup, restored to a fresh Brain node:
- Has all the data the original had.
- Behaves the same as the original.

Tested: backup a node with 100K memories; restore; query both for the same cues; verify identical results.

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

*Continue to [`02_performance_targets.md`](02_performance_targets.md) for performance targets.*
