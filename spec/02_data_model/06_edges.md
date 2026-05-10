# 02.06 Edges

An **edge** is a typed, directed link between two memories. Brain ships with eight edge types in v1, fixed at the substrate level. This file specifies what edges are, how they're stored, the eight types and their semantics, and how edges are created.

## 1. The edge record

```rust
struct Edge {
    source: MemoryId,        // 16 bytes
    target: MemoryId,        // 16 bytes
    kind: EdgeKind,          // 1 byte
    weight: f32,             // 4 bytes; in [0.0, 1.0]
    origin: EdgeOrigin,      // 1 byte; Explicit | AutoDerived
    created_at: u64,         // 8 bytes; unix_nanoseconds
}
```

A single edge is ~50 bytes after alignment.

### 1.1 Direction

Edges are directed: `source → target`. The direction matters semantically. A `CAUSED` edge from M1 to M2 means "M1 caused M2", not "M2 caused M1".

### 1.2 Weight

Each edge carries a weight in [0, 1]. The interpretation is kind-specific:

- For `SIMILAR_TO` — the cosine similarity at edge creation time.
- For `CAUSED` — the agent's confidence in the causal claim.
- For `SUPPORTS` / `CONTRADICTS` — the strength of the supporting/contradicting relationship.

For other kinds, weight typically defaults to 1.0 (the edge either exists or doesn't).

### 1.3 Origin

The `origin` field marks whether an edge was:

- **Explicit** — created by the agent via an explicit `LINK` operation or as part of `ENCODE`.
- **AutoDerived** — added by background workers (e.g., `SIMILAR_TO` edges from the consolidation worker).

This matters for operations like `FORGET`-cascading and edge cleanup. Explicit edges are typically preserved across consolidation; auto-derived edges may be regenerated.

## 2. The eight edge kinds

The fixed set of edge types in v1:

| Kind | Numeric | Semantics |
|---|---|---|
| `CAUSED` | 1 | Source caused target (causal precedence) |
| `FOLLOWED_BY` | 2 | Source temporally precedes target (no causal claim) |
| `DERIVED_FROM` | 3 | Target is the source of derivation; e.g., a consolidated memory derives from episodic memories |
| `SIMILAR_TO` | 4 | Source and target are semantically similar |
| `CONTRADICTS` | 5 | Source contradicts target's claim |
| `SUPPORTS` | 6 | Source supports/evidences target's claim |
| `REFERENCES` | 7 | Source mentions or references target |
| `PART_OF` | 8 | Source is a part of (sub-component of) target |

Each is detailed below.

### 2.1 CAUSED

`source` caused `target` to occur or be true.

**Use cases.**
- "Email arrived" → `CAUSED` → "Decided to take a break".
- "Deployment ran" → `CAUSED` → "Service crashed".

**Direction.** The cause is the source; the effect is the target.

**Weight.** The agent's confidence in the causal claim; default 0.7 if not specified. Auto-derivation is conservative — workers don't add `CAUSED` edges automatically; only the agent's explicit assertions count.

**Used by.** `REASON` (causal explanation), `PLAN` (forward chaining from current state).

### 2.2 FOLLOWED_BY

`source` temporally precedes `target`. No causal claim.

**Use cases.**
- Sequential events in a conversation.
- Timeline reconstruction.

**Direction.** The earlier event is the source; the later is the target.

**Weight.** Typically 1.0 (binary "yes, this happened first").

**Auto-derivation.** Auto-derived during episodic-memory chunking — when the consolidation worker decides two episodic memories belong to the same temporal sequence, it adds `FOLLOWED_BY` between them.

**Used by.** Timeline queries, episode-based reasoning.

### 2.3 DERIVED_FROM

`source` is derived from `target`. The target is the input or origin.

**Use cases.**
- A consolidated memory derives from the episodic memories it summarizes.
- A semantic memory derives from a sequence of episodic observations.
- An agent's inference derives from the supporting evidence.

**Direction.** The new derivation is the source; the originals are targets. A consolidated memory has multiple `DERIVED_FROM` edges, one per source episodic memory.

**Auto-derivation.** Created automatically by the consolidation worker. Required: every consolidated memory has at least one `DERIVED_FROM` edge.

**Used by.** Provenance queries, "what evidence supports this?", `REASON`.

### 2.4 SIMILAR_TO

`source` is semantically similar to `target`.

**Use cases.**
- Pre-computed similarity for fast graph traversal.
- Cluster identification.

**Direction.** Symmetric in semantics, but the edge is stored directed. Conventionally, similarity is bidirectional, so the substrate stores both directions when adding such edges automatically.

**Weight.** The cosine similarity at edge creation time, in [0, 1]. (Negative similarity is treated as 0; we don't represent it as a weight.)

**Auto-derivation.** Auto-derived. The consolidation worker adds `SIMILAR_TO` edges between high-similarity memory pairs to accelerate future traversals. Threshold: similarity ≥ 0.85 by default.

**Used by.** `RECALL` (graph-aware re-ranking), `REASON` (analogy).

### 2.5 CONTRADICTS

`source` contradicts `target`.

**Use cases.**
- "User says X" / Earlier "User said not-X".
- Conflicting evidence.

**Direction.** Symmetric in semantics, but stored as the agent's declaration. If A contradicts B, both directions could be added; v1 just adds one direction (the one the agent specified) and lets graph traversal handle the symmetry.

**Weight.** Strength of contradiction; typically 1.0.

**Auto-derivation.** Not auto-derived. Detecting contradiction requires natural-language understanding beyond what the substrate does.

**Used by.** `REASON` (rebuttal generation), conflict identification.

### 2.6 SUPPORTS

`source` supports/evidences `target`.

**Use cases.**
- Evidence for a claim.
- Citations.

**Direction.** The evidence is the source; the claim is the target.

**Weight.** Strength of the supporting relationship.

**Auto-derivation.** Not auto-derived. Like `CONTRADICTS`, requires semantic understanding.

**Used by.** `REASON` (claim evaluation).

### 2.7 REFERENCES

`source` mentions or references `target`.

**Use cases.**
- A memory mentions another memory.
- Cross-referencing.

**Direction.** The referrer is the source; the referent is the target.

**Weight.** Typically 1.0.

**Auto-derivation.** Not auto-derived. The agent provides reference edges when it observes one memory pointing at another.

**Used by.** Various.

### 2.8 PART_OF

`source` is part of (a sub-component of) `target`.

**Use cases.**
- A specific observation is part of a larger episode.
- A fact is part of a knowledge cluster.

**Direction.** The part is the source; the whole is the target.

**Weight.** Typically 1.0.

**Auto-derivation.** Auto-derived during episodic chunking — when the consolidation worker identifies an episode, it adds `PART_OF` edges from each member episodic memory to the episode (which is itself a memory of kind `Consolidated`).

**Used by.** Hierarchical retrieval, episode reconstruction.

## 3. Why this set, this size

The set is small (8 types). Brain considered:

- **Smaller** (3–4 types). Loses too much expressiveness; agents would have to encode edge semantics into separate fields.
- **Larger** (15–20+ types). Each new type has cost: more code paths, more test cases, more user confusion. The marginal value of more types beyond eight wasn't clear.

The chosen eight cover:

- Time/causality: `CAUSED`, `FOLLOWED_BY`.
- Provenance: `DERIVED_FROM`.
- Similarity: `SIMILAR_TO`.
- Logical: `CONTRADICTS`, `SUPPORTS`.
- Structural: `REFERENCES`, `PART_OF`.

User-defined edge types are an open question, deferred to v2 ([01.10 OQ-10](../01_system_architecture/10_open_questions.md)).

## 4. Edge cardinality

A memory has:

- **Outgoing edges:** owned by the source memory. Stored alongside the memory.
- **Incoming edges:** discoverable via the edge index but not stored on the target.

Practical limits:

- A single memory can have up to 1000 outgoing edges (configurable). Memories with very many edges are unusual; the limit prevents abuse.
- No limit on incoming edges (depends on what other memories choose to point at this one).

Edges are **multi-edges** — multiple edges of the same kind between the same pair are allowed (with different weights, origins, or timestamps). In practice, the substrate deduplicates same-kind edges added at the same time; multi-edges arise when the same connection is re-confirmed at different times.

## 5. Edge identifiers

An `EdgeId` uniquely identifies an edge:

```
EdgeId = (source: MemoryId, target: MemoryId, kind: EdgeKind, edge_seq: u32)
```

`edge_seq` distinguishes multi-edges. When an edge is added between (source, target, kind) and one already exists, the new edge gets the next `edge_seq`.

Edge ids aren't typically exposed to clients; clients see edges as part of memory records or graph-traversal results, not as standalone entities.

## 6. Edge persistence

Edges live in the metadata store ([07. Metadata + Graph Store](../07_metadata_graph/) §4). Two indexes:

- **By source.** Given a memory id, list all outgoing edges. This is the dominant access pattern.
- **By target.** Given a memory id, list all incoming edges. Used during cleanup and bidirectional traversal.

The by-source index is the primary; the by-target index is maintained as a secondary.

## 7. Edge lifecycle

### 7.1 Creation

Edges are created in three ways:

- **Explicit at encode.** The `ENCODE` operation can carry a list of `(target_id, kind, weight)` tuples; the substrate creates the corresponding outgoing edges.
- **Explicit via `LINK`.** A separate operation adds edges to existing memories. (See [09. Cognitive Operations](../09_cognitive_operations/) §LINK.)
- **Auto-derived.** Background workers add `SIMILAR_TO`, `FOLLOWED_BY`, `DERIVED_FROM`, `PART_OF` edges based on patterns.

### 7.2 Mutation

Edges are mostly immutable. The exceptions:

- **Weight updates.** The weight of a `SIMILAR_TO` edge may be re-computed during HNSW maintenance; same-kind edges may merge.
- **Origin upgrades.** An auto-derived edge may be "upgraded" to explicit if the agent later asserts the same connection. The auto-derived edge is replaced by an explicit one.

### 7.3 Removal

Edges are removed:

- When the source or target memory is forgotten and reclaimed (the edge no longer makes sense).
- When the agent explicitly issues an `UNLINK` operation.
- When the consolidation worker re-organizes the graph (removing stale `SIMILAR_TO` edges, etc.).

Edges with a forgotten source are removed eagerly; edges with a forgotten target are detected lazily during traversal and filtered out.

## 8. Cascading on FORGET

When a memory is forgotten:

- **Outgoing edges** (owned by the forgotten memory) are removed.
- **Incoming edges** (from other memories pointing at this one) are *not* removed; they are filtered out during traversal until the forgotten memory's slot is reclaimed, at which point a slot-reuse pass rewrites the by-target index to drop them.

Why the asymmetry: the forgotten memory's outgoing edges are colocated and easy to remove; rewriting all the by-target index entries for everyone pointing at this memory is a heavier operation, deferred until the slot is reclaimed.

## 9. Edge auto-derivation rules

### 9.1 SIMILAR_TO derivation

The consolidation worker periodically scans memories and adds `SIMILAR_TO` edges between pairs whose similarity exceeds a threshold (default: 0.85). The number of edges added per memory is capped (default: 16).

The trade-off: more `SIMILAR_TO` edges accelerate graph-aware traversal but increase storage. The default targets the "useful for navigation" middle.

### 9.2 FOLLOWED_BY derivation

Episodic memories within a small temporal window (default: 30 minutes) and the same context are linked by `FOLLOWED_BY` edges. Done by the consolidation worker as part of episode identification.

### 9.3 DERIVED_FROM derivation

Required for every consolidated memory: the worker creates `DERIVED_FROM` edges from the new consolidated memory to each source episodic memory. Without these, provenance is lost.

### 9.4 PART_OF derivation

When the consolidation worker identifies an episode and creates a `Consolidated` memory representing the episode, each member episodic memory gets a `PART_OF` edge to the consolidated one.

## 10. Edge queries

Common edge query patterns, exposed via the planner / executor:

- "What memories did M cause?" → traverse outgoing `CAUSED`.
- "What memories caused M?" → traverse incoming `CAUSED`.
- "What's similar to M?" → traverse `SIMILAR_TO` (cheap; pre-computed).
- "What's the episode containing M?" → traverse outgoing `PART_OF` to the unique consolidated parent.
- "What evidence supports M?" → traverse incoming `SUPPORTS`.
- "What contradicts M?" → traverse incoming `CONTRADICTS`.

These are not opcodes in the wire protocol; they're access patterns the cognitive operations dispatch through. See [09. Cognitive Operations](../09_cognitive_operations/) for which operations use which patterns.

---

*Continue to [`07_memory_kinds.md`](07_memory_kinds.md) for memory kinds.*
