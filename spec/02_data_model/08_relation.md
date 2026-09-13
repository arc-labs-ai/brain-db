# 02.08 Relation

A **Relation** is a typed edge between two entities, carrying its own properties and validity period. This file collects the relation record shape together with the cardinality-driven supersession rules, the symmetric-relation storage / read semantics, and the evidence model that drives its full lifecycle.

Relations express the graph structure of the data model. They are distinct from the Memory-to-Memory episodic edges (CAUSED, FOLLOWED_BY, etc.) defined in [`./05_edges.md`](./05_edges.md).

| Memory-to-Memory edges ([`05_edges.md`](05_edges.md)) | Entity-to-Entity relations |
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

    // Owner scope — stamped from the caller's authenticated (namespace, agent)
    namespace_id: NamespaceId,       // owning tenant; 0 = reserved `brain` system namespace
    agent_id: AgentId,               // owning agent

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

**Owner scope (`namespace_id` + `agent_id`).** Every relation is owned by exactly one `(namespace, agent)` tenant pair, carried on the relation sidecar and stamped from the caller's authenticated scope at create time (fail-closed by construction). This owner namespace is **distinct** from the qname namespace of `relation_type` — a relation owned by `acme` may use a `brain:`-namespaced type. The reserved `brain` system namespace (id `0`) owns only seeded rows and is never a valid owner of user-written relations.

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

### Relation type origin

Every interned relation type carries a `RelationTypeOrigin` tag:

```rust
enum RelationTypeOrigin {
    SchemaDeclared { version: u32 },           // declared in the schema DSL
    ImplicitFromWrite { first_seen_lsn: u64 }, // interned on first RELATION_CREATE in open-vocabulary mode
}
```

- `SchemaDeclared`: `from` / `to` type constraints, properties, and `cardinality` are enforced on write. Violations produce `CardinalityViolation` (0x0065) or the appropriate type-mismatch error; unknown qnames produce `RelationTypeNotInSchema` (0x004C) in strict mode.
- `ImplicitFromWrite`: the relation type was interned the first time a `RELATION_CREATE` named it in a namespace without an active schema. Implicit types default to `cardinality: many-to-many`, have no `from` / `to` constraints, and never auto-supersede. `CardinalityViolation` is never raised for an implicit type.

When a later `SCHEMA_UPLOAD` declares a relation type whose qname already exists with `ImplicitFromWrite`, the existing `RelationTypeId` is preserved and the origin flips to `SchemaDeclared{version}`. Previously written edges keep their stored ids; the post-upload flagging sweep marks rows whose type is not in the new active schema with `OUTSIDE_ACTIVE_SCHEMA`.

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

Brain is not building a specialized graph traversal engine. For workloads requiring deep, frequent traversals, the user should denormalize (e.g., precompute "manager_chain" with a worker).

## Symmetric relations overview

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

Wire-level opcodes are detailed in [`../04_wire_protocol/`](../04_wire_protocol/00_purpose.md).

## Cardinality detail

How a relation type's `cardinality` declaration drives the auto-supersession behaviour of `RELATION_CREATE`. Mirrors the statement-side treatment of Preference auto-supersession; relations generalise the pattern over four cardinality variants.

Cross-references:
- §"Relation type declaration" + §"Cardinality and supersession" above — variants + intent.
- [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md) §3 — index updates per supersession.
- [`./07_statement.md`](./07_statement.md) §"Statement supersession" — value-side supersession precedent.

### The four variants

```rust
#[repr(u8)]
pub enum Cardinality {
    OneToOne = 0,
    OneToMany = 1,
    ManyToOne = 2,
    ManyToMany = 3,
}
```

Cardinality is declared on the `RelationType`, not on individual relations. All `Relation` rows of a given type share the cardinality of their type.

| Variant | Constraint | Example |
|---|---|---|
| `OneToOne` | At most one current relation of this type touching either `from` OR `to`. | `married_to` (symmetric); `holds_seat_X` (asymmetric, X is unique). |
| `OneToMany` | At most one current relation of this type touching the `to` side. Many `from` allowed. | `employed_by` (a Person can have at most one employer per period, but a company can employ many). |
| `ManyToOne` | At most one current relation of this type touching the `from` side. Many `to` allowed. | `reports_to` (each Person reports to at most one Person; one Person can have many reports). |
| `ManyToMany` | No cardinality constraint. | `discussed_with`, `attended`. |

"Touching" means the entity appears as either `from` or `to`, considering the canonical ordering for symmetric relations (see §"Symmetric storage and indexing" below).

### Auto-supersession rules

`relation_create(wtxn, &Relation, now)` runs this check **before** the insert. The check is read-only first; if it finds a conflicting current relation, it delegates to `relation_supersede` inside the same redb txn.

```text
For new relation N with relation_type T, cardinality C:
    matches = []

    if C in {OneToMany, ManyToMany}:
        // No constraint on the from side.
    if C in {OneToOne, ManyToOne}:
        // Constraint: at most one current relation from N.from.
        matches += relation_lookup_current_from(rtxn, N.from, T)

    if C in {OneToMany, OneToOne}:
        // Constraint: at most one current relation to N.to.
        matches += relation_lookup_current_to(rtxn, N.to, T)

    matches = dedupe(matches)

    if matches.is_empty():
        // No prior current — just insert N.
        insert_new_relation(wtxn, N)
        return Ok(N.id)

    if matches.len() == 1:
        // Auto-supersede the single prior current.
        old = matches[0]
        return relation_supersede(wtxn, old.id, N, now)

    // matches.len() > 1: cardinality is somehow already violated on
    // disk. This is a caller / extractor bug; surface to operator
    // via the same `Conflict` path as a manual supersede.
    return Err(StorageInvariantViolated)
```

Symmetric relations canonicalise `from / to` before the lookup (see §"Symmetric storage and indexing" below); the cardinality check therefore considers the canonical direction only.

### Per-variant write paths

#### `ManyToMany`

Common case. No lookup; new relation inserts cleanly. Two existing `discussed_with(A, B)` relations with different `topic` properties coexist as concurrent current relations.

#### `ManyToOne`

Common case for "Person → Person" hierarchies (`reports_to`, `managed_by`). Auto-supersedes the prior `(from, type)` current relation. Old's `superseded_by` and `valid_to` set per the statement-side supersession contract.

#### `OneToMany`

Symmetric variant of `ManyToOne` (constraint flipped to the `to` side). Less common; example: an `employed_by(Person, Company)` where a Person can be employed by at most one Company at a time.

#### `OneToOne`

Most restrictive. Both directions hold simultaneously. If either `(from, type)` or `(to, type)` is currently held by an existing relation, supersede happens. If **both** are held by different relations (e.g., A married_to B and C married_to D, now asserting A married_to D), the create errors with `INVALID_ARGUMENT` — two-sided supersession is intentionally not auto-resolved (it suggests the schema is mis-modeled or the extractor is confused).

### Cardinality and symmetric relations

When `symmetric = true`, the cardinality applies after canonicalisation. For `OneToOne + symmetric` (the `married_to` shape):

- Canonical relation stores `(canonical_from, canonical_to)` with `from < to`.
- The lookup "is canonical_from already in a current marriage?" queries `RELATIONS_BY_FROM` AND `RELATIONS_BY_TO` at `(canonical_from, type, 1)`.
- Same for `canonical_to`.

If either lookup finds a current relation, that's the one to supersede. The query unions the two indexes per §"Symmetric storage and indexing" below.

### Explicit supersession

`RELATION_SUPERSEDE` (opcode `0x0152`) handles the case where the caller explicitly chains a new relation onto a known prior. Same chain mechanics as the statement-side supersession contract (version, supersedes back-pointer, `valid_to` inheritance). The cardinality check is **not** re-run on explicit supersede — the caller has named the prior id explicitly, so Brain trusts the call.

### Tombstone and cardinality

Tombstoning a relation flips its `is_current` bit to 0. The slot it was "occupying" for cardinality purposes becomes free. A subsequent `RELATION_CREATE` of the same `(from, type)` no longer triggers auto-supersede.

### Cardinality tests

Unit tests cover:

- ManyToMany: two creates → both current.
- ManyToOne: second create auto-supersedes first; chain length 2.
- OneToMany: same but inverted.
- OneToOne: same-side supersede works; two-sided conflict errors.
- Symmetric ManyToMany: canonicalisation kicks in; both index sides see the relation.
- Symmetric OneToOne: marriage scenario.
- Tombstone frees the slot.
- Retombstone is a no-op.

## Symmetric storage and indexing

Storage + read semantics for relations whose `RelationType` declares `symmetric = true`. Symmetric relations express edges where the direction doesn't carry meaning (`discussed_with`, `co_authored`, `married_to`).

Cross-references:
- §"Symmetric relations overview" above.
- [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md) §1 — index layout symmetric relations are projected through.
- §"Cardinality and symmetric relations" above — cardinality interaction with canonicalisation.

### The problem

Without symmetry, "A discussed_with B" and "B discussed_with A" would store two relations for the same conceptual edge. That's:

- Wasted storage.
- A consistency hazard (one tombstoned, the other not).
- Ambiguous for cardinality (`OneToOne` would gate on both rows).

Symmetric relations resolve this by storing **once** in canonical form, and indexing such that reads work regardless of which side the caller queries.

### Canonical form

The canonical direction is `from < to` byte-wise on `EntityId`.

```text
canonical_from = min(caller_from, caller_to)
canonical_to   = max(caller_from, caller_to)
```

`relation_create` for a symmetric relation:

1. Reads `relation_type.is_symmetric`.
2. If symmetric and `caller_from > caller_to`: swap before insert.
3. Persists `(canonical_from, canonical_to)` in the primary row + both directional indexes.

The original caller-supplied direction is lost. Wire responses report the canonical direction (clients aware of symmetry don't care about which side was "from").

### Indexing under symmetry

The relation appears in **both** directional indexes:

```text
RELATIONS_BY_FROM_TABLE.insert(
    (canonical_from_bytes, relation_type_id, is_current),
    relation_id_bytes,
)
RELATIONS_BY_TO_TABLE.insert(
    (canonical_to_bytes, relation_type_id, is_current),
    relation_id_bytes,
)
```

For asymmetric relations, only `RELATIONS_BY_FROM` carries the `from` side and only `RELATIONS_BY_TO` carries the `to` side. For symmetric relations, **both** sides participate in both indexes (the same relation_id is indexed under both endpoints).

This means `relation_list_from(entity, type, current_only)` returns all symmetric relations involving `entity` even when `entity` is the canonical `to`. Same for `list_to`. The dual-index population is what makes "find all `discussed_with` involving Priya" a single lookup regardless of which side Priya was on.

### Reading both sides

For a query that explicitly wants "all symmetric relations of type T involving entity X", the planner unions:

```text
results = []
results += relation_list_from(rtxn, X, T, current_only)
// If T is symmetric, results already contains both directions;
// no second index call needed. The relation is in BOTH
// directional indexes for its canonical endpoints, and X matches
// either canonical_from or canonical_to.
//
// If T is asymmetric, list_from only returns relations where
// X == from. Callers wanting both sides separately call
// list_to as well.

if !relation_type.is_symmetric:
    results += relation_list_to(rtxn, X, T, current_only)
results = dedupe(results)  // for symmetric, dedupe is required
                            // because list_from already returned
                            // the relation; list_to would too.
```

Implementation: `relation_list_from / _to` handle the union + dedup internally when the `RelationType` is loaded and `is_symmetric` is true, so callers don't need to know.

### Cardinality interaction (symmetric)

The cardinality lookups operate on the canonical direction. A `OneToOne + symmetric` relation type means the canonical_from and canonical_to BOTH can only have one current relation of this type.

### Asymmetry verifier

A worker-time invariant verifies:

```text
For each symmetric relation R in RELATIONS_TABLE:
    assert R.from_entity < R.to_entity  // canonical order
    assert R appears in BOTH RELATIONS_BY_FROM and RELATIONS_BY_TO
        at (R.from, R.type, R.is_current) and
        (R.to, R.type, R.is_current).
```

Violations indicate a write-path bug. Brain ships the write path; the sweeper is deferred.

### Wire-layer semantics

`RelationView` reports the **canonical** direction in `from / to` plus a `flags & 1 == 1` bit indicating symmetry. Clients handle this transparently — `RelationHandle::other_side(known_endpoint)` returns the opposite end without the caller caring about canonicalisation.

### Cross-shard considerations (symmetric)

Symmetric relations are sharded by `canonical_from`. The reverse index entry on `RELATIONS_BY_TO` lives on the canonical_to's shard when from / to are on different shards — cross-shard write, same mechanic as statements use for cross-shard subjects. Brain ships same-shard only; cross-shard reverse-index population is deferred.

### Symmetric tests

This section verifies:

- Symmetric create with `caller_from > caller_to` canonicalises internally; row stored with `from < to`.
- Symmetric ManyToMany: query from either side returns the relation; counts dedupe.
- Symmetric OneToOne: canonical-side cardinality enforced consistently.
- Asymmetric relations stored verbatim; per-direction index queries return only matching side.

## Evidence

How a relation references the memories it was derived from. Simpler than statement evidence — flat `Vec<MemoryId>` only, no per-entry metadata, no overflow.

Cross-references:
- §"Schema" above — `evidence: Vec<MemoryId>`.
- [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md) §1.4 — reverse-index table.
- [`./07_statement.md`](./07_statement.md) §"Evidence" — richer statement evidence model that relations may adopt post-v1.0.

### The model

```rust
struct Relation {
    // ...
    pub evidence_inline: Vec<MemoryId>,  // flat list
    // ...
}
```

No per-entry confidence. No per-entry timestamp. No overflow row. A relation cites the memories that support it; relevance + recency are properties of the relation itself, not of individual evidence entries.

The relation's overall `confidence` (top-level f32) reflects the caller's certainty across all evidence, computed externally (extractor combines source confidences).

### Why simpler than statements

Statements carry per-entry metadata because:

- Multiple extractors may contribute independent evidence for the same Fact / Preference / Event over time.
- The noisy-OR aggregation needs per-entry confidences.

Relations are typically single-extraction:

- One extractor sees "Priya manages Bob" in a memory and creates the relation.
- Subsequent extractions trigger supersede (a new version), not evidence accumulation on the existing row.
- Confidence comes from the relation type's extraction precision, not from aggregated votes.

If a relation gains more support over time, the typical pattern is:

- Existing relation continues to be the current version.
- New extractions of the same `(from, type, to)` are dropped at the cardinality / dedup gate (ManyToMany may store duplicates, but the schema designer typically uses a different cardinality for reinforced edges).

If per-entry metadata becomes load-bearing later, the wire shape + storage shape evolve to the statement-style overflow path. Brain ships the flat shape.

### Evidence cap

`evidence_inline` is uncapped at the storage layer but capped at the wire layer:

- `RelationCreateRequest.evidence` is `Vec<[u8; 16]>` with a soft cap of 32 entries. Beyond that, the caller splits the relation creation into multiple supersession steps (each step gaining a few more evidence entries).
- Realistic relation evidence sets are ≤ 5 memories.

The redb row stores all entries verbatim. The reverse index writes one row per evidence entry. If evidence ever needs to scale beyond ~32, an overflow table analogous to `EVIDENCE_OVERFLOW` for statements would be introduced.

### Reverse index population

For every memory in `evidence_inline`, `relation_create` writes:

```text
RELATIONS_BY_EVIDENCE_TABLE.insert((mem_id, relation_id_bytes), ())
```

`relation_supersede` follows the same pattern for the new relation; the old row's reverse-index entries are **preserved** (the old relation still cites those memories).

`relation_tombstone` preserves reverse-index entries as well — audit / FORGET-cascade queries need them.

### FORGET cascade (relations)

When a memory is forgotten (the `FORGET` op runs), the FORGET worker queries `RELATIONS_BY_EVIDENCE_TABLE` at `(mem, *)` and finds every relation citing it.

For each affected relation:

```text
if relation.tombstoned:
    // Already gone; FORGET removes the reverse index entry only.
    delete RELATIONS_BY_EVIDENCE row
    continue

remaining_evidence = relation.evidence_inline.filter(|m| m != forgotten_mem)
if remaining_evidence.is_empty():
    // No evidence left → relation has no support.
    // v1.0: tombstone the relation with reason = SourceMemoryForgotten.
    relation_tombstone(wtxn, relation.id, now)
else:
    // Some evidence remains; rewrite the relation with reduced list.
    relation.evidence_inline = remaining_evidence
    rewrite relation.

delete RELATIONS_BY_EVIDENCE row for (forgotten_mem, relation_id)
```

The cascade runs in a single redb txn per shard. Cross-shard cascade (when the relation lives on shard A and the forgotten memory on shard B) uses the same routing as the Memory-edge cross-shard path; the v1 implementation is same-shard only.

The FORGET cascade worker that calls this path automatically is deferred; Brain ships the entry point but doesn't wire the worker.

### Auto-tombstone discretion

The "no evidence remaining → tombstone" rule is the v1.0 default. Operators wanting a more conservative policy (e.g., preserve the relation as a low-confidence claim) can disable via deployment config — tracked in [`.../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md) Q8.

### Relation evidence tests

This section verifies:

- Reverse index populated on create.
- Reverse index preserved through supersede.
- Reverse index preserved through tombstone.
- FORGET cascade with all-evidence-gone → relation tombstoned.
- FORGET cascade with partial evidence → row rewrites with reduced list, `valid_to_unix_nanos` unchanged.
- Cross-shard cascade: documented as same-shard only in v1.
