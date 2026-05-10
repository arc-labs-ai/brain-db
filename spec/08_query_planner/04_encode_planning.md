# 08.04 ENCODE Planning

How the planner builds an execution plan for an ENCODE request.

## 1. The ENCODE request shape

```rust
struct EncodeRequest {
    text: String,                // The content to encode
    agent_id: AgentId,
    context: ContextRef,         // ContextId or context name
    kind: MemoryKind,            // Episodic / Semantic
    salience_initial: f32,       // [0, 1], default 1.0
    metadata: ExtraMetadata,     // User-defined key-values; small
    edges: Vec<EdgeSpec>,        // Edges to create alongside this memory
    request_id: RequestId,       // Required for idempotency
}
```

## 2. Routing

ENCODE is agent-scoped: the new memory belongs to the requesting agent's primary shard.

```rust
fn resolve_shard(agent_id: AgentId) -> ShardId {
    router.shard_for(agent_id)
}
```

For agents whose data is split across shards (very large agents), a routing rule selects which shard hosts new encodes (typically the agent's primary shard).

## 3. The encode plan structure

```rust
struct EncodePlan {
    shard: ShardId,
    idempotency_check: IdempotencyCheckStep,
    embedding: EmbeddingStep,
    context_resolution: ContextResolutionStep,
    allocation: SlotAllocationStep,
    wal_append: WalAppendStep,
    apply: ApplyStep,
    edges: Vec<EdgeStep>,
    response: ResponseStep,
}
```

The plan describes the full encode pipeline as a sequence of steps.

## 4. Phase 1: Idempotency check

Before doing work, check if this RequestId has been seen:

```rust
fn idempotency_check(req: &EncodeRequest) -> Either<EncodeResponse, ()> {
    let cached = idempotency.get(&req.request_id);
    match cached {
        Some(entry) => Left(entry.cached_response),  // Replay
        None => Right(()),                            // Proceed
    }
}
```

If a cached response exists, return it; skip the rest of the plan. If not, proceed.

This check runs in a brief read transaction.

## 5. Phase 2: Embedding

Embed the text:

```rust
struct EmbeddingStep {
    text: String,
    cache_lookup: true,    // Check the cue cache
}
```

The embedder ([04. Embedding Layer](../04_embedding_layer/)) is called. May hit the cache (~10% hit rate) or compute fresh (~5-10 ms).

## 6. Phase 3: Context resolution

If the request specifies a context by name, resolve to a ContextId:

```rust
fn resolve_context(req: &EncodeRequest) -> ContextId {
    match req.context {
        ContextRef::Id(id) => id,
        ContextRef::Name(name) => {
            metadata.get_context_by_name(req.agent_id, name)
                .or_else(|| create_new_context(req.agent_id, name))
        }
    }
}
```

If the context doesn't exist, it's created (in the same write transaction as the encode).

## 7. Phase 4: Slot allocation

Allocate a slot in the arena ([05.02 Arena Layout](../05_storage_arena_wal/02_arena_layout.md)):

```rust
struct SlotAllocationStep {
    arena_grow_if_needed: bool,    // Yes; arena grows if at capacity
}
```

Allocation is a fast atomic operation. If the arena is near-full, growth is triggered (asynchronous; doesn't block this encode).

## 8. Phase 5: WAL append

Append the encode record to the WAL:

```rust
struct WalAppendStep {
    record: EncodeRecord {
        memory_id: MemoryId,    // Constructed from slot_id + version
        agent_id: AgentId,
        context_id: ContextId,
        text_bytes: Vec<u8>,
        vector: [f32; 384],     // 1.5 KB
        kind: MemoryKind,
        salience_initial: f32,
        edges: Vec<EdgeSpec>,
        request_id: RequestId,
    },
    fsync: true,    // Group commit
}
```

The WAL append is the durability barrier. After fsync, the encode is durable.

## 9. Phase 6: Apply

Apply the durable record to in-memory state:

```rust
struct ApplyStep {
    arena_write: bool,         // Write the vector to the slot
    metadata_write: bool,      // Insert into memories, texts tables
    hnsw_insert: bool,         // Insert into HNSW
}
```

Each sub-step happens after the durability barrier:

- Arena write: memcpy the vector to the slot. ~0.001 ms.
- Metadata write: insert in redb (in a write transaction). ~0.5 ms.
- HNSW insert: add node to the in-memory HNSW. ~0.1-1 ms.

## 10. Phase 7: Edges

For each edge in the request:

```rust
struct EdgeStep {
    edge: EdgeSpec,
    insert_in_metadata: true,    // Insert in edges_out and edges_in tables
}
```

Edge inserts are part of the same write transaction as the metadata write. Atomic.

If the edge's target memory doesn't exist, the edge is rejected (logged as a warning; the encode proceeds without it).

## 11. Phase 8: Response

Build the response:

```rust
struct ResponseStep {
    memory_id: MemoryId,
    persistent_id: bool,         // Yes; client uses this for future references
    edge_results: Vec<EdgeResult>,
}
```

The response confirms the encode and returns the new MemoryId.

## 12. The "encode now, edges later" option

If the request has many edges (>64), the planner may split the encode:

- Encode first (a fast initial response).
- Process edges in subsequent transactions.

The response indicates which edges were processed; the client may retry the rest.

In v1, we cap at 64 edges per encode and reject requests with more. The split-encode mode is a future enhancement.

## 13. Plan size

A typical EncodePlan is ~500 bytes. Each step is small.

For very large texts (~1 MB), the plan size is dominated by the text itself (passed by reference, not copy, so still small).

## 14. Special encode cases

### 14.1 Re-embedding

If the agent re-embeds an existing memory (model migration):

- This is a new ENCODE with a special `MIGRATE_OF: <existing_memory_id>` field.
- The new memory is created; the old is marked stale.

Detailed in [04.08 Migration](../04_embedding_layer/08_migration.md).

### 14.2 Bulk encode

For high-throughput bulk imports, the wire protocol's `ENCODE_BATCH` opcode (a list of ENCODE requests in one frame). The planner produces a batch plan:

- Single embedding batch (efficient).
- Single WAL group commit.
- Single metadata write transaction.
- Single HNSW batch insert.

Latency per memory drops to ~1-2 ms when batched.

### 14.3 Implicit context

If `context` is unspecified:

```rust
let context_id = metadata.get_or_create_context(agent_id, "_default");
```

Default contexts are reserved for this purpose ([07.05 Context Table](../07_metadata_graph/05_context_table.md) §6).

## 15. Plan validation

The planner ensures:

- The text is non-empty and within size limits.
- The kind is valid (not Consolidated; that's worker-only).
- The salience is in [0, 1].
- Edges have valid kinds and weights.
- The agent's quotas allow another memory.

Invalid plans return errors immediately; no work is done.

## 16. The encode latency

For a typical encode (no batching):

| Phase | Latency |
|---|---|
| Idempotency check | 5-10 µs |
| Embedding (cache hit) | 5 µs |
| Embedding (cache miss) | 5-10 ms |
| Context resolution | 5 µs |
| Slot allocation | 1 µs |
| WAL append + fsync | 0.5 ms (group commit) |
| Apply (arena, metadata, HNSW) | 1-2 ms |
| Edges (10 edges) | 0.5 ms |
| Response | 50 µs |
| **Total (cache miss)** | **~7-13 ms** |
| **Total (cache hit)** | **~2-3 ms** |

Embedding dominates the no-cache case.

## 17. The "lazy edges" option

For agents that ENCODE first and LINK later (a common pattern), the substrate doesn't add overhead — both paths are first-class.

But for agents that always encode with edges, embedding+single-write is more efficient than two round trips. The substrate makes the in-encode edges path fast.

---

*Continue to [`05_plan_reason_planning.md`](05_plan_reason_planning.md) for PLAN/REASON planning.*
