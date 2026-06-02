# 02.02 Memory

> **TL;DR.** The Memory record (Brain's central entity), its identifier formats (MemoryId, AgentId, ContextId, RequestId, ShardId), its three kinds (Episodic / Semantic / Consolidated), and its lifecycle (Active → Tombstoned → Reclaimed). One consolidated chapter for everything about an individual Memory.

The **memory** is Brain's central entity. Every operation is in service of creating, recalling, modifying, or removing memories. This file specifies what a memory *is*, how it is identified, what kinds exist, and how it moves through its lifecycle.

## 1. Conceptual definition

A memory is a single piece of agent-observed or agent-derived content, stored with enough metadata to be retrievable by similarity, by reference, or by traversal of its relationships.

The metaphor: a memory is what an agent would call to mind. Brain stores enough that recall is meaningful — not just the content, but its context, importance, time, relationships, and history.

## 2. The Memory record

A memory has the following logical fields. The on-disk and on-wire encodings are spec'd elsewhere; this is the conceptual record.

```rust
struct Memory {
    // Identity
    id: MemoryId,               // 16 bytes; opaque to clients
    agent_id: AgentId,          // 16 bytes; UUIDv7

    // Core content
    text: String,               // The raw text; persisted alongside
    vector: [f32; 384],         // L2-normalized embedding; internal

    // Classification
    kind: MemoryKind,           // Episodic | Semantic | Consolidated
    context_id: ContextId,      // 8 bytes; agent-scoped

    // Lifecycle
    state: LifecycleState,      // Active | Tombstoned | Reclaimed
    created_at: u64,            // unix_nanoseconds
    updated_at: u64,            // unix_nanoseconds
    forgot_at: Option<u64>,     // None until forgotten

    // Salience
    salience: f32,              // [0.0, 1.0]
    last_accessed_at: u64,      // unix_nanoseconds; updated on RECALL hit
    access_count: u32,          // hit counter; saturating

    // Provenance
    embedding_model_fp: [u8; 16],   // Model fingerprint
    source_request_id: RequestId,   // The request that created it

    // Relations (logical; physical storage may differ)
    edges: Vec<EdgeId>,         // Outgoing edges
}
```

The fields are detailed in subsequent sections of this spec. This file describes the entity as a whole and its core invariants.

## 3. Storage size

A typical memory's storage footprint:

| Component | Size |
|---|---|
| `vector` (384 × `f32`) | 1536 bytes |
| `slot_metadata` (flags, version, padding) | 64 bytes |
| **Arena slot total** | **1600 bytes** |
| `text` | varies; typical 100–2000 bytes |
| Metadata in redb (excl. text and edges) | ~150 bytes |
| Edge entries (avg 5 edges × ~30 bytes) | ~150 bytes |
| HNSW graph entries (avg ~16 edges × 8 bytes) | ~130 bytes |
| **Total per memory (typical)** | **~2.2 KB** |

This is the all-in cost: arena + metadata + edges + index. For 1M memories, ~2.2 GiB on disk.

## 4. The vector

Every memory has exactly one vector. The vector is:

- **Dimensionality:** 384 (set by the embedding model).
- **Element type:** `f32`.
- **Normalization:** unit L2 norm; cosine similarity reduces to dot product.
- **Production:** by the embedding layer from the memory's text.
- **Internal:** clients send text, not vectors. (Power users may send pre-computed vectors via `ENCODE_VECTOR_DIRECT`; see [01.10 OQ-5](../00_overview/04_open_questions_archive.md).)

**INVARIANT:** Every memory's vector is the embedding of its text under the embedding model identified by the memory's `embedding_model_fp`. If the embedding model changes, the memory's vector becomes stale until re-embedded.

## 5. The text

The text is the human-readable content the agent encoded. It is:

- **Encoding:** UTF-8.
- **Length:** unbounded in v1 (subject to a server-side cap, default 1 MiB).
- **Content:** opaque to Brain. Brain does not parse, validate, or rewrite the text.
- **Persisted:** always. The text is stored verbatim; it is the input to embedding and the output of `RECALL`.

The text is stored separately from the vector. The arena holds vectors; the text lives in the metadata store ([10. Metadata + Graph Store](../10_metadata/00_purpose.md) §3) for memories where it fits, or in a separate text blob store for very large memories.

## 6. Identity (overview)

A memory has two identity fields:

- **`id` (`MemoryId`)** — the opaque, public identifier. 16 bytes. Used in all client-facing operations. Encodes shard, slot, and version (§7 below).
- **`agent_id` (`AgentId`)** — the owning agent. 16 bytes (UUIDv7). Determines which shard the memory lives in.

**INVARIANT:** A memory's `agent_id` is immutable for the memory's lifetime. To "move" a memory between agents, encode a new memory under the destination agent's id and forget the original.

**INVARIANT:** A memory's `id` is unique within the cluster — no two memories ever share an id, even if one has been forgotten and reclaimed. The version field in the id ensures this.

## 7. Identifier formats

Brain uses several identifier types, each with specific format and stability properties.

| Identifier | Size | Format | Scope | Stability |
|---|---|---|---|---|
| `MemoryId` | 16 bytes | Encoded shard + slot + version + reserved | Cluster | Stable until forgotten + reclaimed |
| `AgentId` | 16 bytes | UUIDv7 | Cluster | Permanent |
| `ContextId` | 8 bytes | Server-assigned u64 | Per-agent | Permanent within agent |
| `RequestId` | 16 bytes | Client-supplied UUIDv7 | Per-agent within idempotency horizon | Bounded TTL |
| `ShardId` (storage) | 16 bytes | UUIDv7 | Cluster | Permanent |
| `ShardId` (runtime) | 2 bytes | u16, mapping table → storage UUID | Cluster | Subject to remapping during cluster ops |

### 7.1 MemoryId

The public identifier of a memory.

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|         shard_id (16)         |        slot_id (high 16)      |
+---------------------------------------------------------------+
|                  slot_id (low 32)                             |
+---------------------------------------------------------------+
|                       version (32)                            |
+---------------------------------------------------------------+
|                       reserved (32)                           |
+---------------------------------------------------------------+
```

Total: 128 bits = 16 bytes.

- `shard_id` (16 bits) — the runtime shard identifier (§7.6 below).
- `slot_id` (48 bits) — the slot within the shard's arena.
- `version` (32 bits) — incremented on each slot reuse.
- `reserved` (32 bits) — must be zero in v1; reserved for future use.

**Properties:**

- **Opaque to clients.** Clients treat `MemoryId` as an opaque 16-byte handle. They MUST NOT attempt to extract or interpret subfields.
- **Endianness:** big-endian for the on-the-wire and on-disk representations. The memory representation in Rust is `[u8; 16]`.
- **Equality:** byte-for-byte equality.
- **Ordering:** byte lexicographic ordering. (Note: this is *not* a meaningful ordering for memories — sorting by `MemoryId` produces an arbitrary order, not a temporal one.)

**Stability:** a `MemoryId` is stable from creation until the memory is forgotten *and* its slot is reclaimed. After reclamation, the slot's version increments, so the same `MemoryId` would map to a different memory only if a subsequent reclamation cycle restored the old version — which Brain forbids.

**INVARIANT:** A `MemoryId` that previously identified memory M never identifies a different memory. If M is forgotten and the slot is reused for memory M', M' has a `MemoryId` with the new (incremented) version. Old `MemoryId`s referencing the previous version detect the mismatch via the version field.

This is what the version field is *for*. Without versions, slot reuse would silently re-target stale references.

**The zero MemoryId.** The all-zero `MemoryId` (16 bytes of zero) is reserved as the "null" value. It MUST NOT be returned by any operation; clients MAY use it as a sentinel for "no memory".

### 7.2 AgentId

The identifier for an agent. [UUIDv7](https://datatracker.ietf.org/doc/rfc9562/), 16 bytes.

UUIDv7 encodes a 48-bit Unix-millisecond timestamp followed by random bits, with version and variant fields per the spec. This gives:

- Time-ordered: agents created later have lexicographically-greater UUIDs.
- Unique: collision probability vanishingly small.
- Sortable: indexes by `AgentId` cluster the most recently created agents at the end.

**Properties:**

- **Permanent.** An agent's `AgentId` never changes.
- **Cluster-scoped.** Unique across the entire cluster.
- **Client-generated.** The agent generates its own UUIDv7 at creation. Brain doesn't issue agent ids; an external identity system or the client does.

**The zero AgentId.** The all-zero `AgentId` is reserved. Operations referencing the zero `AgentId` MUST be refused with `INVALID_ARGUMENT`.

### 7.3 ContextId

A logical scope within an agent's memory. A 64-bit unsigned integer.

**Properties:**

- **Agent-scoped.** Two different agents can both have `context_id = 1`; they are unrelated.
- **Server-assigned.** When an agent first references a context name (a string), the server assigns a `context_id` and persists the name → id mapping.
- **Permanent within an agent.** Once a `context_id` is assigned, it never changes; if a context is "deleted" (out of scope for v1), the id is retired, not reused.

**The default context.** `context_id = 0` is reserved for the default context. Every agent automatically has a context with id 0 named "default". Memories encoded without an explicit context land in the default.

### 7.4 RequestId

The client-supplied idempotency token for `ENCODE` and `FORGET`. [UUIDv7](https://datatracker.ietf.org/doc/rfc9562/), 16 bytes. Recommended but not strictly enforced; any 16 bytes are accepted.

**Properties:**

- **Agent-scoped.** Idempotency is checked within a single agent's namespace.
- **Bounded TTL.** Stored in the idempotency table for a configurable window (default: 5 minutes). After the TTL, the same `RequestId` may be treated as a new operation.
- **Single-use semantically.** Within the TTL, a duplicate `RequestId` results in the original operation's response being replayed. Different operations submitted with the same `RequestId` is an error.

**Why UUIDv7.** UUIDv7's time-ordering helps the idempotency table — old entries are toward the front of the time-ordered keyspace and easy to expire in batch. Other UUID versions or random tokens work but lose this property. The protocol accepts any 16 bytes; UUIDv7 is the recommendation.

### 7.5 ShardId — two senses

The shard identifier has two senses depending on context. Confusing them is a frequent source of bugs.

**Storage shard ID.** The persistent identifier of a shard, used in:

- Storage filesystem paths: `data/<storage_shard_uuid>/arena.bin`.
- Backup and snapshot metadata.
- Cluster control plane records.

Format: UUIDv7, 16 bytes. **Permanent.** A shard's storage UUID is set when the shard is created and never changes. Even when the shard is moved between nodes, its storage UUID stays the same.

**Runtime shard ID.** The compact identifier of a shard, used in:

- The high 16 bits of every `MemoryId`.
- Routing tables.
- Wire-format frames.

Format: 16-bit unsigned integer. **Subject to remapping.** Up to 65,535 shards per cluster (id 0 reserved). The mapping from runtime id → storage UUID is maintained by the cluster control plane.

**The mapping.** A control-plane table:

```
runtime_shard_id | storage_shard_uuid                        | epoch
-----------------+-------------------------------------------+-------
1                | 0190a8e1-0001-7000-8000-000000000001      | 5
2                | 0190a8e1-0001-7000-8000-000000000002      | 5
...
```

The `epoch` field tracks generation; a node sees the table as of some epoch and refuses to operate on later versions until it refreshes. Specified in [16. Sharding + Clustering](../16_sharding/00_purpose.md).

**Why two senses.** The runtime id is small (16 bits) so MemoryIds fit in 16 bytes. The storage UUID is permanent so backups and disaster recovery aren't broken by cluster reorganization.

Mapping between them is a control-plane concern. Clients see only `MemoryId` (which embeds the runtime id at the time the memory was created) and `AgentId` (from which routing computes the runtime id). They never see the storage UUID directly.

**Implication for stale MemoryIds.** A `MemoryId` was created when its memory's shard had runtime id S. If the shard is later renumbered (due to cluster reorganization), the `MemoryId` still encodes S in its high 16 bits — but S now maps to a different storage UUID.

Resolution: the cluster control plane records the *historical* mapping. A `MemoryId` created at epoch E uses the runtime → storage mapping in effect at epoch E. The router resolves it using the historical mapping, not just the current one.

This is rare in practice (cluster reorganization is infrequent), but the mechanism exists to avoid breaking client-cached `MemoryId`s.

### 7.6 Identifier collisions

By construction, no two memories share a `MemoryId`. The combination of (shard_id, slot_id, version) is unique within the shard, and shard_id is unique within the cluster.

UUIDv7 collision probability (for `AgentId`, `RequestId`, storage UUID): ~1 in 2^62 within a millisecond, vanishingly small in practice. Even an organization creating a billion agents per day has effectively zero collision probability.

Runtime shard id is bounded at 65,535 shards per cluster. This is a soft cap; if you need more, add a separate cluster.

### 7.7 Wire and storage representations

| Identifier | Wire (bytes, big-endian) | Storage (bytes, native) |
|---|---|---|
| `MemoryId` | 16, fixed | 16, fixed |
| `AgentId` | 16, fixed | 16, fixed |
| `ContextId` | 8, fixed | 8, fixed (host endianness) |
| `RequestId` | 16, fixed | 16, fixed |
| `ShardId` (storage) | 16, fixed | 16, fixed |
| `ShardId` (runtime) | 2, fixed | 2, fixed (host endianness) |

The wire formats use big-endian for portability. Storage formats use host endianness for performance; cross-architecture migration requires byte-swapping (out of scope for v1; same-architecture restore is the supported path).

## 8. Provenance

Two fields track where a memory came from:

- **`embedding_model_fp`** — fingerprint of the model that produced the vector. 16 bytes (BLAKE3-derived). Used to detect cross-model query attempts and to drive model migration.
- **`source_request_id`** — the `request_id` from the `ENCODE` operation that created the memory. Lets clients trace from a write back to the resulting memory id.

**INVARIANT:** A memory's `embedding_model_fp` is set at encode time and is immutable for the lifetime of the memory. If the model changes, the memory must be re-embedded (which produces a new vector but preserves the memory's id and other metadata).

## 9. Salience

A single number in [0, 1]. The full model — initial computation, update on access, decay over time, normalization — is in [`04_salience.md`](04_salience.md).

The high-level summary: high-salience memories are returned earlier in `RECALL` results and decay more slowly. Low-salience memories rank lower and decay faster. Salience updates happen on access (raise) and via the background decay worker (lower).

## 10. Context

Every memory belongs to exactly one context. The `context_id` field references a context defined in [`03_context.md`](03_context.md). Contexts are agent-scoped — `context_id` 1 in agent A is unrelated to `context_id` 1 in agent B.

The default context (`context_id = 0`) is automatically present in every agent. Memories without an explicit context belong to the default.

**INVARIANT:** A memory's `context_id` is mutable via an admin operation, but not by the agent on the hot path. Once encoded into a context, the memory stays there unless explicitly migrated.

## 11. Memory kinds

Three kinds: `Episodic`, `Semantic`, `Consolidated`. The kind influences how the memory is treated by salience, decay, ranking, and the consolidation worker.

The trichotomy comes from the cognitive-science distinction between [episodic and semantic memory](https://en.wikipedia.org/wiki/Explicit_memory) (Tulving 1972). Brain adds a third kind, `Consolidated`, to represent memories explicitly produced by the consolidation worker — distinct from both raw episodes and pure abstract knowledge.

```rust
enum MemoryKind {
    Episodic = 0,
    Semantic = 1,
    Consolidated = 2,
}
```

Numeric values are stable; persistence uses the integer encoding.

### 11.1 Episodic

A specific event, observation, or experience. Tied to time and (often) place.

**Examples:**

- "User said 'I prefer dark mode' at 14:32 today."
- "Email from Alice arrived at 09:15 mentioning the budget."
- "I (the agent) decided to break up the task into three steps after seeing the failed attempt."
- "The deployment script returned exit code 137."

Each is a single, time-bound observation.

`Episodic` is the **default** kind for `ENCODE`. Most agent observations are episodic; the agent is observing a stream of events and recording them.

**Salience treatment:**

- **Initial salience** — kind weight 0.5 (moderate).
- **Decay** — half-life 30 days (faster). Episodic memories fade as they age.
- **Eviction eligibility** — episodic memories are the primary candidates for eviction once salience falls below threshold.

**Consolidation candidate.** Episodic memories are the *input* to consolidation. The consolidation worker scans episodic memories and clusters related ones into `Consolidated` memories. After consolidation, the original episodic memories may be retained (their salience was high enough) or evicted (their salience was below threshold and consolidation captured the essence). The decision is salience-driven; consolidation doesn't automatically delete its sources.

### 11.2 Semantic

Stable knowledge that's not tied to a specific event.

**Examples:**

- "The user prefers dark mode." (a stable fact, not the specific moment of stating it)
- "Budget approval requires CFO sign-off." (a rule, not a specific approval event)
- "Python uses reference counting plus a generational GC." (general knowledge)

Each is an abstract claim, not tied to a specific observation moment.

**Creation.** Semantic memories are created in two ways:

- **Explicit by the agent.** The agent encodes a memory with `kind = Semantic`, marking it as a stable claim.
- **Promotion from Consolidated.** A consolidated memory may be promoted to semantic when its salience accumulates enough — i.e., Brain observes that the pattern is robust and durable.

There is **no automatic episodic-to-semantic promotion** in v1. Promotion requires either the agent's explicit choice or the consolidation pipeline.

**Salience treatment:**

- **Initial salience** — kind weight 0.7 (higher).
- **Decay** — half-life 365 days (much slower). Semantic memories fade slowly.
- **Eviction eligibility** — semantic memories are the *last* to be evicted. Brain is reluctant to let go of stable knowledge.

Semantic memories are *not* consolidation candidates as input. They are the abstraction layer above what consolidation produces.

### 11.3 Consolidated

A summary or pattern derived from multiple episodic memories by the consolidation worker.

**Examples:**

- "User prefers dark interfaces" (derived from many episodic observations of dark-mode-related interactions).
- "Deployments often fail on Friday afternoons" (derived from a pattern in deployment-event memories).
- "The customer support team escalates X-class issues to engineering" (derived from observations of escalation events).

Each captures a pattern Brain noticed across multiple episodic memories.

**Creation.** `Consolidated` memories are created exclusively by the consolidation worker. Agents do **not** explicitly create consolidated memories — the agent encoding a summary directly should mark it as `Semantic`, not `Consolidated`. The `Consolidated` kind is reserved for Brain's automated outputs.

Each `Consolidated` memory has `DERIVED_FROM` edges pointing to its source episodic memories, providing provenance.

**Salience treatment:**

- **Initial salience** — kind weight 0.6 (between episodic and semantic).
- **Decay** — half-life 90 days (between episodic and semantic).
- **Eviction eligibility** — moderate. Consolidated memories age out slower than episodic but faster than semantic.

**Promotion.** If a consolidated memory's salience climbs (e.g., it's frequently accessed and confirmed by additional observations), it becomes a candidate for promotion to `Semantic`. The promotion threshold is configurable (default: salience ≥ 0.85 sustained for ≥ 30 days). Promotion changes the kind, recomputes the decay, and clears most salience boost (resets to a baseline). The memory's content is unchanged.

### 11.4 Kind transitions

Allowed transitions:

| From | To | Trigger |
|---|---|---|
| Episodic | Semantic | Agent explicit (via `ADMIN_RECLASSIFY` or similar) |
| Episodic | (consolidated as DERIVED_FROM) | Consolidation worker; the episodic memory itself stays episodic; a new Consolidated memory is created |
| Consolidated | Semantic | Promotion based on salience and confirmation |
| Semantic | Episodic | Not allowed |
| Semantic | Consolidated | Not allowed |
| Consolidated | Episodic | Not allowed |

The disallowed transitions exist because they don't make sense — semantic knowledge degrading to a single episodic event is not a coherent operation; consolidated patterns becoming events is similarly nonsense.

### 11.5 Effect on operations

**RECALL ranking.** Different kinds rank differently when other factors are equal:

- For "What did the user say?" — episodic memories rank higher (recent specific events).
- For "What does the user prefer?" — semantic memories rank higher (stable knowledge).
- For "What patterns has the user shown?" — consolidated memories rank higher.

Brain doesn't auto-detect the question type; the agent's filter parameter (or the planner's heuristics) selects which kind(s) to weight.

**PLAN.** Planning generally prefers semantic and consolidated memories as world-model facts; episodic memories are evidence but typically not the planning state.

**REASON.** Reasoning uses all three kinds: episodic memories as evidence, semantic memories as rules, consolidated memories as patterns.

**FORGET.** Forgetting works the same on all kinds. The mode (soft/hard) is the user's choice; the kind doesn't affect the operation.

**Filters by kind.** `RECALL` accepts an optional `kind_filter`:

- `None` — all kinds (default).
- `Some([Episodic])` — only episodic.
- `Some([Semantic, Consolidated])` — exclude episodic.

The set form lets the agent narrow to the kinds it expects (e.g., "tell me what's stable about the user" → semantic + consolidated).

### 11.6 Kind defaults summary

| Kind | Initial salience weight | Decay half-life | Eviction priority | Created by |
|---|---|---|---|---|
| Episodic | 0.5 | 30 days | Highest (first) | Agent (default) |
| Semantic | 0.7 | 365 days | Lowest (last) | Agent (explicit), promotion |
| Consolidated | 0.6 | 90 days | Middle | Consolidation worker only |

These constants are tunable. The relative ordering (episodic decays fastest, semantic decays slowest, consolidated in between) is the design choice; the specific numbers are calibrated against observed agent behavior on benchmark workloads.

### 11.7 Why three kinds, not more

Brain considered:

- **One kind.** Lose the distinction; everything is treated equally. Salience alone wouldn't capture the differences in cognitive role.
- **Two kinds (episodic, semantic).** Closer to classical cognitive science. Loses the explicit category for "things Brain noticed", which is what `Consolidated` represents.
- **Many kinds** (working, autobiographical, procedural, declarative, etc.). Most of the additional categories don't have clear operational distinctions in Brain. Procedural memory is an interesting candidate (deferred to a future version; see [01.10 OQ-8](../00_overview/04_open_questions_archive.md)).

The chosen three are well-grounded: two from classical theory plus one operational category for Brain-derived patterns. They map onto distinct decay profiles, distinct salience baselines, and distinct creation paths.

### 11.8 Wire and storage representation

| Context | Representation |
|---|---|
| Wire (CBOR-encoded) | u8 (0=Episodic, 1=Semantic, 2=Consolidated) |
| Storage (redb) | u8 |
| Memory record (in-memory) | enum `MemoryKind` |
| Client (typed) | language-native enum |

The on-the-wire and on-disk values are stable. New kinds (in a future major version) would be added with new numeric values; old readers seeing an unknown kind reject it.

## 12. Edges

A memory carries a list of outgoing edges. Each edge is a typed link to another memory: `(target_memory_id, edge_kind, weight)`. Specified in [`05_edges.md`](05_edges.md).

Edges may be:

- **Explicit** — set by the agent at encode time or via a separate `LINK` operation (deferred to [01.10 OQ-10](../00_overview/04_open_questions_archive.md)).
- **Auto-derived** — added by background workers based on similarity, temporal adjacency, or causal patterns.

Incoming edges (other memories pointing at this one) are **not** stored on the memory itself; they are reconstructable from the metadata store's edge index.

## 13. Lifecycle

A memory is born, lives, may be forgotten, and eventually has its slot reclaimed.

### 13.1 The state machine

```
              ┌─────────────────────────────────────────┐
              │                                         │
              ▼                                         │
            None        ENCODE              FORGET      │   reclaim
        (no memory)  ──────────►  Active  ─────────►  Tombstoned
                                    ▲   ▲                │
                                    │   │                │
                                    │   │ promotion      │
                                    │   │                │
                                    │  Semantic ←──── Consolidated
                                    │  /Cons.
                                    │
                            (kind transitions
                             — same Active state,
                             different kind)
              ┌──────────────────────────────────────────┘
              ▼
          Reclaimed (slot now holds a new memory with incremented version)
```

Three states: `Active`, `Tombstoned`, `Reclaimed`. The kind transitions (§11.4) happen within the `Active` state.

### 13.2 State definitions

**Active.** The memory is queryable. All `RECALL`, `PLAN`, `REASON` operations may match it.

- The slot is occupied with the memory's vector.
- Metadata in redb is current.
- HNSW index includes the memory.
- Salience updates and decay apply.

**Tombstoned.** The memory has been forgotten but the slot has not yet been reclaimed.

- The slot is still occupied (vector and metadata unchanged from when it was forgotten).
- The memory is hidden from queries — operations skip tombstoned slots.
- HNSW index entries are removed.
- Edges are tagged for cleanup but may still be present.
- The slot is eligible for reuse on the next `ENCODE` allocation.

The tombstone state is brief in normal operation — typically a few seconds to a few minutes between forget and reclaim. It is observable to integrity-checking tools but never to client queries.

**Reclaimed.** The slot has been reused for a different memory. The previous memory's `MemoryId` is no longer valid for any active memory.

- The slot is occupied with a new memory's data.
- The new memory has an incremented `version` field.
- The old `MemoryId` (with the previous version) doesn't match any current memory.

`Reclaimed` is not a state of the *original* memory — it's a description of what happened to the slot. The original memory ceases to exist; the slot lives on, holding something new.

### 13.3 Lifecycle transitions

**None → Active (creation).** Triggered by `ENCODE`. The transition involves:

1. Allocate a slot in the arena (or reuse a tombstoned slot, incrementing version).
2. Embed the text into a vector.
3. Compute initial salience.
4. Write a WAL record (the durability barrier).
5. fsync the WAL.
6. Write the vector to the arena slot.
7. Update metadata in redb.
8. Insert into HNSW.
9. Publish the new `MemoryId` to clients (epoch advance).

The memory is `Active` once step 9 completes. The full sequence is detailed in [08. Storage: Arena & WAL](../08_storage/00_purpose.md) §4.

**Active → Tombstoned (FORGET).** Triggered by `FORGET`. The transition involves:

1. Validate that the `MemoryId` references an active memory owned by the requesting agent.
2. Write a `FORGET` record to the WAL.
3. fsync the WAL.
4. Mark the slot as tombstoned in metadata.
5. Remove the entry from the HNSW index.
6. Hide the memory from queries (epoch advance).
7. Set `forgot_at` timestamp.

For **soft forget**, the slot's data (vector and text) is preserved. The slot is eligible for reuse but the data is recoverable until reuse happens.

For **hard forget**, additional steps:

8. Overwrite the vector with zeros.
9. Clear the text.
10. Schedule the slot for immediate reclamation.

Hard forget makes the memory's content unrecoverable even via filesystem-level inspection. It's the right choice for compliance-driven removals (GDPR right-to-erasure, accidental encoding of secrets).

**Tombstoned → Reclaimed (slot reuse).** Triggered when the next `ENCODE` allocates from the free list and picks this slot.

1. The new `ENCODE` operation finds the tombstoned slot in the per-shard free list.
2. The slot's `version` field is incremented (a 32-bit counter; at saturation the slot is permanently retired).
3. The slot is written with the new memory's vector and metadata.
4. The old memory's edges are cleaned up (incoming edges pointing at the old memory's ID are now stale; they're filtered out lazily during traversal).
5. The new memory is published.

After this, the original memory is gone; the slot holds the new memory.

The version increment ensures stale `MemoryId`s referencing the old memory cleanly mismatch — the lookup `MemoryId.version != slot.version` returns `MemoryNotFound` rather than silently returning the new memory.

**Active → Active (kind transitions).** Several kind changes happen within the `Active` state:

- Episodic → Semantic (via `ADMIN_RECLASSIFY` or agent operation).
- Consolidated → Semantic (via promotion).

These are not lifecycle transitions in the sense of changing visibility or storage; they're metadata updates while the memory remains active and queryable. The mechanics are documented in §11.4.

### 13.4 Background-driven transitions

**Eviction (Active → Tombstoned).** The decay/consolidation worker may *evict* a memory whose salience has fallen below the eviction threshold. Eviction is functionally a `FORGET`:

- WAL record (with origin = "eviction").
- HNSW removal.
- Tombstone mark.

The eviction runs in soft-forget mode by default. The slot's data is preserved until reclaimed; this lets `ADMIN_RESTORE_RECENT` recover unintended evictions within a short window.

**Consolidation (Active → Active, with creation).** The consolidation worker doesn't change a single memory's lifecycle; it creates new `Consolidated` memories from existing episodic ones. The original episodic memories remain `Active` (and may be evicted later based on salience).

Note: an aggressive consolidation policy could choose to evict the source episodic memories after consolidating them, freeing space. Brain's default policy is non-aggressive — episodic memories are kept unless their own salience falls below the eviction threshold.

### 13.5 State observability

What state is observable from outside:

- **`Active`** — fully visible. All fields readable, all queries return it.
- **`Tombstoned`** — invisible to client queries; visible to admin tools (`ADMIN_LIST_TOMBSTONED`).
- **`Reclaimed`** — the original memory is gone. The slot now holds a different memory; querying with the old `MemoryId` returns `MemoryNotFound`.

Clients should not rely on observing tombstoned state. The contract is "after `FORGET` returns, the memory is no longer queryable"; whether it's still on disk briefly is an implementation detail.

### 13.6 Slot version counter

The slot's `version` field is critical for lifecycle correctness.

**Format.** A 32-bit unsigned integer. Initial value: 1. Incremented each time the slot is reclaimed.

**Saturation.** When the version reaches `u32::MAX` (2^32 - 1, ≈ 4 billion), the slot is **permanently retired**. The next reclamation would wrap to 0, which Brain forbids (it would silently re-validate stale `MemoryId`s).

In practice, no real workload reaches this. A workload that reclaims a single slot 4 billion times is doing something pathological; Brain treats the saturation as a signal that something is wrong.

**Why 32 bits.** Brain uses 32 bits over 16 (which would saturate too quickly under churn) or 64 (which would expand `MemoryId` beyond 16 bytes). 32 bits gives ~4 billion reclamations per slot, which is more than enough.

### 13.7 Boundary cases

**FORGET on a tombstoned memory.** The original `MemoryId` references the tombstoned (or already-reclaimed) memory. Behavior:

- If tombstoned: idempotent. The `FORGET` returns success with `was_already_forgotten = true`.
- If reclaimed: the `MemoryId` is stale (version mismatch). Returns `MemoryNotFound`.

**RECALL race with FORGET.** A `RECALL` is in flight when `FORGET` arrives for one of the candidate memories. Behavior depends on timing:

- If the `RECALL` already returned the memory: the result is sent; the next `RECALL` won't include this memory (it's tombstoned).
- If the `RECALL` is still scoring candidates: the tombstoned memory is filtered out.

This is the epoch-based reclamation model; details are in [14. Concurrency + Epoch Model](../14_concurrency/00_purpose.md).

**Crash during ENCODE.** If the server crashes between WAL fsync and full publication of the memory:

- WAL was durably written → recovery replays the encode → memory is published after recovery.
- WAL fsync hadn't completed → the encode is treated as never having happened.

The client, in either case, retries with the same `request_id`. If the encode succeeded pre-crash, the retry is deduplicated and returns the original `MemoryId`. If it didn't succeed, the retry creates the memory.

**Crash during FORGET.** Similar logic: if the WAL `FORGET` record was durable, recovery completes the forget. If not, the memory is still active after recovery; the client's retry will succeed.

### 13.8 Lifecycle timeline

A typical memory's timeline:

```
Time:  0s     0.01s    1s        100s        1d        90d         180d        365d
       │        │      │           │           │          │            │           │
       │        │      │           │           │          │            │           │
       │   ENCODE     RECALL    RECALL      decay      consolidation  forgotten?  evicted?
       │   begins  →  hits      hits         starts     creates new   if low      if no
       │              salience  salience     to bite    Consolidated  salience    activity
       │              boost     boost                    memory
       │
       (None state)
        Active state ──────────────────────────────────────────────────────►
                                                                                  Tombstoned ────►
                                                                                                  Reclaimed
```

The timeline above is illustrative — actual durations depend on the workload, the salience trajectory, and the agent's access patterns.

### 13.9 Memory edge cleanup on lifecycle change

When a memory transitions to `Tombstoned`:

- **Outgoing edges** (owned by the memory) are removed from the by-source index.
- **Incoming edges** (where this memory is the target) are tagged for lazy cleanup. They remain in the by-target index until the slot is reclaimed.

When the slot is `Reclaimed`:

- The by-target index entries pointing to this slot's `MemoryId` (with the old version) are scheduled for deletion in the next index-maintenance pass.
- The new memory's edges are added fresh.

The lazy-cleanup of incoming edges is a deliberate trade-off. Eager cleanup would require scanning every other memory's outgoing edges to find references — expensive. Lazy cleanup defers the cost until the slot is reused, when Brain has to rewrite the index anyway.

## 14. Mutability

A memory's fields are mutable in different degrees:

| Field | Mutability | When |
|---|---|---|
| `id`, `agent_id` | Immutable | Set at encode |
| `text`, `vector` | Immutable | Set at encode (re-embedded only on model migration) |
| `kind` | Mutable | By agent operation; consolidation may also change it |
| `context_id` | Mutable | By admin operation |
| `state` | Mutable | By forget, by reclamation |
| `salience` | Mutable | By access, by background decay |
| `last_accessed_at`, `access_count` | Mutable | On every access |
| `created_at` | Immutable | Set at encode |
| `updated_at` | Mutable | On any non-access mutation |
| `forgot_at` | Set once | On `FORGET` |
| `embedding_model_fp` | Mutable | Only on model migration |
| `source_request_id` | Immutable | Set at encode |
| `edges` | Mutable | Edges added/removed over time |

Most mutations happen via specific operations (`ENCODE` updates several fields atomically; access updates salience and timestamps; `FORGET` sets state and `forgot_at`).

## 15. Equality

Two memories are equal iff they have the same `MemoryId`. Other fields can differ — for instance, after consolidation, a memory may have updated `kind` and `salience`, but it's still the same memory.

There is **no** "content equality" notion in the data model. Two memories with identical text but different ids are different memories. (They might have been encoded by different agents, in different contexts, or at different times.)

Deduplication of identical content is handled at encode time, not by data-model equality. See [05. Operations](../05_operations/00_purpose.md) §ENCODE.

## 16. Validation

What makes a memory record valid:

- `id` non-zero, with valid shard/slot/version components.
- `agent_id` non-zero, valid UUIDv7.
- `text` valid UTF-8, length within configured cap.
- `vector` exactly 384 elements; L2 norm in `[1.0 - epsilon, 1.0 + epsilon]` (epsilon = 1e-4).
- `kind` is one of the three valid values.
- `context_id` references an existing context for `agent_id`.
- `state` is one of the three valid values.
- `created_at` ≤ `updated_at`.
- If `state == Tombstoned`, `forgot_at` is `Some` and ≤ current time.
- `salience` in [0, 1].
- `embedding_model_fp` matches a known model.
- All `EdgeId` references resolve to valid edges.

A record violating any of these is corrupted; recovery procedures are in [18. Failure Modes + Recovery](../18_failure_recovery/00_purpose.md) §Data Integrity.

