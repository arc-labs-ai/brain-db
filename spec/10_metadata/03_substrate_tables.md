# 10.03 Substrate Tables

The substrate-layer redb tables: memory metadata, edges, sessions, idempotency, and text. These are the tables that exist for every shard regardless of whether a typed-graph schema has been declared.

## Memory Metadata Table

The `memories` table is the central index of Brain. Every memory has exactly one row here. This section specifies the row's fields and access patterns.

### 1. The row layout

```rust
struct MemoryMetadata {
    // Identity (owner scope)
    memory_id: MemoryId,                  // 16 bytes (also the key)
    namespace_id: NamespaceId,            // 4 bytes; owning tenant. 0 = reserved `brain` system namespace
    space_id: SpaceId,                    // 16 bytes; owning space
    session_id: SessionId,                // 8 bytes
    slot_id: u64,                         // 8 bytes (effective 48-bit)
    slot_version: u32,                    // 4 bytes

    // Type and content
    kind: MemoryKind,                     // 1 byte (Episodic/Semantic/Consolidated)
    text_size: u32,                       // 4 bytes (in `texts` table)

    // Temporal
    created_at: u64,                      // 8 bytes (unix nanoseconds)
    last_accessed_at: u64,                // 8 bytes
    forgot_at: Option<u64>,               // 8+1 bytes
    tombstoned_at: Option<u64>,           // 8+1 bytes
    consolidated_at: Option<u64>,         // 8+1 bytes (when promoted to Consolidated)
    occurred_at: Option<u64>,             // 8+1 bytes; client EVENT time, distinct from created_at

    // Salience
    salience: f32,                        // 4 bytes (current)
    salience_initial: f32,                // 4 bytes (initial baseline)
    access_count: u32,                    // 4 bytes (lifetime)

    // Embedding
    embedding_model_fp: ModelFingerprint, // 16 bytes
    
    // Status flags
    flags: u32,                           // 4 bytes (bit-packed)

    // Counters
    edges_out_count: u32,                 // 4 bytes (denormalized; updated on edge changes)
    edges_in_count: u32,                  // 4 bytes

    // Dedup index back-reference
    content_hash: Option<[u8; 32]>,       // Some(BLAKE3(text)) iff the write deduplicated

    // Provenance
    encoded_at_lsn: u64,                  // WAL position of the ENCODE that wrote this row; 0 = unknown
}
```

Three fields are worth more than their byte count:

- **`occurred_at`** is the client-supplied **event** time — when the memory's content actually happened — and is deliberately a separate field from `created_at` (the server's write time). Collapsing the two would make "what happened last Tuesday" unanswerable for anything ingested later, and it is `occurred_at` that drives the recency ranking term.
- **`content_hash`** is `Some` only when the write took the dedup path. It exists so a FORGET or a slot reclamation can evict the matching `fingerprints` row **in the same write transaction as the tombstone**, rather than leaving a dedup entry pointing at a memory that no longer exists. It is `None` on the dedup-off path so that path does not pay 32 bytes per row.
- **`encoded_at_lsn`** is the WAL position of the ENCODE that created the row, surfaced to clients as `MemoryResult.lsn`. It is what lets a caller chain `recall → subscribe --from-lsn lsn+1` and follow one memory's downstream events from the moment it was written. `0` means unknown — a deployment with no WAL sink.

Total: ~180 bytes per row. With redb's per-row overhead, ~190 bytes effective.

For 1M memories: ~150 MB.

### 2. Field semantics

#### 2.1 Identity

- `memory_id` — primary key. Repeated as a row field for convenience (rkyv decoders can return the row including the key).
- `namespace_id` — owning tenant (the outer half of the `(namespace, space)` owner scope). `0` is the reserved `brain` system namespace, which owns only seeded rows. Stamped by the writer from the authenticated connection's scope (fail-closed by construction). Distinct from the qname namespace of any *type* the memory's downstream typed-graph rows reference.
- `space_id` — owning agent (the inner half of the owner scope). Searches typically filter by agent.
- `session_id` — bucket the memory belongs to (one of an space's sessions).

The `memories` primary table is `MemoryId`-keyed; the `(namespace_id, space_id)` owner scope lives on the row value and is the leading prefix of the per-tenant `memories_by_agent_timeline` index ([`02_table_layout.md`](02_table_layout.md) §4), so a timeline scan for one `(namespace, space)` can never traverse another tenant's rows.
- `slot_id` and `slot_version` — locate the vector in the arena. Version disambiguates reused slots.

#### 2.2 Kind

One of `Episodic`, `Semantic`, `Consolidated`. Set at creation; can be changed via `UPDATE_KIND` operation.

#### 2.3 Text size

Cached size of the text (in the `texts` table). Lets Brain quickly answer "how big is this memory's text?" without a separate read.

#### 2.4 Temporal fields

- `created_at` — when the memory was first encoded.
- `last_accessed_at` — when the memory was last returned in a RECALL response.
- `forgot_at` — set on FORGET; null otherwise.
- `tombstoned_at` — set on the same FORGET event; redundant with `forgot_at` in v1, distinguished for future fine-grained handling.
- `consolidated_at` — set when an Episodic memory is promoted to Consolidated.

All times are unix nanoseconds; 64-bit handles dates well past year 2200.

#### 2.5 Salience

- `salience` — current salience after decay and access boosts. Range [0, 1].
- `salience_initial` — baseline at creation time (before decay).
- `access_count` — total number of times this memory has been returned.

Salience is recomputed periodically by the decay worker (see [15. Background Workers](../15_background_workers/00_purpose.md) §Decay).

#### 2.6 Embedding model fingerprint

The fingerprint of the model that produced this memory's vector. Used for cross-model exclusion in queries.

#### 2.7 Flags (bit-packed)

| Bit | Meaning |
|---|---|
| 0 | Active (1) vs tombstoned (0) |
| 1 | Hard-forgotten (vector zeroed) |
| 2 | Pinned (won't be auto-evicted) |
| 3 | Reserved for staleness flag (set if vector hasn't been re-embedded after model change) |
| 4-31 | Reserved |

#### 2.8 Edge counts

- `edges_out_count` and `edges_in_count` — denormalized counts.
- Updated during LINK/UNLINK operations.
- Avoid range scans of the edge tables when callers just want a count.

### 3. Access patterns

#### 3.1 By MemoryId

The most common access. O(log N) lookup.

#### 3.2 By agent

To list an space's memories: a range scan of `memories` by MemoryId range corresponding to the agent. Since MemoryIds encode their shard in the high bits, this is a tight range.

Note — looking at the MemoryId layout ([02.02 Memory](../02_data_model/02_memory.md)) — MemoryIds are not strictly space-clustered. The shard_id_runtime is in the high bits, then slot_id, then slot_version. So MemoryIds within a shard are slot-id-ordered (which is roughly creation-time-ordered).

To list an space's memories, an auxiliary index would be required. Brain does not currently have one in the table layout above; an `(SpaceId, MemoryId) → ()` index table would need to be added. This is a [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md) item.

In v1, listing an space's memories means scanning all memories in the shard and filtering by space_id. For shards with thousands of spaces, this is wasteful. For shards with one or few spaces per shard, it's fine.

#### 3.3 By session

`(SpaceId, SessionId, MemoryId) → ()` index would enable this efficiently. Same open question as agent index.

For v1, `RECALL` with a session filter applies the filter post-search; no metadata-side index is used for it.

#### 3.4 Range scans by time

UUIDv7's time-ordered prefix means range scans by MemoryId approximate range scans by creation time. "Memories created since X" is a tight range scan starting from a synthetic MemoryId derived from X.

### 4. Updates

#### 4.1 Common updates

Most metadata updates are read-modify-write:

```
let mut metadata = memories.get(&memory_id)?;
metadata.last_accessed_at = now();
metadata.access_count += 1;
memories.insert(&memory_id, &metadata);
```

These are coalesced when possible (multiple memories' updates in a single transaction).

#### 4.2 The salience update path

Salience updates are the most frequent. They happen:
- Immediately on access (boost).
- Periodically (decay).

Brain batches salience updates: the decay worker processes many memories in a single transaction; the access boost is buffered until the next transaction commit.

#### 4.3 Edge count updates

Edge counts are updated whenever LINK or UNLINK happens. The update is in the same transaction as the edge insert/delete. This keeps the count accurate.

If the count gets out of sync (due to a bug or partial recovery), a maintenance worker periodically recomputes it.

### 5. Tombstoning

A FORGET operation sets the appropriate flags and timestamps; the row remains in the table:

```
metadata.flags &= !ACTIVE_BIT;  // clear active bit
metadata.forgot_at = Some(now());
metadata.tombstoned_at = Some(now());
```

The row stays for the grace period. After grace, the slot is reclaimed and the row is deleted.

### 6. Reclamation

When a slot is reclaimed:

1. The row is deleted from `memories`.
2. The text is deleted from `texts`.
3. Edges referencing the memory are deleted from `edges` and `edges_reverse`.
4. The slot's version in `slot_versions` is incremented.

This is a single redb transaction. After commit, the memory is fully gone.

### 7. The "active" filter

Most queries filter for active memories (flag bit 0 = 1). Brain either:

- Adds a filter expression to the query (and pays the cost of reading inactive rows).
- Maintains a separate index of active memory IDs. Not currently done.

For v1, post-filter is the approach. The cost is minor — most rows are active.

### 8. Sizing analysis

Per-row size: ~150 bytes.

For 10M memories: ~1.5 GB just for the memory table. redb's overhead adds ~20-30%, so plan for 2 GB at this scale.

For comparison, the vector arena at 10M memories is 15 GB. The metadata table is ~10% of arena size.

### 9. Cross-version compatibility

The row's binary layout is rkyv-encoded. Adding fields requires:
- Bumping the table's schema version.
- A migration that rewrites existing rows in the new format.

Migrations are run lazily (per-row on access) or eagerly (full-table scan on startup), depending on the migration type. See [03.05 Schema Versioning](../03_schema/05_versioning.md).

### 10. The "fresh memory" lifecycle

The lifecycle of a row in this table:

1. **Created** by ENCODE: row inserted, flags = ACTIVE.
2. **Updated** by accesses: salience and access_count update.
3. **Updated** by edge changes: edges_out_count, edges_in_count.
4. **Maybe consolidated**: kind changes to Consolidated, consolidated_at set.
5. **Maybe forgotten**: flags clear ACTIVE, forgot_at set.
6. **Eventually reclaimed**: row deleted.

The active lifetime ranges from minutes (transient memories) to years (persistent semantic memories). Brain doesn't impose a maximum lifetime.

## Edge Storage

Edges are the relational layer of Brain's data model. This section specifies how they're stored and indexed.

### 1. Edge model recap

From [02.05 Edges](../02_data_model/05_edges.md):

- An edge connects a source memory to a target memory.
- Each edge has a kind (one of 8 enumerated types).
- Each edge has a weight (f32 in [0, 1]).
- Edges are directed but always have an "implied reverse" semantics (a CAUSED edge implies the target was caused-by the source).

### 2. Two indexes for two directions

Brain maintains two index tables:

- `edges: (from, kind, to, disambiguator) → EdgeData`
- `edges_reverse: (to, kind, from, disambiguator) → EdgeData`

Same data, two indexes. Forward queries use `edges`; reverse queries use `edges_reverse`.

The duplication doubles edge storage but enables both directions to be answered with a single B-tree range scan.

### 3. The composite key

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

### 4. The EdgeData value

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

### 5. Edge insertion (LINK)

```rust
fn link(txn: &mut WriteTxn, edge: Edge) -> Result<()> {
    let key_out = (edge.source, edge.kind, edge.target);
    let key_in = (edge.target, edge.kind, edge.source);
    let value = EdgeData { weight: edge.weight, ... };

    let edges = txn.open_table(EDGES_TABLE)?;
    let edges_reverse = txn.open_table(EDGES_REVERSE_TABLE)?;

    edges.insert(&key_out, &value)?;
    edges_reverse.insert(&key_in, &value)?;

    // Update edge counts in memories table
    update_count(txn, edge.source, "edges_out_count", +1)?;
    update_count(txn, edge.target, "edges_in_count", +1)?;

    Ok(())
}
```

A single transaction handles both index updates and the count updates. Atomic.

### 6. Edge removal (UNLINK)

```rust
fn unlink(txn: &mut WriteTxn, edge: EdgeKey) -> Result<()> {
    let key_out = (edge.source, edge.kind, edge.target);
    let key_in = (edge.target, edge.kind, edge.source);

    edges.remove(&key_out)?;
    edges_reverse.remove(&key_in)?;

    update_count(txn, edge.source, "edges_out_count", -1)?;
    update_count(txn, edge.target, "edges_in_count", -1)?;

    Ok(())
}
```

### 7. Forward queries

"What does memory X cause?" → range scan of `edges`:

```rust
let range_start = (X, EdgeKind::Caused, MemoryId::MIN);
let range_end = (X, EdgeKind::Caused, MemoryId::MAX);
let results: Vec<_> = edges.range(range_start..range_end)?.collect();
```

The B-tree's range scan returns results in target-id order, with cost proportional to the number of returned edges.

### 8. Reverse queries

"What was caused by X?" → range scan of `edges_reverse`:

```rust
let range_start = (X, EdgeKind::Caused, MemoryId::MIN);
let range_end = (X, EdgeKind::Caused, MemoryId::MAX);
let results: Vec<_> = edges_reverse.range(range_start..range_end)?.collect();
```

Symmetric to forward query, just on the other table.

### 9. All-edges-from queries

"All edges (any kind) from X":

```rust
let range_start = (X, EdgeKind::MIN, MemoryId::MIN);
let range_end = (X+1, EdgeKind::MIN, MemoryId::MIN);  // next source
let results: Vec<_> = edges.range(range_start..range_end)?.collect();
```

Returns all 8 edge kinds for source X. Sorted by kind, then target.

### 10. Edge memory cost

For a typical 1M-memory shard with ~8 edges per memory:

- 8M edges × 2 indexes × 30 bytes = 480 MB.
- Plus B-tree overhead: ~10-20%.
- Total: ~500-600 MB for edges.

Sizing scales linearly with edge count. Heavily-connected memories (lots of REFERENCES, SIMILAR_TO) generate many edges.

### 11. Edge limits

To prevent pathological cases (a memory with millions of edges), Brain enforces:

- Per-memory soft limit on outgoing edges per kind: 10K (configurable).
- Per-memory hard limit on outgoing edges per kind: 100K (configurable).
- Per-encode limit: 64 edges per ENCODE operation.

Beyond the soft limit, LINK operations log a warning. Beyond the hard limit, they fail with `TooManyEdges`.

### 12. Multi-edges

Are duplicate (source, kind, target) edges allowed? No. The composite key is a unique key.

A second LINK with the same key updates the existing edge's data (weight, etc.) rather than creating a duplicate. This is the natural behavior of B-tree insertion.

For applications that want multi-edges (e.g., multiple instances of REFERENCES with different annotations), Brain doesn't support that directly. Workaround: encode the differentiator into the kind (e.g., `REFERENCES_v1`, `REFERENCES_v2`).

### 13. Edges and memory deletion

When a memory is reclaimed, all its edges (incoming and outgoing) must be removed. The procedure:

1. Range-scan `edges` for the memory: delete all matches.
2. Range-scan `edges_reverse` for the memory: delete all matches.
3. For each edge deleted, decrement the edge counts on the other endpoint.

This is a single transaction. For memories with many edges, the transaction is large.

For memories with very many edges (>10K), the transaction may be split into batches:
- Delete in batches of 1000 edges per transaction.
- Each batch is committed independently.
- Recovery is correct because partial deletion just means more cleanup work later.

### 14. Auto-derived edges

Some edges are added automatically:

- `DERIVED_FROM` from a Consolidated memory to its source Episodic memories.
- `SIMILAR_TO` between memories whose vectors are very close (an option, not the default).

Auto-derived edges have `origin: AutoDerived` to distinguish from `Explicit` (client-asserted) edges. This lets Brain cleanly remove auto-derived edges during maintenance without affecting client-asserted ones.

### 15. Edge weight

Weights are in [0, 1] (or sometimes [-1, 1] for "negative" relationships like CONTRADICTS). They can be:

- Set explicitly by the client (e.g., agent says "I'm 80% sure A caused B" → weight 0.8).
- Auto-computed (e.g., SIMILAR_TO weight is the cosine similarity).
- Updated over time (a worker may strengthen frequently-co-accessed CAUSED edges).

The default weight, if unspecified, is 1.0 (full confidence).

### 16. Edge graph queries beyond direct lookups

For multi-hop graph queries ("what does X cause that's similar to Y?"), Brain's primitives are:

- Single-hop edge enumeration (from this table).
- Vector similarity (from the vector index).

The query planner ([12. Query Optimizer](../12_query_optimizer/00_purpose.md)) composes these. There's no native graph-query-language support (Cypher, GQL); Brain does not fork into being a graph database.

Brain is well-suited for narrow, common patterns (one or two hops); not for arbitrary graph traversal queries.

## Space & Session Registry Tables

Spaces and sessions are **registries**: derived, recomputable view state that summarizes data which stays authoritative in the memory and graph tables. Nothing correctness-bearing reads them. This section specifies their storage.

### 1. The model

From [`../02_data_model/03_sessions.md`](../02_data_model/03_sessions.md) and [`../20_tenancy/00_purpose.md`](../20_tenancy/00_purpose.md):

- A **space** is the isolation unit within a namespace. Its 16-byte `space_id` is `UUIDv5(namespace, space_string)` — a non-invertible derivation, so the registry row is the only place the original string survives.
- A **session** is a grouping within a space. Its `session_id` is a client-chosen opaque `u64`; `0` is the default session.
- A memory belongs to exactly one `(space, session)`.

### 2. Three tables

#### 2.1 `spaces: (namespace_id, space_id) → SpaceMetadata`

20-byte key: `[namespace_id (4, BE) | space_id (16)]`. The leading namespace bytes make one namespace's spaces a contiguous keyspace, so `SPACE_LIST` is a range scan over a 4-byte prefix.

```rust
struct SpaceMetadata {
    space_string: String,          // the structured selector, e.g. "support-bot:user123"
    created_at_unix_nanos: u64,
    last_active_unix_nanos: u64,
    memory_count: u64,             // denormalized; display-only
    session_count: u32,            // denormalized; display-only
    metadata: Option<Vec<u8>>,     // opaque caller blob (quota hints, labels)
}
```

`space_string` is empty when a space was first seen without one — a raw key-bound space that was never named by an `act_as` selector.

#### 2.2 `sessions: (namespace_id, space_id, session_id) → SessionMetadata`

28-byte key: `[namespace_id (4, BE) | space_id (16) | session_id (8, BE)]`.

```rust
struct SessionMetadata {
    created_at_unix_nanos: u64,
    last_active_unix_nanos: u64,
    title: Option<String>,         // optional caller-supplied display title
    memory_count: u32,             // denormalized; display-only
}
```

The scope lives in the **key**, not the value. That is what makes isolation structural: a range scan for one `(namespace, space)` physically cannot walk into another's rows, with no filter to forget to apply.

#### 2.3 `session_by_scope: (namespace_id, space_id, last_active, session_id) → ()`

36-byte key. The ordering index: a reverse range scan over the 20-byte `(namespace, space)` prefix yields `SESSION_LIST`'s newest-first-by-activity order directly.

### 3. Why these three

Each answers a question the system actually asks:

- *"What are this namespace's spaces?"* → `spaces`, range scan.
- *"What is this session's state?"* → `sessions`, point lookup.
- *"What are my recent conversations?"* → `session_by_scope`, reverse range scan.

There is **no name → id table and no membership table**. Both existed in a design where the server allocated session ids behind client-supplied names; with a client-chosen `u64` there is no name to resolve and nothing to intern, and membership is implied by the key prefix — `(namespace, space)` already bounds the scan. Paying for an ordering index at write time is cheaper than sorting a scope's sessions on every read, which is why that one remains.

### 4. Id derivation and allocation

Brain allocates **neither** id.

- `space_id` is derived: `UUIDv5(namespace, space_string)`, computed at request ingress. The same string always derives the same id, so a client never has to store a handle.
- `session_id` is the client's own `u64`. There is no allocator, no monotonic counter to exhaust, and no gap-free guarantee to maintain.

### 5. Creation — implicit and explicit

Both rows are created two ways, and both are first-class:

1. **Implicitly, on first write.** The apply path upserts the space and session rows in the *same* redb write transaction as the memory, stamping `created_at` on first sight and bumping `last_active` and the counts thereafter. Because it is one transaction, the registry cannot drift from the data on a crash: either both landed or neither did. WAL recovery replays the same helper, so a recovered shard rebuilds the registry rather than inheriting a stale one.
2. **Explicitly**, via `SPACE_CREATE` / `SESSION_CREATE`. Idempotent — a create for an existing row returns it with `created = false`. This is for attaching metadata or a title ahead of the first write, never a precondition for writing.

### 6. The default session

`session_id = 0` is the default. It is implicit for every space — no row needs to exist for it to be writable — and memories encoded without an explicit session land there. It is never deletable.

There is no reserved-name convention (`_default` or otherwise), because there are no server-side session names to reserve.

### 7. Limits

There is no per-space session cap. `session_id` is the client's `u64`, not a server-allocated handle out of a bounded table, so there is nothing to exhaust and no `TooManyContexts`-style error. A space with an implausible number of sessions is an operational signal, not a protocol condition.

### 8. Renaming

Nothing in these tables is renameable. A space's string is the input its id was derived from — changing it would derive a different space, which is a different space. A session's `title` is display metadata set at create time.

The admin ops `ADMIN_CREATE_SESSION` / `ADMIN_RENAME_SESSION` are a separate operator surface that carries a `name`; they are admin-gated and not part of the tenant-facing registry.

### 9. Deletion

`SESSION_DELETE` and `SPACE_DELETE` are **cascades**, not unlinks, and they share one implementation:

1. Range-scan the `(namespace, space)` memory timeline — optionally narrowed to one session — to collect the live memory ids.
2. Tombstone each through the single `submit(Write)` path.
3. Each tombstone fans out post-commit to the FORGET cascade, which tears down the backing entity / statement / relation rows.

So a scope delete reuses the forget machinery rather than re-implementing graph teardown. `SESSION_DELETE` defaults to a soft tombstone (`hard = true` zeroes immediately); `SPACE_DELETE` is GDPR erasure and is always hard. Deleting a scope with no registry row is a lenient success with `existed = false`.

The v1 cascade is synchronous — it tombstones every matching memory in one pass. A very large delete should eventually chunk behind a resumable cursor.

### 10. Denormalized counts

`memory_count` and `session_count` are **display-only**. They are updated incrementally on write and corrected by the counter-reconciliation maintenance worker, which recomputes the true values from the authoritative tables. Nothing branches on them; an exact count comes from the data, not the registry.

### 11. Listing

`SESSION_LIST` for one `(namespace, space)`:

```rust
let (lo, hi) = session_scope_range_bounds(namespace_id, space_id);
let sessions = session_by_scope.range(lo..=hi)?.rev();   // newest-first by last_active
```

`SPACE_LIST` for one namespace is the analogous forward scan over `spaces`. In v1 it covers only the caller-shard's spaces, which is why the response carries `cross_shard_complete` — a client treats `false` as "this listing is partial", not as an error.

### 12. Cross-space separation

Two namespaces can each have a space string `"personal"`. Their derived `space_id`s differ, because the derivation takes the namespace as the UUIDv5 namespace argument. Their rows sit under different key prefixes and their memories are separate; there is no global space namespace and nothing for tenants to coordinate.

### 13. Total table sizes

For typical workloads, all three are small relative to memories and edges:

- `spaces`: up to ~10⁶ rows per namespace at the tenancy target; tens of MB.
- `sessions`: proportional to conversations; tens of MB.
- `session_by_scope`: 36 bytes per session plus B-tree overhead; a few MB.

## Idempotency Table

Brain must handle client retries gracefully. If a client sends ENCODE, doesn't get a response (network drop), and retries, the second attempt must not produce a duplicate memory.

The mechanism: every state-mutating request carries a `RequestId`. Brain remembers which RequestIds it's seen and replays the original response for duplicates.

### 1. The RequestId

From [02.02 Memory](../02_data_model/02_memory.md):

- `RequestId` is a 16-byte UUIDv7.
- Generated by the client.
- Unique within the client (the client doesn't reuse RequestIds across distinct logical operations).

### 2. The table

```rust
table: idempotency
key: RequestId
value: IdempotencyEntry
```

```rust
struct IdempotencyEntry {
    response_kind: u8,           // 1 = ENCODE, 2 = FORGET, 3 = LINK, etc.
    memory_id: Option<MemoryId>, // Resulting memory (if any)
    response_payload: Vec<u8>,   // The original response, encoded
    created_at: u64,             // For TTL
}
```

### 3. The lookup-then-act protocol

For every state-mutating request:

```rust
fn handle_encode(request: EncodeRequest) -> EncodeResponse {
    let txn = db.begin_read()?;
    let idem = txn.open_table(IDEMPOTENCY_TABLE)?;
    
    if let Some(prior) = idem.get(&request.request_id)? {
        // Duplicate: replay original response
        return decode_response(prior.response_payload);
    }
    drop(txn);

    // Not a duplicate; proceed
    let memory = create_memory(&request)?;
    let response = build_response(memory);

    let mut wtxn = db.begin_write()?;
    {
        let mut idem = wtxn.open_table(IDEMPOTENCY_TABLE)?;
        idem.insert(&request.request_id, &IdempotencyEntry {
            response_kind: 1,
            memory_id: Some(memory.id),
            response_payload: encode_response(&response),
            created_at: now(),
        })?;
        // ... other table writes for the encode
    }
    wtxn.commit()?;

    response
}
```

The lookup is in a read transaction (cheap, MVCC). The act is in a write transaction (atomic with the rest of the encode).

### 3a. Key scoping under `act_as`

The lookup above shows a bare-`RequestId` key, which is correct **only** because a connection's identity is fixed for its whole life. When a request carries the `act_as` field — a trusted service principal running the op on behalf of a tenant agent, defined in [`../04_wire_protocol/04_handshake.md`](../04_wire_protocol/04_handshake.md) §"Per-request identity (`act_as`)" — that assumption no longer holds: one service principal issues ops for many effective identities over one shared connection.

Therefore, under `act_as`, the idempotency key MUST be scoped by the **effective** `(namespace_id, space_id)` named in `act_as`, not by the connection principal — the key is the composite `(namespace_id, space_id, RequestId)`, and shard routing likewise keys on the effective agent (see §12). Consistent with [`../05_operations/02_write_pipeline.md`](../05_operations/02_write_pipeline.md) §4. This is a load-bearing safety rule: with a bare-RequestId key, a service principal that reuses a RequestId across two tenants would **collide**, replaying tenant A's cached response to tenant B — a cross-tenant data leak, invisible because the pool and the idempotency table are shared. Scoping the key by the effective identity makes the collision impossible; the two tenants land on disjoint keys.

### 4. The replay safety

When replaying, Brain returns the original response — same MemoryId, same metadata. The client gets exactly what it would have gotten on the original successful response.

This is correct because:
- Brain has the response stored verbatim.
- Brain has the resulting memory still around (or, if forgotten, the response just shows the original successful state).

### 5. Conflict detection

If a RequestId is reused with *different* parameters (e.g., same RequestId but different text), this is a client bug. Brain detects it:

```rust
if let Some(prior) = idem.get(&request.request_id)? {
    // Verify the request matches what was originally seen
    if !request_matches_original(&request, &prior) {
        return Err(IdempotencyConflict);
    }
    return decode_response(prior.response_payload);
}
```

The "match" check uses a hash of the canonical form of the request. If the original hash doesn't match the new hash, Brain returns `IdempotencyConflict`.

### 6. TTL

Idempotency entries don't live forever. The default TTL is 24 hours, configurable.

Entries older than the TTL are pruned by a background worker:

```
worker:
    every hour:
        scan idempotency for entries with created_at older than now - TTL
        delete them in batches
```

Pruning is incremental; doesn't affect normal operation.

### 7. Why a TTL

Without TTL, the table would grow forever. Eventually the table dominates the metadata store.

With a 24-hour TTL:
- A 1000-RPS shard creates 86M idempotency entries per day.
- At ~50 bytes each, ~4 GB per day.
- After 24 hours, pruning catches up; table size stabilizes around 4 GB.

For most shards, TTL is sufficient. Operators can shorten or lengthen it.

### 8. The trade-off: TTL vs retry window

Clients retry within their own timeout (typically seconds to minutes). The 24-hour TTL is far longer than any client retry window — chosen to cover even unusual cases (client crashes, restarts later, retries with the same RequestId).

For very rapid request rates, even 24 hours is excessive. Operators can shorten to 1 hour without affecting normal client behavior.

For very long retries (a client offline for days), 24 hours may not be enough. Operators can lengthen.

### 9. Replay vs re-execute

Brain replays (returns the cached response), not re-executes. Re-execution would:

- Risk creating a different MemoryId on retry.
- Apply side effects again (incremented salience boosts, etc.).
- Be expensive.

Replay returns exactly what the original returned. Idempotent semantics preserved.

### 10. Idempotency for non-mutating ops

RECALL, PLAN, REASON don't mutate state. They don't need idempotency. Their RequestIds (if present) are just for observability — tracing and metrics.

Brain skips the idempotency check for read-only operations.

### 11. Multi-record requests

For requests that produce multiple records (e.g., ENCODE with edges), the response_payload includes all the records. Replay returns all of them.

For a transaction (TXN_BEGIN/COMMIT bracket spanning multiple operations), each operation has its own RequestId. The transaction itself doesn't have a top-level RequestId (the operations within it provide idempotency).

### 12. Cross-shard idempotency

The idempotency table is per-shard. If a request is routed to shard A, replays go to shard A.

This works because routing is deterministic — the same RequestId+agent always goes to the same shard. So a retry with the same RequestId always hits the shard with the cached response.

If the routing changes (rebalancing), idempotency for in-flight retries may break briefly. Brain's rebalancing protocol coordinates to drain in-flight requests before moving the shard, minimizing the window.

### 13. RequestId uniqueness

Clients are responsible for generating unique RequestIds. UUIDv7 with a clock-derived prefix and random bytes is the recommended generation method.

If a client accidentally reuses a RequestId for a different operation (a bug), Brain detects via the request-match check and returns `IdempotencyConflict`. The client must use a fresh RequestId.

### 14. The "happy path" cost

For a non-duplicate request:

- Read transaction begin: ~1 µs.
- Idempotency table lookup: ~5 µs (typically not in cache).
- Write transaction insert (along with other inserts): ~5 µs.

Total idempotency overhead per request: ~10 µs. Small relative to the encode's other costs.

### 15. The pessimistic-conflict path

For a duplicate:

- Read transaction lookup hits.
- Decode the cached response.
- Return.

Latency: ~10-20 µs. Much faster than re-doing the encode.

### 16. Idempotency in batched commits

Multiple requests within a single group commit each get their own idempotency entry. The transaction inserts all of them.

If one of the requests is a duplicate (replay), it doesn't reach the writer task — the duplicate is detected at the request handler before the writer is involved.

### 17. The idempotency-required scope

Currently, Brain enforces idempotency-required for:

- ENCODE
- FORGET
- LINK
- UNLINK
- UPDATE_KIND, UPDATE_SESSION
- TXN_BEGIN, TXN_COMMIT

For these operations, RequestId is mandatory. The wire protocol's validation checks for it.

For RECALL, PLAN, REASON, ADMIN_*, RequestId is optional. If supplied, it's recorded for observability; if not, the operation proceeds without a RequestId.

## Text Storage

Memory text lives in a dedicated `texts` table, keyed by MemoryId. This section specifies that table.

### 1. The table

```rust
table: texts
key: MemoryId
value: Vec<u8>  // UTF-8 bytes
```

A simple key-value table. Each memory has one entry.

### 2. Why a separate table

Memory text is variable-length and not always read. Putting it in the main `memories` table would:

- Bloat that table: text varies from a few bytes to ~1 MB.
- Slow random-access on `memories` (more bytes per row).

Separating text means:

- `memories` rows are fixed-size (~150 bytes).
- Reading metadata doesn't pay for reading text.
- Text is read on demand (when the response actually needs it).

### 3. Read patterns

Text is read:

- **In RECALL responses** when the client requests text (an option, not the default).
- **By the consolidation worker** when summarizing source memories.
- **By the migration worker** when re-embedding with a new model.
- **Rarely** for debugging or admin tools.

Most queries don't need text. The default `RECALL` returns memory IDs and metadata, not text. Clients explicitly opt in via a flag.

### 4. The text size

Text size varies by application:

- Short messages: ~50-200 bytes.
- Document chunks: ~500-2000 bytes.
- Long content: up to the model's max length (~3000 chars for 512 tokens).

Brain enforces a max text size (default 1 MB; configurable). Larger texts are rejected at the wire-validation layer.

### 5. Text encoding

Text is stored as UTF-8 bytes. The wire protocol carries UTF-8; Brain stores it byte-for-byte.

Brain validates:
- The bytes parse as valid UTF-8.
- The byte length matches the protocol-declared length.

Invalid UTF-8 is rejected at validation.

### 6. Text deduplication

Brain supports **opt-in fingerprint deduplication** at ENCODE time. When the caller passes `EncodeRequest.deduplicate = true`, Brain consults a fingerprint index before allocating a new slot; on a hit, the existing `MemoryId` is returned and no new slot, WAL record, or HNSW node is created.

#### 6.1. Scope

Fingerprint dedup is scoped per `(shard, space_id, session_id)`. The same text encoded by:

- the same agent in the same session → **dedup hit** (returns existing `MemoryId`).
- the same agent in a *different* session → **no hit** (different memory, allocated fresh). This preserves the spec's original observation that the same utterance in different episodic sessions is semantically different.
- a different space → **no hit**. The fingerprint table is partitioned by `space_id` for both privacy (one space's encoded text never matches against another's index) and ownership clarity (each agent owns its own dedup index).

Cross-shard dedup is not supported. The fingerprint table is per-shard, and routing already hashes the space to a single shard, so all of that space's memories live in one shard's table.

#### 6.2. Hash

The fingerprint is `BLAKE3(canonical_utf8(text))[..32]` — the first 32 bytes of BLAKE3 over the UTF-8 byte representation of the text. Canonicalisation in v1 is a no-op (the bytes go in as-is); future spec revisions may add NFC normalisation if cross-platform consistency becomes a real concern.

#### 6.3. Tombstone semantics

Dedup only hits **Active** memories. If the matching memory has been tombstoned (soft FORGET, hard FORGET, or worker-reclaimed), the dedup lookup misses and a fresh memory is allocated. Implementations are free to either:

(a) check the memory's state on every lookup (simpler), or
(b) evict the fingerprint entry on every FORGET / reclamation (faster lookup, more write paths to maintain).

v1 chooses **(b)** — eviction on FORGET / reclamation — because the read path is the hot path. `do_forget` removes the matching `(space_id, session_id, content_hash)` entry in the same write transaction as the tombstone.

#### 6.4. Default

Dedup is **off by default**. Callers that want it must opt in explicitly. The default-off reflects Brain's primitive: "one ENCODE call → one memory" is the simpler, more predictable model, and avoids silently merging memories whose distinct identity might matter to a downstream cognitive operation.

#### 6.5. Storage cost

The fingerprint table adds, per Active memory under dedup, one row of `space_id(16) + session_id(8) + content_hash(32) + memory_id(16) = 72 bytes`. At 1M Active memories per shard with 100% dedup-on, this is ~72 MiB of additional redb storage — comfortably within the spec's metadata-budget envelope.

#### 6.6. Refcount?

Earlier drafts considered a refcount table (so a single stored text could back N memories). v1 rejects that: dedup hit means the *same MemoryId is returned*, not "a new MemoryId backed by shared storage." Two callers asking for dedup get the same `MemoryId`; there's no refcount because there's only ever one row.

#### 6.7. Use cases

- Template-based agents that emit the same observation repeatedly.
- Idempotent batch ingestion where the source already has stable content.
- Caching layers that re-encode the same prompt during retries (request-level idempotency handles the explicit retry case; fingerprint dedup handles the case where the same content appears under a different `request_id`).

### 7. Text size limits

The text byte size is configurable:

```
[memory]
max_text_bytes = 1048576    # 1 MB default
```

Practical limits:

- The model's max context (512 tokens ≈ 2000 chars) means content beyond that doesn't influence the vector. So storing > 2000 chars is mostly for reference, not for vector quality.
- Typical agent text is well under 1 KB.

For deployments with longer content, the limit can be raised. Above 1 MB, performance considerations (transaction sizes, network bandwidth) become more relevant.

### 8. Text immutability

Once written, text is immutable. Brain doesn't support "update the text of memory M" — that would invalidate the vector, which depends on the text.

To "update" a memory:
1. Encode the new text as a new memory.
2. Optionally link new and old via a `DERIVED_FROM` or `REFERENCES` edge.
3. Optionally FORGET the old.

This pattern preserves the embedded-vector consistency with the text.

### 9. Hard-forget zeroing

When a memory is hard-forgotten:

1. The slot's vector is zeroed (the arena's bytes for that slot become all zeros).
2. The text in `texts` is overwritten with zeros (same length) before deletion.
3. The metadata is updated.

The zero-then-delete pattern ensures the text isn't recoverable from the file. (An attacker with disk access might still recover from filesystem-level fragments, but Brain has done its part.)

For paranoid deployments, Brain can also call `FALLOC_FL_PUNCH_HOLE` on the text region, encouraging the filesystem to release the underlying blocks.

### 10. Text and snapshots

Snapshots include the `texts` table (it's part of the metadata.redb file). Restoring from a snapshot brings back the text.

For deployments that want to retain memories without text (e.g., to honor right-to-be-forgotten requests), the operator can hard-forget specific memories before taking a snapshot.

### 11. Text and consolidation

When the consolidation worker creates a Consolidated memory:

1. Reads the text of source memories (via `texts` table).
2. Generates a summary (via an external LLM call, typically).
3. Encodes the summary as a new memory: writes new vector to arena, new text to `texts`.

The source memories' text is unchanged. The Consolidated memory has its own text — the summary.

### 12. Bulk text retrieval

For workloads needing many memory texts (e.g., bulk export), Brain doesn't optimize specially. Each lookup is a separate `texts.get(&memory_id)`. With redb's MVCC, a read transaction can iterate efficiently:

```rust
let txn = db.begin_read()?;
let texts = txn.open_table(TEXTS)?;
for memory_id in memory_ids {
    let text = texts.get(&memory_id)?.unwrap();
    process(text);
}
```

For 1000 lookups on a warm cache: ~5 ms total.

### 13. Text storage size

For 1M memories with avg 500 byte text each: ~500 MB.

For 10M memories: ~5 GB.

Text typically dominates the metadata store's disk footprint. Operators planning capacity should size for text at the expected average size.

### 14. The "store-text=no" mode

For deployments that don't want text stored in Brain (e.g., text is in another system; Brain just holds vectors):

```
[memory]
store_text = false
```

In this mode:
- The wire protocol's ENCODE still requires text (so Brain can embed it).
- After embedding, the text is discarded — not written to `texts`.
- RECALL responses can't return text.
- Migration can't re-embed (it would need the text); migration is unsupported in this mode.

The mode is a niche optimization. Most deployments store text.

### 15. Text vs metadata coupling

The text and the rest of the memory's metadata are written in the same transaction:

```rust
let mut wtxn = db.begin_write()?;
{
    let mut memories = wtxn.open_table(MEMORIES)?;
    let mut texts = wtxn.open_table(TEXTS)?;
    memories.insert(&memory_id, &metadata)?;
    texts.insert(&memory_id, &text_bytes)?;
}
wtxn.commit()?;
```

Atomic. After commit, both the metadata row and the text are durable.

If a crash happens before commit, neither is durable. Recovery from WAL replays the ENCODE record, which contains both.

### 16. Text and the wire protocol

The wire protocol's ENCODE carries text inline. RECALL with `include_text = true` returns text inline. There's no chunked text retrieval; for very large texts (~MB), the response carries them in one frame.

The frame size limit is 16 MiB ([04.02 Wire Format](../04_wire_protocol/02_wire_format.md)). Text up to ~16 MB minus protocol overhead is fine. The default max_text_bytes (1 MB) is well below this.

---

*Continue to [`04_transactions.md`](04_transactions.md) for transactions.*
