# Storage — Knowledge Layer

The substrate storage layout (sections 05–07) provides arena, WAL, redb, and HNSW. The knowledge layer reuses all of it and adds further on-disk artifacts within the same shard directory.

## Per-shard layout

```
data/
  shard-000/
    arena.bin              ── substrate (memory slots, section 05)
    arena.wal              ── substrate (write-ahead log, section 05)
    metadata.redb          ── substrate + knowledge tables (sections 07, 26)
    memory.hnsw            ── substrate (memory vectors, section 06)
    
    # knowledge-layer additions
    statements.tantivy/    ── full-text index over statements
    memory_text.tantivy/   ── full-text index over memory text
    entity.hnsw            ── entity-embedding HNSW
    statement.hnsw         ── statement-embedding HNSW
    llm_cache.redb         ── LLM extractor result cache
```

## New redb tables

Added to `metadata.redb` alongside the substrate's tables:

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

## Per-shard tantivy

Two tantivy indexes per shard:

### `memory_text.tantivy/`

Indexes memory text. Fields:
- `text` (TEXT, tokenized, stemmed, indexed for BM25)
- `agent_id` (STRING, indexed for filter)
- `kind` (STRING, indexed for filter)
- `created_at` (DATE, indexed for range queries)
- `memory_id` (STORED only, not indexed)

Tokenizer: lowercase + Porter stemmer + URL/code-identifier preservation (custom token filter).

### `statements.tantivy/`

Indexes statement text representations. Fields:
- `subject_name` (TEXT, indexed)
- `predicate_name` (STRING, indexed for exact match)
- `object_text` (TEXT, indexed)
- `kind` (STRING, indexed for filter)
- `confidence_bucket` (INT, indexed for range; bucketed in 0.1 increments)
- `extracted_at` (DATE, indexed)
- `statement_id` (STORED only)

When a statement is created, a worker computes its text representation and indexes it. On supersession/tombstone, the worker removes the index entry.

## Per-shard HNSW additions

Beyond the substrate's memory HNSW (section 06):

### `entity.hnsw`

384-dim HNSW for entity embeddings. Smaller than memory HNSW (entities are typically 100-1000x fewer than memories). Used by the entity resolver for tier-3 similarity matching.

Parameters: M=16, ef_construction=100, ef_search=64 (default).

Maintenance: entity-embedding worker re-embeds on entity create or rename.

### `statement.hnsw`

384-dim HNSW for statement embeddings. Used by the semantic retriever to find statements similar to a query.

The embedded representation: `subject.canonical_name + " " + predicate.name + " " + object_text`. Compact and captures the statement's semantic core.

Parameters: M=32, ef_construction=200, ef_search=128 (similar to memory HNSW; statements may be ~0.1–1x as many as memories depending on extraction density).

Maintenance: statement-embedding worker re-embeds on statement create or update.

## LLM extractor cache

Separate redb file per shard: `llm_cache.redb`. Kept apart from `metadata.redb` so the heavy cache doesn't bloat the hot metadata file.

| Table | Key | Value | Purpose |
|---|---|---|---|
| `llm_responses` | (input_hash, extractor_id, extractor_version, model_id) | LLM raw response (rkyv) | Idempotency cache |
| `llm_response_ttl` | (expiry_timestamp, cache_key) | () | TTL index for sweeper |

The cache sweeper worker (low priority) periodically removes expired entries.

Cache size cap: configurable, default 10 GB per shard. When cap is hit, LRU eviction.

## WAL frame types (knowledge layer)

The substrate WAL writes memory frames (section 05). The knowledge layer adds frame types:

| Frame type | Body |
|---|---|
| 0x01 MEMORY_WRITE | (substrate) memory record |
| 0x02 MEMORY_TOMBSTONE | (substrate) tombstone mark |
| 0x10 ENTITY_CREATE | entity record |
| 0x11 ENTITY_UPDATE | entity delta |
| 0x12 ENTITY_MERGE | merge record |
| 0x13 ENTITY_TOMBSTONE | tombstone mark |
| 0x20 STATEMENT_CREATE | statement record |
| 0x21 STATEMENT_SUPERSEDE | (old, new) supersession |
| 0x22 STATEMENT_TOMBSTONE | tombstone |
| 0x30 RELATION_CREATE | relation record |
| 0x31 RELATION_SUPERSEDE | supersession |
| 0x32 RELATION_TOMBSTONE | tombstone |
| 0x40 SCHEMA_UPDATE | schema document |
| 0x50 AUDIT | audit entry (for replay) |

WAL frame header is unchanged (32-byte fixed). The frame type field selects the body parser.

Recovery: on startup, the WAL is replayed. Memory frames hydrate substrate state; entity/statement/relation frames hydrate knowledge-layer state. Derived indexes (tantivy, HNSW) are rebuilt from authoritative state if missing or corrupt.

## Index rebuild

The tantivy and HNSW indexes are derived. On disaster recovery or operator request, they can be rebuilt from the authoritative redb tables and the WAL:

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

## Storage budget

For a 1M-memory deployment with extraction density ~1 statement per 2 memories, ~10K entities, ~500 relations:

| Storage | Size |
|---|---|
| Substrate arena + WAL | ~4 GB |
| Substrate memory HNSW | ~5 GB |
| Substrate metadata (memory) | ~500 MB |
| Entities table | ~50 MB |
| Statements table | ~150 MB |
| Relations table | ~5 MB |
| Entity HNSW | ~50 MB |
| Statement HNSW | ~2 GB |
| Memory tantivy | ~500 MB |
| Statement tantivy | ~100 MB |
| LLM cache | ~1 GB (configurable cap) |
| Audit logs | ~200 MB |
| **Total knowledge-layer addition** | ~4 GB |

The knowledge layer roughly doubles storage cost compared to substrate-only deployments. Acceptable for the capabilities gained.
