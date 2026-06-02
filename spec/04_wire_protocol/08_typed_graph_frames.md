# 04.08 Typed-Graph Noun Frames

Request/response body schemas for the typed-graph "noun" opcodes — the entity (`0x0130–0x013F`), statement (`0x0140–0x014F`), and relation (`0x0150–0x015F`) ranges. The opcodes in these ranges create, read, mutate, and expire the typed records that ride on top of Brain's substrate memory model.

Brain's wire protocol — 32-byte header, opcode framing, CRC32C, payload encoding — is covered in [`./02_wire_format.md`](./02_wire_format.md), [`./03_opcodes.md`](./03_opcodes.md), and [`./05_frame_layouts.md`](./05_frame_layouts.md). This file specifies only the CBOR field schemas of the request/response payloads for the noun opcodes.

Cross-references:
- [`../02_data_model/06_entity_lifecycle.md`](../02_data_model/06_entity_lifecycle.md) — entity record semantics.
- [`../02_data_model/07_statement.md`](../02_data_model/07_statement.md) — statement record semantics, supersession, contradiction.
- [`../02_data_model/08_relation.md`](../02_data_model/08_relation.md) — relation record semantics, cardinality, symmetry, evidence.
- [`./07_error_handling.md`](./07_error_handling.md) — error code mapping into the ERROR frame and per-field validation rules.

## Entity frames

Request/response body schemas for every opcode in the `0x0130–0x013F` entity range.

### Common types

Defined once, reused across this section and Brain's [`./05_frame_layouts.md`](./05_frame_layouts.md).

| Wire alias | Rust type | Meaning |
|---|---|---|
| `WireUuid` | `[u8; 16]` | UUIDv7-shaped 128-bit identifier. Used for `EntityId`, `request_id`, `audit_id`, `merge_id`, etc. The all-zeros value `[0u8; 16]` is **reserved** as a sentinel — see "None encoding" below. |
| `EntityTypeId` | `u32` | Raw form of the registry id. `Person` is permanently `1` (seeded at db open); user-declared types from the schema DSL get monotonically-increasing ids ≥ 2. |
| `AttributesBlob` | `Vec<u8>` | Opaque encoded attributes — rkyv-encoded `BTreeMap<String, Value>` validated against the entity type's attribute schema. The wire layer treats it as opaque bytes. |

#### CBOR conventions

All payloads in this section are CBOR maps validated against the per-opcode field schema. Senders use deterministic encoding (RFC 8949 §4.2.1). Receivers reject unknown fields and malformed CBOR.

CBOR validation is mandatory: the server decodes every received payload and validates it against the opcode's field schema, rejecting malformed buffers with `MalformedPayload` (see [`07_error_handling.md`](./07_error_handling.md)).

#### `None` encoding for `WireUuid` fields

A CBOR `null` is avoided for optional `WireUuid` fields. Where a struct field carries an optional `EntityId`, the wire shape uses a bare `WireUuid` and treats `[0u8; 16]` as the sentinel for "absent." UUIDv7 cannot produce the all-zeros value (its first 48 bits are a unix-ms timestamp), so the collision is impossible by construction. Documented per struct below.

### Entity opcode index

| Opcode | Name | Section | Status |
|---|---|---|---|
| `0x0130` | `ENTITY_CREATE` | "ENTITY_CREATE" | implemented  |
| `0x0131` | `ENTITY_GET` | "ENTITY_GET" | implemented  |
| `0x0132` | `ENTITY_UPDATE` | "ENTITY_UPDATE" | implemented  |
| `0x0133` | `ENTITY_RENAME` | "ENTITY_RENAME" | implemented  |
| `0x0134` | `ENTITY_MERGE` | "ENTITY_MERGE" | spec-only |
| `0x0135` | `ENTITY_UNMERGE` | "ENTITY_UNMERGE" | spec-only |
| `0x0136` | `ENTITY_RESOLVE` | "ENTITY_RESOLVE" | spec-only |
| `0x0137` | `ENTITY_LIST` | "ENTITY_LIST" | spec-only |
| `0x0138` | `ENTITY_TOMBSTONE` | "ENTITY_TOMBSTONE" | spec-only |

Responses occupy `0x01B0–0x01B8` (same low byte with high bit set, matching Brain's `0x2N → 0xAN` convention; see [`./03_opcodes.md`](./03_opcodes.md) §3).

### ENTITY_CREATE (0x0130)

#### Request body — `EntityCreateRequest`

```rust
pub struct EntityCreateRequest {
    pub entity_type_id: u32,        // EntityTypeId raw form
    pub canonical_name: String,     // primary display name (pre-normalization)
    pub aliases: Vec<String>,       // initial aliases (may be empty)
    pub attributes_blob: Vec<u8>,   // opaque attributes; may be empty
    pub request_id: WireUuid,       // idempotency key for retry-safe writes
}
```

Semantics:

1. The server normalizes `canonical_name` (lowercase + whitespace collapse) and uses it for the exact-name index.
2. A fresh `EntityId` (UUIDv7) is allocated by the server. Clients **must not** supply one.
3. `attributes_blob` is stored verbatim and not interpreted by the wire layer. The schema DSL validates contents before commit.
4. Aliases are stored verbatim; normalized forms feed the alias index.
5. `request_id` participates in Brain's idempotency cache (24h TTL; see [`../05_operations/02_write_pipeline.md`](../05_operations/02_write_pipeline.md) for the idempotency contract). Resubmitting with the same `request_id` and identical params returns the cached response; identical id with different params → `ENTITY_AMBIGUOUS` error.

Validation rules: see [`./07_error_handling.md`](./07_error_handling.md).

#### Response body — `EntityCreateResponse`

```rust
pub struct EntityCreateResponse {
    pub entity_id: WireUuid,        // freshly allocated EntityId
}
```

#### Error responses

The server returns an ERROR frame (opcode `0x00FF`) with one of:

- `ENTITY_TYPE_MISMATCH` (`0x31`) — `entity_type_id` not registered.
- `DUPLICATE_CANONICAL_NAME` (mapped via substrate `Conflict`) — `(entity_type_id, normalized_name)` already exists.
- `INVALID_ARGUMENT` (substrate `0x40`) — empty / oversized name, oversized attributes, alias-count exceeds cap.
- `IDEMPOTENCY_CONFLICT` (substrate `Conflict`) — same `request_id` with different params.

See [`./07_error_handling.md`](./07_error_handling.md) for the complete mapping.

#### Example

```text
C → S  frame: opcode=0x0130 stream_id=1 EOS
       payload: cbor(EntityCreateRequest {
           entity_type_id: 1,                 // Person
           canonical_name: "Priya Patel",
           aliases: vec!["Priya", "P. Patel"],
           attributes_blob: vec![],            // none
           request_id: <UUIDv7>,
       })
S → C  frame: opcode=0x01B0 stream_id=1 EOS
       payload: cbor(EntityCreateResponse {
           entity_id: <fresh UUIDv7>,
       })
```

### ENTITY_GET (0x0131)

#### Request body — `EntityGetRequest`

```rust
pub struct EntityGetRequest {
    pub entity_id: WireUuid,
}
```

#### Response body — `EntityGetResponse`

```rust
pub struct EntityGetResponse {
    pub entity: EntityView,
}
```

`EntityView` is defined in §"EntityView" below (read-side projection of `brain_core::Entity`).

#### Error responses

- `ENTITY_NOT_FOUND` (`0x30`) — no row with that id.
- Note: tombstoned entities are returned with `flags & TOMBSTONED != 0`. Merged entities are returned with `merged_into != [0; 16]`; redirection to the survivor is the **client's** responsibility. (`ENTITY_GET` is faithful — wider response semantics with auto-redirect are tracked as an open question.)

#### No idempotency

`ENTITY_GET` is a read; it carries no `request_id` and is not cached by the idempotency layer.

### ENTITY_UPDATE (0x0132)

#### Request body — `EntityUpdateRequest`

```rust
pub struct EntityUpdateRequest {
    pub entity_id: WireUuid,
    pub canonical_name: String,     // new desired canonical_name
    pub aliases: Vec<String>,       // full desired alias list (NOT a delta)
    pub attributes_blob: Vec<u8>,   // full desired attributes (NOT a delta)
    pub request_id: WireUuid,
}
```

Semantics:

1. **Replace-not-merge for `aliases` and `attributes_blob`**. The handler reads the current row, replaces these fields, and writes back. Delta-encoded variants are a future option; the shipping shape is full-replace for simplicity.
2. If `canonical_name` differs from the current row's, the handler triggers the rename path internally (old name moves into `aliases`, `embedding_version` bumps, exact-name index is rewritten). Equivalent to `ENTITY_RENAME` with `move_to_alias = true`.
3. `entity_type` is **not mutable** via `ENTITY_UPDATE`. A future `RETYPE_ENTITY` opcode handles type changes.
4. `updated_at_unix_nanos` is set to the server's clock.
5. Idempotency via `request_id`.

#### Response body — `EntityUpdateResponse`

```rust
pub struct EntityUpdateResponse {
    pub entity: EntityView,         // post-update view (avoids a follow-up GET)
}
```

#### Error responses

- `ENTITY_NOT_FOUND` (`0x30`)
- `DUPLICATE_CANONICAL_NAME` — if the rename component would collide with an existing entity of the same type.
- `INVALID_ARGUMENT` — empty / oversized fields.
- `IDEMPOTENCY_CONFLICT`.

### ENTITY_RENAME (0x0133)

#### Request body — `EntityRenameRequest`

```rust
pub struct EntityRenameRequest {
    pub entity_id: WireUuid,
    pub new_canonical_name: String,
    pub move_to_alias: bool,        // default = true; preserves the old name as an alias
    pub request_id: WireUuid,
}
```

Semantics:

1. Strictly a name change. Attributes and existing aliases are unchanged.
2. If `move_to_alias = true`, the **old** canonical name is appended to the alias list (deduplicated by normalized form).
3. `embedding_version` is bumped so the embedding worker re-embeds.
4. **Current constraint:** the handler rejects `move_to_alias = false` with `INVALID_ARGUMENT`. A "no-trail" rename mode is deferred; the flag is wired through end-to-end so that path is ready when it lands.

#### Response body — `EntityRenameResponse`

```rust
pub struct EntityRenameResponse {
    pub entity: EntityView,         // post-rename view
}
```

#### Error responses

- `ENTITY_NOT_FOUND` (`0x30`)
- `DUPLICATE_CANONICAL_NAME` — `new_canonical_name` collides under the same type.
- `INVALID_ARGUMENT` — empty name, name too long, or `move_to_alias=false` (currently unsupported).

### ENTITY_MERGE (0x0134) — spec-only

#### Request body — `EntityMergeRequest`

```rust
pub struct EntityMergeRequest {
    pub survivor: WireUuid,         // entity that absorbs the merged
    pub merged: WireUuid,           // entity that gets redirected
    pub confidence: f32,            // [0.0, 1.0]; ≥0.95 = autonomous, [0.7,0.95) = needs review
    pub reason: String,             // human-readable; stored in audit
    pub request_id: WireUuid,
}
```

Spec semantics: see [`../02_data_model/06_entity_lifecycle.md`](../02_data_model/06_entity_lifecycle.md) §"Entity merge" — `merged.merged_into = Some(survivor)`, aliases / attributes folded with conflict rules, all statements / relations re-routed inside one redb transaction, audit record written, `MERGED` event emitted on SUBSCRIBE.

#### Response body — `EntityMergeResponse`

```rust
pub struct EntityMergeResponse {
    pub audit_id: WireUuid,         // ENTITY_RESOLUTION_AUDIT row id
    pub grace_period_seconds: u64,  // how long UNMERGE can still reverse this
}
```

#### Error responses

- `ENTITY_NOT_FOUND` — either id missing.
- `ENTITY_MERGE_CONFLICT` (`0x33`) — `survivor` and `merged` are the same entity, or `merged` is already merged into a third entity.
- `INVALID_ARGUMENT` — `confidence` outside `[0.0, 1.0]`, `reason` too long.

#### Open questions

- Cross-type merges (Person ↔ Organization): forbidden by default, or allowed with attribute drop?
- Should the grace period be returned absolute (unix nanos) or relative (seconds)? Currently relative.

### ENTITY_UNMERGE (0x0135) — spec-only

#### Request body — `EntityUnmergeRequest`

```rust
pub struct EntityUnmergeRequest {
    pub merged_entity: WireUuid,    // the entity that was merged
    pub request_id: WireUuid,
}
```

Reverses a recent merge by clearing `merged_into`, splitting back the contributed aliases / attributes (from the merge audit's recorded delta), and re-routing statements / relations whose audit trail attributes them to the original merged entity.

Time-bound: only valid within the merge audit's `grace_period_seconds`. After that, the redirect is permanent and `UNMERGE` returns `ENTITY_MERGE_CONFLICT`.

#### Response body — `EntityUnmergeResponse`

```rust
pub struct EntityUnmergeResponse {
    pub restored_entity_id: WireUuid,
}
```

#### Error responses

- `ENTITY_NOT_FOUND` — `merged_entity` doesn't exist or was never merged.
- `ENTITY_MERGE_CONFLICT` — grace period expired, or `survivor` has been merged further since.

### ENTITY_RESOLVE (0x0136) — spec-only

Exposes the entity resolver over the wire so clients can run resolution without re-implementing the tier ladder.

#### Request body — `EntityResolveRequest`

```rust
pub struct EntityResolveRequest {
    pub candidate_name: String,
    pub context: String,            // surrounding text (≤ 100 chars consumed)
    pub entity_type_hint: u32,      // 0 = no hint; otherwise an EntityTypeId
    pub allow_create: bool,         // if true, tier 5 creates a fresh entity
    pub request_id: WireUuid,
}
```

#### Response body — `EntityResolveResponse`

```rust
pub struct EntityResolveResponse {
    pub outcome: ResolutionOutcome,
    pub tier: u8,                   // which tier resolved (1..=5, 0 if unresolved)
    pub confidence: f32,
    pub candidate_ids: Vec<WireUuid>, // present when outcome=Ambiguous; ranked
    pub audit_id: WireUuid,         // [0;16] unless an ambiguity audit was written
}

#[repr(u8)]
pub enum ResolutionOutcome {
    Resolved = 1,                   // exactly one match
    Created = 2,                    // tier 5 created a new entity
    Ambiguous = 3,                  // multiple candidates above threshold; audit written
    NotFound = 4,                   // all tiers exhausted, allow_create=false
}
```

#### Error responses

- `INVALID_ARGUMENT` — empty `candidate_name`, oversized `context`.
- `SCHEMA_NOT_DECLARED` (substrate `0x21` for now; §03-specific code possible) — if no schema declared (resolver currently requires the entity_type registry seeded).

### ENTITY_LIST (0x0137) — spec-only

Paginated scan over the entity table. Cheap for small deployments; the query router is the better path for production-sized graphs.

#### Request body — `EntityListRequest`

```rust
pub struct EntityListRequest {
    pub entity_type_id: u32,        // 0 = no filter
    pub name_prefix: String,        // "" = no filter; normalized server-side
    pub mention_count_min: u32,     // 0 = no filter
    pub include_tombstoned: bool,
    pub include_merged: bool,
    pub limit: u32,                 // max results (capped at 1000)
    pub cursor: Vec<u8>,            // opaque continuation token; empty on first page
}
```

#### Response body — `EntityListResponse`

Streaming response (one `STREAM_ITEM` per match, `STREAM_END` with cursor). Per-item shape:

```rust
pub struct EntityListItem {
    pub entity: EntityView,
}

pub struct EntityListResponseTail {
    pub next_cursor: Vec<u8>,       // empty if exhausted
    pub total_returned: u32,
}
```

The frame layout mirrors substrate `RECALL_RESP` — see [`./06_streaming.md`](./06_streaming.md).

#### Error responses

- `INVALID_ARGUMENT` — `limit` > 1000, malformed cursor.

### ENTITY_TOMBSTONE (0x0138) — spec-only

#### Request body — `EntityTombstoneRequest`

```rust
pub struct EntityTombstoneRequest {
    pub entity_id: WireUuid,
    pub reason: String,
    pub request_id: WireUuid,
}
```

Semantics:

1. Sets the `TOMBSTONED` flag bit (see `brain_metadata::tables::knowledge::entity::flags`).
2. Tears down the exact-name + alias + trigram secondary indexes so the resolver never sees the row again.
3. Keeps the primary record for audit / unmerge.
4. Tombstoned entities are **not** auto-collected. A separate GC sweep (off by default) reclaims after a grace period.

#### Response body — `EntityTombstoneResponse`

```rust
pub struct EntityTombstoneResponse {
    pub tombstoned_at_unix_nanos: u64,
}
```

#### Error responses

- `ENTITY_NOT_FOUND` (`0x30`)
- already-tombstoned entities return success (idempotent).

### `EntityView` — shared read-side projection

```rust
pub struct EntityView {
    pub entity_id: WireUuid,
    pub entity_type_id: u32,
    pub canonical_name: String,
    pub normalized_name: String,            // server-computed
    pub aliases: Vec<String>,
    pub attributes_blob: Vec<u8>,
    pub mention_count: u32,
    pub created_at_unix_nanos: u64,
    pub updated_at_unix_nanos: u64,
    pub merged_into: WireUuid,              // [0;16] when not merged
    pub embedding_version: u32,
    pub flags: u32,                         // bit 0 = TOMBSTONED, bit 1 = MERGED, …
}
```

Field semantics mirror `brain_core::Entity`. One projection used by `GET`, `UPDATE`, `RENAME`, and `LIST` to avoid divergent response shapes.

#### What `EntityView` deliberately omits

- The raw embedding bytes. Clients that need the embedding query the entity HNSW directly via `RECALL_HYBRID` or `ADMIN_GET_AUDIT`-style debug paths.
- Reference counts to specific statements / relations. Use `STATEMENT_LIST` / `RELATION_LIST_FROM`.

### Idempotency cache key (entity ops)

For every opcode in this section that carries a `request_id`, Brain's idempotency layer keys on:

```
(agent_id, opcode_u16, request_id, blake3(payload_bytes))
```

(Same shape as substrate, with the typed-graph opcode taking the same `Opcode` slot.) TTL is 24h. Stored responses include the full frame bytes so a duplicate hit is byte-identical, EOS-flag and all.

### Entity-frames implementation note

The wire shapes for `ENTITY_CREATE` through `ENTITY_RENAME` are implemented with round-trip CBOR conformance tests in the `brain-protocol` crate. The shapes for `ENTITY_MERGE` through `ENTITY_TOMBSTONE` are **spec-only**; their Rust counterparts may be refined during implementation. Refinements must update this file before code lands.

## Statement frames

Request/response body schemas for every opcode in the `0x0140–0x014F` statement range. Statements are the typed graph's typed claims about entities (Fact / Preference / Event); see [`../02_data_model/07_statement.md`](../02_data_model/07_statement.md) for the value-type semantics.

### Statement opcode index

| Opcode | Name | Section | Status |
|---|---|---|---|
| `0x0140` | `STATEMENT_CREATE` | "STATEMENT_CREATE" | spec-only |
| `0x0141` | `STATEMENT_GET` | "STATEMENT_GET" | spec-only |
| `0x0142` | `STATEMENT_SUPERSEDE` | "STATEMENT_SUPERSEDE" | spec-only |
| `0x0143` | `STATEMENT_TOMBSTONE` | "STATEMENT_TOMBSTONE" | spec-only |
| `0x0144` | `STATEMENT_RETRACT` | "STATEMENT_RETRACT" | spec-only |
| `0x0145` | `STATEMENT_HISTORY` | "STATEMENT_HISTORY" | spec-only |
| `0x0146` | `STATEMENT_LIST` | "STATEMENT_LIST" | spec-only |

Responses live at `0x01C0–0x01C6`.

### Shared statement types

#### `StatementKindWire`

```rust
#[repr(u8)]
pub enum StatementKindWire {
    Fact = 1,
    Preference = 2,
    Event = 3,
}
```

#### `StatementObjectWire` — tagged union mirroring `StatementObject`

```rust
pub enum StatementObjectWire {
    EntityRef(WireUuid),               // EntityId
    Value(StatementValueWire),         // typed literal
    MemoryRef(u128),                   // MemoryId (raw packed form)
    StatementRef(WireUuid),            // meta-statement
}

pub enum StatementValueWire {
    Text(String),
    Integer(i64),
    Float(f64),
    Bool(bool),
    UnixNanos(u64),
    Blob(Vec<u8>),                     // ≤ 64 KiB per blob cap
}
```

The schema DSL enforces object-type constraints per predicate (e.g. `manages` requires `EntityRef<Person>`). The wire layer carries the typed value; semantic validation happens in the handler against the predicate's declared type.

#### `EvidenceRefWire`

```rust
pub enum EvidenceRefWire {
    Inline(Vec<u128>),                 // up to 8 MemoryIds; reject otherwise (caps below)
    Overflow(WireUuid),                // EvidenceOverflowId
}
```

#### `StatementView` — read-side projection

```rust
pub struct StatementView {
    pub statement_id: WireUuid,
    pub kind: StatementKindWire,
    pub subject: WireUuid,             // EntityId or [0;16] for Pending; see flags
    pub subject_pending_audit_id: WireUuid,  // [0;16] unless subject is pending
    pub predicate: String,             // "namespace:name" canonical form
    pub object: StatementObjectWire,
    pub confidence: f32,
    pub evidence: EvidenceRefWire,
    pub extractor_id: u32,
    pub extracted_at_unix_nanos: u64,
    pub schema_version: u32,
    pub valid_from_unix_nanos: u64,    // 0 if None
    pub valid_to_unix_nanos: u64,      // 0 if None
    pub event_at_unix_nanos: u64,      // 0 if None / not an Event
    pub version: u32,
    pub superseded_by: WireUuid,       // [0;16] if not superseded
    pub supersedes: WireUuid,          // [0;16] if root of chain
    pub chain_root: WireUuid,
    pub tombstoned: bool,
    pub tombstoned_at_unix_nanos: u64,
    pub tombstone_reason: u8,          // 0=none, 1=SourceMemoryForgotten, 2=UserRequest, 3=SchemaInvalidation, 4=ExtractorRetraction
    pub flags: u32,                    // bit 0 = subject_pending
}
```

`StatementView` mirrors `brain_core::Statement`. Optional fields become "sentinel zero" rather than `Option<T>` for the same sentinel-zero wire reason as `EntityView`.

### STATEMENT_CREATE (0x0140)

#### Request — `StatementCreateRequest`

```rust
pub struct StatementCreateRequest {
    pub kind: StatementKindWire,
    pub subject: WireUuid,             // EntityId; resolution by client (use ENTITY_RESOLVE first if unsure)
    pub predicate: String,             // "namespace:name"
    pub object: StatementObjectWire,
    pub confidence: f32,               // [0, 1]
    pub evidence: EvidenceRefWire,
    pub extractor_id: u32,             // 0 = user-authored (no extractor)
    pub valid_from_unix_nanos: u64,    // 0 = use extracted_at; Event must pass 0
    pub valid_to_unix_nanos: u64,      // 0 = open-ended
    pub event_at_unix_nanos: u64,      // required for Event kind; 0 for others
    pub schema_version: u32,           // 0 = current
    pub request_id: WireUuid,
}
```

Semantics:

1. Resolve `predicate` (a `"namespace:name"` qname). If the namespace has no active schema, intern the qname with `SchemaOrigin::ImplicitFromWrite` on first use; the resulting statement row carries the `IMPLICIT_PREDICATE` flag. If a schema is active for the namespace, the qname must be declared — unknown qnames produce `PredicateNotInSchema` (0x004B). When a declared predicate carries kind / object-type constraints they are enforced — mismatches produce `STATEMENT_OBJECT_TYPE_MISMATCH` (0x41). Implicit predicates carry no constraints.
2. Validate `subject` exists (or is a known Pending audit id).
3. For `Preference` kind: if a current Preference with same `(subject, predicate)` exists, auto-supersede it (no separate `STATEMENT_SUPERSEDE` call required).
4. Allocate `StatementId` (UUIDv7).
5. Write to `statements` + all indexes + tantivy text index inside one redb transaction.
6. Emit `STATEMENT_CREATED` event (see §"SUBSCRIBE events" in [`./09_typed_graph_admin.md`](./09_typed_graph_admin.md)).

#### Response — `StatementCreateResponse`

```rust
pub struct StatementCreateResponse {
    pub statement_id: WireUuid,
    pub auto_superseded: WireUuid,     // [0;16] unless auto-supersession fired
    pub chain_root: WireUuid,
}
```

#### Errors

- `PredicateNotInSchema` (`0x004B`) — strict mode only; predicate qname is not declared in the active schema for the namespace.
- `STATEMENT_OBJECT_TYPE_MISMATCH` (`0x41`) — object type violates a declared predicate's constraint (schema-declared predicates only).
- `STATEMENT_CONTRADICTS_EXISTING` (`0x42`) — for Fact kind, an active contradictory Fact already exists; resolution requires explicit `STATEMENT_SUPERSEDE`.
- `ENTITY_NOT_FOUND` (`0x30`) — `subject` doesn't exist.
- `INVALID_ARGUMENT` — Event kind without `event_at`; Fact / Preference with `event_at`; confidence outside `[0, 1]`; malformed predicate qname.

#### Evidence cap

`EvidenceRefWire::Inline(Vec<u128>)` MUST contain ≤ 8 MemoryIds. Larger evidence sets require pre-writing an `evidence_overflow` row (via the worker-side path) and using `Overflow(EvidenceOverflowId)`.

### STATEMENT_GET (0x0141)

#### Request

```rust
pub struct StatementGetRequest {
    pub statement_id: WireUuid,
    pub follow_supersession: bool,     // true = if superseded, return the current one in the chain
}
```

#### Response

```rust
pub struct StatementGetResponse {
    pub statement: StatementView,
    pub returned_via_supersession: bool,  // true if follow_supersession redirected
}
```

#### Errors

- `STATEMENT_NOT_FOUND` (`0x40`).

### STATEMENT_SUPERSEDE (0x0142)

#### Request

```rust
pub struct StatementSupersedeRequest {
    pub old_statement_id: WireUuid,
    pub new_statement: StatementCreateRequest,  // embedded; the server runs CREATE then links
    pub request_id: WireUuid,
}
```

Semantics: atomic two-step inside one redb transaction — create the new statement, then link `old.superseded_by = new` and `new.supersedes = old`. `chain_root` computed per [`../02_data_model/07_statement.md`](../02_data_model/07_statement.md). `valid_to` on the old statement is set to `new.extracted_at` (for Fact / Preference kinds).

#### Response

```rust
pub struct StatementSupersedeResponse {
    pub new_statement_id: WireUuid,
    pub chain_root: WireUuid,
    pub version: u32,                  // new statement's version
}
```

#### Errors

- `STATEMENT_NOT_FOUND` — `old_statement_id` missing.
- `INVALID_ARGUMENT` — old is already tombstoned, or kind=`Event` (Events cannot be superseded).
- Any error from the embedded `STATEMENT_CREATE`.

### STATEMENT_TOMBSTONE (0x0143)

#### Request

```rust
pub struct StatementTombstoneRequest {
    pub statement_id: WireUuid,
    pub reason: u8,                    // matches StatementView.tombstone_reason values
    pub reason_message: String,        // ≤ 4 KiB
    pub request_id: WireUuid,
}
```

#### Response

```rust
pub struct StatementTombstoneResponse {
    pub tombstoned_at_unix_nanos: u64,
}
```

Soft delete. The statement remains queryable via `STATEMENT_HISTORY` and `STATEMENT_GET` for the duration of the configured grace period before hard reclamation (default 7 days, same as memory tombstone grace — see [`../02_data_model/07_statement.md`](../02_data_model/07_statement.md) §"Retract" and [`../02_data_model/02_memory.md`](../02_data_model/02_memory.md) §"Lifecycle").

#### Errors

- `STATEMENT_NOT_FOUND`.
- Already-tombstoned statements return success (idempotent).

### STATEMENT_RETRACT (0x0144)

#### Request

```rust
pub struct StatementRetractRequest {
    pub statement_id: WireUuid,
    pub reason: u8,
    pub reason_message: String,
    pub request_id: WireUuid,
}
```

Hard delete: tombstone immediately and **zero out** the fields after the grace period. Used for incorrect-extraction or privacy-driven removal. Distinct from `STATEMENT_TOMBSTONE` in that retracted statements are also removed from `STATEMENT_HISTORY` results.

#### Response

```rust
pub struct StatementRetractResponse {
    pub retracted_at_unix_nanos: u64,
    pub will_zero_at_unix_nanos: u64,  // when GC sweep will reclaim
}
```

#### Errors

- `STATEMENT_NOT_FOUND`.
- `Authorization` (substrate) — retraction requires admin permissions per the schema-frames conventions.

### STATEMENT_HISTORY (0x0145)

#### Request

```rust
pub struct StatementHistoryRequest {
    pub anchor_id: WireUuid,           // either StatementId or chain_root
    pub include_tombstoned: bool,
}
```

#### Response — streaming, per-item

```rust
pub struct StatementHistoryItem {
    pub statement: StatementView,
}

pub struct StatementHistoryTail {
    pub chain_root: WireUuid,
    pub total_versions: u32,
}
```

Returns the full chain in `version` order (ascending). Suppresses retracted statements regardless of `include_tombstoned`.

#### Errors

- `STATEMENT_NOT_FOUND` — `anchor_id` doesn't exist.

### STATEMENT_LIST (0x0146)

#### Request — `StatementListRequest`

```rust
pub struct StatementListRequest {
    pub subject: WireUuid,             // [0;16] = no filter
    pub predicate: String,             // "" = no filter
    pub kind: u8,                      // 0 = no filter; otherwise matches StatementKindWire
    pub min_confidence: f32,
    pub time_range_start_unix_nanos: u64,
    pub time_range_end_unix_nanos: u64,
    pub only_current: bool,            // true = exclude superseded
    pub include_tombstoned: bool,
    pub limit: u32,                    // 1..=1000
    pub cursor: Vec<u8>,
}
```

Filter semantics:

- `subject != [0;16]` → match `statements_by_subject` index.
- `predicate != ""` → match `statements_by_predicate`.
- `time_range_*`: for Events, matches `event_at`; for Fact / Preference, matches `valid_*` overlap with the range.
- `only_current`: short-circuits to `superseded_by_is_null = true` predicate; equivalent to "current state" queries from [`../02_data_model/07_statement.md`](../02_data_model/07_statement.md).

#### Response — streaming `StatementView`

Same shape as `STATEMENT_HISTORY` — one `StatementView` per match, tail frame carries `next_cursor` + `total_returned`.

#### Errors

- `INVALID_ARGUMENT` — `limit` > 1000, malformed cursor, invalid kind byte.
- `ENTITY_NOT_FOUND` — `subject != [0;16]` but no such entity (server short-circuits).

### Pending subjects on the wire

A statement with `subject_pending_audit_id != [0;16]` indicates the subject is unresolved (an ambiguity audit is pending). Clients should treat such statements as **queryable but excluded from graph joins on subject** until `ADMIN_RESOLVE_AMBIGUITY` (see [`./09_typed_graph_admin.md`](./09_typed_graph_admin.md)) decides the binding.

`STATEMENT_LIST` does not filter pending subjects by default; clients that want only resolved subjects filter client-side on `flags & 1 == 0`.

### Cross-shard / sharding (statements)

Statements are sharded by `subject` EntityId. `STATEMENT_LIST` with a `subject` filter routes to the subject's shard. Without a subject filter the query fans out to all shards and merges client-side (or via the planner).

## Relation frames

Request/response body schemas for opcodes `0x0150–0x0156` (relation operations). Relations are typed edges between entities — distinct from substrate memory-to-memory edges; see [`../02_data_model/08_relation.md`](../02_data_model/08_relation.md).

### Relation opcode index

| Opcode | Name | Section | Status |
|---|---|---|---|
| `0x0150` | `RELATION_CREATE` | "RELATION_CREATE" | spec-only |
| `0x0151` | `RELATION_GET` | "RELATION_GET" | spec-only |
| `0x0152` | `RELATION_SUPERSEDE` | "RELATION_SUPERSEDE" | spec-only |
| `0x0153` | `RELATION_TOMBSTONE` | "RELATION_TOMBSTONE" | spec-only |
| `0x0154` | `RELATION_LIST_FROM` | "RELATION_LIST_FROM" | spec-only |
| `0x0155` | `RELATION_LIST_TO` | "RELATION_LIST_TO" | spec-only |
| `0x0156` | `RELATION_TRAVERSE` | "RELATION_TRAVERSE" | spec-only |

Responses live at `0x01D0–0x01D6`.

### Shared relation types

#### `RelationPropertiesBlob`

```rust
pub type RelationPropertiesBlob = Vec<u8>;
```

Opaque rkyv-encoded `BTreeMap<String, StatementValueWire>`. Schema enforces the property names and types per relation type — same approach as `EntityAttributes`.

#### `RelationView` — read-side projection

```rust
pub struct RelationView {
    pub relation_id: WireUuid,
    pub relation_type: String,         // canonical "namespace:name"
    pub from_entity: WireUuid,
    pub to_entity: WireUuid,
    pub properties_blob: RelationPropertiesBlob,
    pub evidence: EvidenceRefWire,     // shared with statements
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

### RELATION_CREATE (0x0150)

#### Request — `RelationCreateRequest`

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
4. Validate **cardinality**. Only schema-declared types carry an enforceable cardinality contract. For declared `one_to_one` / `one_to_many` / `many_to_one`, the server checks existing edges before inserting; violation → `CardinalityViolation` (0x0065). Implicit types are always `many_to_many` and never trigger this error.
5. Allocate `RelationId` (UUIDv7).
6. Write to `relations` + `relations_by_from` + `relations_by_to` indexes inside one redb transaction.
7. Emit `RELATION_CREATED` event.

#### Response — `RelationCreateResponse`

```rust
pub struct RelationCreateResponse {
    pub relation_id: WireUuid,
}
```

#### Errors

- `RelationTypeNotInSchema` (`0x004C`) — strict mode only; relation type qname is not declared in the active schema for the namespace.
- `ENTITY_NOT_FOUND`, `ENTITY_TYPE_MISMATCH`.
- `CardinalityViolation` (`0x0065`) — write would violate the declared cardinality of a schema-declared relation type.
- `INVALID_ARGUMENT` — malformed `relation_type` qname, malformed `properties_blob`, confidence out of `[0, 1]`.

### RELATION_GET (0x0151)

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

Errors: substrate `NotFound` (no typed-graph-specific "relation_not_found" code currently — re-uses `MemoryNotFound` per [`./07_error_handling.md`](./07_error_handling.md) Strategy B until that strategy lands).

### RELATION_SUPERSEDE (0x0152)

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

### RELATION_TOMBSTONE (0x0153)

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

### RELATION_LIST_FROM (0x0154)

#### Request

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

#### Response — streaming `RelationView`

One `RelationView` per match; tail with `next_cursor` and `total_returned`.

#### Errors

- `ENTITY_NOT_FOUND` — `from_entity` doesn't exist.
- `INVALID_ARGUMENT` — limit / cursor.

### RELATION_LIST_TO (0x0155)

Identical shape to `RELATION_LIST_FROM` but filters on `to_entity` via `relations_by_to` index.

### RELATION_TRAVERSE (0x0156)

Graph walk from a starting entity over selected relation types.

#### Request

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

#### Response — streaming per-frame `RelationTraverseFrame`

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

#### Errors

- `ENTITY_NOT_FOUND` — `start_entity` missing.
- `INVALID_ARGUMENT` — `max_depth` > 8, `max_nodes` > 1000, unknown relation type in `relation_types`.
- `QUERY_TIMEOUT` — wall-time budget exceeded mid-traversal (substrate `Unavailable`).

### Cardinality enforcement on the wire

Relation type declarations carry cardinality rules:

| Cardinality | Server check on CREATE | On SUPERSEDE |
|---|---|---|
| `one_to_one` | reject if either endpoint already has an active relation of this type | reject if new endpoints already have one |
| `one_to_many` (default) | reject if `from_entity` already has an active relation of this type | reject if new `from_entity` already has one |
| `many_to_one` | reject if `to_entity` already has an active relation of this type | symmetric |
| `many_to_many` | no check | no check |

Errors → `CardinalityViolation` (`0x0065`). `ErrorDetails.expected` carries the declared cardinality string; `ErrorDetails.actual` is empty. Cardinality is enforced only on schema-declared relation types; implicit (open-vocabulary) types are always `many_to_many` and never trigger this error.
