# Relations

## Purpose

A **Relation** is a typed edge between two entities, carrying its own properties and validity period.

Relations express the graph structure of the knowledge layer. They are distinct from the Memory-to-Memory episodic edges (CAUSED, FOLLOWED_BY, etc.) defined in section 02.

| Memory-to-Memory edges (section 02) | Entity-to-Entity relations |
|---|---|
| Connect Memory ↔ Memory | Connect Entity ↔ Entity |
| Fixed 8 kinds (CAUSED, etc.) | User-declared kinds |
| Salience-relevant | Time-relevant |
| Used for episodic chaining | Used for graph traversal |

Both coexist. They are independent.

## Schema

```rust
struct Relation {
    id: RelationId,                  // UUIDv7
    relation_type: RelationTypeId,   // user-declared type
    from_entity: EntityId,
    to_entity: EntityId,
    
    // Properties: typed key-value, schema-enforced per relation_type
    properties: BTreeMap<String, Value>,
    
    // Provenance
    evidence: Vec<MemoryId>,
    extractor_id: ExtractorId,
    extracted_at: u64,
    confidence: f32,
    
    // Validity
    valid_from: Option<u64>,
    valid_to: Option<u64>,
    
    // Versioning
    version: u32,
    superseded_by: Option<RelationId>,
    
    // Status
    tombstoned: bool,
    tombstoned_at: Option<u64>,
}
```

## Relation type declaration

```
define relation_type reports_to {
    from: Person
    to: Person
    properties {
        team:          text optional
        started_on:    date optional
    }
    cardinality: many-to-one         // each Person reports to at most one Person
    symmetric: false
}

define relation_type discussed_with {
    from: Person
    to: Person
    properties {
        topic:         text optional
        outcome:       enum[positive, neutral, negative] optional
    }
    cardinality: many-to-many
    symmetric: true                  // A discussed_with B ⇔ B discussed_with A
}

define relation_type owns {
    from: Person
    to: Project
    properties {}
    cardinality: many-to-many
    symmetric: false
}
```

Properties:
- **`from`, `to`**: entity type constraints. Enforced on write.
- **`properties`**: typed schema, like entity attributes.
- **`cardinality`**: `one-to-one`, `one-to-many`, `many-to-one`, `many-to-many`. Affects supersession rules (see below).
- **`symmetric`**: if true, queries for either direction return the relation. Stored once but indexed both ways.

## Cardinality and supersession

For `many-to-one` (each Person reports to at most one Person):

When a new `reports_to` is asserted for Priya:
- If Priya has no current `reports_to`: just create.
- If Priya has a current `reports_to` to someone else: supersede the old one. New one's `supersedes` points to old; old's `superseded_by` and `valid_to` are set.

For `many-to-many`: no automatic supersession. Multiple concurrent `discussed_with` relations between the same pair are valid (and may have different `topic` properties).

For `one-to-one`: most restrictive; supersession runs both ways.

## Indexes

| Index | Key | Purpose |
|---|---|---|
| `relations_by_id` | RelationId | Primary lookup. |
| `relations_by_from` | (from_entity, relation_type, superseded_by_is_null) | Outgoing edges from entity. |
| `relations_by_to` | (to_entity, relation_type, superseded_by_is_null) | Incoming edges to entity. |
| `relations_by_type` | (relation_type, valid_to_is_null) | All current relations of a type. |
| `relations_by_evidence_memory` | (memory_id, relation_id) | Reverse: which relations depend on this memory? |

For symmetric relations: stored once (deterministic from/to ordering by EntityId byte-order), but the from/to indexes treat the relation as present in both directions.

## Graph queries

Common patterns the planner supports:

```
# Outgoing: "Who does Priya manage?"
relations WHERE from = priya AND type = manages
  → set of to-entities

# Incoming: "Who manages Priya?"
relations WHERE to = priya AND type = manages
  → set of from-entities

# Hop-2: "Who manages anyone Priya works with?"
hop1 = relations WHERE from = priya AND type = works_with
        → coworkers
hop2 = relations WHERE to ∈ coworkers AND type = manages
        → managers of coworkers

# Cycle-safe traversal: "Anyone in Priya's reporting chain (up or down) up to 3 hops"
BFS from priya through reports_to, depth ≤ 3, cycle detection via visited set.
```

The planner enforces a depth bound (default 3) on traversals to prevent runaway queries. Configurable per-query, capped server-side.

## Hop performance

For 1-2 hop queries with indexed `from`/`to`, performance is comparable to SQL joins (O(log N) per hop seek + adjacency scan). For deeper hops, performance degrades; users should structure queries to bound branching factor.

We are not building a specialized graph traversal engine for the knowledge layer. For workloads requiring deep, frequent traversals, the user should denormalize (e.g., precompute "manager_chain" with a worker).

## Symmetric relations

Stored once. The chosen direction is `from < to` byte-wise on EntityId.

Reads from either direction work because both `relations_by_from` and `relations_by_to` index the relation. For a query "find all `discussed_with` involving Priya," the planner unions:
- `relations_by_from` WHERE from = priya AND type = discussed_with
- `relations_by_to` WHERE to = priya AND type = discussed_with

(After deduplication.)

## Relation vs Statement: when to use which

This is the most common modeling question for users. Guidance:

| Use Relation when... | Use Statement when... |
|---|---|
| Connecting two entities | One entity has a property/claim |
| Graph traversal queries are common | Filter-by-subject queries are common |
| Cardinality matters | Cardinality doesn't matter |
| Bidirectional reads are useful | Unidirectional reads suffice |
| The edge has its own type identity | The edge is just a typed predicate |

Concrete examples:

- "(Priya, manages, Bob)" → **Relation** (manages, from=priya, to=bob). Bidirectional, cardinality important.
- "(Priya, has_email, priya@example.com)" → **Statement (Fact)**. Email is not an entity; cardinality is uninteresting.
- "(Priya, prefers, async_meetings)" → **Statement (Preference)**. Object is a concept, not a tracked entity.
- "(Priya, member_of, EngineeringTeam)" → **Relation** if EngineeringTeam is tracked as an entity; else **Fact**.

The schema designer makes the call when declaring the schema. Wrong calls are not catastrophic — both are queryable, just with different ergonomics.

## Operations

- `CREATE_RELATION(from, to, type, properties)` — write.
- `LIST_RELATIONS_FROM(entity, type?, valid_at?)` — outgoing.
- `LIST_RELATIONS_TO(entity, type?, valid_at?)` — incoming.
- `TRAVERSE(start, types, depth, direction)` — BFS/DFS with constraints.
- `SUPERSEDE_RELATION(old, new)` — version chain.
- `TOMBSTONE_RELATION(id)` — soft delete.

All wire-level opcodes detailed in `28_knowledge_wire_protocol/`.
