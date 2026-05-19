# 09 — Knowledge layer

**Audience:** operators evaluating Brain past the vector-database
checkbox, developers wiring a typed-data feature, anyone asking
"what changes when I declare a schema?"

**Goal:** by the end you should understand the three-layer model
(memories → entities/statements/relations), what activates when
a schema is uploaded, where the new state lives on disk, and
why a substrate-only deployment is a first-class posture rather
than a "legacy mode."

This chapter assumes [01](01-system-architecture.md) (one shard,
one executor) and [05](05-redb-metadata.md) (redb tables). The
specifics of extractor tiers are
[chapter 10](10-extractors.md); the retrieval side is
[chapter 11](11-hybrid-retrieval-rrf.md) and
[chapter 12](12-query-router.md). This chapter covers the
*structure* — what the knowledge layer is and how it sits on top
of the substrate.

---

## The three layers

```
┌──────────────────────────────────────────────────────────────┐
│  Layer 3 — Relations                                          │
│    Typed edges between entities (REPORTS_TO, MENTIONED_WITH)  │
│    Provenance: which Memories witnessed this Relation         │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│  Layer 2 — Entities + Statements                              │
│    Entities: typed identity anchors (Person, Place, …)        │
│    Statements: Fact / Preference / Event about entities       │
│    Provenance: every Statement points back to evidence Memories
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│  Layer 1 — Memories (the substrate)                           │
│    Text + 384-dim vector + metadata                           │
│    Arena slot, HNSW node, redb row                            │
│    *Authoritative.* Everything above derives from this.       │
└──────────────────────────────────────────────────────────────┘
```

One rule binds the layers together:

> **Every Statement and every Relation has provenance traceable
> to one or more Memories. The substrate is authoritative;
> derived data can always be recomputed.**

That means:

- Memories don't reference Statements. The arrow points one way:
  Statements list their evidence Memories, never the reverse.
  A `memory_to_statement` reverse index exists but is *derived*
  and rebuildable.
- Forgetting a Memory triggers re-derivation of Statements that
  cited it as evidence. A Statement losing all evidence becomes
  superseded-with-null.
- A disaster that loses every knowledge-layer table is
  survivable — the substrate's WAL is the source of truth, and
  the extractor tier can re-derive Entities, Statements, and
  Relations on demand.

The reverse couplings are intentional: Statements are
disposable; Memories are not.

---

## What activates when a schema is declared

A fresh shard ships with **all** the knowledge-layer tables
present in `metadata.redb` and **all** the knowledge-layer
files (`entity.hnsw`, `statement.hnsw`, the two tantivy dirs,
`llm_cache.redb`) created on disk
([chapter 03](03-arena-and-wal.md) §
"What's actually on disk"). They're just empty.

The thing that switches behaviour is a **schema declaration**.
A client sends `SCHEMA_UPLOAD` ([chapter 02](02-wire-protocol.md))
with a schema document (the Schema DSL — entity types, predicates,
relation types, extractor configs). After a successful commit,
the shard's *schema gate* flips and the knowledge-layer
codepaths start running.

The gate itself is a per-shard `Arc<ArcSwap<bool>>`
(`crates/brain-ops/src/schema_gate.rs:32`):

```rust
pub struct SchemaGate {
    inner: Arc<ArcSwap<bool>>,
}
```

Why an `ArcSwap<bool>`? The substrate's hot RECALL path checks
`is_declared()` per request to decide whether to fan out to the
hybrid retriever or stay on the pure substrate path. The check
has to be lock-free. `ArcSwap::load()` is exactly that — one
atomic read.

The gate's lifecycle
(`crates/brain-ops/src/schema_gate.rs:8`):

1. **Shard spawn.** `SchemaGate::initial` reads the metadata
   DB's `schema_namespaces` once. If any namespace has an
   active schema version, the gate starts `true`; otherwise
   `false`.
2. **`SCHEMA_UPLOAD` commit.** A successful (non-`dry_run`)
   upload flips the gate to `true` via `set_declared(true)`
   (`crates/brain-ops/src/schema_gate.rs:66`).
3. **Subsequent RECALLs.** `is_declared()` is consulted to pick
   the planner branch.

A small but important property: **the gate is monotone in v1**.
Once flipped to `true`, it stays. There is no `SCHEMA_DROP`
opcode — dropping the knowledge layer for a shard would require
draining its tables and an admin tool, which v1 doesn't ship.
The "substrate-only" posture is a fresh-deployment choice, not
a runtime toggle.

The substrate-only mode (gate `false`) is **first class**:

- RECALL takes the pure-substrate fast path
  ([chapter 11](11-hybrid-retrieval-rrf.md) describes the
  hybrid branch this skips).
- Extractor workers run their loops but do no work (empty
  registry).
- Knowledge-layer tables sit empty on disk but cost almost
  nothing — an unused redb table is a B-tree with zero nodes.
- The HNSW indexes for entities and statements never grow past
  empty.

A deployment that never declares a schema runs forever like
this, paying only the trivial cost of the empty tables.

---

## Entities

Entities are typed identity anchors. "Priya," "Acme Corp,"
"the planning session." They have:

- A 16-byte `EntityId` (UUIDv7, like every other shard-local
  identifier).
- An `entity_type_id` (`u32`, interned in the `entity_types`
  table). Operators declare `Person`, `Organization`,
  `Project`, … in the schema; the registry assigns the id.
- A canonical name + a normalized name (lowercased, NFKC,
  whitespace-collapsed).
- A list of aliases.
- An attribute blob (typed schema-defined fields; rkyv-encoded
  `BTreeMap<String, Value>`).
- Per-entity mention count, creation/update timestamps, and a
  flags bitfield.

The `EntityMetadata` row
(`crates/brain-metadata/src/tables/knowledge/entity.rs:112`):

```rust
pub struct EntityMetadata {
    pub entity_id_bytes: [u8; 16],
    pub entity_type_id: u32,
    pub canonical_name: String,
    pub normalized_name: String,
    pub aliases: Vec<String>,
    pub attributes_blob: Vec<u8>,
    pub mention_count: u32,
    pub created_at_unix_nanos: u64,
    pub updated_at_unix_nanos: u64,
    pub merged_into_bytes: Option<[u8; 16]>,
    pub embedding_version: u32,
    pub flags: u32,
}
```

`merged_into_bytes` carries the merge target when an entity has
been merged into another — the original row stays as a
forwarding pointer.

### The five entity tables

Per `crates/brain-metadata/src/tables/knowledge/entity.rs:23`:

| Table | Key | Value | Purpose |
|---|---|---|---|
| `entities` | `EntityId` (16 B) | `EntityMetadata` | Primary record. |
| `entity_by_canonical_name` | `(entity_type_id, normalized_name)` | `EntityId` | Exact-match resolution. |
| `entity_aliases` | `(entity_type_id, normalized_alias, EntityId)` | `()` | Alias-match resolution. |
| `entity_trigrams` | `(entity_type_id, trigram, EntityId)` | `()` | Fuzzy resolution (typos). |
| `entity_mentions` | `(EntityId, MemoryId)` | `MentionMetadata` | Reverse: memories mentioning entity. |

Five tables for one logical thing because entity resolution is
the hard part of the knowledge layer
([chapter 10](10-extractors.md) covers the resolver tiers).
Each table is one of four resolution strategies:

- Exact canonical name → `entity_by_canonical_name`.
- Alias hit → `entity_aliases`.
- Trigram fuzzy match → `entity_trigrams`.
- Embedding similarity → `entity.hnsw` (the on-disk HNSW file).

The mention index is for the reverse query: "show me memories
that talk about Priya."

### Entity HNSW

Entities get their own HNSW index (`entity.hnsw`) for the
similarity-based resolution tier
([chapter 04](04-hnsw-index.md) on the per-shard HNSW shape).
Smaller than the memory HNSW — entities are typically 100–1000×
fewer than memories — with knowledge-layer-tuned parameters
(`M=16, ef_construction=100, ef_search=64`).

The embedding worker re-embeds an entity when it's created or
renamed; the embedding is the entity's canonical name + a
context slice. The version is tracked in `embedding_version`
on the entity row, so the worker knows whether to re-embed
after a schema upgrade.

---

## Statements

Statements are typed claims about entities. The data model
distinguishes three *kinds*:

| Kind | What it captures | Mutation policy | Time field |
|---|---|---|---|
| **Fact** | Stable claims about the world. "Priya is the engineering manager." | Append-only. Contradicting Facts are stored; the planner exposes the conflict. | `valid_from` (extraction time), no `valid_to` until contradicted. |
| **Preference** | Revisable beliefs / choices. "Priya prefers async meetings." | Versioned via supersession. A new Preference supersedes the old. | `valid_from` (extraction time), `valid_to` = `superseded_by.extracted_at`. |
| **Event** | Discrete occurrences at a moment. "Priya scheduled a meeting on Tuesday." | Immutable. Corrections add a new Event, never modify. | `event_at` (the moment); no validity range. |

The three kinds live in one storage schema. The `kind` field is
a `u8` discriminator on `StatementMetadata`
(`crates/brain-metadata/src/tables/knowledge/statement.rs:319`):

```rust
pub struct StatementMetadata {
    pub statement_id_bytes: [u8; 16],
    pub kind: u8,                       // Fact=0, Preference=1, Event=2
    pub subject_entity_bytes: [u8; 16],
    pub predicate_id: u32,
    pub object: …,                       // EntityId | TextLiteral | etc.
    pub confidence: f32,
    pub evidence_memory_ids: Vec<[u8; 16]>,
    pub evidence_overflow_id: Option<u128>, // long evidence lists spill here
    pub extractor_id: u32,
    pub extracted_at_unix_nanos: u64,
    pub valid_from_unix_nanos: Option<u64>,
    pub valid_to_unix_nanos: Option<u64>,
    pub event_at_unix_nanos: Option<u64>,
    pub version: u32,
    pub superseded_by_bytes: Option<[u8; 16]>,
    pub flags: u32,
    …
}
```

The kind is the first column of every compound index. Queries
filtered by kind get a tight range scan; cross-kind queries
still work, they just don't get the prefix benefit.

### The eight statement tables

Per
`crates/brain-metadata/src/tables/knowledge/statement.rs:37`:

| Table | Key | Purpose |
|---|---|---|
| `statements` | `StatementId` | Primary record. |
| `statements_by_subject` | `(EntityId, kind, predicate, is_current)` | Subject-anchored queries. |
| `statements_by_predicate` | `(PredicateId, kind, confidence_bucket)` | Predicate-anchored queries. |
| `statements_by_object_entity` | `(EntityId, kind)` | Reverse: what statements have X as object? |
| `statements_by_event_time` | `(event_at, EntityId)` | Time-range Event queries. |
| `statements_by_evidence` | `(MemoryId, StatementId)` | Reverse: what statements cite this memory? |
| `statement_chain` | `(chain_root, version)` | Supersession-chain traversal for Preferences. |
| `evidence_overflow` | `EvidenceOverflowId` | Long evidence lists (>N inline). |

Why so many indexes? Because the planner's hybrid query
([chapter 11](11-hybrid-retrieval-rrf.md)) needs each of these
access paths to be a range scan, not a full-table walk. The
storage cost is real (eight tables instead of one), but every
table is the prefix the query plans want.

The `is_current` flag in the by-subject index makes
"current-only" queries cheap: filter on `is_current = true` and
the supersession chain falls out of the scan.

`evidence_overflow` is the spill table for statements whose
evidence list is long. Most statements cite a few memories;
some (especially highly-confirmed Facts that accumulate
evidence over time) might cite hundreds. Rather than balloon
the primary row, we store a small inline list plus an optional
overflow id.

### Supersession chains

Preferences supersede each other; a "current" Preference is the
head of a chain. The traversal goes:

```
PreferenceV1 → superseded_by=V2 → PreferenceV2 → superseded_by=V3 → … 
                                                              ↑
                                                       no superseded_by:
                                                       this is current.
```

`statement_chain` keyed by `(chain_root, version)` makes "give
me the full history of this preference" a range scan from
`(chain_root, 0)` to `(chain_root, ∞)`. The chain root is the
oldest version's StatementId.

Contradicting Facts (same subject + predicate, different object)
are *not* supersessions — both are stored, and the planner
returns the higher-confidence one with the conflict surfaced
in the response metadata. This is intentional: a cognitive
substrate should surface contradictions, not hide them.

---

## Relations

Relations are typed edges between entities. Examples:
`REPORTS_TO(Priya, Devi)`, `WORKS_AT(Priya, Acme)`,
`MENTIONED_WITH(Priya, planning_session_id)`.

Like Statements, Relations are derived from memories — every
Relation carries an evidence list. Unlike Statements, Relations
don't have the three-kind distinction; they're typed by
`relation_type_id`, with the type definitions in
`relation_types`.

The four relation tables
(`crates/brain-metadata/src/tables/knowledge/relation.rs:24`):

| Table | Key | Purpose |
|---|---|---|
| `relations` | `RelationId` | Primary record. |
| `relations_by_from` | `(EntityId, relation_type, is_current)` | Outgoing edges from an entity. |
| `relations_by_to` | `(EntityId, relation_type, is_current)` | Incoming edges to an entity. |
| `relations_by_evidence` | `(MemoryId, RelationId)` | Reverse: what relations cite this memory? |

Two direction indexes for the same reason edges in the
substrate have two ([chapter 05](05-redb-metadata.md)): forward
traversal and reverse traversal are both common, and asking
either to scan the other's index full-table is unacceptable.

`relation_traversal`
(`crates/brain-metadata/src/relation_traversal.rs`) walks these
indexes for queries like "find everyone who reports to Priya
transitively" with bounded depth and branching factor.

---

## Tantivy: BM25 for text

The semantic side of retrieval is HNSW; the lexical side is
tantivy ([chapter 11](11-hybrid-retrieval-rrf.md) covers how
they combine). Each shard owns two tantivy indexes:

- `memory_text.tantivy/` — indexes the text of every memory.
  Fields: `text` (tokenized, stemmed, BM25-indexed), `agent_id`
  (filter), `kind` (filter), `created_at` (date range),
  `memory_id` (stored-only, used to join back to the arena).
- `statements.tantivy/` — indexes statement text
  representations. Fields: `subject_name`, `predicate_name`,
  `object_text`, `kind`, `confidence_bucket`, `extracted_at`,
  `statement_id` (stored-only).

Tokenisation: lowercase + Porter stemmer + a custom URL- and
code-identifier-preserving filter (preserves things that BM25
would otherwise tokenise away).

The memory text indexer worker keeps `memory_text.tantivy/` in
sync with `MEMORIES_TABLE`: on every ENCODE / FORGET it
enqueues an Upsert / Delete event, drained in batches off the
hot path
([chapter 07](07-background-workers.md)'s sweepers don't
include these; they're index-shaped dispatchers wired through
`OpsContext::memory_text_dispatcher`).

The statement indexer is parallel: on every statement create /
supersede / tombstone / retract, the statement's text
representation gets enqueued. The text repr is
`subject.canonical_name + " " + predicate.name + " " + object_text`
— compact, captures the semantic core, indexed once at write
time.

---

## Provenance: WAL frame types

The substrate's WAL ([chapter 03](03-arena-and-wal.md)) carries
memory frames. The knowledge layer adds frame types:

| Frame type | Body |
|---|---|
| `0x10 ENTITY_CREATE` | Entity record. |
| `0x11 ENTITY_UPDATE` | Entity delta. |
| `0x12 ENTITY_MERGE` | Merge record (source → target). |
| `0x13 ENTITY_TOMBSTONE` | Tombstone mark. |
| `0x20 STATEMENT_CREATE` | Statement record. |
| `0x21 STATEMENT_SUPERSEDE` | (old, new) supersession. |
| `0x22 STATEMENT_TOMBSTONE` | Tombstone. |
| `0x30 RELATION_CREATE` | Relation record. |
| `0x31 RELATION_SUPERSEDE` | Supersession. |
| `0x32 RELATION_TOMBSTONE` | Tombstone. |
| `0x40 SCHEMA_UPDATE` | Schema document. |
| `0x50 AUDIT` | Audit entry (for replay). |

The same WAL, same group commit, same fsync barrier. The
recovery driver dispatches each frame type to its sink: memory
frames hydrate substrate state, knowledge-layer frames hydrate
the redb knowledge tables. Derived indexes (tantivy, HNSW) are
rebuilt from the redb authoritative state.

This is what makes the durability story uniform: the same
"log is truth" invariant applies. A crashed shard recovers by
replaying its WAL; the knowledge layer comes back with the
substrate, not after it.

---

## Schema upload, in detail

The `SCHEMA_UPLOAD` flow
(`crates/brain-metadata/src/schema_store.rs:66`):

1. **Client sends `SCHEMA_UPLOAD`** with the schema source (the
   DSL text — entity types, predicates, relation types,
   extractor declarations) and a `namespace` plus optional
   `dry_run` flag.
2. **`parse_schema` + `validate`** runs in `brain-protocol`.
   Syntax errors and semantic errors (undefined types,
   duplicate predicates, etc.) come back as a structured
   response without touching redb.
3. **If `dry_run`, the response is the validation result and
   nothing is written.**
4. **Otherwise, a redb write transaction opens**:
   - The schema document is stored in `schema_versions` at the
     next version number for this namespace.
   - `apply_schema_definitions`
     (`crates/brain-metadata/src/schema_apply.rs:38`) interns
     every new entity type, predicate, and relation type into
     its `*_types` table.
   - Every declared extractor lands in `extractors` with its
     enabled flag.
   - The schema becomes the namespace's active version.
5. **The WAL writes a `SCHEMA_UPDATE` frame** so recovery
   replays the schema.
6. **The shard's `SchemaGate::set_declared(true)`** is called.
   Subsequent RECALLs see the new gate state.
7. **The response carries the new schema version**.

Schema uploads are *additive*. A new upload can:

- Add new entity types, predicates, relation types.
- Add new extractors.
- Modify existing extractors' enabled flag (or add new
  versions).

It cannot drop anything that existing Statements or Relations
reference — that would orphan their `entity_type_id` /
`predicate_id` / `relation_type_id`. A future operator tool
could handle that with a `FORGET_*` cascade, but `SCHEMA_UPLOAD`
itself doesn't.

After upload, the `schema_migration` worker
([chapter 07](07-background-workers.md)) applies any
extractor-version changes — re-running extractors over memories
that were extracted under the old version. The `backfill`
worker handles memories that pre-date the schema, on operator
trigger.

---

## Substrate-only mode

A deployment that never sends `SCHEMA_UPLOAD` runs as a pure
vector substrate. Every knowledge-layer file and table exists
on disk but is empty. The schema gate stays `false`. RECALL
takes the pure-substrate code path:

- HNSW search over the memory index ([chapter 04](04-hnsw-index.md)).
- Optional filter, optional re-rank.
- Response.

Substrate-only mode is the right deployment posture when:

- You only need vector retrieval.
- You don't want the extractor cost (CPU, LLM tokens).
- You're prototyping and haven't designed a schema yet.

The cost of *staying* in substrate-only mode forever is the
unused tables and files. Empty B-trees and empty `.hnsw` files
are essentially free.

Switching from substrate-only to knowledge-active is a one-way
trip in v1: send `SCHEMA_UPLOAD`, gate flips, retrievers fan
out. There's no way to flip back at runtime; that would require
draining the knowledge tables and the operator hasn't asked us
to build that.

---

## Storage budget

For a 1M-memory deployment with extraction density ~1 statement
per 2 memories, ~10K entities, ~500 relations:

| Storage | Substrate | Knowledge layer | Total |
|---|---|---|---|
| Arena + WAL | ~4 GiB | — | ~4 GiB |
| Memory HNSW | ~5 GiB | — | ~5 GiB |
| `metadata.redb` — substrate tables | ~500 MiB | — | ~500 MiB |
| `metadata.redb` — knowledge tables | — | ~200 MiB | ~200 MiB |
| Entity HNSW | — | ~50 MiB | ~50 MiB |
| Statement HNSW | — | ~2 GiB | ~2 GiB |
| `memory_text.tantivy/` | — | ~500 MiB | ~500 MiB |
| `statements.tantivy/` | — | ~100 MiB | ~100 MiB |
| `llm_cache.redb` | — | up to 10 GiB (configurable) | varies |
| Audit logs | — | ~200 MiB | ~200 MiB |

A knowledge-active shard is roughly **2× the storage** of a
substrate-only shard with the same memory count, before the LLM
cache. The cache is configurable and operator-bounded.

---

## Failure modes

**`SCHEMA_UPLOAD` validation fails.** Client gets a structured
error with line/column info. Nothing is written, gate doesn't
flip.

**`SCHEMA_UPLOAD` commits but a follow-up worker (backfill,
schema_migration) hits an error.** The schema is active; the
worker's `last_error` records the failure; subsequent cycles
retry. Statements extracted under the old schema version are
flagged stale (by the `stale_extraction_detector` worker, chapter 07).

**Knowledge-layer redb tables are corrupted.** Same handling as
substrate redb corruption — `MetadataDb::open` fails to open,
the shard refuses to spawn. The fix is restore from snapshot.

**Tantivy index is corrupt.** Detected at `TantivyShard::open`.
The shard's recovery path
(`crates/brain-server/src/shard/tantivy_recovery.rs`) can
trigger a rebuild from `statements` / `MEMORIES_TABLE` rather
than refuse to spawn. The result is a fresh index plus a `IndexStatus::NeedsRebuild`
log line.

**Entity HNSW or statement HNSW corrupt.** Same fallback as
the substrate HNSW ([chapter 04](04-hnsw-index.md)): rebuild
from the authoritative tables. The rebuild is slower than the
substrate's because it needs to re-embed every entity name /
statement text representation.

**An extractor produces a malformed Statement** (e.g.,
references an `entity_type_id` not in `entity_types`).
Validation rejects it before the redb write; the extraction
attempt lands in the audit log with the failure reason.

---

## Configuration & tuning

Per-shard tuning at the knowledge layer is mostly downstream of
schema choices, but a few standing knobs:

| Knob | Where | Default | Notes |
|---|---|---|---|
| LLM extractor cache cap | `[knowledge.llm_cache]` TOML | 10 GiB | Hard cap before LRU eviction. |
| Statement HNSW params | code (`StatementHnswParams`) | M=32, ef_c=200, ef_s=128 | Denser than memory HNSW because statement count is lower per insert. |
| Entity HNSW params | code (`EntityHnswParams`) | M=16, ef_c=100, ef_s=64 | Lighter; entity count is small. |
| Backfill batch size | `WorkerConfig::defaults_for(Backfill)` | 256 | Bigger = more work per cycle, less responsive to shutdown. |
| Audit log retention | `audit_log_sweeper` cadence | 24 h | Daily sweep. |

Operational rules:

- **Pick the schema once, evolve it carefully.** Adding new
  entity types and predicates is free. Removing them needs
  data migration.
- **Watch the `extractor_audit` table.** A high failure rate
  there is a schema design problem.
- **Don't disable `forget_cascade`.** Without it, a hard FORGET
  on a Memory leaves orphan Statements pointing at zeroed
  evidence.
- **The LLM cache is the dominant disk cost.** Tune
  `llm_cache.cap_bytes` based on extractor traffic and disk
  budget.
- **Substrate-only is fine.** If you're not using the knowledge
  layer, you don't need to. The empty tables don't cost
  anything material.

---

## Where it lives in the code

| Topic | Path |
|---|---|
| Per-shard schema gate (`ArcSwap<bool>`) | `crates/brain-ops/src/schema_gate.rs` |
| Knowledge-table module map | `crates/brain-metadata/src/tables/knowledge/mod.rs` |
| Entity tables, `EntityMetadata` | `crates/brain-metadata/src/tables/knowledge/entity.rs` |
| Statement tables, `StatementMetadata` | `crates/brain-metadata/src/tables/knowledge/statement.rs` |
| Relation tables, `RelationMetadata` | `crates/brain-metadata/src/tables/knowledge/relation.rs` |
| Predicates, entity types, relation types | `crates/brain-metadata/src/tables/knowledge/{predicate.rs, entity_type.rs, relation_type.rs}` |
| Extractors, schema_versions, audit, merge | `crates/brain-metadata/src/tables/knowledge/{extractor.rs, schema_version.rs, audit.rs, merge.rs}` |
| `schema_upload` flow | `crates/brain-metadata/src/schema_store.rs` |
| Schema apply (intern types) | `crates/brain-metadata/src/schema_apply.rs` |
| Entity / statement / relation ops | `crates/brain-metadata/src/{entity_ops.rs, statement_ops.rs, relation_ops.rs}` |
| LLM extractor cache (separate redb file) | `crates/brain-metadata/src/llm_cache.rs` |
| Entity HNSW | `crates/brain-index/src/entity_hnsw.rs` |
| Statement HNSW | `crates/brain-index/src/statement_hnsw.rs` |
| Tantivy shard wrapper | `crates/brain-index/src/tantivy_shard/` |
| Tantivy recovery | `crates/brain-server/src/shard/tantivy_recovery.rs` |
| Path layout (`entity.hnsw`, `statement.hnsw`, `*.tantivy/`, `llm_cache.redb`) | `crates/brain-storage/src/layout.rs` |

---

## Further reading

- [03 — Arena and WAL](03-arena-and-wal.md) for the substrate
  layer the knowledge layer derives from, and the WAL frame
  format that carries knowledge-layer mutations.
- [04 — HNSW index](04-hnsw-index.md) for the per-shard HNSW
  shape that `entity.hnsw` and `statement.hnsw` follow.
- [05 — redb metadata](05-redb-metadata.md) for the redb
  patterns the knowledge tables use (composite keys, rkyv
  encoding, `redb::Value` impls via macro).
- [10 — Extractors](10-extractors.md) for the pattern →
  classifier → LLM tier composition that populates the
  knowledge layer.
- [11 — Hybrid retrieval (RRF)](11-hybrid-retrieval-rrf.md) for
  how the three retrievers (semantic / lexical / graph) read
  from these tables and fuse results.
- [12 — Query router](12-query-router.md) for how RECALL routes
  through the hybrid path when the schema gate is set.
