# Composition: How the Three Layers Interact

## The fundamental invariant

> Every Statement and every Relation has provenance traceable to one or more Memories. The substrate (Layer 1) is always authoritative; derived data (Layers 2 and 3) can always be recomputed.

This invariant drives several rules:

## Rule 1: Memories never reference statements

A Memory does not have a field linking to its derived statements. Direction is one-way: Statements point to evidence Memories, not the other way around.

Reason: Memories are the substrate. Statements are derived. If we let Memories reference Statements, we couple the substrate to a layer that may be regenerated. The reverse coupling (Statement → Memory) is fine because Statements are disposable.

To query "what statements were derived from this memory?", we maintain a `memory_to_statement` reverse index, updated by extractors. The index is a derived structure; it can be rebuilt from the statements.

## Rule 2: Entities can exist without statements

An Entity is created when first mentioned (by an extractor) and persists even if all statements about it are forgotten or invalidated. This is intentional:

- Entities are identity anchors. Removing them every time their last statement is removed creates churn.
- An Entity may be referenced by other Entities through Relations. If we drop Entities, we'd need to cascade through Relations.
- The cost of an "orphan" Entity is small: one row in the entity table. The cost of recreating it later (re-resolving aliases, re-generating embedding) is higher.

A background worker (entity garbage collection, optional and configurable) can prune Entities with no recent references after a long grace period (e.g., 90 days). Off by default.

## Rule 3: FORGET on a Memory triggers re-derivation

When a user forgets a Memory:

1. **Soft FORGET (tombstone)**: 
   - The Memory is marked tombstoned (see section 02 lifecycle).
   - Statements with this Memory in their `evidence` list are flagged for re-derivation.
   - The re-derivation worker decides: with this evidence removed, does the Statement still hold? If yes (other evidence remains), update confidence. If no, supersede with `superseded_by = null`.
   
2. **Hard FORGET (purge)**:
   - The Memory's vector and text are zeroed (see section 02 lifecycle).
   - Statements with this Memory in their `evidence` list have the reference removed.
   - If a Statement loses *all* its evidence, it is supersedes-with-null, and the supersede record's evidence becomes the FORGET audit entry itself (so the chain stays auditable even without the original).

The substrate's FORGET handler to check the `memory_to_statement` reverse index and queue re-derivation jobs.

## Rule 4: Entities are deduplicated by canonical name + embedding

When an extractor mentions "Priya," the entity resolver checks (in order):

1. Exact match on canonical name or alias in the entity table.
2. Fuzzy match (trigram similarity above threshold) on canonical names.
3. Embedding similarity (the candidate name + context embedded; nearest entity in the entity HNSW above threshold).
4. (Optional, configurable) LLM resolution against top-K candidates.

If no match above threshold: a new Entity is created.

If multiple matches: the extractor reports ambiguity. The Statement is still written, but the subject is a placeholder `EntityId` linked to a pending-resolution audit record. A background worker, possibly with LLM assistance, attempts to resolve. Until resolved, the Statement is queryable but not graph-joined.

## Rule 5: Cross-layer queries are first-class

The query engine supports queries that span all three layers:

- "Get the memories that supported this Fact" → Statement → evidence list → fetch Memories.
- "Find recent memories mentioning Priya" → Entity → mention index → Memories with timestamp filter.
- "Find statements about anyone who reports to Priya" → Entity (Priya) → Relations (reports_to, reverse) → Entities (subordinates) → Statements (subjects in subordinate set).

The planner (Section 08) plans cross-layer queries by composing per-layer retrievers and joins.

## Rule 6: Schemas can evolve; data must be migratable

When a user changes the schema (adds a new entity type, adds a new predicate, modifies an extractor):

- Existing Entities and Statements are not invalidated.
- A `schema_version` field tracks which version of the schema each Statement was extracted under.
- When schemas change, a migration worker can re-run extractors over the relevant Memories. The new Statements get the new `schema_version`. Old Statements remain queryable but flagged as stale.
- Users can choose to keep stale statements (audit value), supersede them with new extractions, or hard-delete them.

This is detailed in `21_schema_dsl/` and `25_provenance_versioning/`.

## Rule 7: The substrate can run without Layer 2/3

A user can deploy the knowledge layer and never declare a schema. In this case:

- The substrate behaves exactly in pure substrate mode: ENCODE, RECALL by cosine.
- The Entity, Statement, and Relation tables exist but are empty.
- The tantivy index is empty.
- The query router falls back to single-retriever HNSW.

This is the substrate-only mode, used by deployments that only need vector retrieval.

When the user declares their first schema, extractors begin running on incoming memories. They can optionally trigger a backfill over existing memories. The substrate switches the query router to hybrid mode.

## What gets stored where: summary

| Data | Storage | Authoritative? |
|---|---|---|
| Raw memory text and vector | Arena + WAL  | Yes |
| Memory metadata (timestamps, agent, kind) | redb `memory_meta`  | Yes |
| Memory-to-memory edges (section 02) | redb `edges`  | Yes |
| Entity records | redb `entities` | Yes (declarative identity) |
| Entity embeddings (for resolution) | Entity HNSW | Derived (recomputable from entity table) |
| Statement records | redb `statements` | Derived (recomputable from memories + schema) |
| Relation records | redb `relations` | Derived (recomputable from memories + schema) |
| Memory ↔ Statement reverse index | redb `memory_statements` | Derived (maintained by extractors) |
| Memory ↔ Entity mention index | redb `memory_entities` | Derived (maintained by entity resolver) |
| BM25 statement index | tantivy | Derived (rebuildable from statements) |
| BM25 memory text index | tantivy | Derived (rebuildable from memories) |
| Extractor audit log | redb `extractor_audit` | Yes (operational truth) |

"Derived" means: in a disaster, this can be recomputed from the authoritative sources. The WAL covers the authoritative sources. The derived structures are checkpointed separately and rebuilt on demand.
