# 02.02 Memory

> **TL;DR.** The Memory record (Brain's central entity), its identifier formats (MemoryId, SpaceId, SessionId, RequestId, ShardId), its three kinds (Episodic / Semantic / Consolidated), and its lifecycle (Active → Tombstoned → Reclaimed). One consolidated chapter for everything about an individual Memory.

The **memory** is Brain's central entity. Every operation is in service of creating, recalling, modifying, or removing memories. This file specifies what a memory *is*, how it is identified, what kinds exist, and how it moves through its lifecycle.

## 1. Conceptual definition

A memory is a single piece of agent-observed or agent-derived content, stored with enough metadata to be retrievable by similarity, by reference, or by traversal of its relationships.

The metaphor: a memory is what an agent would call to mind. Brain stores enough that recall is meaningful — not just the content, but its session, importance, time, relationships, and history.

## 2. The Memory record

A memory has the following logical fields. The on-disk and on-wire encodings are spec'd elsewhere; this is the conceptual record.

```rust
struct Memory {
    // Identity (owner scope)
    id: MemoryId,               // 16 bytes; opaque to clients
    namespace_id: NamespaceId,  // 4 bytes; owning tenant. 0 = reserved `brain` system namespace
    space_id: SpaceId,          // 16 bytes; UUIDv7

    // Core content
    text: String,               // The raw text; persisted alongside
    vector: [f32; 384],         // L2-normalized embedding; internal

    // Classification
    kind: MemoryKind,           // Episodic | Semantic | Consolidated
    session_id: SessionId,      // 8 bytes; space-scoped

    // Lifecycle
    state: LifecycleState,      // Active | Tombstoned | Reclaimed
    created_at: u64,            // unix_nanoseconds; server write time
    updated_at: u64,            // unix_nanoseconds
    occurred_at: Option<u64>,   // unix_nanoseconds; client event time, None when unsupplied
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

**Owner scope (`namespace_id` + `space_id`).** Every memory is owned by exactly one `(namespace, space)` tenant pair. `namespace_id` is the outer wall (the company-level tenant); `space_id` is the inner wall (the app). Both are stamped by the writer from the authenticated connection's scope — a row can never be built without naming its owner. This owner namespace is **distinct** from the qname namespace of any *type* the memory's downstream typed-graph rows reference (a memory owned by `acme` may still drive statements against the shared `brain:Person` type). The reserved `brain` system namespace (`NamespaceId::SYSTEM`, id `0`) owns only seeded system rows; it is read-only to users and is **never** a valid owner of user-written data.

**`created_at` vs `occurred_at`.** `created_at` is the *write time* — when the server durably committed the memory — and is always stamped. `occurred_at` is the optional *event time* — when the memory's content actually happened in the world — supplied by the client on `ENCODE` (`occurred_at_unix_nanos`). They are distinct axes: a memory written today about an event in 2020 has `created_at ≈ now` and `occurred_at ≈ 2020`. `occurred_at` is `None` when the client doesn't supply one; it is stored verbatim, carried durably through the WAL (survives recovery), and echoed back on `RECALL`. It lets time-aware clients keep the real timeline instead of cramming dates into the text. `ENCODE_VECTOR_DIRECT` carries no event-time field — those writes default the timeline to write time.

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
- **Internal:** clients send text, not vectors. A caller that has already embedded its content can hand Brain the vector directly via `ENCODE_VECTOR_DIRECT`, but that is the exception; the ordinary path is text in, vectors hidden.

**INVARIANT:** Every memory's vector is the embedding of its text under the embedding model identified by the memory's `embedding_model_fp`. If the embedding model changes, the memory's vector becomes stale until re-embedded.

## 5. The text

The text is the human-readable content the agent encoded. It is:

- **Encoding:** UTF-8.
- **Length:** unbounded in v1 (subject to a server-side cap, default 1 MiB).
- **Content:** opaque to Brain. Brain does not parse, validate, or rewrite the text.
- **Persisted:** always. The text is stored verbatim; it is the input to embedding and the output of `RECALL`.

The text and the vector are separate rows. Both have their durable home in the metadata store ([10. Metadata + Graph Store](../10_metadata/00_purpose.md) §3): the text in the texts table, the vector in the memory-vectors table as a flat little-endian `f32` run. Very large texts spill to a separate text blob store. The vector arena is a parallel at-rest copy rebuilt from the log at startup, not the write-time destination — see §13.

## 6. Identity (overview)

A memory has these identity fields:

- **`id` (`MemoryId`)** — the opaque, public identifier. 16 bytes. Used in all client-facing operations. Encodes shard, slot, and version (§7 below).
- **`namespace_id` (`NamespaceId`)** — the owning tenant. 4 bytes. The outer half of the `(namespace, space)` owner scope; `0` is the reserved `brain` system namespace.
- **`space_id` (`SpaceId`)** — the owning agent. 16 bytes (UUIDv7). The inner half of the owner scope; determines which shard the memory lives in.

**INVARIANT:** A memory's `space_id` is immutable for the memory's lifetime. To "move" a memory between spaces, encode a new memory under the destination space's id and forget the original.

**INVARIANT:** A memory's `id` is unique within the cluster — no two memories ever share an id, even if one has been forgotten and reclaimed. Slot ids come from a per-shard monotonic counter and are retired rather than recycled, and the version field in the id discriminates occupants independently of that.

## 7. Identifier formats

Brain uses several identifier types, each with specific format and stability properties.

| Identifier | Size | Format | Scope | Stability |
|---|---|---|---|---|
| `MemoryId` | 16 bytes | Encoded shard + slot + version + reserved | Cluster | Stable until forgotten + reclaimed |
| `NamespaceId` | 4 bytes | Server-interned u32 | Cluster | Permanent (`0` = reserved `brain` system namespace) |
| `SpaceId` | 16 bytes | UUIDv7 | Cluster | Permanent |
| `SessionId` | 8 bytes | Server-assigned u64 | Per-agent | Permanent within agent |
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
- `version` (32 bits) — distinguishes successive occupants of one slot id, so a stale handle mismatches rather than resolving to whatever sits there now.
- `reserved` (32 bits) — must be zero in v1; reserved for future use.

**Properties:**

- **Opaque to clients.** Clients treat `MemoryId` as an opaque 16-byte handle. They MUST NOT attempt to extract or interpret subfields.
- **Endianness:** big-endian for the on-the-wire and on-disk representations. The memory representation in Rust is `[u8; 16]`.
- **Equality:** byte-for-byte equality.
- **Ordering:** byte lexicographic ordering. (Note: this is *not* a meaningful ordering for memories — sorting by `MemoryId` produces an arbitrary order, not a temporal one.)

**Stability:** a `MemoryId` is stable from creation until the memory is forgotten *and* reclaimed. After that it resolves to nothing: reclamation deletes the row, and a slot id is never handed to a second memory, so no later occupant can inherit the handle.

**INVARIANT:** A `MemoryId` that previously identified memory M never identifies a different memory. Two mechanisms hold this, and either alone would suffice. Slot ids are minted from a per-shard monotonic counter that only ever advances, so no two memories are issued the same slot id. And the `version` field discriminates occupants of a slot id independently of that, so a handle carrying a stale version mismatches rather than resolving to whatever sits at the slot now.

That redundancy is deliberate. The monotonic counter is what makes stale handles harmless today; the version field is what keeps them harmless if slot ids ever become a reusable resource.

**The zero MemoryId.** The all-zero `MemoryId` (16 bytes of zero) is reserved as the "null" value. It MUST NOT be returned by any operation; clients MAY use it as a sentinel for "no memory".

### 7.2 SpaceId

The identifier for an agent. [UUIDv7](https://datatracker.ietf.org/doc/rfc9562/), 16 bytes.

UUIDv7 encodes a 48-bit Unix-millisecond timestamp followed by random bits, with version and variant fields per the spec. This gives:

- Time-ordered: agents created later have lexicographically-greater UUIDs.
- Unique: collision probability vanishingly small.
- Sortable: indexes by `SpaceId` cluster the most recently created agents at the end.

**Properties:**

- **Permanent.** A memory's `SpaceId` never changes.
- **Cluster-scoped.** Unique across the entire cluster.
- **Client-generated.** The agent generates its own UUIDv7 at creation. Brain doesn't issue agent ids; an external identity system or the client does.

**The zero SpaceId.** The all-zero `SpaceId` is reserved. Operations referencing the zero `SpaceId` MUST be refused with `INVALID_ARGUMENT`.

### 7.3 SessionId

A logical scope within a space's memory. A 64-bit unsigned integer.

**Properties:**

- **Agent-scoped.** Two different agents can both have `session_id = 1`; they are unrelated.
- **Server-assigned.** When an agent first references a session name (a string), the server assigns a `session_id` and persists the name → id mapping.
- **Permanent within a space.** Once a `session_id` is assigned, it never changes; if a session is "deleted" (out of scope for v1), the id is retired, not reused.

**The default session.** `session_id = 0` is reserved for the default session. Every agent automatically has a session with id 0 named "default". Memories encoded without an explicit session land in the default.

### 7.4 RequestId

The client-supplied idempotency token for `ENCODE` and `FORGET`. [UUIDv7](https://datatracker.ietf.org/doc/rfc9562/), 16 bytes. Recommended but not strictly enforced; any 16 bytes are accepted.

**Properties:**

- **Agent-scoped.** Idempotency is checked within a single space's namespace.
- **Bounded TTL.** Stored in the idempotency table for a configurable window (default: **24 hours**). After the TTL, the same `RequestId` may be treated as a new operation.
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

Mapping between them is a control-plane concern. Clients see only `MemoryId` (which embeds the runtime id at the time the memory was created) and `SpaceId` (from which routing computes the runtime id). They never see the storage UUID directly.

**Implication for stale MemoryIds.** A `MemoryId` was created when its memory's shard had runtime id S. If the shard is later renumbered (due to cluster reorganization), the `MemoryId` still encodes S in its high 16 bits — but S now maps to a different storage UUID.

Resolution: the cluster control plane records the *historical* mapping. A `MemoryId` created at epoch E uses the runtime → storage mapping in effect at epoch E. The router resolves it using the historical mapping, not just the current one.

This is rare in practice (cluster reorganization is infrequent), but the mechanism exists to avoid breaking client-cached `MemoryId`s.

### 7.6 Identifier collisions

By construction, no two memories share a `MemoryId`. The combination of (shard_id, slot_id, version) is unique within the shard, and shard_id is unique within the cluster.

UUIDv7 collision probability (for `SpaceId`, `RequestId`, storage UUID): ~1 in 2^62 within a millisecond, vanishingly small in practice. Even an organization creating a billion agents per day has effectively zero collision probability.

Runtime shard id is bounded at 65,535 shards per cluster. This is a soft cap; if you need more, add a separate cluster.

### 7.7 Wire and storage representations

| Identifier | Wire (bytes, big-endian) | Storage (bytes, native) |
|---|---|---|
| `MemoryId` | 16, fixed | 16, fixed |
| `NamespaceId` | 4, fixed | 4, fixed (host endianness) |
| `SpaceId` | 16, fixed | 16, fixed |
| `SessionId` | 8, fixed | 8, fixed (host endianness) |
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

## 10. Session

Every memory belongs to exactly one session. The `session_id` field names a session as defined in [`03_sessions.md`](03_sessions.md). Sessions are space-scoped — `session_id` 1 in space A is unrelated to `session_id` 1 in space B, and neither is resolved through any global table.

The default session (`session_id = 0`) is automatically present in every agent. Memories without an explicit session belong to the default.

**INVARIANT:** A memory's `session_id` is mutable via an admin operation, but not by the agent on the hot path. Once encoded into a session, the memory stays there unless explicitly migrated.

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
| Episodic | Semantic | Operator reclassification (no client-reachable surface today) |
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
- **Many kinds** (working, autobiographical, procedural, declarative, etc.). Most of the additional categories don't have clear operational distinctions in Brain. Procedural memory is the one candidate with a real operational distinction, and it is deferred to a future version.

The chosen three are well-grounded: two from classical theory plus one operational category for Brain-derived patterns. They map onto distinct decay profiles, distinct salience baselines, and distinct creation paths.

### 11.8 Wire and storage representation

| Session | Representation |
|---|---|
| Wire (CBOR-encoded) | u8 (0=Episodic, 1=Semantic, 2=Consolidated) |
| Storage (redb) | u8 |
| Memory record (in-memory) | enum `MemoryKind` |
| Client (typed) | language-native enum |

The on-the-wire and on-disk values are stable. New kinds (in a future major version) would be added with new numeric values; old readers seeing an unknown kind reject it.

## 12. Edges

A memory carries a list of outgoing edges. Each edge is a typed link to another memory: `(target_memory_id, edge_kind, weight)`. Specified in [`05_edges.md`](05_edges.md).

Edges may be:

- **Explicit** — set by the agent at encode time or via a separate `LINK` operation.
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
          Reclaimed (row deleted; the handle resolves to nothing)
```

Three states: `Active`, `Tombstoned`, `Reclaimed`. The kind transitions (§11.4) happen within the `Active` state.

### 13.2 State definitions

**Active.** The memory is queryable. All `RECALL`, `PLAN`, `REASON` operations may match it.

- Its row, text and vector are present in redb and current.
- HNSW index includes the memory.
- Salience updates and decay apply.

**Tombstoned.** The memory has been forgotten but not yet reclaimed.

- The row survives, with a tombstone timestamp stamped on it and its active flag cleared. For a soft forget the vector and text survive with it.
- The memory is hidden from queries — operations skip tombstoned memories.
- Its HNSW node is marked tombstoned, so the semantic lane already excludes it; the node itself leaves the graph at the next index rebuild.
- Its lexical-index and hypothetical-question rows are dropped, its timeline-index entry is removed, and its deduplication fingerprint is evicted, so a re-encode of the same text does not fold into a dead memory.
- Edges adjacent to it are tagged for cleanup but may still be present.

The tombstone state is **not** brief. It is a grace window, seven days by default, and it exists to be long: a soft forget stays reversible for its whole duration. Reversal is an admin-plane operation on the memory id, and it is refused in exactly two cases — the forget was hard, or the grace window has passed. The state is never observable to client queries. An opcode is reserved for listing tombstoned memories, but like the rest of the substrate admin set it is refused on the wire and has no route on the admin listener, so there is no operator surface reading it today either.

**Reclaimed.** The grace window has passed and the reclamation worker has reaped the memory. Its `MemoryId` now resolves to nothing.

- The memory's row, its text, and its write-artifact bundle (stored embedding plus derived graph) are deleted.
- Its adjacent edge-index rows go with it; edges pointing at it from the other direction are left to the edge-scrub worker.
- Its undo journal is deleted too, which is what makes the forget irreversible from this point on.

The slot id itself is not recycled. Each `ENCODE` mints a fresh slot id from a monotonic counter, so a reclaimed slot id is simply retired — nothing is ever written there again. `Reclaimed` is therefore terminal for both the memory and its handle.

### 13.3 Lifecycle transitions

**None → Active (creation).** Triggered by `ENCODE`. The transition involves:

1. Embed the text into a vector.
2. Compute initial salience.
3. Reserve a fresh `MemoryId` from the shard's monotonic slot counter. Nothing is written; an id whose write never commits is skipped and the counter moves on.
4. Write a WAL record and group-commit it — the recoverability barrier.
5. Insert the vector into the HNSW index. This happens before the metadata transaction opens, so a failed insert abandons the encode before anything commits.
6. In one redb transaction: write the memory row, its text, its vector, and the idempotency stamp. Commit.
7. Publish the new `MemoryId` and advance the shard's committed watermark.

The memory is `Active` once step 6 commits — that commit, not the WAL fsync, is the barrier the client's acknowledgement waits on. The full sequence is detailed in [08. Storage: Arena & WAL](../08_storage/00_purpose.md) §4.

**Active → Tombstoned (FORGET).** Triggered by `FORGET`. The transition involves:

1. Validate that the `MemoryId` references an active memory the caller owns. The check is made against the stored row's own owner scope, inside the write transaction — a row owned by someone else reads as absent.
2. Write a `FORGET` record to the WAL and group-commit it.
3. Tombstone the memory's HNSW node, and drop its lexical-index and hypothetical-question rows.
4. In one redb transaction: stamp the tombstone timestamp on the row, clear its active flag, remove its timeline-index entry, and evict its deduplication fingerprint. Commit.
5. Publish the forget and advance the committed watermark.

For **soft forget**, the vector and the text are preserved and the cascade writes an undo journal as it strips the memory from dependent statements and relations — which is what lets the whole forget be replayed backwards within grace.

For **hard forget**, the same transaction additionally:

- Sets a hard-forgotten flag on the row. This is what makes the state irreversible and self-describing: the restore path reads this bit and refuses.
- Deletes the text, the stored embedding, and the derived-graph artifacts.

It writes no undo journal — irreversibility is the point. Once the write commits, the at-rest arena copy of the vector is zeroed under a slot-version check, and WAL replay re-zeroes it so the guarantee survives a crash before writeback. Hard forget makes the memory's content unrecoverable even via filesystem-level inspection. It's the right choice for compliance-driven removals (GDPR right-to-erasure, accidental encoding of secrets).

**Tombstoned → Reclaimed (grace expiry).** Triggered by the reclamation worker, not by a new write.

1. The worker finds memories whose tombstone timestamp plus the grace window is in the past.
2. For each, in its own transaction: delete the memory row, the text, the write-artifact bundle, the adjacent edge-index rows, and the undo journal.
3. The slot id is retired rather than freed — nothing is pushed back for reuse.

After this, the memory is gone and its `MemoryId` resolves to `MemoryNotFound`. Nothing later occupies the slot, so there is no window in which a stale handle could resolve to a different memory.

**Active → Active (kind transitions).** Several kind changes happen within the `Active` state:

- Episodic → Semantic.
- Consolidated → Semantic (via promotion).

Neither is reachable from a client today: reclassification is an operator concern and has no route on the admin plane, and promotion is the consolidation worker's own decision.

These are not lifecycle transitions in the sense of changing visibility or storage; they're metadata updates while the memory remains active and queryable. The mechanics are documented in §11.4.

### 13.4 Background-driven transitions

**Eviction (Active → Tombstoned).** The decay/consolidation worker may *evict* a memory whose salience has fallen below the eviction threshold. Eviction is functionally a `FORGET`:

- WAL record (with origin = "eviction").
- HNSW removal.
- Tombstone mark.

The eviction runs in soft-forget mode by default. The vector and text are preserved until reclamation, which is what lets the admin restore path recover an unintended eviction anywhere inside the grace window.

**Consolidation (Active → Active, with creation).** The consolidation worker doesn't change a single memory's lifecycle; it creates new `Consolidated` memories from existing episodic ones. The original episodic memories remain `Active` (and may be evicted later based on salience).

Note: an aggressive consolidation policy could choose to evict the source episodic memories after consolidating them, freeing space. Brain's default policy is non-aggressive — episodic memories are kept unless their own salience falls below the eviction threshold.

### 13.5 State observability

What state is observable from outside:

- **`Active`** — fully visible. All fields readable, all queries return it.
- **`Tombstoned`** — invisible to client queries; visible to admin tools (`ADMIN_LIST_TOMBSTONED`).
- **`Reclaimed`** — the memory is gone and nothing replaces it; querying with its `MemoryId` returns `MemoryNotFound`.

Clients should not rely on observing tombstoned state. The contract is "after `FORGET` returns, the memory is no longer queryable"; how long it stays on disk is an operator-visible retention policy, not a client-visible one.

### 13.6 Slot version counter

The `version` field discriminates occupants of a slot id.

**Format.** A 32-bit unsigned integer. Initial value: 1. It advances only when a slot id takes a new occupant.

Because slot ids come from a monotonic counter and are retired rather than recycled, no slot id has a second occupant, and so in practice every live memory carries version 1. The field is nonetheless part of the identifier and is checked on every lookup: a handle whose version does not match the stored one resolves to `MemoryNotFound` rather than to the row. That check is what keeps stale handles harmless without depending on the allocation policy that happens to be in force.

**Saturation.** The value `u32::MAX` marks a slot id as **permanently retired**. Advancing past it would wrap to 0, which Brain forbids — it would silently re-validate stale `MemoryId`s.

**Why 32 bits.** Brain uses 32 bits over 16 (which would saturate too quickly under churn) or 64 (which would expand `MemoryId` beyond 16 bytes).

### 13.7 Boundary cases

**FORGET on a tombstoned memory.** The original `MemoryId` references the tombstoned (or already-reclaimed) memory. Behavior:

- If tombstoned: idempotent. The `FORGET` returns success with `was_already_forgotten = true`.
- If reclaimed: there is no row to find. Returns `MemoryNotFound`.

**RECALL race with FORGET.** A `RECALL` is in flight when `FORGET` arrives for one of the candidate memories. Behavior depends on timing:

- If the `RECALL` already returned the memory: the result is sent; the next `RECALL` won't include this memory (it's tombstoned).
- If the `RECALL` is still scoring candidates: the tombstoned memory is filtered out.

Both outcomes are consistent, and neither needs the two operations to coordinate: the `RECALL` searches one published generation of the index to completion, and the tombstone the `FORGET` sets is applied as a search-time filter. Details are in [14. Concurrency + Publication Model](../14_concurrency/00_purpose.md).

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
- **Incoming edges** (where this memory is the target) are left in place and filtered during traversal.

When the memory is `Reclaimed`:

- Its adjacent edge-index rows are deleted with it — its outgoing entries, and the by-target entries that name it.
- Rows elsewhere that name it from the other direction survive the reclamation and are cleaned up by the edge-scrub worker.

The lazy cleanup is a deliberate trade-off. Removing every reference eagerly would mean scanning every other memory's outgoing edges — expensive, and paid inside a forget. Traversal filtering makes a surviving stale row unobservable, so the scan can be a sweeper's problem instead of a write's.

## 14. Mutability

A memory's fields are mutable in different degrees:

| Field | Mutability | When |
|---|---|---|
| `id`, `space_id` | Immutable | Set at encode |
| `text`, `vector` | Immutable | Set at encode (re-embedded only on model migration) |
| `kind` | Mutable | By agent operation; consolidation may also change it |
| `session_id` | Mutable | By admin operation |
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

There is **no** "content equality" notion in the data model. Two memories with identical text but different ids are different memories. (They might have been encoded by different agents, in different sessions, or at different times.)

Deduplication of identical content is handled at encode time, not by data-model equality. See [05. Operations](../05_operations/00_purpose.md) §ENCODE.

## 16. Validation

What makes a memory record valid:

- `id` non-zero, with valid shard/slot/version components.
- `namespace_id` references a registered namespace; user-written memories are never owned by the `brain` system namespace (`0`).
- `space_id` non-zero, valid UUIDv7.
- `text` valid UTF-8, length within configured cap.
- `vector` exactly 384 elements; L2 norm in `[1.0 - epsilon, 1.0 + epsilon]` (epsilon = 1e-4).
- `kind` is one of the three valid values.
- `session_id` references an existing session for `space_id`.
- `state` is one of the three valid values.
- `created_at` ≤ `updated_at`.
- If `state == Tombstoned`, `forgot_at` is `Some` and ≤ current time.
- `salience` in [0, 1].
- `embedding_model_fp` matches a known model.
- All `EdgeId` references resolve to valid edges.

A record violating any of these is corrupted; recovery procedures are in [18. Failure Modes + Recovery](../18_failure_recovery/00_purpose.md) §Data Integrity.

