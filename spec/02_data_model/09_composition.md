# 02.09 Composition

> **TL;DR.** How Brain's four record types compose: Memories are authoritative; Statements and Relations are derived and cite Memories as evidence; Entities are identity anchors that persist independently of their statements. The graph storage uses a property-graph model (typed nodes and edges with direct property attachment), not RDF triples — every Statement carries 8+ metadata fields, and operational query patterns map naturally onto property graphs.

This file specifies how the four record types interact and why Brain uses a property-graph model over RDF triples for the typed graph.

## The fundamental invariant

> Every Statement and every Relation has provenance traceable to one or more Memories. Memories are always authoritative; derived data (Statements, Relations) can always be recomputed.

This invariant drives the rules below.

## Rule 1: Memories never reference statements

A Memory does not have a field linking to its derived statements. Direction is one-way: Statements point to evidence Memories, not the other way around.

Reason: Memories are authoritative. Statements are derived. Letting Memories reference Statements would couple the authoritative layer to a layer that may be regenerated. The reverse coupling (Statement → Memory) is fine because Statements are disposable.

To query "what statements were derived from this memory?", Brain maintains a `memory_to_statement` reverse index, updated by extractors. The index is a derived structure; it can be rebuilt from the statements.

## Rule 2: Entities can exist without statements

An Entity is created when first mentioned (by an extractor) and persists even if all statements about it are forgotten or invalidated. This is intentional:

- Entities are identity anchors. Removing them every time their last statement is removed creates churn.
- An Entity may be referenced by other Entities through Relations. Dropping Entities would require cascading through Relations.
- The cost of an "orphan" Entity is small: one row in the entity table. The cost of recreating it later (re-resolving aliases, re-generating embedding) is higher.

A background worker (entity garbage collection, optional and configurable) can prune Entities with no recent references after a long grace period (e.g., 90 days). Off by default.

## Rule 3: FORGET on a Memory triggers re-derivation

When a user forgets a Memory:

1. **Soft FORGET (tombstone)**:
   - The Memory is marked tombstoned (see §13 of [`02_memory.md`](02_memory.md)).
   - Statements with this Memory in their `evidence` list are flagged for re-derivation.
   - The re-derivation worker decides: with this evidence removed, does the Statement still hold? If yes (other evidence remains), update confidence. If no, supersede with `superseded_by = null`.

2. **Hard FORGET (purge)**:
   - The Memory's vector and text are zeroed.
   - Statements with this Memory in their `evidence` list have the reference removed.
   - If a Statement loses *all* its evidence, it is supersedes-with-null, and the supersede record's evidence becomes the FORGET audit entry itself (so the chain stays auditable even without the original).

Brain's FORGET handler checks the `memory_to_statement` reverse index and queues re-derivation jobs.

Implementation status: shipped in v1.0 via the `forget_cascade` worker. The handler enqueues `(memory_id, mode)` onto the worker's queue post-commit; the worker walks `STATEMENTS_BY_EVIDENCE_TABLE` at prefix `(memory_id, *)`, decides per-kind whether to re-derive against the surviving evidence or to supersede-with-null, and emits a `StatementsCascadedFromForget` subscription event when the cascade completes. The cascade survives mid-flight crashes: the WAL preserves the original FORGET, and the worker's idempotency check (statements that already lost the evidence ref are skipped on re-run) handles re-enqueue on restart.

## Rule 4: Entities are deduplicated by canonical name + embedding

When an extractor mentions "Priya," the entity resolver checks (in order):

1. Exact match on canonical name or alias in the entity table.
2. Fuzzy match (trigram similarity above threshold) on canonical names.
3. Embedding similarity (the candidate name + context embedded; nearest entity in the entity HNSW above threshold).
4. (Optional, configurable) LLM resolution against top-K candidates.

If no match above threshold: a new Entity is created.

If multiple matches: the extractor reports ambiguity. The Statement is still written, but the subject is a placeholder `EntityId` linked to a pending-resolution audit record. A background worker, possibly with LLM assistance, attempts to resolve. Until resolved, the Statement is queryable but not graph-joined.

## Rule 5: Cross-layer queries are first-class

The query engine supports queries that span all four record types:

- "Get the memories that supported this Fact" → Statement → evidence list → fetch Memories.
- "Find recent memories mentioning Priya" → Entity → mention index → Memories with timestamp filter.
- "Find statements about anyone who reports to Priya" → Entity (Priya) → Relations (reports_to, reverse) → Entities (subordinates) → Statements (subjects in subordinate set).

The planner ([12. Query Optimizer](../12_query_optimizer/00_purpose.md)) plans cross-layer queries by composing per-layer retrievers and joins.

### Rule 5a: Statements carry a four-timestamp record

Every Statement carries four timestamps, splitting object time (when the claim is true in the world) from record time (when Brain believed the claim):

| Field | Meaning |
|---|---|
| `valid_from_unix_nanos` | Object time start |
| `valid_to_unix_nanos` | Object time end |
| `extracted_at_unix_nanos` | Record time start |
| `record_invalidated_at_unix_nanos` | Record time end (Option; set on supersede) |

Object time answers "when was this true?"; record time answers "when did Brain believe this?". The split is what makes `as_of(record_time)` queries possible: "what did Brain believe about Priya's role on March 1st?" returns rows whose record window contains March 1st, even if those rows have since been superseded.

See [`07_statement.md`](07_statement.md) for the storage layout and [`../13_retrievers/05_hybrid_query.md`](../13_retrievers/05_hybrid_query.md) for the filter-chain integration.

## Rule 6: Schemas can evolve; data must be migratable

When a user changes the schema (adds a new entity type, adds a new predicate, modifies an extractor):

- Existing Entities and Statements are not invalidated.
- A `schema_version` field tracks which version of the schema each Statement was extracted under.
- When schemas change, a migration worker can re-run extractors over the relevant Memories. The new Statements get the new `schema_version`. Old Statements remain queryable but flagged as stale.
- Users can choose to keep stale statements (audit value), supersede them with new extractions, or hard-delete them.

This is detailed in [`../03_schema/`](../03_schema/00_purpose.md) and [`../10_metadata/`](../10_metadata/00_purpose.md) §Provenance.

## Rule 7: Brain can run without the typed graph

A user can deploy Brain and never declare a schema. In this case:

- ENCODE, RECALL, and the knowledge opcodes (STATEMENT_CREATE, RELATION_CREATE, QUERY) all accept traffic.
- The Entity, Statement, and Relation tables populate from writes against an open vocabulary — predicates and relation types are interned on first use with origin `ImplicitFromWrite`.
- The lexical (tantivy) index and statement HNSW are populated by the extractors as memories arrive.
- The query router runs in both modes — it is the default RECALL path for every deployment.

This is the open-vocabulary mode. It is a first-class deployment posture, not a degraded one.

When the user declares their first schema, extractors gain a typed vocabulary and may optionally trigger a backfill over existing memories. Declaring a schema activates **strict validation** for statements, relations, and predicate filters within that namespace — unknown qnames produce `PredicateNotInSchema` / `RelationTypeNotInSchema`, and declared cardinalities are enforced. It does NOT activate hybrid retrieval; hybrid (semantic + lexical + memory-edge graph) is already the default. What schema adds is typed entity-anchored graph traversal and predicate-vocabulary checking.

## What gets stored where: summary

| Data | Storage | Authoritative? |
|---|---|---|
| Raw memory text and vector | Arena + WAL | Yes |
| Memory metadata (timestamps, agent, kind) | redb `memory_meta` | Yes |
| Memory-to-memory edges | redb `edges` | Yes |
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

## Property graph vs RDF triples

Brain's typed graph uses a **property-graph** model. Brain does not use RDF triples as the storage or query primitive.

### RDF in brief

RDF (Resource Description Framework) represents knowledge as triples: `(subject, predicate, object)`. To attach metadata to a triple — say, confidence, source, time — RDF uses *reification*: turning a triple into multiple triples about the original triple.

```
Original:
  (priya, prefers, async_meetings)

Reified to attach confidence=0.87:
  (s1, rdf:type, rdf:Statement)
  (s1, rdf:subject, priya)
  (s1, rdf:predicate, prefers)
  (s1, rdf:object, async_meetings)
  (s1, brain:confidence, 0.87)
```

One conceptual fact becomes five triples. Query patterns require joining the reified statement to its components. This compounds with every metadata field.

### Property graph in brief

A property graph represents knowledge as nodes and edges, both of which carry typed key-value properties directly.

```
Node: priya (id=entity_42, type=Person, name="Priya Patel", ...)
Node: async_meetings (id=entity_99, type=MeetingStyle, ...)
Edge: (entity_42) -[prefers {confidence: 0.87, evidence: [m1, m2],
                              valid_from: 2025-03-01}]-> (entity_99)
```

One conceptual fact is one edge with properties. Queries access properties directly: `MATCH (p)-[r:prefers]->(o) WHERE r.confidence > 0.7`.

### Why property graph wins for Brain's use case

**1. Every statement has metadata.** In the typed graph, every Fact/Preference/Event carries:

- `confidence` (float)
- `evidence` (list of MemoryIds)
- `extractor_id` (the extractor that produced it)
- `extracted_at` (timestamp)
- `valid_from`, `valid_to` (validity period)
- `version`, `superseded_by` (for revisable statements)

Eight properties per statement. In RDF, that's nine triples to express one statement. In property graph, it's one edge with eight properties. Storage and query cost differ by ~9x.

**2. Query patterns are direct.** A typical Brain query asks "what's Priya's *current* preference about meetings, with confidence ≥ 0.7?"

Property graph:

```cypher
MATCH (p:Person {name: "Priya"})-[r:prefers]->(o)
WHERE r.confidence >= 0.7
  AND r.valid_to IS NULL
  AND r.superseded_by IS NULL
RETURN o
```

RDF (SPARQL with reification):

```sparql
SELECT ?o WHERE {
  ?s rdf:subject ?priya .
  ?s rdf:predicate <prefers> .
  ?s rdf:object ?o .
  ?s brain:confidence ?c .
  ?s brain:validTo ?vt .
  ?s brain:supersededBy ?sb .
  FILTER (?c >= 0.7)
  FILTER (!BOUND(?vt))
  FILTER (!BOUND(?sb))
  ?priya foaf:name "Priya" .
}
```

The SPARQL is doable but burns syntax on metadata management instead of expressing intent.

**3. Operational systems converged here.** Neo4j, Memgraph, JanusGraph, Amazon Neptune (in property-graph mode), TigerGraph, and most modern operational KG systems use property graphs. Stardog, GraphDB, AllegroGraph remain in the RDF/SPARQL space but their share of new builds has been shrinking since ~2020.

The pattern is clear: **operational knowledge graphs use property graphs; semantic-web / ontology workloads use RDF**. Brain is operational.

**4. Brain does not need the W3C stack.** RDF's strengths are interoperability (federated SPARQL queries across datasets), ontologies (OWL inference), and standardization. None of these matter for a single-node memory database. Brain would pay the storage and query cost without using the upside.

### What Brain keeps from RDF

Two things are worth borrowing:

1. **The triple as an API abstraction.** When a user writes a Fact, they think `(Priya, role, "engineering manager")`. A client constructs this shape. Internally it becomes a property-graph edge or node-with-property.

2. **URI-style identifiers for predicates.** Brain uses namespaced predicate strings like `brain:prefers` or `crm:reports_to` to avoid collisions. This is RDF's convention and it's good.

Brain does *not* keep:

- Reification.
- SPARQL.
- OWL or any inferencing.
- RDF datasets, named graphs, blank nodes.

### Storage implications

A Statement is one row in a redb table, with the object stored as a tagged union:

```rust
struct StatementRow {
    id: StatementId,
    kind: StatementKind,        // Fact | Preference | Event
    subject: EntityId,
    predicate: PredicateId,     // interned string
    object: StatementObject,    // tagged union, rkyv-serialized
    confidence: f32,
    evidence: Vec<MemoryId>,    // rkyv-serialized list
    extractor_id: ExtractorId,
    extracted_at: u64,          // unix micros
    valid_from: Option<u64>,
    valid_to: Option<u64>,
    version: u32,
    superseded_by: Option<StatementId>,
    // ... kind-specific fields packed at end
}
```

A Relation is one row in a separate redb table, similarly structured but with two entity references (`from`, `to`) instead of one subject.

This is fewer rows than reified RDF would require, with native indexes on `subject`, `predicate`, `confidence`, `valid_to`, etc. Queries hit indexes directly.

### What this constrains

The property-graph choice constrains a few things to accept:

1. **No federated queries.** If someone wanted to run a SPARQL endpoint over Brain, they'd need a translation layer. Not in scope.

2. **No ontological inference.** If a Fact says "Priya is a Person" and another says "all Persons have an Email," Brain does not automatically derive "Priya has an Email." Inference, if needed, is explicit (a user-declared extractor that produces the derived statement).

3. **Predicate vocabulary is not globally meaningful.** A `brain:prefers` predicate in one Brain deployment has no defined relationship to a `crm:prefers` predicate in another. Users define their own vocabularies. If they want shared meaning, that's a convention they establish.

These constraints are acceptable for an operational, single-node, memory database. Users who need the missing things would not have chosen Brain anyway.

