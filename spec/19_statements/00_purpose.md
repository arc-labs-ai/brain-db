# Statements

## Purpose

A **Statement** is a typed claim about an entity. It is the unit of structured knowledge derived from memories.

the knowledge layer has three statement kinds:

- **Fact** — stable claim. "(Priya, role, engineering_manager)"
- **Preference** — revisable belief. "(Priya, prefers, async_meetings)"
- **Event** — moment-in-time occurrence. "(Priya, scheduled, planning_session, event_at=...)"

All three share storage; the kind affects mutation rules and default query behavior.

## Schema (unified row)

```rust
struct Statement {
    id: StatementId,                 // UUIDv7
    kind: StatementKind,             // Fact | Preference | Event
    
    // Subject + predicate are required for all kinds
    subject: SubjectRef,             // EntityId or Pending(AuditId)
    predicate: PredicateId,          // interned namespaced string
    
    // Object: tagged union
    object: StatementObject,
    
    // Provenance and confidence (all kinds)
    confidence: f32,                 // [0, 1]
    evidence: EvidenceRef,           // inline (Vec<MemoryId>) or overflow pointer
    extractor_id: ExtractorId,
    extracted_at: u64,               // unix micros
    schema_version: u32,             // version of schema under which this was extracted
    
    // Time fields (kind-dependent)
    valid_from: Option<u64>,         // Fact, Preference
    valid_to: Option<u64>,           // Fact, Preference (None = still valid)
    event_at: Option<u64>,           // Event (required for Event kind)
    
    // Versioning
    version: u32,                    // increments on supersession
    superseded_by: Option<StatementId>,
    supersedes: Option<StatementId>, // back-pointer; for chain traversal
    
    // Status flags
    tombstoned: bool,                // soft-delete
    tombstoned_at: Option<u64>,
    tombstone_reason: Option<TombstoneReason>,
}

enum StatementObject {
    Entity(EntityId),
    Value(rkyv_Value),               // typed literal
    Memory(MemoryId),
    Statement(StatementId),          // meta-statement
}

enum SubjectRef {
    Entity(EntityId),
    Pending(AuditId),                // resolution pending
}

enum EvidenceRef {
    Inline(SmallVec<MemoryId, 8>),   // up to 8 inline
    Overflow(EvidenceOverflowId),    // points to evidence_overflow table
}

enum TombstoneReason {
    SourceMemoryForgotten,
    UserRequest,
    SchemaInvalidation,
    ExtractorRetraction,
}
```

## Storage characteristics

The unified row averages ~256 bytes:
- Fixed fields: ~160 bytes
- Object (tagged union): typically 16-64 bytes
- Inline evidence: 0-128 bytes

For 10M statements: ~2.5 GB. Acceptable.

For deployments with very large evidence lists, the overflow table absorbs the size.

## Kind-specific contracts

### Fact

- **Object**: typically `Entity` (predicate=role, predicate=manages) or `Value` (predicate=email, predicate=birth_year).
- **`valid_from`**: optional; if not provided, defaults to `extracted_at`.
- **`valid_to`**: optional; remains `None` until contradicted or explicitly retracted.
- **`event_at`**: must be `None`.
- **Contradiction handling**: another Fact with same `(subject, predicate)` but different `object`, where the timestamps suggest concurrency (overlapping valid intervals), is a *contradiction*. Both are stored. Queries by default return the higher-confidence one and indicate the contradiction.
- **Supersession**: rare; only when an explicit "this was wrong" Fact retracts.

### Preference

- **Object**: typically `Value` (predicate=prefers with object="async_meetings") or `Entity`.
- **`valid_from`**: defaults to `extracted_at`.
- **`valid_to`**: defaults to `superseded_by.extracted_at` when superseded.
- **`event_at`**: must be `None`.
- **Supersession**: new Preference with same `(subject, predicate)` *supersedes* the previous one. The previous one's `superseded_by` is set; new one's `supersedes` points back. Chain is queryable.
- **Default query**: only return current (non-superseded) Preferences.

### Event

- **Object**: typically `Entity` (predicate=scheduled, predicate=met_with) or `Value` (predicate=said, with the said-thing as value).
- **`event_at`**: required.
- **`valid_from`, `valid_to`**: must be `None` (Events are point-in-time, not validity ranges).
- **Supersession**: not allowed. Corrections are new Events with provenance noting the correction.
- **Default query**: events in a time range, sorted by `event_at`.

## Predicate vocabulary

Predicates are namespaced strings interned into a `predicates` table:

```rust
struct Predicate {
    id: PredicateId,
    namespace: String,               // e.g. "brain", "user", "crm"
    name: String,                    // e.g. "prefers", "role", "scheduled"
    kind_constraint: Option<StatementKind>,  // if set, only valid for this kind
    object_type_constraint: Option<ObjectTypeConstraint>,
    schema_version: u32,
    description: String,             // for documentation
}
```

Namespacing prevents collisions across schemas. Two deployments using `prefers` for different semantics can be disambiguated by namespace (`brain:prefers` vs `crm:prefers`).

**Built-in predicates** (namespace `brain:`):
- `is_a` — entity type assertion (e.g., Fact: (Priya, is_a, Person))
- `has_name` — entity naming
- `mentions` — generic Fact
- `related_to` — generic Relation

**User-declared predicates**: declared in the schema DSL. See `21_schema_dsl/`.

**Object type constraint** lets the schema enforce that, e.g., `manages` always has an `Entity<Person>` as object:

```
define predicate manages {
    kind: Fact
    object: Entity<Person>
}
```

## Indexes

| Index | Key | Purpose |
|---|---|---|
| `statements_by_id` | StatementId | Primary lookup. |
| `statements_by_subject` | (subject, kind, predicate, superseded_by_is_null, valid_to_is_null) | All statements about an entity, filtered by kind/predicate/currency. |
| `statements_by_predicate` | (predicate, kind, confidence) | All statements with a given predicate, ranked by confidence. |
| `statements_by_object` | (object, kind) | Reverse: who has this entity as object? (for graph queries). |
| `statements_by_event_time` | (event_at, subject) | Events in time range. |
| `statements_by_evidence_memory` | (memory_id, statement_id) | Reverse: which statements depend on this memory? |
| `statements_supersession_chain` | (chain_root, version) | Walk the supersession chain. |

`chain_root` is the StatementId of the original (un-superseded) statement in a chain. All statements in the chain share this. Computed at supersession time.

## Operations

### CREATE_STATEMENT (write)

```rust
fn create_statement(req: CreateStatementRequest) -> Result<StatementId>
```

1. Validate against predicate definition (kind, object type).
2. Validate subject EntityId exists.
3. For Preferences: check if a Preference with the same `(subject, predicate)` exists and is current. If yes, supersede it.
4. Generate StatementId.
5. Write to `statements`.
6. Update indexes.
7. Update `statements_by_evidence_memory` for each evidence MemoryId.
8. If `kind = Event` and `predicate` matches a Relation pattern, optionally also create a Relation (configurable per-predicate).
9. Index in tantivy BM25.
10. Commit.

### SUPERSEDE_STATEMENT

```rust
fn supersede(old: StatementId, new: StatementId) -> Result<()>
```

1. Set `old.superseded_by = Some(new)`.
2. Set `old.valid_to = new.extracted_at` (if old has valid_to fields).
3. Set `new.supersedes = Some(old)`.
4. Set `new.version = old.version + 1`.
5. Compute `chain_root`: if `old.supersedes.is_none()`, root is `old.id`; else inherit from `old`.
6. Update indexes.

### TOMBSTONE_STATEMENT

```rust
fn tombstone(id: StatementId, reason: TombstoneReason) -> Result<()>
```

Soft delete. Statement is removed from default queries but visible to provenance / audit queries. Grace period before hard delete: 30 days default.

### RETRACT_STATEMENT (hard)

```rust
fn retract(id: StatementId) -> Result<()>
```

Hard delete: tombstone + zero out fields after grace period. Used when statement was created in error or for privacy reasons.

## Querying current state

A query like "what's Priya's current role?" needs to return only the non-superseded, non-tombstoned, currently-valid statement:

```sql
-- Conceptual; actual query is via planner
SELECT * FROM statements
WHERE subject = entity_42
  AND predicate = role
  AND kind = Fact
  AND superseded_by IS NULL
  AND tombstoned = false
  AND (valid_from IS NULL OR valid_from <= now())
  AND (valid_to IS NULL OR valid_to > now())
ORDER BY confidence DESC
LIMIT 1
```

The `statements_by_subject` compound index makes this efficient.

## Querying history

"What did we believe about Priya's role over time?":

```
chain_root = statement.chain_root or statement.id
SELECT * FROM statements
WHERE chain_root = ...
  AND subject = entity_42
  AND predicate = role
ORDER BY version ASC
```

Returns the full chain: the chain root, the first supersession, then the current statement.
