# 02.02 The Memory Entity

The **memory** is Brain's central entity. Every operation is in service of creating, recalling, modifying, or removing memories. This file specifies what a memory *is*.

## 1. Conceptual definition

A memory is a single piece of agent-observed or agent-derived content, stored with enough metadata to be retrievable by similarity, by reference, or by traversal of its relationships.

The metaphor: a memory is what an agent would call to mind. The substrate stores enough that recall is meaningful — not just the content, but its context, importance, time, relationships, and history.

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

The fields are detailed in subsequent sections of this spec. In this file, we describe the entity as a whole and its core invariants.

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
- **Internal:** clients send text, not vectors. (Power users may send pre-computed vectors via `ENCODE_VECTOR_DIRECT`; see [01.10 OQ-5](../01_system_architecture/10_open_questions.md).)

**INVARIANT:** Every memory's vector is the embedding of its text under the embedding model identified by the memory's `embedding_model_fp`. If the embedding model changes, the memory's vector becomes stale until re-embedded.

## 5. The text

The text is the human-readable content the agent encoded. It is:

- **Encoding:** UTF-8.
- **Length:** unbounded in v1 (subject to a server-side cap, default 1 MiB).
- **Content:** opaque to the substrate. Brain does not parse, validate, or rewrite the text.
- **Persisted:** always. The text is stored verbatim; it is the input to embedding and the output of `RECALL`.

The text is stored separately from the vector. The arena holds vectors; the text lives in the metadata store ([07. Metadata + Graph Store](../07_metadata_graph/) §3) for memories where it fits, or in a separate text blob store for very large memories.

## 6. Identity

A memory has two identity fields:

- **`id` (`MemoryId`)** — the opaque, public identifier. 16 bytes. Used in all client-facing operations. Encodes shard, slot, and version. Defined in [`03_identifiers.md`](03_identifiers.md).
- **`agent_id` (`AgentId`)** — the owning agent. 16 bytes (UUIDv7). Determines which shard the memory lives in.

**INVARIANT:** A memory's `agent_id` is immutable for the memory's lifetime. To "move" a memory between agents, encode a new memory under the destination agent's id and forget the original.

**INVARIANT:** A memory's `id` is unique within the cluster — no two memories ever share an id, even if one has been forgotten and reclaimed. The version field in the id ensures this.

## 7. Lifecycle states

A memory has three lifecycle states:

| State | Queryable | Storage |
|---|:-:|---|
| Active | Yes | Slot occupied; content available |
| Tombstoned | No | Slot occupied; content available pending reclamation |
| Reclaimed | No (the slot now holds a different memory) | Slot reused |

The detailed state transitions, eligibility for transition, and timing are in [`08_lifecycle.md`](08_lifecycle.md).

## 8. Provenance

Two fields track where a memory came from:

- **`embedding_model_fp`** — fingerprint of the model that produced the vector. 16 bytes (BLAKE3-derived). Used to detect cross-model query attempts and to drive model migration.
- **`source_request_id`** — the `request_id` from the `ENCODE` operation that created the memory. Lets clients trace from a write back to the resulting memory id.

**INVARIANT:** A memory's `embedding_model_fp` is set at encode time and is immutable for the lifetime of the memory. If the model changes, the memory must be re-embedded (which produces a new vector but preserves the memory's id and other metadata).

## 9. Salience

A single number in [0, 1]. The full model — initial computation, update on access, decay over time, normalization — is in [`05_salience.md`](05_salience.md).

The high-level summary: high-salience memories are returned earlier in `RECALL` results and decay more slowly. Low-salience memories rank lower and decay faster. Salience updates happen on access (raise) and via the background decay worker (lower).

## 10. Context

Every memory belongs to exactly one context. The `context_id` field references a context defined in [`04_context.md`](04_context.md). Contexts are agent-scoped — `context_id` 1 in agent A is unrelated to `context_id` 1 in agent B.

The default context (`context_id = 0`) is automatically present in every agent. Memories without an explicit context belong to the default.

**INVARIANT:** A memory's `context_id` is mutable via an admin operation, but not by the agent on the hot path. Once encoded into a context, the memory stays there unless explicitly migrated.

## 11. Kind

Three kinds: `Episodic`, `Semantic`, `Consolidated`. Specified in [`07_memory_kinds.md`](07_memory_kinds.md).

The default kind for `ENCODE` is `Episodic`. The agent may set the kind explicitly, or it may be promoted from `Episodic` to `Semantic` over time, or `Consolidated` memories may be created by the consolidation worker.

## 12. Edges

A memory carries a list of outgoing edges. Each edge is a typed link to another memory: `(target_memory_id, edge_kind, weight)`. Specified in [`06_edges.md`](06_edges.md).

Edges may be:

- **Explicit** — set by the agent at encode time or via a separate `LINK` operation (deferred to [01.10 OQ-10](../01_system_architecture/10_open_questions.md)).
- **Auto-derived** — added by background workers based on similarity, temporal adjacency, or causal patterns.

Incoming edges (other memories pointing at this one) are **not** stored on the memory itself; they are reconstructable from the metadata store's edge index.

## 13. Mutability

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

## 14. Equality

Two memories are equal iff they have the same `MemoryId`. Other fields can differ — for instance, after consolidation, a memory may have updated `kind` and `salience`, but it's still the same memory.

There is **no** "content equality" notion in the data model. Two memories with identical text but different ids are different memories. (They might have been encoded by different agents, in different contexts, or at different times.)

Deduplication of identical content is handled at encode time, not by data-model equality. See [09. Cognitive Operations](../09_cognitive_operations/) §ENCODE.

## 15. Validation

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

A record violating any of these is corrupted; recovery procedures are in [15. Failure Modes + Recovery](../15_failure_recovery/) §Data Integrity.

---

*Continue to [`03_identifiers.md`](03_identifiers.md) for identifier formats.*
