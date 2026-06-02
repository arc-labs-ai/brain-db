# 05.02 Write Pipeline

The write-side cognitive primitives: ENCODE (store), FORGET (delete), and LINK / UNLINK (relate). These share the single write path: validate → WAL fsync → metadata + index updates → ack. MATERIALIZE_PROCEDURAL (render an agent's procedural memory) is documented at the end of this file: it is a structured read — no WAL record — but conceptually belongs with the per-primitive surface and is grouped here for that reason.

## ENCODE

The ENCODE primitive: store a memory.

### 1. Semantic contract

```
ENCODE(text, agent_id, context, kind, metadata, edges, request_id)
  → MemoryId
```

Brain:

1. Embeds the text into a vector.
2. Allocates a slot in the arena.
3. Writes a WAL record (durability barrier).
4. Updates metadata, HNSW, and edge tables.
5. Returns a stable MemoryId.

After the response, the memory is durable, searchable, and connected.

### 2. The arguments

#### text

The content to encode. UTF-8. 1 byte to ~1 MB (configurable upper bound).

The text is what's embedded; it determines the vector and thus where the memory sits in the similarity space.

For very long text (> the model's context, typically ~2000 chars), the embedder truncates. The full text is still stored; only the embedding is truncated.

#### agent_id

The owning agent. Authentication ensures the caller can encode under this agent_id.

#### context

A `ContextRef` — either a name (resolved to a ContextId) or an explicit ContextId.

If unspecified: the agent's default context is used.

If a name doesn't exist: Brain creates the context.

#### kind

One of `Episodic` or `Semantic`. (`Consolidated` is worker-only — clients can't directly create Consolidated memories; they're produced by the consolidation worker.)

If unspecified: defaults to `Episodic`.

#### metadata

Key-value pairs of extra fields, agent-specific. Limited to a few KB total.

These are stored verbatim and available in the metadata-include responses. Brain doesn't index them; they're just blobs.

#### edges

A list of `EdgeSpec` — edges to create alongside this memory. Each edge has:

- target: another MemoryId.
- kind: `EdgeKind`.
- weight: f32 in [0, 1] (default 1.0).

If a target memory doesn't exist, that edge is rejected (logged); the encode itself proceeds.

Up to 64 edges per encode (configurable).

#### request_id

Required. A `RequestId` for idempotency. The same RequestId returns the original response for retries.

#### deduplicate

Optional, default `false`. When `true`, Brain consults a per-`(shard, agent_id, context_id)` fingerprint index (see [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md) §"Fingerprint deduplication") before allocating a new slot. On a hit, the existing `MemoryId` is returned and no new slot, WAL record, or HNSW node is created; `EncodeResponse.was_deduplicated = true`.

Default-off: the simpler "one ENCODE → one memory" model is Brain's primitive. Callers opt in explicitly when they know the content is dedup-safe (template outputs, idempotent ingestion).

### 3. The response

```rust
struct EncodeResponse {
    memory_id: MemoryId,
    was_deduplicated: bool,           // Fingerprint dedup hit (§4a)
    salience: f32,                    // Server-stamped salience
    auto_edges_added: u32,            // Count of edge_results.Inserted
    edge_results: Vec<EdgeResult>,    // Per-edge success/error
    persisted_at: u64,                // Brain's timestamp
    fingerprint: ModelFingerprint,    // The model that produced this vector
}
```

The MemoryId is the agent's primary handle. Stable. Use it to refer to this memory in all future operations.

`was_deduplicated` is `true` only when the request asked for dedup AND Brain found a matching fingerprint (see §4a and [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md) §"Fingerprint deduplication"). Idempotency-replay does not set this flag — replay is transparent to the caller and returns whatever the original response carried.

### 4. Idempotency

If the same RequestId is sent twice (e.g., due to network retry):

- Brain returns the original response.
- No duplicate memory is created.
- No additional WAL record is written.

This is Brain's commitment: at-most-once execution per RequestId, with replay-safe responses. Idempotency replay is **transparent** — `was_deduplicated` reports whatever the original response carried, not whether the wire-level replay occurred.

### 4a. Fingerprint deduplication

A distinct mechanism from §4 idempotency. Idempotency dedupes by *request identity* (same `RequestId`); fingerprint dedup dedupes by *content identity* (same `BLAKE3(text)` under the same `agent_id` + `context_id`).

Opt-in via `EncodeRequest.deduplicate = true`. On a hit:

- Brain returns the existing `MemoryId` for the matched memory.
- No new slot is allocated, no WAL record is written, no HNSW node is inserted.
- `EncodeResponse.was_deduplicated = true`.

On a miss (or `deduplicate = false`): the normal allocation path runs, the new memory's fingerprint is inserted into the per-`(shard, agent_id, context_id)` index, and `was_deduplicated = false`.

Only **Active** memories can dedup. Tombstoned and reclaimed memories are evicted from the fingerprint index in the same write transaction as the FORGET / reclamation, so a dedup lookup never returns a memory that RECALL would not find.

Full design: [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md) §"Fingerprint deduplication".

### 5. The "what gets stored" question

After ENCODE, Brain has:

- The vector (in the arena).
- The text (in the `texts` table).
- The metadata (in the `memories` table).
- Edges (in the edge tables).
- A WAL record describing the encode.

The text and vector together let Brain serve future queries. The metadata describes what the memory is.

### 6. The "what gets searched" question

After ENCODE, RECALL finds the new memory if its vector is close to a cue's vector. Brain's HNSW publishes the new node typically within 10 ms after the ENCODE response.

For read-after-write: a recall with `consistency=ReadAfterWrite` waits until the new memory is in the searchable HNSW.

### 7. Failure modes

#### EmbeddingFailed

The embedder couldn't process the text (invalid UTF-8, too long, embedder unavailable).

The encode fails; no memory is created.

#### QuotaExceeded

The agent has too many memories or contexts.

The encode fails; no memory is created.

#### ContextLimitReached

The context has too many memories (configurable per-context limit).

The encode fails.

#### InvalidEdge

An edge specifies a non-existent target, an invalid kind, or violates other constraints.

The encode succeeds; the bad edge is logged but not created. The response indicates which edges were created.

#### TooManyEdges

The encode specifies more than 64 edges.

The encode fails entirely; the agent should split into multiple operations.

### 8. The "context-derived inheritance"

A context can have default settings (kind, metadata defaults). When ENCODE doesn't specify these, they're inherited from the context.

This isn't currently implemented. Each ENCODE specifies its own kind explicitly (or uses the global default of Episodic).

### 9. The "create new context" semantic

If the context name doesn't exist, ENCODE creates it. The new context inherits default settings.

This makes the agent's first ENCODE in a new context "just work" — no separate context-creation step.

If the agent wants to ensure a context's settings (kind defaults, etc.) before encoding, it can use `ADMIN_CONTEXT_CREATE` first.

### 10. The "small text" case

For very short texts (a few words), the embedding is still meaningful but less informative. RECALL on short cues is also less precise.

Brain doesn't have a minimum text length; even a single character is encodable. The agent decides what's worth encoding.

### 11. The latency promise

For typical workloads:

- p50: ~5-10 ms.
- p99: ~25 ms.

The latency is dominated by the embedder. With cache hits (~10% of cues), latency drops to ~2-3 ms.

For batched encodes, throughput is much higher; per-encode latency rises slightly due to batching delay but throughput goes up to ~10K/sec/shard.

### 12. The "encode-and-recall" loop

A common pattern:

```
brain.encode("first thing", request_id=1)
brain.encode("second thing", request_id=2)
results = brain.recall("things", consistency=ReadAfterWrite)
```

Without `consistency=ReadAfterWrite`, the recall might miss the just-encoded memories (HNSW publication lag). With it, the recall waits.

Most workloads don't need this; Brain's eventual consistency is fine for typical agent behaviors.

### 13. The "agent's context" semantics

A memory belongs to exactly one agent and one context. Cross-agent or cross-context relationships need explicit edges (which can span contexts within an agent, but not across agents).

This means a memory can't be "shared" between agents — each agent has its own memory store. If a single piece of text is relevant to multiple agents, each encodes its own copy.

This is a deliberate design. Cross-agent memory sharing has complex semantics (who can update what, who can forget) and isn't a use case Brain optimizes for.

### 14. The "consolidated" cannot-be-encoded rule

A client can encode `Episodic` or `Semantic` memories. Not `Consolidated` — those are worker-only.

If a client tries `kind: Consolidated`, ENCODE returns `InvalidRequest`. To create a consolidated-like memory directly, the client uses `Semantic`.

### 15. The encode is a "single-write commit"

ENCODE is one atomic operation. After it succeeds:

- The memory exists.
- All specified edges (that were valid) exist.
- The metadata is current.

If the encode fails, none of these exist (partial state isn't visible to other clients).

### 16. The "encode fails partway" guarantee

If Brain crashes between WAL fsync and the ack:

- The memory is durable (in the WAL).
- Brain's in-memory state may be incomplete.
- On recovery, the WAL is replayed; the memory becomes fully visible.
- The client may see a network error; on retry with the same RequestId, the cached response is returned.

So the agent never sees a half-encoded memory.

## FORGET

The FORGET primitive: delete a memory.

### 1. Semantic contract

```
FORGET(target, agent_id, mode, request_id) → ForgetResponse
```

Brain:

1. Marks the memory(ies) as tombstoned.
2. Optionally zeroes the vector and text (hard mode).
3. After a grace period, reclaims the slot.

After FORGET succeeds, the memory is invisible to future RECALL, PLAN, and REASON.

### 2. The arguments

#### target

What to forget. Either:

- `Memory(MemoryId)` — single memory.
- `Memories(Vec<MemoryId>)` — list.
- `Filter(ForgetFilter)` — declarative criteria.

The filter form is for bulk operations (e.g., "forget all memories in this context with salience < 0.1").

#### mode

Either `Soft` or `Hard`:

- **Soft** (default): tombstone the memory; data remains for the grace period (default 7 days), then is reclaimed.
- **Hard**: tombstone AND immediately zero the vector and text.

Soft is the default — it allows undo within the grace period.

Hard is for compliance use cases (right-to-be-forgotten, sensitive data deletion).

#### request_id

Required. Provides idempotency.

### 3. The response

```rust
struct ForgetResponse {
    forgotten: Vec<MemoryId>,        // Successfully forgotten
    not_found: Vec<MemoryId>,        // Already-gone IDs (silent no-op)
    failed: Vec<(MemoryId, Error)>,  // Per-memory errors
    grace_until: Option<u64>,        // For Soft, when reclaim happens
}
```

Per-memory errors mean some IDs failed but others succeeded. The agent can retry the failures.

### 4. Idempotency

If the same RequestId is sent twice, Brain replays the original response. No double-forget.

Forgetting an already-forgotten memory is a no-op (returned in `not_found`); not an error.

### 5. The "soft forget" lifecycle

```
soft FORGET → tombstoned (active = false)
              ↓ (grace period, default 7 days)
              reclaimed (slot available for reuse)
              ↓
              MemoryId no longer valid (slot version incremented)
```

During the grace period:
- The memory doesn't appear in search results.
- The memory's MemoryId returns "not found" if queried directly (or returns the tombstoned record with `active=false`, depending on the operation).
- Operators can recover the memory via `ADMIN_RESTORE_FORGOTTEN` (if configured to allow).

After grace:
- The vector slot is zeroed and made available.
- The metadata row is deleted.
- The MemoryId is permanently invalid.

### 6. The "hard forget" lifecycle

```
hard FORGET → tombstoned + vector zeroed + text zeroed
              ↓ (grace period, default 7 days)
              reclaimed
```

The data is gone immediately. The grace period is just for slot management; there's nothing to recover.

Hard forget is irreversible. Use carefully.

### 7. The cascading question

When a memory is forgotten, what happens to:

#### Edges

Edges referencing the forgotten memory become "stale":

- Outgoing edges: tombstoned (the source is gone).
- Incoming edges: tombstoned.

Tombstoned edges are eventually cleaned up by maintenance workers.

Queries during the cleanup window may see edges that lead nowhere — they skip them.

#### Memories that DERIVED_FROM the forgotten

These keep existing. The DERIVED_FROM edge becomes a dangling reference (the source is gone). The derived memory stands on its own; only its provenance is lost.

Brain considered cascading FORGET (also forget memories DERIVED_FROM the target). Rejected — it's surprising semantics. The agent must request explicit cascade if desired.

#### Consolidations

Consolidated memories aren't auto-forgotten when their sources are. The Consolidated stands as its own memory.

### 8. The "filter forget" path

```rust
brain.forget(
    target = Filter(ForgetFilter {
        context: Some(ctx_id),
        max_salience: Some(0.1),
    })
)
```

Brain:

1. Discovers matching memories (a query-like step).
2. Forgets them in batch.

Limited to 100,000 memories per call. For larger bulk operations, the agent splits into multiple calls.

### 9. The "context delete" pattern

To delete an entire context:

```
brain.forget(target = Filter(ForgetFilter { context: Some(ctx_id) }))
brain.admin_context_delete(ctx_id)
```

Or use `ADMIN_CONTEXT_DELETE` directly (which combines the steps and handles paging for very large contexts).

### 10. The "agent delete" pattern

To delete all memories for an agent:

```
brain.admin_agent_delete(agent_id)
```

This is heavy — potentially millions of forgets. Brain processes in batches, may take minutes for large agents.

### 11. Latency

For a single FORGET:

- p50: ~1 ms.
- p99: ~5 ms.

For batch (100 IDs in one FORGET): ~5-10 ms total.

For filter-based with large match set (10K memories): ~100-500 ms.

### 12. The "dangling reference" semantic

After FORGET, an agent that holds the MemoryId externally:

- Can't RECALL it (it's tombstoned).
- Can't UPDATE_KIND it.
- Can't LINK to it (it's not a valid target).

The agent should treat held MemoryIds as potentially-stale; check via RECALL or a direct lookup before using.

### 13. The "undo" facility

For Soft FORGET, an admin operation `ADMIN_RESTORE_FORGOTTEN` can undo the forget within the grace period:

```
brain.admin_restore_forgotten(memory_id)
```

This is admin-only (not exposed to typical agents) and works only during the grace period.

After grace, restoration is impossible — the slot is gone.

### 14. Failure modes

#### MemoryNotFound

The MemoryId doesn't exist (never did, or was already reclaimed past grace). Returned in `not_found`; not an error.

#### NotOwned

The memory belongs to a different agent. Error.

#### TooManyMemories

Filter-based forget exceeds the per-call cap. Error.

#### Conflict

The memory is in a transaction held by another client. The forget waits briefly; if the transaction doesn't commit, returns `Conflict`.

### 15. The privacy guarantee

Hard FORGET zeros the vector (in the arena file) and text (in the metadata file). After the OS flushes, the bytes are no longer recoverable from the file.

Filesystem-level recovery (e.g., undelete tools) might find fragments in unallocated blocks. For paranoid deployments, Brain can also call `FALLOC_FL_PUNCH_HOLE` to encourage block release.

For full privacy guarantees, encrypt the underlying disk and rotate keys after FORGET. Brain doesn't manage encryption keys — that's deployment-level.

### 16. The reclamation timing

The grace period is configurable (default 7 days). After grace, a maintenance worker reclaims:

- Wakes periodically (every 5 min default).
- Identifies memories with `forgot_at + grace < now`.
- Reclaims in batches.

So the actual reclamation may be a few minutes after the grace expires. This isn't a problem — the memory is already invisible during grace.

### 17. The "FORGET while encoding" race

A subtle race: an ENCODE in flight while FORGET targets the same MemoryId. Possible only if:

- The agent has a stale MemoryId from a previous query.
- A new ENCODE happens to reuse the slot (after reclamation).

The slot version field prevents confusion: the old MemoryId has version N; the new memory has version N+1. The FORGET targeting version N hits the `not_found` path.

Practically, this race is rare (requires the agent to hold a stale ID across reclamation; the grace period makes this very unlikely).

### 18. The audit trail

FORGET operations are logged in the WAL (and visible in SUBSCRIBE). Operators can audit who forgot what when.

This is essential for compliance — "show me all forgets in the last 30 days".

### 19. The "forget for compliance" workflow

A typical right-to-be-forgotten flow:

```
1. Identify memories: brain.recall("user X's data", filter=...)
2. Hard forget: brain.forget(memory_ids, mode=Hard)
3. Verify: brain.recall("user X's data") returns empty
4. Audit log entry confirms.
```

Brain's hard forget zeros the bytes; the audit log preserves the forget event itself (the memory's text is gone, but the fact that something was forgotten remains).

### 20. Limits and caps

- Single FORGET: up to 1000 memories per request.
- Filter FORGET: up to 100,000 memories per request.
- Per-agent rate limit: 100 FORGETs per second.

These prevent runaway deletion. For bulk operations beyond the caps, the agent paginates.

## LINK and UNLINK

LINK creates an edge between two memories. UNLINK removes one.

### 1. LINK semantic contract

```
LINK(source, target, kind, weight, metadata, request_id) → LinkResponse
```

Brain:

1. Validates that source and target exist.
2. Inserts the edge into `edges_out` and `edges_in`.
3. Updates edge counts on both endpoints.
4. Writes a WAL record.

After LINK, the edge is visible to PLAN, REASON, and direct edge-listing operations.

### 2. The arguments

#### source

The MemoryId at the source of the edge.

#### target

The MemoryId at the target of the edge.

#### kind

One of the eight edge kinds:

- `CAUSED` — source led to target (causal precedence).
- `FOLLOWED_BY` — source then target (temporal sequence).
- `DERIVED_FROM` — target was derived from source.
- `SIMILAR_TO` — semantic similarity (often auto-derived).
- `CONTRADICTS` — they oppose each other.
- `SUPPORTS` — source supports target's claim.
- `REFERENCES` — source mentions target.
- `PART_OF` — source is part of target.

#### weight

f32 in [0, 1] (or [-1, 1] for some kinds like CONTRADICTS where negative makes sense). Default 1.0.

Higher weight = higher confidence in the relationship.

#### metadata

Optional small key-values for the edge. Stored verbatim. For things like edge annotations.

#### request_id

Required. Idempotency.

### 3. The response

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

Note: `edge_id` is computed deterministically as `(source, kind, target)` rather than being an independent ID. Two edges with the same (source, kind, target) collide; the second LINK updates the first.

### 4. UNLINK semantic contract

```
UNLINK(source, target, kind, request_id) → UnlinkResponse
```

Brain:

1. Removes the edge from `edges_out` and `edges_in`.
2. Decrements edge counts.
3. Writes a WAL record.

### 5. UNLINK arguments

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

### 6. Edge uniqueness

Each (source, kind, target) triple has at most one edge. Re-LINK overwrites:

```
brain.link(A, B, CAUSED, weight=0.5)    // creates
brain.link(A, B, CAUSED, weight=0.8)    // updates weight to 0.8
```

For applications wanting multiple edges of the same kind, use different kinds (e.g., `REFERENCES_v1`, `REFERENCES_v2`) or external versioning.

### 7. Edge direction

Edges are directed. `LINK(A, B, CAUSED)` is different from `LINK(B, A, CAUSED)`:

- The first says A caused B.
- The second says B caused A.

Some edge kinds have implicit reverse semantics (e.g., A CAUSED B implies B was caused-by A), but the storage is directed. PLAN and REASON consider both directions during traversal.

For SIMILAR_TO (which is symmetric), the convention is to LINK in one direction (typically lower-ID → higher-ID). Brain doesn't enforce this; it's an agent convention.

### 8. Edge weight semantics

The weight is a hint:

- 1.0: strong / certain.
- 0.5: moderate.
- 0.1: weak.

Used in:
- PLAN's path scoring.
- REASON's evidence scoring.
- Maintenance heuristics (low-weight edges may be pruned over time; not currently implemented).

The weight is opaque to Brain beyond these uses. Agents are free to use it as they see fit.

### 9. Edge-creation patterns

#### Inline at ENCODE

```
brain.encode(text, edges=[
    EdgeSpec(target=parent_id, kind=DERIVED_FROM),
    EdgeSpec(target=topic_id, kind=PART_OF),
])
```

Up to 64 edges in one ENCODE. Atomic with the encode.

#### Post-encode LINK

```
memory_id = brain.encode(text)
brain.link(memory_id, related_id, kind=SIMILAR_TO)
```

Two operations. The LINK can be at any time after both memories exist.

The inline form is preferred when edges are known at encode time. Post-encode LINK is for edges discovered later (e.g., after analyzing the memory).

### 10. Failure modes

#### MemoryNotFound

Source or target doesn't exist (or is tombstoned). Error.

#### InvalidKind

Kind isn't one of the 8 enumerated values. Error.

#### InvalidWeight

Weight is outside the allowed range. Error.

#### TooManyEdges

The source has reached the max-edges-per-memory limit (default 10K soft, 100K hard). Soft: warning + creation. Hard: error.

#### NotOwned

Either memory belongs to a different agent. Error.

#### CrossAgent

Source and target belong to different agents. Error — edges are agent-scoped.

### 11. Edge counts and observability

Each memory has denormalized edge counts:

- `edges_out_count`: edges where this memory is source.
- `edges_in_count`: edges where this memory is target.

Updated on every LINK / UNLINK. Useful for:
- Quickly answering "how connected is this memory?"
- Identifying highly-linked hubs.
- Dashboards.

The counts may temporarily drift due to crash-recovery edge cases. Periodic maintenance reconciles.

### 12. Auto-derived edges

Some edges are created by Brain, not the agent:

- `DERIVED_FROM`: from Consolidated memories to their Episodic sources.
- `SIMILAR_TO` (optional, off by default): between memories with cosine > 0.9.

These have `origin: AutoDerived` to distinguish from agent-created.

UNLINK can remove auto-derived edges. The maintenance worker may recreate them on its next pass (e.g., if the same condition still applies).

### 13. Latency

LINK / UNLINK latency:

- p50: ~1-2 ms.
- p99: ~5-10 ms.

Mostly the WAL fsync. Bulk LINK (many edges in one ENCODE) is more efficient.

### 14. Throughput

Per shard: ~5K-10K LINK/UNLINKs per second. Limited by the writer task's group commit.

For high-edge-volume agents (e.g., building a knowledge graph), batching via inline-ENCODE-edges is the throughput path.

### 15. Edge metadata semantics

The `metadata` field on edges is for agent annotations:

```
brain.link(A, B, REFERENCES, metadata={
    "page": "42",
    "passage": "the quick brown fox",
})
```

These are stored in the edge value. Available when the edge is read back. Not indexed; Brain doesn't query on edge metadata.

### 16. The "edge versioning" question

Brain considered making edges versioned (so the history of relationships is preserved). Rejected:

- Storage cost grows.
- Most agents don't need history.
- Agents that do can encode "edge versions" as separate edges with different kinds.

Currently, edges are last-write-wins. Updating an edge's weight overwrites the previous value.

### 17. The "transaction-bracketed" LINK pattern

For consistency, multiple LINKs can be in a single transaction:

```
txn = brain.txn_begin()
brain.link(A, B, CAUSED, txn=txn)
brain.link(B, C, CAUSED, txn=txn)
brain.txn_commit(txn)
```

Either both succeed or neither. Useful when a graph fragment must appear atomically.

Detailed in [`04_transactions.md`](04_transactions.md).

### 18. The "delete-then-relink" pattern

To change an edge's weight, you can either:

1. LINK again (overwrites): `brain.link(A, B, CAUSED, weight=0.7)`.
2. UNLINK then LINK (more explicit but two ops).

The first is recommended.

### 19. The "link or no-op" idempotency

LINK with the same (source, kind, target) and same RequestId:

- First call: creates the edge.
- Retry with same RequestId: replays original response.
- Manual re-LINK with different RequestId but same triple: overwrites (creates if not exists).

The third case is common for "ensure this edge exists" patterns. The agent doesn't need to check first.

### 20. The "edges as first-class memories" question

Should edges be queryable like memories? E.g., "find all CAUSED edges with weight > 0.8".

Currently, edges are not first-class queryables. They're attributes of memories. Listing edges from a memory is supported (via direct edge enumeration); cross-cutting edge queries aren't.

For agents that need this, they iterate memories and inspect edges in the agent layer. Future enhancement: an edge-query primitive.

## MATERIALIZE_PROCEDURAL

The MATERIALIZE_PROCEDURAL primitive: render an agent's procedural memory into a single system-prompt block.

### 1. Semantic contract

```
MATERIALIZE_PROCEDURAL(agent_id, target_predicates, request_id)
  → ProceduralBlock {
      block_text: String,            // rendered system prompt
      sourced_statements: u32,       // count of Preferences materialised
      generated_at_unix_nanos: u64,
      fingerprint: BLAKE3,           // stable hash of (statements, ordering)
    }
```

Procedural memory in Brain is not a separate data type — it is the set of `Statement{kind: Preference}` rows whose `subject` is the agent itself and whose `predicate` is one of the `brain:behavior_*` family (see [`../03_schema/06_system_schema.md`](../03_schema/06_system_schema.md) §"behavior_*"). MATERIALIZE_PROCEDURAL is the read path that turns those statements into a system-prompt-shaped block the agent re-injects at conversation start.

### 2. The arguments

#### agent_id

The agent whose procedural memory is being materialised. The op reads only Preferences with `subject = agent_id` and a `behavior_*` predicate.

#### target_predicates

Optional. A subset of the `behavior_*` predicates to include. Empty means "all behavior predicates the agent has Preferences for".

Use cases:
- A coding agent might materialise only `behavior_tone` + `behavior_style` for a code-generation flow.
- A scheduling agent might materialise only `behavior_avoids` + `behavior_constraint`.

#### request_id

Required. Standard idempotency key — the same RequestId returns the same `ProceduralBlock` for the configured TTL.

### 3. The response

`ProceduralBlock.block_text` is a single rendered string ready to inject as a system message. The renderer concatenates each Preference's surface in a stable order, prefixed by the predicate family:

```
You prefer:
- async-first communication (behavior_tone)
- terse explanations over verbose ones (behavior_style)

You avoid:
- speculation about user motives (behavior_avoids)

Hard constraints:
- never modify .env files without confirmation (behavior_constraint)
```

`sourced_statements` is the count of distinct Preferences that contributed to the block. Useful for the agent's instrumentation — a drop suggests the agent forgot about its preferences and may want to re-encode.

`fingerprint` is BLAKE3 over the canonical statement set + ordering. Stable across identical materialisations, changes when any sourced Preference is superseded or tombstoned.

### 4. Code path

1. Validate agent_id; check the connection has read scope for this agent.
2. Load active Preferences for `(subject = agent_id, predicate in behavior_*)` via `STATEMENTS_BY_SUBJECT_TABLE`.
3. Apply `target_predicates` filter if non-empty.
4. Sort by predicate then by `extracted_at_unix_nanos` ascending.
5. Render the canonical block.
6. Compute fingerprint.
7. Return.

The op is a structured read — no WAL record, no idempotency-table write. The `request_id` is used only for response-cache hits; the cache is in-memory per-shard.

### 5. Why this is a separate primitive

A client could do this themselves by issuing a `STATEMENT_LIST` with the right filters and rendering the result. MATERIALIZE_PROCEDURAL exists because:

1. **The renderer is opinionated.** Brain's renderer encodes the canonical phrasing for each `behavior_*` predicate, so the system block is consistent across agents and across clients.
2. **The fingerprint is stable.** Clients that build their own renderer get a different hash on every re-render; the server-side renderer pins canonical ordering.
3. **Future evolution.** A future version may add learning hooks (e.g., "agent updates its own preferences after a failed task") that go through the same op for auditability.

### 6. Latency promise

This is a read against a typically-small statement set (few dozen preferences per agent). p99 target: ~5 ms.

### 7. Consistency

Returns whatever the current statement set says at op time. No `ReadAfterWrite` mode — procedural memory is the agent's standing context and tolerates eventual consistency.

If an ENCODE that produces a new `behavior_*` Preference is in flight, the materialised block may not include it until the next call. This is acceptable; procedural memory is consulted at conversation start, not on every turn.

### 8. Worked example

```
agent-001 ENCODE "I prefer short answers"
  → extractor produces Statement{
      kind: Preference, subject: agent-001,
      predicate: brain:behavior_style, object: "short answers"
    }

agent-001 MATERIALIZE_PROCEDURAL
  → ProceduralBlock {
      block_text: "You prefer:\n- short answers (behavior_style)\n",
      sourced_statements: 1,
      ...
    }
```

The agent re-injects `block_text` at the start of its next conversation; the model now knows the preference without the agent re-fetching individual statements.

### 9. Failure modes

#### AgentNotFound

`agent_id` doesn't exist on this shard. Returns `AgentNotFound`.

#### SchemaNotDeclared

The agent's namespace has no schema; `behavior_*` predicates aren't available. Returns `SchemaNotDeclared`.

#### EmptyResult

No matching preferences. Returns a `ProceduralBlock` with `block_text` set to a minimal placeholder and `sourced_statements = 0` — not an error.

### 10. Wire shape

Opcode `0x0164` (request) / `0x01E4` (response). Typed-graph namespace. See [`../04_wire_protocol/03_opcodes.md`](../04_wire_protocol/03_opcodes.md) for the opcode table and [`../04_wire_protocol/08_typed_graph_frames.md`](../04_wire_protocol/08_typed_graph_frames.md) for the request/response shape.

---

*Continue to [`03_read_pipeline.md`](03_read_pipeline.md) for RECALL, PLAN, and REASON.*
