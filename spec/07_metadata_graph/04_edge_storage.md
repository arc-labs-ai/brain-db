# 07.04 Edge Storage

Edges are the relational layer of Brain's data model. This file specifies how they're stored and indexed.

## 1. Edge model recap

From [02.06 Edges](../02_data_model/06_edges.md):

- An edge connects a source memory to a target memory.
- Each edge has a kind (one of 8 enumerated types).
- Each edge has a weight (f32 in [0, 1]).
- Edges are directed but always have an "implied reverse" semantics (a CAUSED edge implies the target was caused-by the source).

## 2. Two indexes for two directions

The substrate maintains two index tables:

- `edges_out: (source, kind, target) → EdgeData`
- `edges_in: (target, kind, source) → EdgeData`

Same data, two indexes. Forward queries use `edges_out`; reverse queries use `edges_in`.

The duplication doubles edge storage but enables both directions to be answered with a single B-tree range scan.

## 3. The composite key

Both tables use 3-field composite keys:

```
(MemoryId source, EdgeKind kind, MemoryId target)
```

Encoding: little-endian concatenation of the three fields. redb sorts lexicographically; the encoding makes that order match logical order:

- All edges from source S come before edges from source S+1.
- Within source S, all edges of kind K come before kind K+1.
- Within source S, kind K, edges sorted by target.

This means range queries are tight:

- "All edges from S" → `(S, 0, 0)..(S+1, 0, 0)`.
- "All edges from S of kind K" → `(S, K, 0)..(S, K+1, 0)`.

## 4. The EdgeData value

```rust
struct EdgeData {
    weight: f32,                  // 4 bytes
    origin: u8,                   // 1 byte (Explicit / AutoDerived)
    derived_by: u8,               // 1 byte (which worker created it; e.g., consolidation)
    created_at: u64,              // 8 bytes
    annotation: Option<String>,   // variable; rare
}
```

Typical edge value: ~14 bytes. With redb overhead, ~30 bytes per edge.

## 5. Edge insertion (LINK)

```rust
fn link(txn: &mut WriteTxn, edge: Edge) -> Result<()> {
    let key_out = (edge.source, edge.kind, edge.target);
    let key_in = (edge.target, edge.kind, edge.source);
    let value = EdgeData { weight: edge.weight, ... };

    let edges_out = txn.open_table(EDGES_OUT)?;
    let edges_in = txn.open_table(EDGES_IN)?;

    edges_out.insert(&key_out, &value)?;
    edges_in.insert(&key_in, &value)?;

    // Update edge counts in memories table
    update_count(txn, edge.source, "edges_out_count", +1)?;
    update_count(txn, edge.target, "edges_in_count", +1)?;

    Ok(())
}
```

A single transaction handles both index updates and the count updates. Atomic.

## 6. Edge removal (UNLINK)

```rust
fn unlink(txn: &mut WriteTxn, edge: EdgeKey) -> Result<()> {
    let key_out = (edge.source, edge.kind, edge.target);
    let key_in = (edge.target, edge.kind, edge.source);

    edges_out.remove(&key_out)?;
    edges_in.remove(&key_in)?;

    update_count(txn, edge.source, "edges_out_count", -1)?;
    update_count(txn, edge.target, "edges_in_count", -1)?;

    Ok(())
}
```

## 7. Forward queries

"What does memory X cause?" → range scan of `edges_out`:

```rust
let range_start = (X, EdgeKind::Caused, MemoryId::MIN);
let range_end = (X, EdgeKind::Caused, MemoryId::MAX);
let results: Vec<_> = edges_out.range(range_start..range_end)?.collect();
```

The B-tree's range scan returns results in target-id order, with cost proportional to the number of returned edges.

## 8. Reverse queries

"What was caused by X?" → range scan of `edges_in`:

```rust
let range_start = (X, EdgeKind::Caused, MemoryId::MIN);
let range_end = (X, EdgeKind::Caused, MemoryId::MAX);
let results: Vec<_> = edges_in.range(range_start..range_end)?.collect();
```

Symmetric to forward query, just on the other table.

## 9. All-edges-from queries

"All edges (any kind) from X":

```rust
let range_start = (X, EdgeKind::MIN, MemoryId::MIN);
let range_end = (X+1, EdgeKind::MIN, MemoryId::MIN);  // next source
let results: Vec<_> = edges_out.range(range_start..range_end)?.collect();
```

Returns all 8 edge kinds for source X. Sorted by kind, then target.

## 10. Edge memory cost

For a typical 1M-memory shard with ~8 edges per memory:

- 8M edges × 2 indexes × 30 bytes = 480 MB.
- Plus B-tree overhead: ~10-20%.
- Total: ~500-600 MB for edges.

Sizing scales linearly with edge count. Heavily-connected memories (lots of REFERENCES, SIMILAR_TO) generate many edges.

## 11. Edge limits

To prevent pathological cases (a memory with millions of edges), the substrate enforces:

- Per-memory soft limit on outgoing edges per kind: 10K (configurable).
- Per-memory hard limit on outgoing edges per kind: 100K (configurable).
- Per-encode limit: 64 edges per ENCODE operation.

Beyond the soft limit, LINK operations log a warning. Beyond the hard limit, they fail with `TooManyEdges`.

## 12. Multi-edges

Are duplicate (source, kind, target) edges allowed? No. The composite key is a unique key.

A second LINK with the same key updates the existing edge's data (weight, etc.) rather than creating a duplicate. This is the natural behavior of B-tree insertion.

For applications that want multi-edges (e.g., multiple instances of REFERENCES with different annotations), the substrate doesn't support that directly. Workaround: encode the differentiator into the kind (e.g., `REFERENCES_v1`, `REFERENCES_v2`).

## 13. Edges and memory deletion

When a memory is reclaimed, all its edges (incoming and outgoing) must be removed. The procedure:

1. Range-scan `edges_out` for the memory: delete all matches.
2. Range-scan `edges_in` for the memory: delete all matches.
3. For each edge deleted, decrement the edge counts on the other endpoint.

This is a single transaction. For memories with many edges, the transaction is large.

For memories with very many edges (>10K), the transaction may be split into batches:
- Delete in batches of 1000 edges per transaction.
- Each batch is committed independently.
- Recovery is correct because partial deletion just means more cleanup work later.

## 14. Auto-derived edges

Some edges are added automatically:

- `DERIVED_FROM` from a Consolidated memory to its source Episodic memories.
- `SIMILAR_TO` between memories whose vectors are very close (an option, not the default).

Auto-derived edges have `origin: AutoDerived` to distinguish from `Explicit` (client-asserted) edges. This lets the substrate cleanly remove auto-derived edges during maintenance without affecting client-asserted ones.

## 15. Edge weight

Weights are in [0, 1] (or sometimes [-1, 1] for "negative" relationships like CONTRADICTS). They can be:

- Set explicitly by the client (e.g., agent says "I'm 80% sure A caused B" → weight 0.8).
- Auto-computed (e.g., SIMILAR_TO weight is the cosine similarity).
- Updated over time (a worker may strengthen frequently-co-accessed CAUSED edges).

The default weight, if unspecified, is 1.0 (full confidence).

## 16. Edge graph queries beyond direct lookups

For multi-hop graph queries ("what does X cause that's similar to Y?"), the substrate's primitives are:

- Single-hop edge enumeration (from this table).
- Vector similarity (from the ANN index).

The query planner ([08. Query Planner](../08_query_planner/)) composes these. There's no native graph-query-language support (Cypher, GQL); we don't fork into being a graph database.

The substrate is well-suited for narrow, common patterns (one or two hops); not for arbitrary graph traversal queries.

---

*Continue to [`05_context_table.md`](05_context_table.md) for contexts.*
