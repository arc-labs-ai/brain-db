# 10.02 Table Layout

The tables in the metadata store. Each table is a typed B-tree maintained by redb.

## 1. Catalog of tables

| Table name | Key | Value | Purpose |
|---|---|---|---|
| `memories` | `MemoryId` | `MemoryMetadata` | Per-memory metadata |
| `texts` | `MemoryId` | `Vec<u8>` | Memory text content (UTF-8) |
| `edges_out` | `(MemoryId, EdgeKind, MemoryId)` | `EdgeData` | Outgoing edges, indexed by (source, kind, target) |
| `edges_in` | `(MemoryId, EdgeKind, MemoryId)` | `EdgeData` | Incoming edges, indexed by (target, kind, source) |
| `contexts` | `ContextId` | `ContextMetadata` | Context records |
| `context_names` | `(AgentId, &str)` | `ContextId` | Context name → ID lookup per agent |
| `agent_contexts` | `(AgentId, ContextId)` | `()` | Membership of contexts in agents |
| `idempotency` | `RequestId` | `IdempotencyEntry` | Replay protection for ENCODE/FORGET |
| `agents` | `AgentId` | `AgentMetadata` | Per-agent metadata |
| `model_fingerprints` | `ModelFingerprint` | `ModelInfo` | Registry of seen model fingerprints |
| `checkpoints` | `u64` | `CheckpointInfo` | Checkpoint records |
| `next_lsn` | `()` | `u64` | The next WAL LSN (singleton) |
| `slot_versions` | `u64` (slot_id) | `u32` (version) | Per-slot versions, for lazy reclaim |

The table count is intentional: each table has a single, focused purpose. Brain does not pack multiple types into one table.

## 2. Memory ID as primary key

Most tables key by `MemoryId`. The 16-byte identifier is:

- Time-ordered (UUIDv7 prefix), so iterating gives chronological order.
- Cluster-friendly (same agent's memories share a high-bit prefix).

Range queries by MemoryId are common: "all memories created in the last hour", "all memories in this agent".

## 3. Edge tables: two for two directions

Edges are stored twice: once keyed by source (in `edges_out`) and once by target (in `edges_in`). This duplication enables:

- Forward queries ("what does memory X causally lead to?") via `edges_out`.
- Reverse queries ("what supports memory X?") via `edges_in`.

The duplication doubles edge storage but is essential for query performance. Without it, a reverse query would require scanning all edges.

For 1M memories with avg 8 edges each, 8M edges, doubled to 16M index entries. At ~30 bytes per entry, ~500 MB. Significant but bounded.

## 4. Composite keys

Several tables use composite keys for efficient range queries:

- `edges_out: (source, kind, target)` — listing all edges of a kind from a source is a tight range scan.
- `context_names: (agent_id, name)` — listing context names per agent is a range scan.

The composite key encoding is little-endian concatenation; redb sorts keys lexicographically, and the encoding makes that order match logical order (e.g., all edges from source X come before edges from source Y).

## 5. Value encoding

Values are encoded with **rkyv** (Brain's internal on-disk storage encoding). rkyv:

- Zero-copy deserialization: read a value, get a typed reference into the redb-mmap'd page.
- Compact: no per-field tags or alignment overhead.
- Schema-aware: each value type has a defined layout.

For variable-length values (text, edge lists), rkyv handles the indirection via offsets within the value blob.

## 6. Schema evolution

Each table has a format version embedded in its metadata. When Brain opens the metadata store:

- Read the format version of each table.
- If older than current, run any registered migrations.
- If newer than current, refuse to open (Brain is too old).

Migrations are detailed in [03.05 Schema Versioning](../03_schema/05_versioning.md).

## 7. The singleton tables

Some tables have at most one row:

- `next_lsn` — the next LSN counter.
- Any "global config" tables (rare).

Brain uses redb's `()` key type for singletons. Reading is `table.get(&())`, writing is `table.insert(&(), &value)`.

## 8. Index-only tables

`agent_contexts` is an index — its key is `(AgentId, ContextId)` and its value is `()`. This is a simple "is this context in this agent?" check.

Could Brain uses a HashSet in memory? Yes, but persistence matters; Brain may need to re-look-up after restart.

## 9. The texts table

The `texts` table holds the original memory text:

- Key: MemoryId.
- Value: UTF-8 bytes (variable length).

Text is read on demand:
- For RECALL responses (when the client requests the text).
- For consolidation (the worker reads source texts).
- For migration (re-embedding from the original text).

Detailed in [`03_substrate_tables.md`](03_substrate_tables.md) § Text Storage.

## 10. Context lookup

Two tables together make context lookup efficient:

- `contexts`: `ContextId → ContextMetadata`. Lookup by ID.
- `context_names`: `(AgentId, &str) → ContextId`. Lookup by name within an agent.

A context's name is typed scoped to its agent. Different agents can have contexts with the same name, but they're distinct contexts (different ContextIds).

The context's full record (with stats, timestamps, etc.) lives in `contexts`. The name table is the index.

## 11. Idempotency table TTL

The `idempotency` table grows with every ENCODE/FORGET. It's pruned on a TTL — entries older than the configured idempotency window (default 24 hours) are deleted by the maintenance worker.

Pruning is a periodic batch operation, not per-row. The worker scans for expired entries and deletes them in a single transaction. See [15. Background Workers](../15_background_workers/00_purpose.md) §Idempotency Cleanup.

## 12. The agents table

`agents` carries per-agent metadata:

- AgentId.
- Display name (optional).
- Created at.
- Stats (memory count, contexts count, etc. — updated periodically).
- Configuration overrides (per-agent quotas, etc.).

Looking up an agent by ID is O(log N) where N is the number of agents in the shard. Typical: a few thousand to a few million agents per shard.

## 13. The slot_versions table

When a slot is reclaimed (after FORGET + grace period), its version is incremented. The new version is recorded so that future MemoryIds with the new version know which slot they refer to.

The table maps `slot_id → current_version`. Looked up:

- During ENCODE to allocate a fresh MemoryId for a reclaimed slot.
- During recovery to verify HNSW node IDs match the slot's current state.

## 14. Total table count

In the v1 spec:

- 13 tables.

Adding a table is a schema change — it requires a format version bump in the metadata store. Tables are not added lightly.

## 15. Tables added when a schema is declared

Declaring a schema (via `SCHEMA_UPLOAD`) activates entity / statement / relation persistence. The following tables are added to the same `metadata.redb` file:

| Table | Key | Value | Purpose |
|---|---|---|---|
| `entities` | EntityId | Entity record (rkyv) | Primary entity table |
| `entity_by_canonical_name` | (entity_type_id, normalized_name) | EntityId | Exact-match resolution |
| `entity_aliases` | (entity_type_id, normalized_alias, EntityId) | () | Alias resolution |
| `entity_trigrams` | (entity_type_id, trigram, EntityId) | () | Fuzzy resolution |
| `entity_mentions` | (EntityId, MemoryId) | MentionMetadata | Reverse: memories mentioning entity |
| `statements` | StatementId | Statement record (rkyv) | Primary statement table |
| `statements_by_subject` | (EntityId, kind, predicate, is_current) | StatementId | Subject-anchored queries |
| `statements_by_predicate` | (PredicateId, kind, confidence_bucket) | StatementId | Predicate-anchored queries |
| `statements_by_object_entity` | (EntityId, kind) | StatementId | Reverse: who has X as object |
| `statements_by_event_time` | (event_at, EntityId) | StatementId | Time-range Event queries |
| `statements_by_evidence` | (MemoryId, StatementId) | () | Reverse: statements from memory |
| `statement_chain` | (chain_root, version) | StatementId | Supersession chain traversal |
| `evidence_overflow` | EvidenceOverflowId | Vec<MemoryId> (rkyv) | Long evidence lists |
| `relations` | RelationId | Relation record (rkyv) | Primary relation table |
| `relations_by_from` | (EntityId, relation_type, is_current) | RelationId | Outgoing edges |
| `relations_by_to` | (EntityId, relation_type, is_current) | RelationId | Incoming edges |
| `relations_by_evidence` | (MemoryId, RelationId) | () | Reverse: relations from memory |
| `predicates` | PredicateId | Predicate definition | Interned predicates |
| `entity_types` | EntityTypeId | EntityType definition | Interned entity types |
| `relation_types` | RelationTypeId | RelationType definition | Interned relation types |
| `extractors` | ExtractorId | ExtractorDefinition | Active extractors |
| `schema_versions` | u32 | SchemaDocument | Schema history |
| `extractor_audit` | AuditId | ExtractionAudit | Per-extraction audit |
| `entity_resolution_audit` | AuditId | ResolutionAudit | Per-resolution audit |
| `merge_log` | (timestamp, merge_id) | MergeRecord | Entity merges |

The same composite-key + rkyv-value conventions apply (§4, §5).

## 16. Per-shard non-redb storage

A shard with a declared schema also maintains derived indexes outside `metadata.redb`: two tantivy directories, two HNSW files, and a separate redb file for the LLM extractor cache.

### 16.1 Tantivy indexes

Two tantivy indexes per shard.

#### `memory_text.tantivy/`

Indexes memory text. Fields:
- `text` (TEXT, tokenized, stemmed, indexed for BM25)
- `agent_id` (STRING, indexed for filter)
- `kind` (STRING, indexed for filter)
- `created_at` (DATE, indexed for range queries)
- `memory_id` (STORED only, not indexed)

Tokenizer: lowercase + Porter stemmer + URL/code-identifier preservation (custom token filter).

#### `statements.tantivy/`

Indexes statement text representations. Fields:
- `subject_name` (TEXT, indexed)
- `predicate_name` (STRING, indexed for exact match)
- `object_text` (TEXT, indexed)
- `kind` (STRING, indexed for filter)
- `confidence_bucket` (INT, indexed for range; bucketed in 0.1 increments)
- `extracted_at` (DATE, indexed)
- `statement_id` (STORED only)

When a statement is created, a worker computes its text representation and indexes it. On supersession/tombstone, the worker removes the index entry.

### 16.2 HNSW additions

Beyond the memory HNSW (section 06):

#### `entity.hnsw`

384-dim HNSW for entity embeddings. Smaller than memory HNSW (entities are typically 100-1000x fewer than memories). Used by the entity resolver for tier-3 similarity matching.

Parameters: M=16, ef_construction=100, ef_search=64 (default).

Maintenance: entity-embedding worker re-embeds on entity create or rename.

#### `statement.hnsw`

384-dim HNSW for statement embeddings. Used by the semantic retriever to find statements similar to a query.

The embedded representation: `subject.canonical_name + " " + predicate.name + " " + object_text`. Compact and captures the statement's semantic core.

Parameters: M=32, ef_construction=200, ef_search=128 (similar to memory HNSW; statements may be ~0.1–1x as many as memories depending on extraction density).

Maintenance: statement-embedding worker re-embeds on statement create or update.

### 16.3 LLM extractor cache

Separate redb file per shard: `llm_cache.redb`. Kept apart from `metadata.redb` so the heavy cache doesn't bloat the hot metadata file.

| Table | Key | Value | Purpose |
|---|---|---|---|
| `llm_responses` | (input_hash, extractor_id, extractor_version, model_id) | LLM raw response (rkyv) | Idempotency cache |
| `llm_response_ttl` | (expiry_timestamp, cache_key) | () | TTL index for sweeper |

The cache sweeper worker (low priority) periodically removes expired entries.

Cache size cap: configurable, default 10 GB per shard. When cap is hit, LRU eviction.

## 17. Index rebuild

Tantivy and HNSW indexes are derived. On disaster recovery or operator request, they can be rebuilt from the authoritative redb tables and the WAL:

```
rebuild --index=statements.tantivy --shard=000
  ├─ Open redb `statements` table
  ├─ For each non-tombstoned, non-superseded statement:
  │     compute text repr
  │     add to tantivy index
  ├─ Commit
  └─ Mark index version
```

Rebuilds run in the background; the server continues to serve from existing indexes during the rebuild. On completion, the new index replaces the old atomically.

## 18. Entity storage detail

The entity tables introduced in §14 carry additional structure for the resolver pipeline and the merge / unmerge audit trail.

### 18.1 `entities`

```
key:   EntityId (16 bytes)
value: rkyv-serialized Entity (canonical_name, aliases, type, attributes, mention_count, timestamps, merged_into, embedding_version)
```

### 18.2 `entity_by_canonical_name`

```
key:   (entity_type_id: u32, normalized_name: String)
value: EntityId
```

Secondary index for tier-1 exact resolution.

### 18.3 `entity_aliases`

```
key:   (entity_type_id: u32, normalized_alias: String, entity_id: EntityId)
value: () (membership only)
```

Aliases index for tier-1 resolution. Composite key allows the same alias to map to entities of different types.

### 18.4 `entity_trigrams`

```
key:   (entity_type_id: u32, trigram: [u8; 3], entity_id: EntityId)
value: ()
```

Trigram index for tier-2 fuzzy resolution. Each entity's canonical_name contributes its trigrams. Similarity scored at query time via index intersection + Jaccard.

### 18.5 `entity_mentions`

```
key:   (entity_id: EntityId, memory_id: MemoryId)
value: MentionMetadata (offset in memory text, confidence, extractor_id)
```

Reverse: which memories mention each entity. Used for graph queries and provenance.

### 18.6 `entity_resolution_audit`

```
key:   AuditId (16 bytes)
value: rkyv-serialized ResolutionAudit
```

### 18.7 `entity_merge_log`

```
key:   (timestamp_unix_nanos: u64, merge_id: [u8; 16])
value: MergeRecord
```

`MergeRecord` carries the **complete diff** between pre-merge and post-merge state. Unmerge replays this diff in reverse:

```rust
pub struct MergeRecord {
    pub merge_id_bytes: [u8; 16],
    pub survivor_bytes: [u8; 16],
    pub merged_bytes: [u8; 16],

    // Pre-merge / post-merge identity.
    pub merged_at_unix_nanos: u64,
    pub grace_period_until_unix_nanos: u64,
    pub confidence: f32,
    pub reason: String,                                // ≤ 4 KiB; operator-supplied
    pub actor_kind: u8,                                // 0 = System, 1 = Agent
    pub actor_agent_bytes: [u8; 16],                   // [0;16] when actor_kind=System

    // Diffs against the survivor (replayed in reverse by unmerge).
    pub aliases_added: Vec<String>,                    // aliases merged contributed to survivor
    pub trigrams_added: Vec<[u8; 3]>,                  // trigrams contributed (derived from
                                                       // aliases_added + merged.canonical_name)
    pub attribute_conflicts: Vec<AttributeConflictRecord>,

    // Re-routing counts (lists deferred to overflow rows when large).
    pub statements_rerouted: u32,
    pub relations_rerouted: u32,
    pub mention_count_added: u32,                      // survivor.mention_count += this on merge

    // Status.
    pub finalized: u8,                                 // 0 = reversible, 1 = grace expired / unmerge invalid
    pub unmerged_at_unix_nanos: u64,                   // 0 = still merged
    pub unmerged_by_actor_kind: u8,                    // 0 if !unmerged
    pub unmerged_by_agent_bytes: [u8; 16],             // [0;16] if !unmerged or actor=System
}

pub struct AttributeConflictRecord {
    pub attribute_key: String,
    pub survivor_value_blob: Vec<u8>,                  // rkyv-encoded original survivor value
    pub merged_value_blob: Vec<u8>,                    // rkyv-encoded original merged value
    pub policy: u8,                                    // 1=survivor_wins, 2=merged_wins,
                                                       // 3=newest_wins, 4=concat_text, 5=reject_merge
    pub outcome: u8,                                   // 1=KeptSurvivor, 2=ReplacedWithMerged,
                                                       // 3=Concatenated
}
```

The per-statement / per-relation rerouted-id lists live in a sibling overflow table:

```
ENTITY_MERGE_AUDIT_OVERFLOW
key:   ([u8; 16] merge_id, u32 chunk_index)
value: MergeAuditOverflow { rerouted_statement_ids: Vec<[u8; 16]>, rerouted_relation_ids: Vec<[u8; 16]> }
```

### 18.8 Entity embedding HNSW

A per-shard HNSW index, separate from the main memory HNSW:

- Index: `entity_embeddings.hnsw`
- Vector dim: 384 (same as memory)
- Parameters: M=16, ef_construction=100 (lower than memory; entity count is smaller), ef_search=64
- Tombstoned entities are removed via the standard HNSW tombstone+rebuild cycle.

### 18.9 Entity storage costs

For a deployment with N entities, each averaging:
- canonical_name: 30 chars
- aliases: 5 × 30 chars = 150 chars
- 5 attributes averaging 50 bytes each = 250 bytes
- embedding: 1536 bytes

Per entity: ~2 KB in the main table. Plus index entries: ~200 bytes per entity across all indexes. Plus HNSW: ~3 KB per entity (vector + HNSW links).

Total: ~5 KB per entity. 100K entities = 500 MB. 1M entities = 5 GB.

This is small relative to memory storage (memories are typically 2 KB of text + 1.6 KB slot = ~4 KB each, with M memories typically >> N entities).

### 18.10 Entity read paths

| Query | Path |
|---|---|
| Get entity by ID | redb `entities` lookup (O(log N) seek) |
| Exact name resolution | `entity_by_canonical_name` or `entity_aliases` lookup |
| Fuzzy resolution | `entity_trigrams` intersection of candidate trigrams, scored |
| Embedding resolution | Entity HNSW search |
| All memories mentioning entity | `entity_mentions` prefix scan |
| All statements with subject = entity | `statements_by_subject` index |
| All relations involving entity | `relations_by_from` + `relations_by_to` |

### 18.11 Entity write paths

Entity creation (new):
1. Generate EntityId (UUIDv7).
2. Write to `entities`.
3. Write to `entity_by_canonical_name`.
4. Write trigrams to `entity_trigrams`.
5. Embed and write to entity HNSW (async, doesn't block).
6. Commit redb transaction.

All steps except 5 are in a single redb transaction (single-writer-per-shard discipline). Step 5 is async; embedding-based resolution may miss this entity for a few seconds.

Entity update (rename, attribute change):
1. Read current entity.
2. Compute delta (which indexes need update).
3. Update `entities`.
4. Update `entity_by_canonical_name` (remove old, add new) if canonical_name changed.
5. Update `entity_trigrams` (remove old set, add new) if canonical_name changed.
6. Queue async re-embedding.

## 19. Statement storage detail

The 8 statement tables introduced in §14 carry the following structure.

### 19.1 `statements` (primary)

```
key:   StatementId.to_bytes() ([u8; 16])
value: StatementMetadata
```

Primary lookup. `StatementMetadata` is the rkyv-archived row carrying every statement field.

#### The four-timestamp model

`StatementMetadata` carries a **four-timestamp** record (the Zep model):

| Field | Meaning |
|---|---|
| `valid_from_unix_nanos` | Object time start — when the claim *became true in the world*. |
| `valid_to_unix_nanos` | Object time end — when the claim stopped being true. |
| `extracted_at_unix_nanos` | Record time start — when Brain learned the claim. |
| `record_invalidated_at_unix_nanos: Option<u64>` | Record time end — when Brain *unlearned* the claim (typically set on supersede; `None` while the row is the current belief). |

Object time (`valid_from`/`valid_to`) is "what was true in the world"; record time (`extracted_at`/`record_invalidated_at`) is "what Brain believed". Splitting them lets time-travel queries answer "what did Brain believe on date X" without resurrecting tombstones — the row stays in the table with `record_invalidated_at` set, and the planner's `as_of(record_time)` filter selects rows whose record window contains the target time.

`record_invalidated_at` is server-internal in v1.0 — it lands as a field on `StatementMetadata` and as a filter on the planner's filter chain, but no wire op exposes it yet. Wire exposure is a v1.1 surface decision.

### 19.2 `statements_by_subject`

```
key:   (subject_entity_bytes: [u8; 16], kind: u8, predicate_id: u32, is_current: u8)
value: StatementId.to_bytes()
```

Compound key lets "what's Priya's current role?" be a point lookup at `(priya_id, Fact, role_predicate_id, 1)`.

`is_current = 1` iff `superseded_by.is_none() && !tombstoned && valid_at(now)`. The bit is **derived** — supersession / tombstone / validity-time-out flips it; the underlying StatementMetadata also has the source-of-truth fields.

### 19.3 `statements_by_predicate`

```
key:   (predicate_id: u32, kind: u8, confidence_bucket: u8)
value: StatementId.to_bytes()
```

`confidence_bucket = floor(confidence * 10).clamp(0, 10)`. Coarse quantisation so the index is dense (11 buckets) but still useful for "all high-confidence Facts with predicate `manages`".

### 19.4 `statements_by_object_entity`

```
key:   (object_entity_bytes: [u8; 16], kind: u8)
value: StatementId.to_bytes()
```

Reverse index for "what statements have X as their object?". Populated only when `object` is the `Entity(...)` variant — `Value` / `Memory` / `Statement` objects skip this index.

### 19.5 `statements_by_event_time`

```
key:   (event_at_unix_nanos: u64, subject_entity_bytes: [u8; 16])
value: StatementId.to_bytes()
```

Time-range queries for Events. `event_at` only — populated only for `kind == Event`. The compound second-component (subject) disambiguates same-time events about the same subject.

### 19.6 `statements_by_evidence`

```
key:   (memory_id_bytes: [u8; 16], statement_id_bytes: [u8; 16])
value: ()
```

Reverse index: "which statements reference memory M as evidence?". Used by FORGET cascade: when memory M is forgotten / retracted, Brain finds all dependent statements and decides per-kind whether to tombstone, supersede, or just record provenance loss.

Population: one row per `(MemoryId, StatementId)` pair in `evidence.inline` (or every `MemoryId` reachable from `evidence.Overflow`).

### 19.7 `statement_chain`

```
key:   (chain_root_bytes: [u8; 16], version: u32)
value: StatementId.to_bytes()
```

Supersession chain. Prefix-scan `(chain_root, *)` returns the full chain in version order.

### 19.8 `evidence_overflow`

```
key:   EvidenceOverflowId.to_bytes() ([u8; 16])
value: EvidenceOverflow { memory_ids: Vec<[u8; 16]>, extractor_ids: Vec<u32> }
```

For statements with > 8 evidence memories (the inline cap).

### 19.9 Per-create index writes

`statement_create` writes to multiple tables in one redb txn:

```text
For each new Statement S:
  1. STATEMENTS_TABLE.insert(S.id, StatementMetadata::from(S))
  2. STATEMENTS_BY_SUBJECT_TABLE.insert(
         (S.subject_bytes, S.kind, S.predicate_id, is_current_bit), S.id_bytes)
  3. STATEMENTS_BY_PREDICATE_TABLE.insert(
         (S.predicate_id, S.kind, confidence_bucket(S.confidence)), S.id_bytes)
  4. if let Object::Entity(eid) = S.object:
         STATEMENTS_BY_OBJECT_ENTITY_TABLE.insert((eid_bytes, S.kind), S.id_bytes)
  5. if S.kind == Event:
         STATEMENTS_BY_EVENT_TIME_TABLE.insert(
             (S.event_at_unix_nanos, S.subject_bytes), S.id_bytes)
  6. For each mem_id in evidence.inline:
         STATEMENTS_BY_EVIDENCE_TABLE.insert((mem_id_bytes, S.id_bytes), ())
     For overflow_id in evidence.Overflow:
         load EvidenceOverflow; iterate memory_ids; same insertion
  7. STATEMENT_CHAIN_TABLE.insert((S.chain_root_bytes, S.version), S.id_bytes)
```

Total: 7 index writes (plus per-evidence inserts) on a typical create.

### 19.10 Per-supersede index updates

In addition to `statement_create` of the new statement, `statement_supersede` also updates the **old** statement:

```text
old.superseded_by = Some(new.id)
old.valid_to_unix_nanos = new.extracted_at_unix_nanos

Rewrite old in STATEMENTS_TABLE.

Remove old's STATEMENTS_BY_SUBJECT_TABLE entry with is_current=1;
re-insert with is_current=0.
```

Other indexes (`by_predicate`, `by_object_entity`, `by_event_time`, `by_evidence`) don't care about `is_current` and stay unchanged.

### 19.11 Per-tombstone index updates

```text
Set fields:
  tombstoned = true
  tombstoned_at_unix_nanos = now
  tombstone_reason = reason byte

Rewrite in STATEMENTS_TABLE.

Re-insert into STATEMENTS_BY_SUBJECT_TABLE with is_current=0 (flipping
the bit; the lookup for "current state of X" no longer finds this row).
```

Reverse-evidence index entries (§19.6 `statements_by_evidence`) are **preserved** so audit / cascade can still find the tombstoned statement.

### 19.12 Per-retract reclamation

`statement_retract` is the hard-delete variant. It:

1. Tombstones as in §19.11.
2. Schedules zero-out after `RETRACT_GRACE_NANOS` (default 30 days) — handled by the periodic GC worker.
3. At reclamation: remove from **all** tables except the audit row in `entity_resolution_audit` (kind discriminator `STATEMENT_RETRACTED`).

`STATEMENTS_BY_EVIDENCE_TABLE` is also stripped — the dependency is gone since the row no longer exists.

### 19.13 Statement storage costs

For a deployment with M statements averaging:

- Fixed fields (`StatementMetadata`): ~256 bytes.
- `object` (tagged union): 16-64 bytes typical.
- Inline evidence: 0-128 bytes (8 × 16-byte MemoryIds, max).
- Indexes: ~200 bytes per statement across all 6 secondary indexes.

Total: ~500-700 bytes per statement primary row + indexes. 10M statements ≈ 5-7 GB. Plus statement HNSW: ~3 KB per statement (1536-byte vector + HNSW links).

### 19.14 Statement read paths

| Query | Path |
|---|---|
| Get statement by id | `STATEMENTS_TABLE` point lookup (O(log M)). |
| Current state for `(subject, predicate)` | `STATEMENTS_BY_SUBJECT_TABLE` point lookup at `(subject, kind, predicate_id, 1)`. |
| History of a chain | `STATEMENT_CHAIN_TABLE` prefix scan at `(chain_root, *)`. |
| All Facts with predicate X | `STATEMENTS_BY_PREDICATE_TABLE` prefix scan at `(predicate_id, Fact, *)`. |
| What references entity X as object? | `STATEMENTS_BY_OBJECT_ENTITY_TABLE` prefix scan at `(X_bytes, *)`. |
| Events in time range | `STATEMENTS_BY_EVENT_TIME_TABLE` range scan. |
| Statements depending on memory M | `STATEMENTS_BY_EVIDENCE_TABLE` prefix scan at `(M_bytes, *)`. |

### 19.15 Statement write paths summary

| Operation | Tables written |
|---|---|
| `statement_create` | `STATEMENTS` + 6 indexes + chain (7 inserts) + 1 per evidence memory |
| `statement_supersede` | All `statement_create` writes for new + 2 rewrites (old `STATEMENTS` + flip `is_current` in `STATEMENTS_BY_SUBJECT`) |
| `statement_tombstone` | `STATEMENTS` rewrite + flip `is_current` in `STATEMENTS_BY_SUBJECT` (2 writes) |
| `statement_retract` | tombstone-equivalent at write time; reclaim later |

All operations execute inside one redb `WriteTransaction`. Commit makes them atomic — half-completed indexes never observed.

### 19.16 Statement sharding

Statements live on the **subject's** shard. `statement_create` routes to that shard via the routing table.

Cross-shard concerns:
- `statements_by_object_entity` is on the **object** entity's shard. A statement with `subject` on shard A and `object` on shard B writes its `by_object_entity` index entry to shard B's redb, via the existing cross-shard write path (`WriterHandle::route_index_write`).
- Cross-shard joins (e.g. "all statements where subject = X and object = Y") aren't first-class; the query router fans out.

## 20. Relation storage detail

The 4 relation tables introduced in §14 carry the following structure.

### 20.1 `relations` (primary)

```
key:   RelationId.to_bytes() ([u8; 16])
value: RelationMetadata
```

Primary lookup. `RelationMetadata` is the rkyv-archived row carrying every relation field.

### 20.2 `relations_by_from`

```
key:   (from_entity_bytes: [u8; 16], relation_type_id: u32, is_current: u8)
value: RelationId.to_bytes()
```

Outgoing-edges index. For asymmetric relations, populated only with the row's actual `from`. For symmetric relations, populated with the **canonical_from**.

`is_current = 1` iff `superseded_by.is_none() && !tombstoned`. Derived bit; the `RelationMetadata` carries the source-of-truth fields.

### 20.3 `relations_by_to`

```
key:   (to_entity_bytes: [u8; 16], relation_type_id: u32, is_current: u8)
value: RelationId.to_bytes()
```

Incoming-edges index. For asymmetric relations, populated with `to`. For symmetric relations, populated with **canonical_to** — **plus** an entry under `(canonical_from, type, is_current)` so either endpoint queries return the relation.

### 20.4 `relations_by_evidence`

```
key:   (memory_id_bytes: [u8; 16], relation_id_bytes: [u8; 16])
value: ()
```

Reverse index for the FORGET cascade. One row per `(MemoryId, RelationId)` pair in `RelationMetadata.evidence_inline`. When a memory is forgotten, this index finds all relations that referenced it; the FORGET worker decides per-cardinality whether to tombstone, supersede with reduced evidence, or just record provenance loss.

### 20.5 Deferred: `relations_by_type`

A per-type index for "all current relations of type T" queries. Not present in v1.0 scaffolding. Deferred if traversal performance demands it — most queries filter by `from / to` first, which already narrows the candidate set.

If added later, the key would be `(relation_type_id, is_current, created_at_unix_nanos)` and the value `RelationId.to_bytes()`. The `created_at` suffix gives time-ordered scans for admin queries.

### 20.6 Per-create index writes

`relation_create` writes to all relevant tables in one redb txn:

```text
For each new Relation R:
  1. RELATIONS_TABLE.insert(R.id, RelationMetadata::from(R))
  2. RELATIONS_BY_FROM_TABLE.insert(
        (effective_from_bytes, R.relation_type_id, is_current_bit),
        R.id_bytes)
  3. RELATIONS_BY_TO_TABLE.insert(
        (effective_to_bytes, R.relation_type_id, is_current_bit),
        R.id_bytes)
  4. For each mem_id in R.evidence_inline:
       RELATIONS_BY_EVIDENCE_TABLE.insert(
           (mem_id_bytes, R.id_bytes), ())
```

For symmetric relations, **both** endpoints get an entry in **both** directional indexes — so the relation is reachable from either side regardless of which directional table the query consulted. Total index writes: 4 (BY_FROM × 2 + BY_TO × 2 if symmetric, else 1 each) plus per-evidence inserts.

### 20.7 Per-supersede index updates

`relation_supersede` runs `relation_create` for the new relation, then updates the **old** in place:

```text
old.superseded_by = Some(new.id)
old.valid_to_unix_nanos = new.extracted_at  (if not pinned)

Rewrite old in RELATIONS_TABLE.

Remove old's RELATIONS_BY_FROM entry at is_current=1;
re-insert at is_current=0.
Same for RELATIONS_BY_TO (including symmetric dual-index removal).
```

### 20.8 Per-tombstone index updates

```text
Set fields:
  tombstoned = true
  tombstoned_at_unix_nanos = now

Rewrite in RELATIONS_TABLE.

Re-insert in BY_FROM / BY_TO with is_current=0 (flipping the bit).
```

Reverse-evidence index entries (§20.4 `relations_by_evidence`) are **preserved** so FORGET cascade can still find tombstoned relations whose evidence is being deleted.

### 20.9 Hard reclamation

V1.0 doesn't ship a `RELATION_RETRACT` opcode — tombstone is soft by default; a future GC sweeper analogous to the statement retract path may add this.

### 20.10 Relation storage costs

For a deployment with R relations averaging:

- Fixed fields (`RelationMetadata`): ~200 bytes.
- `properties_blob`: 0 bytes (no schema-DSL properties in v1.0 minimal).
- Inline evidence: 0–128 bytes (8 × 16-byte MemoryIds, max).
- Indexes: ~80 bytes per relation across BY_FROM + BY_TO + BY_EVIDENCE.

Total: ~400–500 bytes per relation primary row + indexes. 10M relations ≈ 4–5 GB.

### 20.11 Relation read paths

| Query | Path |
|---|---|
| Get relation by id | `RELATIONS_TABLE` point lookup (O(log R)). |
| Outgoing for entity | `RELATIONS_BY_FROM_TABLE` prefix scan at `(entity, *)`. |
| Incoming for entity | `RELATIONS_BY_TO_TABLE` prefix scan at `(entity, *)`. |
| Filtered by type | Same with type byte in key. |
| Current-only | Prefix terminates at `is_current = 1`. |
| Relations dependent on memory M | `RELATIONS_BY_EVIDENCE_TABLE` prefix scan at `(M, *)`. |

### 20.12 Relation write paths summary

| Operation | Tables written |
|---|---|
| `relation_create` (asymmetric) | RELATIONS + BY_FROM + BY_TO + BY_EVIDENCE (4 + evidence count) |
| `relation_create` (symmetric)  | RELATIONS + BY_FROM × 2 + BY_TO × 2 + BY_EVIDENCE (5 + evidence count) |
| `relation_supersede` | All create writes for new + 2 rewrites (old RELATIONS + flip is_current in BY_FROM / BY_TO) |
| `relation_tombstone` | RELATIONS rewrite + flip is_current in BY_FROM / BY_TO |

All operations execute inside one redb `WriteTransaction`.

### 20.13 Relation sharding

Relations are sharded by `canonical_from` (or `from` for asymmetric) EntityId. Cross-shard concerns:

- A symmetric relation with `canonical_from` on shard A and `canonical_to` on shard B: primary lives on A, but the `RELATIONS_BY_TO` entry for `canonical_to` belongs on B's shard. V1.0 ships same-shard only; cross-shard reverse-index writes follow the entity-side path later.

---

*Continue to [`03_substrate_tables.md`](03_substrate_tables.md) for memory metadata, edge, context, idempotency, and text storage details.*
