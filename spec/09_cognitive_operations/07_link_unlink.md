# 09.07 LINK and UNLINK

LINK creates an edge between two memories. UNLINK removes one.

## 1. LINK semantic contract

```
LINK(source, target, kind, weight, metadata, request_id) → LinkResponse
```

The substrate:

1. Validates that source and target exist.
2. Inserts the edge into `edges_out` and `edges_in`.
3. Updates edge counts on both endpoints.
4. Writes a WAL record.

After LINK, the edge is visible to PLAN, REASON, and direct edge-listing operations.

## 2. The arguments

### source

The MemoryId at the source of the edge.

### target

The MemoryId at the target of the edge.

### kind

One of the eight edge kinds:

- `CAUSED` — source led to target (causal precedence).
- `FOLLOWED_BY` — source then target (temporal sequence).
- `DERIVED_FROM` — target was derived from source.
- `SIMILAR_TO` — semantic similarity (often auto-derived).
- `CONTRADICTS` — they oppose each other.
- `SUPPORTS` — source supports target's claim.
- `REFERENCES` — source mentions target.
- `PART_OF` — source is part of target.

### weight

f32 in [0, 1] (or [-1, 1] for some kinds like CONTRADICTS where negative makes sense). Default 1.0.

Higher weight = higher confidence in the relationship.

### metadata

Optional small key-values for the edge. Stored verbatim. For things like edge annotations.

### request_id

Required. Idempotency.

## 3. The response

```rust
struct LinkResponse {
    edge_id: EdgeId,             // Stable identifier
    source: MemoryId,
    target: MemoryId,
    kind: EdgeKind,
    weight: f32,
    created_at: u64,
}
```

Note: in v1, `edge_id` is computed deterministically as `(source, kind, target)` rather than being an independent ID. Two edges with the same (source, kind, target) collide; the second LINK updates the first.

## 4. UNLINK semantic contract

```
UNLINK(source, target, kind, request_id) → UnlinkResponse
```

The substrate:

1. Removes the edge from `edges_out` and `edges_in`.
2. Decrements edge counts.
3. Writes a WAL record.

## 5. UNLINK arguments

Same identifying triple as LINK: source, target, kind. The triple uniquely identifies the edge.

```rust
struct UnlinkResponse {
    removed: bool,         // True if edge existed and was removed
    source: MemoryId,
    target: MemoryId,
    kind: EdgeKind,
}
```

If the edge doesn't exist, `removed: false` and no error. UNLINK is idempotent — re-unlinking a non-existent edge is a no-op.

## 6. Edge uniqueness

Each (source, kind, target) triple has at most one edge. Re-LINK overwrites:

```
brain.link(A, B, CAUSED, weight=0.5)    // creates
brain.link(A, B, CAUSED, weight=0.8)    // updates weight to 0.8
```

For applications wanting multiple edges of the same kind, use different kinds (e.g., `REFERENCES_v1`, `REFERENCES_v2`) or external versioning.

## 7. Edge direction

Edges are directed. `LINK(A, B, CAUSED)` is different from `LINK(B, A, CAUSED)`:

- The first says A caused B.
- The second says B caused A.

Some edge kinds have implicit reverse semantics (e.g., A CAUSED B implies B was caused-by A), but the storage is directed. PLAN and REASON consider both directions during traversal.

For SIMILAR_TO (which is symmetric), the convention is to LINK in one direction (typically lower-ID → higher-ID). The substrate doesn't enforce this; it's an agent convention.

## 8. Edge weight semantics

The weight is a hint:

- 1.0: strong / certain.
- 0.5: moderate.
- 0.1: weak.

Used in:
- PLAN's path scoring.
- REASON's evidence scoring.
- Maintenance heuristics (low-weight edges may be pruned over time; not in v1).

The weight is opaque to the substrate beyond these uses. Agents are free to use it as they see fit.

## 9. Edge-creation patterns

### Inline at ENCODE

```
brain.encode(text, edges=[
    EdgeSpec(target=parent_id, kind=DERIVED_FROM),
    EdgeSpec(target=topic_id, kind=PART_OF),
])
```

Up to 64 edges in one ENCODE. Atomic with the encode.

### Post-encode LINK

```
memory_id = brain.encode(text)
brain.link(memory_id, related_id, kind=SIMILAR_TO)
```

Two operations. The LINK can be at any time after both memories exist.

The inline form is preferred when edges are known at encode time. Post-encode LINK is for edges discovered later (e.g., after analyzing the memory).

## 10. Failure modes

### MemoryNotFound

Source or target doesn't exist (or is tombstoned). Error.

### InvalidKind

Kind isn't one of the 8 enumerated values. Error.

### InvalidWeight

Weight is outside the allowed range. Error.

### TooManyEdges

The source has reached the max-edges-per-memory limit (default 10K soft, 100K hard). Soft: warning + creation. Hard: error.

### NotOwned

Either memory belongs to a different agent. Error.

### CrossAgent

Source and target belong to different agents. Error — edges are agent-scoped.

## 11. Edge counts and observability

Each memory has denormalized edge counts:

- `edges_out_count`: edges where this memory is source.
- `edges_in_count`: edges where this memory is target.

Updated on every LINK / UNLINK. Useful for:
- Quickly answering "how connected is this memory?"
- Identifying highly-linked hubs.
- Dashboards.

The counts may temporarily drift due to crash-recovery edge cases. Periodic maintenance reconciles.

## 12. Auto-derived edges

Some edges are created by the substrate, not the agent:

- `DERIVED_FROM`: from Consolidated memories to their Episodic sources.
- `SIMILAR_TO` (optional, off by default): between memories with cosine > 0.9.

These have `origin: AutoDerived` to distinguish from agent-created.

UNLINK can remove auto-derived edges. The maintenance worker may recreate them on its next pass (e.g., if the same condition still applies).

## 13. Latency

LINK / UNLINK latency:

- p50: ~1-2 ms.
- p99: ~5-10 ms.

Mostly the WAL fsync. Bulk LINK (many edges in one ENCODE) is more efficient.

## 14. Throughput

Per shard: ~5K-10K LINK/UNLINKs per second. Limited by the writer task's group commit.

For high-edge-volume agents (e.g., building a knowledge graph), batching via inline-ENCODE-edges is the throughput path.

## 15. Edge metadata semantics

The `metadata` field on edges is for agent annotations:

```
brain.link(A, B, REFERENCES, metadata={
    "page": "42",
    "passage": "the quick brown fox",
})
```

These are stored in the edge value. Available when the edge is read back. Not indexed; the substrate doesn't query on edge metadata.

## 16. The "edge versioning" question

We considered making edges versioned (so the history of relationships is preserved). Rejected:

- Storage cost grows.
- Most agents don't need history.
- Agents that do can encode "edge versions" as separate edges with different kinds.

In v1, edges are last-write-wins. Updating an edge's weight overwrites the previous value.

## 17. The "transaction-bracketed" LINK pattern

For consistency, multiple LINKs can be in a single transaction:

```
txn = brain.txn_begin()
brain.link(A, B, CAUSED, txn=txn)
brain.link(B, C, CAUSED, txn=txn)
brain.txn_commit(txn)
```

Either both succeed or neither. Useful when a graph fragment must appear atomically.

Detailed in [`08_transactions.md`](08_transactions.md).

## 18. The "delete-then-relink" pattern

To change an edge's weight, you can either:

1. LINK again (overwrites): `brain.link(A, B, CAUSED, weight=0.7)`.
2. UNLINK then LINK (more explicit but two ops).

The first is recommended.

## 19. The "link or no-op" idempotency

LINK with the same (source, kind, target) and same RequestId:

- First call: creates the edge.
- Retry with same RequestId: replays original response.
- Manual re-LINK with different RequestId but same triple: overwrites (creates if not exists).

The third case is common for "ensure this edge exists" patterns. The agent doesn't need to check first.

## 20. The "edges as first-class memories" question

Should edges be queryable like memories? E.g., "find all CAUSED edges with weight > 0.8".

In v1, edges are not first-class queryables. They're attributes of memories. Listing edges from a memory is supported (via direct edge enumeration); cross-cutting edge queries aren't.

For agents that need this, they iterate memories and inspect edges in the agent layer. Future enhancement: an edge-query primitive.

---

*Continue to [`08_transactions.md`](08_transactions.md) for transactional brackets.*
