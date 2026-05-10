# 09.01 Semantics Overview

The big-picture semantics of Brain's cognitive operations.

## 1. The agent-substrate contract

An agent talks to Brain over a connection. Each operation is a request-response interaction (or a long-lived stream for SUBSCRIBE).

The agent:
- Sends requests with text-level inputs (cue text, memory text, etc.) and metadata.
- Receives responses with stable identifiers and structured results.

The substrate:
- Embeds text into vectors as needed.
- Stores, indexes, and retrieves.
- Returns deterministic, well-defined results.

The contract is at the level of **what** the agent wants, not **how** the substrate accomplishes it.

## 2. Identity guarantees

Every memory has a stable, unique `MemoryId`:

- Returned at ENCODE.
- Stable across the memory's lifetime (until reclaimed after FORGET + grace).
- Persists across restarts.
- Globally unique within the substrate (with high probability via UUIDv7 + slot version).

Agents can store MemoryIds and refer to them in future operations.

## 3. Time and ordering

Each memory has a `created_at` timestamp (set by the substrate, not the agent). Within a shard, memory creation times are monotonic — later-created memories have later timestamps.

Across shards, timestamps are local; they're not synchronized via NTP or similar. Cross-shard time comparison can be off by milliseconds.

## 4. Embedding determinism

Given the same text and the same model, the substrate produces the same vector. So:

- ENCODE("hello") → vector V₁.
- ENCODE("hello") → vector V₁ (same).
- RECALL("hello") finds memories with vector V₁ exactly.

The substrate doesn't deliberately introduce randomness. Embedding is deterministic.

When the model is upgraded, vectors change. Memory metadata records the model fingerprint; cross-model queries are excluded by default.

## 5. The "what's similar?" question

RECALL answers: "what memories' vectors are close to this cue's vector?"

The substrate uses HNSW for ANN search. Returns are approximate; recall@K typically 95%+ for typical parameters.

Score = 1 - cosine_distance, range [-1, 1]. Higher = more similar.

For agents that need exact (no recall loss) results, the substrate falls back to brute force for small shards. For larger shards, exact search isn't supported (would be too slow).

## 6. The "what's connected?" question

PLAN and REASON answer relationship questions:

- PLAN: "from state A, can I reach goal B following these edge types?"
- REASON: "what supports / contradicts query Q?"

These traverse the metadata's edge graph. Edges are explicit (created via LINK) or auto-derived (e.g., DERIVED_FROM from consolidation).

## 7. The "delete" story

FORGET marks a memory as inactive:

- The memory disappears from RECALL results.
- The memory's MemoryId can no longer be used in queries (it returns "not found").
- The vector and text are eventually reclaimed.

Soft FORGET: a 7-day grace period before reclamation. Allows recovery if the FORGET was a mistake.

Hard FORGET: immediate zeroing of vector and text. Reclamation still happens after grace, but the data is gone immediately.

## 8. The "edges" story

Edges are explicit relationships:

- LINK creates an edge.
- UNLINK removes one.
- Edges have a kind (CAUSED, FOLLOWED_BY, etc.) and a weight.

Edges enable graph queries. PLAN and REASON traverse them.

Some edges are auto-created:
- DERIVED_FROM: between Consolidated memories and their Episodic sources.
- SIMILAR_TO (optional): between memories with very similar vectors.

## 9. The "transaction" story

TXN_BEGIN / TXN_COMMIT bracket multiple operations atomically:

```
brain.txn_begin(txn_id)
brain.encode("first thing", txn=txn_id)
brain.encode("second thing", txn=txn_id)
brain.link(first_id, EdgeKind::FOLLOWED_BY, second_id, txn=txn_id)
brain.txn_commit(txn_id)
```

If TXN_COMMIT succeeds, all three operations are durable. If it fails (or TXN_ABORT is called), none are.

Transactions are single-shard. Cross-shard atomicity isn't supported.

## 10. The consistency model

Reads see writes that have been published. Publication happens periodically (every ~10 ms typically).

Read-after-write: an explicit flag tells the substrate to wait until the latest writes are published before serving the read.

This is **eventual consistency by default**, with **on-demand strong consistency**.

## 11. The "best-effort" caveat

Some operations are best-effort:

- Salience updates (decay, access boosts) are batched and applied eventually.
- Edge counts (denormalized) may temporarily drift from actual counts.
- Statistics (memory_count, etc.) are updated periodically.

These don't affect correctness — exact counts are recomputable. They affect what an agent observes if it reads them between updates.

## 12. The error model

Operations fail with structured errors:

- `InvalidRequest` — the request is malformed.
- `NotFound` — referenced memory or context doesn't exist.
- `QuotaExceeded` — agent limits exceeded.
- `Unauthorized` — credentials don't allow this operation.
- `Conflict` — idempotency mismatch.
- `Overloaded` — substrate is shedding load.
- `InternalError` — substrate-side bug or unrecoverable state.

Each error has a stable code, a human-readable message, and a `retryable` flag.

## 13. The streaming model

SUBSCRIBE is the only streaming primitive. It opens a long-lived connection that delivers change events:

- The client provides filters.
- The substrate streams events that match.
- Events are delivered in WAL order (per shard).

The client controls flow with windowing and acknowledgments.

## 14. The schema-stability promise

Brain's data model is stable within a major version:

- Field names and types don't change.
- New fields can be added (with defaults).
- New error codes can be added.

Major version bumps may include breaking changes; documented in [02.09 Schema Evolution](../02_data_model/09_schema_evolution.md).

## 15. The "extension hooks" story

Brain doesn't have a plugin system. Agents that want custom logic do it on their side:

- Agents do their own filtering of RECALL results.
- Agents create their own auto-edges by listening to SUBSCRIBE.
- Agents trigger consolidation by running their own logic and ENCODE-ing the summary.

This keeps Brain simple. Custom logic lives in the agent layer.

## 16. The "vendor lock-in" story

Brain stores text and vectors. Both are recoverable:

- Text is in the metadata store (and exportable via SUBSCRIBE or admin tools).
- Vectors can be re-derived from the text if the embedder is available.

So agents using Brain aren't locked in. The data is plain-text and portable.

The MemoryIds, contexts, and edges are Brain-specific, but they map directly to the underlying memories. An export-import flow can preserve them in a different system.

---

*Continue to [`02_encode.md`](02_encode.md) for ENCODE details.*
