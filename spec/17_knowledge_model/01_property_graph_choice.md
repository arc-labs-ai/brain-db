# Property Graph vs RDF Triples

## The choice

the knowledge layer uses a **property-graph** model for its knowledge layer. We do not use RDF triples as the storage or query primitive.

## RDF in brief

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

## Property graph in brief

A property graph represents knowledge as nodes and edges, both of which carry typed key-value properties directly.

```
Node: priya (id=entity_42, type=Person, name="Priya Patel", ...)
Node: async_meetings (id=entity_99, type=MeetingStyle, ...)
Edge: (entity_42) -[prefers {confidence: 0.87, evidence: [m1, m2], 
                              valid_from: 2025-03-01}]-> (entity_99)
```

One conceptual fact is one edge with properties. Queries access properties directly: `MATCH (p)-[r:prefers]->(o) WHERE r.confidence > 0.7`.

## Why property graph wins for our use case

### 1. Every statement has metadata

In the knowledge layer, every Fact/Preference/Event carries:
- `confidence` (float)
- `evidence` (list of MemoryIds)
- `extractor_id` (the extractor that produced it)
- `extracted_at` (timestamp)
- `valid_from`, `valid_to` (validity period)
- `version`, `superseded_by` (for revisable statements)

Eight properties per statement. In RDF, that's nine triples to express one statement. In property graph, it's one edge with eight properties. Storage and query cost differ by ~9x.

### 2. Query patterns are direct

A typical Brain query asks "what's Priya's *current* preference about meetings, with confidence ≥ 0.7?"

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

### 3. Operational systems converged here

Neo4j, Memgraph, JanusGraph, Amazon Neptune (in property-graph mode), TigerGraph, and most modern operational KG systems use property graphs. Stardog, GraphDB, AllegroGraph remain in the RDF/SPARQL space but their share of new builds has been shrinking since ~2020.

The pattern is clear: **operational knowledge graphs use property graphs; semantic-web / ontology workloads use RDF**. We are operational.

### 4. We don't need the W3C stack

RDF's strengths are interoperability (federated SPARQL queries across datasets), ontologies (OWL inference), and standardization. None of these matter for a single-node cognitive substrate. We pay the storage and query cost without using the upside.

## What we keep from RDF

Two things are worth borrowing:

1. **The triple as an API abstraction.** When a user writes a Fact, they think `(Priya, role, "engineering manager")`. The SDK accepts this shape. Internally it becomes a property-graph edge or node-with-property.

2. **URI-style identifiers for predicates.** We use namespaced predicate strings like `brain:prefers` or `crm:reports_to` to avoid collisions. This is RDF's convention and it's good.

We do *not* keep:
- Reification.
- SPARQL.
- OWL or any inferencing.
- RDF datasets, named graphs, blank nodes.

## Storage implications

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

## What this constrains

The property-graph choice constrains a few things we should accept:

1. **No federated queries.** If someone wanted to run a SPARQL endpoint over Brain, they'd need a translation layer. Not in the knowledge layer.

2. **No ontological inference.** If a Fact says "Priya is a Person" and another says "all Persons have an Email," we do not automatically derive "Priya has an Email." Inference, if needed, is explicit (a user-declared extractor that produces the derived statement).

3. **Predicate vocabulary is not globally meaningful.** A `brain:prefers` predicate in one Brain deployment has no defined relationship to a `crm:prefers` predicate in another. Users define their own vocabularies. If they want shared meaning, that's a convention they establish.

These constraints are acceptable for an operational, single-node, cognitive substrate. Users who need the missing things would not have chosen Brain anyway.
