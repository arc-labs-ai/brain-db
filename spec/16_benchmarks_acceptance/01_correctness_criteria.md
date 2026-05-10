# 16.01 Correctness Criteria

The substrate behaves as specified. This file enumerates correctness requirements that MUST hold.

## 1. Wire-protocol correctness

**MUST**: every frame conforms to the protocol spec ([03. Wire Protocol](../03_wire_protocol/)).

- Magic bytes "BRN0" present.
- Version byte matches.
- Header CRC32C valid.
- Payload CRC32C valid (when included).
- Length fields match actual payload sizes.

Tests:
- Round-trip every opcode through encode → decode → re-encode; expect equality.
- Malformed frames (bad CRC, wrong magic, length mismatch) are rejected with the correct error code.
- Fuzz testing: 10⁶ random byte sequences as input; substrate doesn't crash; only valid frames are accepted.

## 2. ENCODE correctness

**MUST**: ENCODE creates a memory satisfying:

- The vector matches `embed(text, model_version)` (deterministic for a given model).
- The metadata fields match the request (agent, context, salience, kind, etc.).
- A new MemoryId is generated and returned.
- The memory is queryable in subsequent RECALLs.

Tests:
- Encode 1000 memories. Verify each is retrievable by exact-match (RECALL with the original text returns the memory with similarity ≈ 1.0).

## 3. RECALL correctness

**MUST**: RECALL returns memories ranked by relevance.

- The top-1 result is the closest in cosine similarity (modulo HNSW's approximation).
- Filters (agent, context, kind, salience, age) are honored.
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

- Agent ID filter: only that agent's memories.
- Context filter: only that context's memories.
- Kind filter: only that kind.
- Salience filter: only memories ≥ threshold.
- Time filter: only memories within window.

Tests:
- Encode memories across different agents. RECALL with agent filter. Verify only the requested agent's memories appear.

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
- Apply load, kill substrate, restart. Verify final state matches expectation.
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

**MUST**: where the substrate claims determinism (e.g., for the same input, same output):

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

*Continue to [`02_latency_targets.md`](02_latency_targets.md) for latency targets.*
