# 02.10 Data Model Failure Modes

This file lists the things that can go wrong at the data-model level — corruption, inconsistency, invalid records — and Brain's response to each. The system-level failure-mode catalog is in [18. Failure Recovery](../18_failure_recovery/00_purpose.md); this file covers the entity-level cases.

## 1. Stale MemoryId references

**Failure mode.** A client holds a `MemoryId` that no longer corresponds to any active memory. The memory was forgotten (and possibly its slot reclaimed for a new memory).

**Detection.** Brain looks up the `MemoryId`'s slot. If:

- The slot is tombstoned with the same version: the original memory was forgotten but slot not yet reclaimed. Return `MemoryNotFound`.
- The slot's version differs from the `MemoryId`'s version: the slot has been reclaimed; the memory is gone. Return `MemoryNotFound`.
- The slot's agent_id differs from the requesting agent: an authorization bug or a stale `MemoryId` from a different agent. Return `Unauthorized`.

**Response.** Return an explicit error rather than silently returning a different memory. The version field exists exactly to prevent silent re-targeting.

**Implication for clients.** Clients should treat `MemoryId` as a value that may go stale. Operations like `RECALL` always return current `MemoryId`s; clients caching `MemoryId`s need a strategy for re-validating them periodically.

## 2. Invalid request_id (idempotency)

**Failure mode.** A client retries an operation with a `RequestId` that has been seen before, but with different parameters than the original.

**Detection.** The idempotency table records the `RequestId`'s original parameters. On a retry, Brain compares the new parameters to the stored ones. If they match, return the original response. If they differ, that's a client bug.

**Response.** Return `IdempotencyConflict` with details of the mismatch. Brain refuses to overwrite the original response with new data.

**Implication for clients.** Use a fresh `RequestId` for each logically-distinct operation. Reusing `RequestId`s only for actual retries (same parameters, same intent).

## 3. Cross-agent reference

**Failure mode.** An operation in agent A's session references a `MemoryId` belonging to agent B.

**Detection.** Each operation validates that the `MemoryId`'s agent matches the session's agent. The validation happens at the routing layer (the agent owns a shard; the memory's shard ID is in the `MemoryId`).

**Response.** `Unauthorized`. Brain does not leak the existence of the cross-agent memory; the error is the same as if the memory didn't exist.

**Implication for clients.** Don't share `MemoryId`s across agents. They are agent-scoped.

## 4. Invalid context reference

**Failure mode.** An operation references a `ContextId` that doesn't exist in the agent's namespace.

**Detection.** Brain validates the context reference against the agent's context table.

**Response.** `InvalidContext`. Different from `Unauthorized` — the context doesn't exist (whereas in the cross-agent case, the memory might exist but isn't accessible).

**Implication for clients.** Use context names rather than `ContextId`s when uncertain; the lazy-creation behavior creates contexts on first use of their names.

## 5. Vector corruption

**Failure mode.** A vector in the arena has been corrupted (cosmic ray, disk error, software bug). The vector no longer represents the memory's text.

**Detection.** Brain doesn't actively verify vectors against text on every read; that would be too expensive. Detection is passive:

- **Norm check on read** — `RECALL` candidates have their norm checked. A vector with norm far from 1.0 (epsilon = 1e-3) is suspicious. The candidate is excluded from results, and the memory is flagged for repair.
- **Periodic background scrub** — the integrity-check worker scans memories periodically, recomputing norms and checking against a per-shard checksum.

**Response.**

- If the text is intact and the embedding model is still available, re-embed and overwrite the corrupted vector.
- If the text is also corrupted (or unavailable), the memory is unrecoverable from the live state. Restore from snapshot if available; otherwise, the memory is lost.

**Implication for operators.** Run regular snapshots. Monitor the integrity-check worker's output.

## 6. Text corruption

**Failure mode.** A memory's text has been corrupted in the metadata store.

**Detection.** The metadata store (redb) checksums its own pages. Page-level corruption is detected by redb. A specific memory's text being corrupted while the page checksum is intact requires both the data and the checksum to be corrupted in a consistent way — extremely rare.

If Brain stores a separate per-memory text checksum (it does, via [BLAKE3](https://github.com/BLAKE3-team/BLAKE3) on the text), it can detect corruption that bypasses the page checksum.

**Response.** Restore from snapshot. If Brain also has a `text_hash` field on the memory and the hash mismatches the text, the memory is flagged corrupted; the text is treated as missing for all operations until repaired.

## 7. Edge corruption

**Failure mode.** An edge points at a non-existent target memory.

**Detection.** Edge traversal during `RECALL`, `PLAN`, or `REASON` may dereference an edge target. If the target's slot is reclaimed (different version), the edge is stale; Brain filters it out lazily.

**Response.** Filter out stale edges silently during traversal. Schedule a background job to clean up the stale edges.

**Implication.** Brain is designed to tolerate stale edges. The cost is a small extra check during traversal; the alternative (eager edge cleanup on every memory's deletion) would be expensive.

## 8. Salience saturation

**Failure mode.** A memory is accessed so many times that its salience saturates at 1.0. After saturation, the access boost has no effect.

**Detection.** This isn't really a failure — it's expected behavior. The salience formula's clamping at 1.0 means a memory's salience tops out.

**Response.** Brain operates correctly. Saturated salience just means "this is highly important"; further accesses don't move the score.

**Implication.** Salience is a relative ranking signal; absolute values matter less than rankings. If many memories saturate at 1.0, the salience signal is weak among them, and other ranking factors (recency, similarity) dominate.

## 9. Salience floor: a memory that won't decay enough

**Failure mode.** A memory's salience is at the floor (default 0.05) and stays there. The agent wants the memory truly forgotten but it lingers.

**Detection.** This is again expected behavior. The floor exists to prevent automatic forgetting via decay alone; the agent must explicitly `FORGET` memories it wants gone.

**Response.** Use `FORGET`. If the agent observes lots of memories at the floor, they're candidates for explicit forgetting based on the agent's policy.

## 10. Embedding model mismatch

**Failure mode.** A query is issued; Brain has memories from a different embedding model. Cross-model results would be noise; Brain filters them out.

**Detection.** The fingerprint comparison happens during `RECALL` (and similarly for `PLAN`, `REASON`).

**Response.** Memories with mismatched fingerprints are excluded from results. The client sees fewer (or no) results until migration completes.

**Implication.** During migration, queries return partial results. Operators should communicate migration status to clients (via `ADMIN_STATS` or external monitoring).

## 11. Concurrent access creating ordering anomalies

**Failure mode.** Two `RECALL`s for the same memory race; both salience updates are applied. Or: a `RECALL` and a `FORGET` race; the read sees the memory but the write committed afterward.

**Detection.** These are not bugs; they are expected concurrency. Brain provides per-shard linearizability ([01.06 Targets](../01_architecture/05_hardware_and_targets.md) §4.4): the order is well-defined, even if it's not the order the client might expect.

**Response.** Brain operates as specified. Anomalies are timing artifacts, not correctness errors.

**Implication.** Clients should not assume read-your-own-write across operations unless they're sequenced through the same connection.

## 12. Schema version mismatch

**Failure mode.** A client (or storage file) is at a different format version than the server. Cannot understand the data.

**Detection.** Format-version checks at load time (storage) and handshake (wire).

**Response.** Refuse to load (storage) or refuse the connection (wire). Provide a clear error indicating the version mismatch and the migration path.

**Implication.** Operators run clients and servers at the same Brain release, and migrate on-disk files with `brainctl migrate` when upgrading.

## 13. Out-of-space conditions

**Failure mode.** The arena is full; no slots available. Or: the WAL fails to extend; no disk space.

**Detection.** Slot allocator returns no available slot; or `fallocate` returns ENOSPC.

**Response.** The encode operation fails with `OutOfStorage`. The error is propagated to the client; the operation is not retried automatically (the client must handle).

**Implication.** Operators must monitor disk usage and capacity.

## 14. Encoding model mismatch with stored vectors

This is a special case of model mismatch but worth calling out: if the model is changed *and* `ADMIN_MIGRATE_EMBEDDINGS` is run, but the migration is interrupted, Brain has a mix of fingerprints. Recovery:

- Resume the migration (it's idempotent).
- Until complete, only the migrated memories are queryable.

A clean abort path exists: if the operator wants to revert the model change, switching back to the old model preserves access to the originally-encoded memories. Memories encoded under the new model are stranded until either (a) re-migrated to the old model, or (b) the new model is restored.

## 15. Validation failures at ingest

**Failure mode.** A client submits an `ENCODE` with invalid parameters: empty text, invalid context, malformed `RequestId`, oversize text, etc.

**Detection.** Validation at the protocol layer ([04. Wire Protocol](../04_wire_protocol/00_purpose.md) §11).

**Response.** Return a specific error: `InvalidArgument` with details indicating which parameter was wrong.

**Implication.** Clients should validate before submitting. Clients should validate before submitting; the server also validates server-side.

