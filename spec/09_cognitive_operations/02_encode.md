# 09.02 ENCODE

The ENCODE primitive: store a memory.

## 1. Semantic contract

```
ENCODE(text, agent_id, context, kind, metadata, edges, request_id)
  → MemoryId
```

The substrate:

1. Embeds the text into a vector.
2. Allocates a slot in the arena.
3. Writes a WAL record (durability barrier).
4. Updates metadata, HNSW, and edge tables.
5. Returns a stable MemoryId.

After the response, the memory is durable, searchable, and connected.

## 2. The arguments

### text

The content to encode. UTF-8. 1 byte to ~1 MB (configurable upper bound).

The text is what's embedded; it determines the vector and thus where the memory sits in the similarity space.

For very long text (> the model's context, typically ~2000 chars), the embedder truncates. The full text is still stored; only the embedding is truncated.

### agent_id

The owning agent. Authentication ensures the caller can encode under this agent_id.

### context

A `ContextRef` — either a name (resolved to a ContextId) or an explicit ContextId.

If unspecified: the agent's default context is used.

If a name doesn't exist: the substrate creates the context.

### kind

One of `Episodic` or `Semantic`. (`Consolidated` is worker-only — clients can't directly create Consolidated memories; they're produced by the consolidation worker.)

If unspecified: defaults to `Episodic`.

### metadata

Key-value pairs of extra fields, agent-specific. Limited to a few KB total.

These are stored verbatim and available in the metadata-include responses. Brain doesn't index them; they're just blobs.

### edges

A list of `EdgeSpec` — edges to create alongside this memory. Each edge has:

- target: another MemoryId.
- kind: `EdgeKind`.
- weight: f32 in [0, 1] (default 1.0).

If a target memory doesn't exist, that edge is rejected (logged); the encode itself proceeds.

Up to 64 edges per encode (configurable).

### request_id

Required. A `RequestId` for idempotency. The same RequestId returns the original response for retries.

## 3. The response

```rust
struct EncodeResponse {
    memory_id: MemoryId,
    edge_results: Vec<EdgeResult>,    // Per-edge success/error
    persisted_at: u64,                // The substrate's timestamp
    fingerprint: ModelFingerprint,    // The model that produced this vector
}
```

The MemoryId is the agent's primary handle. Stable. Use it to refer to this memory in all future operations.

## 4. Idempotency

If the same RequestId is sent twice (e.g., due to network retry):

- The substrate returns the original response.
- No duplicate memory is created.
- No additional WAL record is written.

This is the substrate's commitment: at-most-once execution per RequestId, with replay-safe responses.

## 5. The "what gets stored" question

After ENCODE, the substrate has:

- The vector (in the arena).
- The text (in the `texts` table).
- The metadata (in the `memories` table).
- Edges (in the edge tables).
- A WAL record describing the encode.

The text and vector together let Brain serve future queries. The metadata describes what the memory is.

## 6. The "what gets searched" question

After ENCODE, RECALL finds the new memory if its vector is close to a cue's vector. The substrate's HNSW publishes the new node typically within 10 ms after the ENCODE response.

For read-after-write: a recall with `consistency=ReadAfterWrite` waits until the new memory is in the searchable HNSW.

## 7. Failure modes

### EmbeddingFailed

The embedder couldn't process the text (invalid UTF-8, too long, embedder unavailable).

The encode fails; no memory is created.

### QuotaExceeded

The agent has too many memories or contexts.

The encode fails; no memory is created.

### ContextLimitReached

The context has too many memories (configurable per-context limit).

The encode fails.

### InvalidEdge

An edge specifies a non-existent target, an invalid kind, or violates other constraints.

The encode succeeds; the bad edge is logged but not created. The response indicates which edges were created.

### TooManyEdges

The encode specifies more than 64 edges.

The encode fails entirely; the agent should split into multiple operations.

## 8. The "context-derived inheritance"

A context can have default settings (kind, metadata defaults). When ENCODE doesn't specify these, they're inherited from the context.

This isn't implemented in v1. Currently each ENCODE specifies its own kind explicitly (or uses the global default of Episodic).

## 9. The "create new context" semantic

If the context name doesn't exist, ENCODE creates it. The new context inherits default settings.

This makes the agent's first ENCODE in a new context "just work" — no separate context-creation step.

If the agent wants to ensure a context's settings (kind defaults, etc.) before encoding, it can use `ADMIN_CONTEXT_CREATE` first.

## 10. The "small text" case

For very short texts (a few words), the embedding is still meaningful but less informative. RECALL on short cues is also less precise.

The substrate doesn't have a minimum text length; even a single character is encodable. The agent decides what's worth encoding.

## 11. The latency promise

For typical workloads:

- p50: ~5-10 ms.
- p99: ~25 ms.

The latency is dominated by the embedder. With cache hits (~10% of cues), latency drops to ~2-3 ms.

For batched encodes, throughput is much higher; per-encode latency rises slightly due to batching delay but throughput goes up to ~10K/sec/shard.

## 12. The "encode-and-recall" loop

A common pattern:

```
brain.encode("first thing", request_id=1)
brain.encode("second thing", request_id=2)
results = brain.recall("things", consistency=ReadAfterWrite)
```

Without `consistency=ReadAfterWrite`, the recall might miss the just-encoded memories (HNSW publication lag). With it, the recall waits.

Most workloads don't need this; the substrate's eventual consistency is fine for typical agent behaviors.

## 13. The "agent's context" semantics

A memory belongs to exactly one agent and one context. Cross-agent or cross-context relationships need explicit edges (which can span contexts within an agent, but not across agents).

This means a memory can't be "shared" between agents — each agent has its own memory store. If a single piece of text is relevant to multiple agents, each encodes its own copy.

This is a deliberate design. Cross-agent memory sharing has complex semantics (who can update what, who can forget) and isn't a use case Brain optimizes for.

## 14. The "consolidated" cannot-be-encoded rule

A client can encode `Episodic` or `Semantic` memories. Not `Consolidated` — those are worker-only.

If a client tries `kind: Consolidated`, ENCODE returns `InvalidRequest`. To create a consolidated-like memory directly, the client uses `Semantic`.

## 15. The encode is a "single-write commit"

ENCODE is one atomic operation. After it succeeds:

- The memory exists.
- All specified edges (that were valid) exist.
- The metadata is current.

If the encode fails, none of these exist (partial state isn't visible to other clients).

## 16. The "encode fails partway" guarantee

If the substrate crashes between WAL fsync and the ack:

- The memory is durable (in the WAL).
- The substrate's in-memory state may be incomplete.
- On recovery, the WAL is replayed; the memory becomes fully visible.
- The client may see a network error; on retry with the same RequestId, the cached response is returned.

So the agent never sees a half-encoded memory.

---

*Continue to [`03_recall.md`](03_recall.md) for RECALL.*
