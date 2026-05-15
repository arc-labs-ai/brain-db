# 28.06 Statement Wire Frames

Request/response body schemas for every opcode in the `0x0140–0x014F` statement range. Statements are the knowledge layer's typed claims about entities (Fact / Preference / Event); see [`../19_statements/00_purpose.md`](../19_statements/00_purpose.md) for the value-type semantics.

Cross-references:
- [`../19_statements/00_purpose.md`](../19_statements/00_purpose.md) — `Statement` schema, kind contracts, supersession.
- [`../21_schema_dsl/`](../21_schema_dsl/) — predicate vocabulary.
- [`./03_errors.md`](./03_errors.md), [`./04_validation.md`](./04_validation.md) — error mapping and field caps.

## 1. Opcode index

| Opcode | Name | Section | Status |
|---|---|---|---|
| `0x0140` | `STATEMENT_CREATE` | §3 | spec-only — phase 17 |
| `0x0141` | `STATEMENT_GET` | §4 | spec-only — phase 17 |
| `0x0142` | `STATEMENT_SUPERSEDE` | §5 | spec-only — phase 17 |
| `0x0143` | `STATEMENT_TOMBSTONE` | §6 | spec-only — phase 17 |
| `0x0144` | `STATEMENT_RETRACT` | §7 | spec-only — phase 17 |
| `0x0145` | `STATEMENT_HISTORY` | §8 | spec-only — phase 17 |
| `0x0146` | `STATEMENT_LIST` | §9 | spec-only — phase 17 |

Responses live at `0x01C0–0x01C6`.

## 2. Shared types

### 2.1 `StatementKindWire`

```rust
#[repr(u8)]
pub enum StatementKindWire {
    Fact = 1,
    Preference = 2,
    Event = 3,
}
```

### 2.2 `StatementObjectWire` — tagged union mirroring `StatementObject`

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

Phase 19's schema DSL will enforce object-type constraints per predicate (e.g. `manages` requires `EntityRef<Person>`). The wire layer carries the typed value; semantic validation happens in the handler against the predicate's declared type.

### 2.3 `EvidenceRefWire`

```rust
pub enum EvidenceRefWire {
    Inline(Vec<u128>),                 // up to 8 MemoryIds; reject otherwise (caps in §4.4)
    Overflow(WireUuid),                // EvidenceOverflowId
}
```

### 2.4 `StatementView` — read-side projection

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

`StatementView` mirrors `brain_core::Statement`. Optional fields become "sentinel zero" rather than `Option<T>` for the same rkyv-archive reason as `EntityView` ([`./01_entity_frames.md`](./01_entity_frames.md) §1.2).

## 3. STATEMENT_CREATE (0x0140)

### 3.1 Request — `StatementCreateRequest`

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

1. Validate against predicate definition (kind, object type) — `STATEMENT_OBJECT_TYPE_MISMATCH` on mismatch.
2. Validate `subject` exists (or is a known Pending audit id).
3. For `Preference` kind: if a current Preference with same `(subject, predicate)` exists, auto-supersede it (no separate `STATEMENT_SUPERSEDE` call required).
4. Allocate `StatementId` (UUIDv7).
5. Write to `statements` + all indexes + tantivy text index inside one redb transaction.
6. Emit `STATEMENT_CREATED` event (see [`./02_subscribe_events.md`](./02_subscribe_events.md) §3.2).

### 3.2 Response — `StatementCreateResponse`

```rust
pub struct StatementCreateResponse {
    pub statement_id: WireUuid,
    pub auto_superseded: WireUuid,     // [0;16] unless §3 step 3 fired
    pub chain_root: WireUuid,
}
```

### 3.3 Errors

- `STATEMENT_OBJECT_TYPE_MISMATCH` (`0x41`) — object type violates predicate constraint.
- `STATEMENT_CONTRADICTS_EXISTING` (`0x42`) — for Fact kind, an active contradictory Fact already exists; resolution requires explicit `STATEMENT_SUPERSEDE`. (Contradiction detection per [§19](../19_statements/00_purpose.md) §"Kind-specific contracts".)
- `ENTITY_NOT_FOUND` (`0x30`) — `subject` doesn't exist.
- `INVALID_ARGUMENT` — Event kind without `event_at`; Fact / Preference with `event_at`; confidence outside `[0, 1]`; predicate unknown.

### 3.4 Evidence cap

`EvidenceRefWire::Inline(Vec<u128>)` MUST contain ≤ 8 MemoryIds. Larger evidence sets require pre-writing an `evidence_overflow` row (via a worker-side path — phase 22) and using `Overflow(EvidenceOverflowId)`.

## 4. STATEMENT_GET (0x0141)

### 4.1 Request

```rust
pub struct StatementGetRequest {
    pub statement_id: WireUuid,
    pub follow_supersession: bool,     // true = if superseded, return the current one in the chain
}
```

### 4.2 Response

```rust
pub struct StatementGetResponse {
    pub statement: StatementView,
    pub returned_via_supersession: bool,  // true if follow_supersession redirected
}
```

### 4.3 Errors

- `STATEMENT_NOT_FOUND` (`0x40`).

## 5. STATEMENT_SUPERSEDE (0x0142)

### 5.1 Request

```rust
pub struct StatementSupersedeRequest {
    pub old_statement_id: WireUuid,
    pub new_statement: StatementCreateRequest,  // embedded; the server runs CREATE then links
    pub request_id: WireUuid,
}
```

Semantics: atomic two-step inside one redb transaction — create the new statement, then link `old.superseded_by = new` and `new.supersedes = old`. `chain_root` computed per [§19](../19_statements/00_purpose.md) §"SUPERSEDE_STATEMENT". `valid_to` on the old statement is set to `new.extracted_at` (for Fact / Preference kinds).

### 5.2 Response

```rust
pub struct StatementSupersedeResponse {
    pub new_statement_id: WireUuid,
    pub chain_root: WireUuid,
    pub version: u32,                  // new statement's version
}
```

### 5.3 Errors

- `STATEMENT_NOT_FOUND` — `old_statement_id` missing.
- `INVALID_ARGUMENT` — old is already tombstoned, or kind=`Event` (Events cannot be superseded per [§19](../19_statements/00_purpose.md) §"Event").
- Any error from the embedded `STATEMENT_CREATE`.

## 6. STATEMENT_TOMBSTONE (0x0143)

### 6.1 Request

```rust
pub struct StatementTombstoneRequest {
    pub statement_id: WireUuid,
    pub reason: u8,                    // matches StatementView.tombstone_reason values
    pub reason_message: String,        // ≤ 4 KiB
    pub request_id: WireUuid,
}
```

### 6.2 Response

```rust
pub struct StatementTombstoneResponse {
    pub tombstoned_at_unix_nanos: u64,
}
```

Soft delete. The statement remains queryable via `STATEMENT_HISTORY` and `STATEMENT_GET`. Grace period before hard reclamation: 30 days (per [§19](../19_statements/00_purpose.md) §"TOMBSTONE_STATEMENT").

### 6.3 Errors

- `STATEMENT_NOT_FOUND`.
- Already-tombstoned statements return success (idempotent).

## 7. STATEMENT_RETRACT (0x0144)

### 7.1 Request

```rust
pub struct StatementRetractRequest {
    pub statement_id: WireUuid,
    pub reason: u8,
    pub reason_message: String,
    pub request_id: WireUuid,
}
```

Hard delete: tombstone immediately and **zero out** the fields after the grace period. Used for incorrect-extraction or privacy-driven removal. Distinct from `STATEMENT_TOMBSTONE` in that retracted statements are also removed from `STATEMENT_HISTORY` results.

### 7.2 Response

```rust
pub struct StatementRetractResponse {
    pub retracted_at_unix_nanos: u64,
    pub will_zero_at_unix_nanos: u64,  // when GC sweep will reclaim
}
```

### 7.3 Errors

- `STATEMENT_NOT_FOUND`.
- `Authorization` (substrate) — retraction requires admin permissions per [`./05_schema_frames.md`](./05_schema_frames.md) §8 conventions.

## 8. STATEMENT_HISTORY (0x0145)

### 8.1 Request

```rust
pub struct StatementHistoryRequest {
    pub anchor_id: WireUuid,           // either StatementId or chain_root
    pub include_tombstoned: bool,
}
```

### 8.2 Response — streaming, per-item

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

### 8.3 Errors

- `STATEMENT_NOT_FOUND` — `anchor_id` doesn't exist.

## 9. STATEMENT_LIST (0x0146)

### 9.1 Request — `StatementListRequest`

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
- `only_current`: short-circuits to `superseded_by_is_null = true` predicate; equivalent to "current state" queries from [§19](../19_statements/00_purpose.md) §"Querying current state".

### 9.2 Response — streaming `StatementView`

Same shape as `STATEMENT_HISTORY` — one `StatementView` per match, tail frame carries `next_cursor` + `total_returned`.

### 9.3 Errors

- `INVALID_ARGUMENT` — `limit` > 1000, malformed cursor, invalid kind byte.
- `ENTITY_NOT_FOUND` — `subject != [0;16]` but no such entity (server short-circuits).

## 10. Pending subjects on the wire

A statement with `subject_pending_audit_id != [0;16]` indicates the subject is unresolved (an ambiguity audit is pending). Clients should treat such statements as **queryable but excluded from graph joins on subject** until `ADMIN_RESOLVE_AMBIGUITY` ([`./14_admin_frames.md`](./14_admin_frames.md)) decides the binding.

`STATEMENT_LIST` does not filter pending subjects by default; clients that want only resolved subjects filter client-side on `flags & 1 == 0`.

## 11. Cross-shard / sharding

Statements are sharded by `subject` EntityId. `STATEMENT_LIST` with a `subject` filter routes to the subject's shard. Without a subject filter the query fans out to all shards and merges client-side (or via the planner — phase 22).
