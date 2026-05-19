# 28.07 Relation Wire Frames

Request/response body schemas for opcodes `0x0150–0x0156` (relation operations). Relations are typed edges between entities — distinct from substrate memory-to-memory edges; see [`../20_relations/00_purpose.md`](../20_relations/00_purpose.md).

Cross-references:
- [`../20_relations/00_purpose.md`](../20_relations/00_purpose.md) — `Relation` schema, type system, cardinality.
- [`../21_schema_dsl/`](../21_schema_dsl/) — relation type declarations.
- [`./03_errors.md`](./03_errors.md), [`./04_validation.md`](./04_validation.md).

## 1. Opcode index

| Opcode | Name | Section | Status |
|---|---|---|---|
| `0x0150` | `RELATION_CREATE` | §3 | spec-only — phase 18 |
| `0x0151` | `RELATION_GET` | §4 | spec-only — phase 18 |
| `0x0152` | `RELATION_SUPERSEDE` | §5 | spec-only — phase 18 |
| `0x0153` | `RELATION_TOMBSTONE` | §6 | spec-only — phase 18 |
| `0x0154` | `RELATION_LIST_FROM` | §7 | spec-only — phase 18 |
| `0x0155` | `RELATION_LIST_TO` | §8 | spec-only — phase 18 |
| `0x0156` | `RELATION_TRAVERSE` | §9 | spec-only — phase 18 |

Responses live at `0x01D0–0x01D6`.

## 2. Shared types

### 2.1 `RelationPropertiesBlob`

```rust
pub type RelationPropertiesBlob = Vec<u8>;
```

Opaque rkyv-encoded `BTreeMap<String, StatementValueWire>`. Schema enforces the property names and types per relation type — same approach as `EntityAttributes` ([§01](./01_entity_frames.md) §1).

### 2.2 `RelationView` — read-side projection

```rust
pub struct RelationView {
    pub relation_id: WireUuid,
    pub relation_type: String,         // canonical "namespace:name"
    pub from_entity: WireUuid,
    pub to_entity: WireUuid,
    pub properties_blob: RelationPropertiesBlob,
    pub evidence: EvidenceRefWire,     // shared with statements (§06 §2.3)
    pub extractor_id: u32,
    pub extracted_at_unix_nanos: u64,
    pub confidence: f32,
    pub valid_from_unix_nanos: u64,    // 0 = None
    pub valid_to_unix_nanos: u64,      // 0 = None
    pub version: u32,
    pub superseded_by: WireUuid,
    pub tombstoned: bool,
    pub tombstoned_at_unix_nanos: u64,
    pub flags: u32,
}
```

## 3. RELATION_CREATE (0x0150)

### 3.1 Request — `RelationCreateRequest`

```rust
pub struct RelationCreateRequest {
    pub relation_type: String,         // "namespace:name"; open-vocabulary in schemaless mode
    pub from_entity: WireUuid,
    pub to_entity: WireUuid,
    pub properties_blob: RelationPropertiesBlob,
    pub evidence: EvidenceRefWire,
    pub extractor_id: u32,             // 0 = user-authored
    pub confidence: f32,
    pub valid_from_unix_nanos: u64,    // 0 = use extracted_at
    pub valid_to_unix_nanos: u64,      // 0 = open-ended
    pub request_id: WireUuid,
}
```

Semantics:

1. Resolve `relation_type` (a `"namespace:name"` qname). If the namespace has no active schema, intern the qname with `RelationTypeOrigin::ImplicitFromWrite` on first use; implicit types default to `cardinality: many_to_many` and carry no `from_type` / `to_type` constraint. If a schema is active for the namespace, the qname must be declared — unknown qnames produce `RelationTypeNotInSchema` (0x004C).
2. Validate `from_entity` / `to_entity` exist. → `ENTITY_NOT_FOUND`.
3. Validate **type-signature** (schema-declared types only): `from_entity.entity_type` and `to_entity.entity_type` match the relation's declared `from_type` / `to_type`. → `ENTITY_TYPE_MISMATCH`. Implicit types skip this check.
4. Validate **cardinality** ([`../20_relations/00_purpose.md`](../20_relations/00_purpose.md) §"Cardinality"). Only schema-declared types carry an enforceable cardinality contract. For declared `one_to_one` / `one_to_many` / `many_to_one`, the server checks existing edges before inserting; violation → `CardinalityViolation` (0x0065). Implicit types are always `many_to_many` and never trigger this error.
5. Allocate `RelationId` (UUIDv7).
6. Write to `relations` + `relations_by_from` + `relations_by_to` indexes inside one redb transaction.
7. Emit `RELATION_CREATED` event.

### 3.2 Response — `RelationCreateResponse`

```rust
pub struct RelationCreateResponse {
    pub relation_id: WireUuid,
}
```

### 3.3 Errors

- `RelationTypeNotInSchema` (`0x004C`) — strict mode only; relation type qname is not declared in the active schema for the namespace.
- `ENTITY_NOT_FOUND`, `ENTITY_TYPE_MISMATCH`.
- `CardinalityViolation` (`0x0065`) — write would violate the declared cardinality of a schema-declared relation type.
- `INVALID_ARGUMENT` — malformed `relation_type` qname, malformed `properties_blob`, confidence out of `[0, 1]`.

## 4. RELATION_GET (0x0151)

```rust
pub struct RelationGetRequest {
    pub relation_id: WireUuid,
    pub follow_supersession: bool,
}

pub struct RelationGetResponse {
    pub relation: RelationView,
    pub returned_via_supersession: bool,
}
```

Errors: substrate `NotFound` (no §28-specific "relation_not_found" code in v1.0 — re-uses `MemoryNotFound` per [`./03_errors.md`](./03_errors.md) Strategy B until that strategy lands).

## 5. RELATION_SUPERSEDE (0x0152)

```rust
pub struct RelationSupersedeRequest {
    pub old_relation_id: WireUuid,
    pub new_relation: RelationCreateRequest,   // embedded
    pub request_id: WireUuid,
}

pub struct RelationSupersedeResponse {
    pub new_relation_id: WireUuid,
    pub version: u32,
}
```

Atomic: create-new + link in one redb txn. Old relation's `valid_to` set to new's `extracted_at`.

## 6. RELATION_TOMBSTONE (0x0153)

```rust
pub struct RelationTombstoneRequest {
    pub relation_id: WireUuid,
    pub reason: String,
    pub request_id: WireUuid,
}

pub struct RelationTombstoneResponse {
    pub tombstoned_at_unix_nanos: u64,
}
```

Soft delete; behaves like statement tombstone.

## 7. RELATION_LIST_FROM (0x0154)

### 7.1 Request

```rust
pub struct RelationListFromRequest {
    pub from_entity: WireUuid,
    pub relation_type_filter: String,  // "" = all
    pub time_range_start_unix_nanos: u64,
    pub time_range_end_unix_nanos: u64,
    pub include_superseded: bool,
    pub include_tombstoned: bool,
    pub limit: u32,                    // 1..=1000
    pub cursor: Vec<u8>,
}
```

### 7.2 Response — streaming `RelationView`

One `RelationView` per match; tail with `next_cursor` and `total_returned`.

### 7.3 Errors

- `ENTITY_NOT_FOUND` — `from_entity` doesn't exist.
- `INVALID_ARGUMENT` — limit / cursor.

## 8. RELATION_LIST_TO (0x0155)

Identical shape to §7 but filters on `to_entity` via `relations_by_to` index.

## 9. RELATION_TRAVERSE (0x0156)

Graph walk from a starting entity over selected relation types.

### 9.1 Request

```rust
pub struct RelationTraverseRequest {
    pub start_entity: WireUuid,
    pub relation_types: Vec<String>,   // empty = all declared types
    pub direction: u8,                 // 0=out (LIST_FROM), 1=in (LIST_TO), 2=both
    pub max_depth: u32,                // 1..=8
    pub max_nodes: u32,                // 1..=1000 (caps output size)
    pub time_at_unix_nanos: u64,       // 0 = now; otherwise as-of view
    pub include_superseded: bool,
    pub request_id: WireUuid,
}
```

### 9.2 Response — streaming per-frame `RelationTraverseFrame`

```rust
pub struct RelationTraverseFrame {
    pub entity: EntityView,            // node visited
    pub depth: u32,                    // 0 = start, 1 = direct neighbour, ...
    pub via_relation: RelationView,    // [0;16]-id when entity is the start node
}

pub struct RelationTraverseTail {
    pub nodes_visited: u32,
    pub edges_visited: u32,
    pub truncated: bool,               // true if max_depth or max_nodes capped output
}
```

Traversal order is breadth-first; the server emits one `RelationTraverseFrame` per node visit. Cross-shard traversals fan out at shard boundaries; ordering across shards is **not** guaranteed (the per-shard breadth-first order is preserved, but interleaving is arbitrary).

### 9.3 Errors

- `ENTITY_NOT_FOUND` — `start_entity` missing.
- `INVALID_ARGUMENT` — `max_depth` > 8, `max_nodes` > 1000, unknown relation type in `relation_types`.
- `QUERY_TIMEOUT` — wall-time budget exceeded mid-traversal (substrate `Unavailable`).

## 10. Cardinality enforcement on the wire

Relation type declarations carry cardinality rules:

| Cardinality | Server check on CREATE | On SUPERSEDE |
|---|---|---|
| `one_to_one` | reject if either endpoint already has an active relation of this type | reject if new endpoints already have one |
| `one_to_many` (default) | reject if `from_entity` already has an active relation of this type | reject if new `from_entity` already has one |
| `many_to_one` | reject if `to_entity` already has an active relation of this type | symmetric |
| `many_to_many` | no check | no check |

Errors → `CardinalityViolation` (`0x0065`). `ErrorDetails.expected` carries the declared cardinality string; `ErrorDetails.actual` is empty. Cardinality is enforced only on schema-declared relation types; implicit (open-vocabulary) types are always `many_to_many` and never trigger this error.
