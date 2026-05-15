# 28.04 Entity Wire Frames

Request/response body schemas for every opcode in the `0x0130–0x013F` entity range. The substrate's [03 wire protocol](../03_wire_protocol/) covers the 32-byte header, opcode framing, CRC32C, and payload encoding — this file specifies only the rkyv-archived structs that live inside a request/response payload.

Cross-references:
- [`../18_entities/00_purpose.md`](../18_entities/00_purpose.md) — entity record semantics (the *what*).
- [`../18_entities/01_resolution.md`](../18_entities/01_resolution.md) — resolver tiers.
- [`../18_entities/02_storage.md`](../18_entities/02_storage.md) — redb layout.
- [`./03_errors.md`](./03_errors.md) — error code mapping into the ERROR frame.
- [`./04_validation.md`](./04_validation.md) — per-field validation rules.

## 1. Common types

Defined once, reused across this section and the substrate's [`../03_wire_protocol/07_request_frames.md`](../03_wire_protocol/07_request_frames.md).

| Wire alias | Rust type | Meaning |
|---|---|---|
| `WireUuid` | `[u8; 16]` | UUIDv7-shaped 128-bit identifier. Used for `EntityId`, `request_id`, `audit_id`, `merge_id`, etc. The all-zeros value `[0u8; 16]` is **reserved** as a sentinel — see §1.2. |
| `EntityTypeId` | `u32` | Raw form of the registry id. `Person` is permanently `1` (seeded at db open); user-declared types from phase 19's schema DSL get monotonically-increasing ids ≥ 2. |
| `AttributesBlob` | `Vec<u8>` | Opaque encoded attributes. Phase 16: caller-defined byte string. Phase 19+: rkyv-encoded `BTreeMap<String, Value>` validated against the entity type's attribute schema. The wire layer treats it as opaque bytes. |

### 1.1 rkyv conventions

All structs in this section derive:

```rust
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
```

`check_bytes` is mandatory: the server runs `rkyv::check_archived_root::<T>` on every received payload and rejects malformed buffers with `MalformedRkyv` ([substrate §10/3.1](../03_wire_protocol/10_errors.md)).

### 1.2 `None` encoding for `WireUuid` fields

rkyv 0.7's `Option<[u8; 16]>` archive shape is awkward in some derive paths. Where a struct field carries an optional `EntityId`, the wire shape uses a bare `WireUuid` and treats `[0u8; 16]` as the sentinel for "absent." UUIDv7 cannot produce the all-zeros value (its first 48 bits are a unix-ms timestamp), so the collision is impossible by construction. Documented per struct below.

## 2. Opcode index

| Opcode | Name | Section | Status |
|---|---|---|---|
| `0x0130` | `ENTITY_CREATE` | §3 | implemented (phase 16.6c) |
| `0x0131` | `ENTITY_GET` | §4 | implemented (phase 16.6c) |
| `0x0132` | `ENTITY_UPDATE` | §5 | implemented (phase 16.6c) |
| `0x0133` | `ENTITY_RENAME` | §6 | implemented (phase 16.6c) |
| `0x0134` | `ENTITY_MERGE` | §7 | spec-only — code lands phase 16.7 |
| `0x0135` | `ENTITY_UNMERGE` | §8 | spec-only — code lands phase 16.7 |
| `0x0136` | `ENTITY_RESOLVE` | §9 | spec-only — code lands phase 16.7 |
| `0x0137` | `ENTITY_LIST` | §10 | spec-only — code lands phase 16.7 |
| `0x0138` | `ENTITY_TOMBSTONE` | §11 | spec-only — code lands phase 16.7 |

Responses occupy `0x01B0–0x01B8` (same low byte with high bit set, matching the substrate's `0x2N → 0xAN` convention; see [`../03_wire_protocol/05_opcodes.md`](../03_wire_protocol/05_opcodes.md) §3).

## 3. ENTITY_CREATE (0x0130)

### 3.1 Request body — `EntityCreateRequest`

```rust
pub struct EntityCreateRequest {
    pub entity_type_id: u32,        // EntityTypeId raw form
    pub canonical_name: String,     // primary display name (pre-normalization)
    pub aliases: Vec<String>,       // initial aliases (may be empty)
    pub attributes_blob: Vec<u8>,   // opaque attributes; may be empty
    pub request_id: WireUuid,       // idempotency key (substrate §09/02 §4)
}
```

Semantics:

1. The server normalizes `canonical_name` (lowercase + whitespace collapse, per [`../18_entities/02_storage.md`](../18_entities/02_storage.md)) and uses it for the exact-name index.
2. A fresh `EntityId` (UUIDv7) is allocated by the server. Clients **must not** supply one.
3. `attributes_blob` is stored verbatim and not interpreted by the wire layer. Phase 19's schema DSL validates contents before commit.
4. Aliases are stored verbatim; normalized forms feed the alias index.
5. `request_id` participates in the substrate's idempotency cache (24h TTL, spec §09/02 §4). Resubmitting with the same `request_id` and identical params returns the cached response; identical id with different params → `ENTITY_AMBIGUOUS` error.

Validation rules: see [`./04_validation.md`](./04_validation.md) §2.

### 3.2 Response body — `EntityCreateResponse`

```rust
pub struct EntityCreateResponse {
    pub entity_id: WireUuid,        // freshly allocated EntityId
}
```

### 3.3 Error responses

The server returns an ERROR frame (opcode `0x00FF`) with one of:

- `ENTITY_TYPE_MISMATCH` (`0x31`) — `entity_type_id` not registered.
- `DUPLICATE_CANONICAL_NAME` (mapped via substrate `Conflict`) — `(entity_type_id, normalized_name)` already exists.
- `INVALID_ARGUMENT` (substrate `0x40`) — empty / oversized name, oversized attributes, alias-count exceeds cap.
- `IDEMPOTENCY_CONFLICT` (substrate `Conflict`) — same `request_id` with different params.

See [`./03_errors.md`](./03_errors.md) for the complete mapping.

### 3.4 Example

```text
C → S  frame: opcode=0x0130 stream_id=1 EOS
       payload: rkyv(EntityCreateRequest {
           entity_type_id: 1,                 // Person
           canonical_name: "Priya Patel",
           aliases: vec!["Priya", "P. Patel"],
           attributes_blob: vec![],            // none
           request_id: <UUIDv7>,
       })
S → C  frame: opcode=0x01B0 stream_id=1 EOS
       payload: rkyv(EntityCreateResponse {
           entity_id: <fresh UUIDv7>,
       })
```

## 4. ENTITY_GET (0x0131)

### 4.1 Request body — `EntityGetRequest`

```rust
pub struct EntityGetRequest {
    pub entity_id: WireUuid,
}
```

### 4.2 Response body — `EntityGetResponse`

```rust
pub struct EntityGetResponse {
    pub entity: EntityView,
}
```

`EntityView` is defined in §12 (read-side projection of `brain_core::Entity`).

### 4.3 Error responses

- `ENTITY_NOT_FOUND` (`0x30`) — no row with that id.
- Note: tombstoned entities are returned with `flags & TOMBSTONED != 0`. Merged entities are returned with `merged_into != [0; 16]`; redirection to the survivor is the **client's** responsibility. (Phase 16.7's wider response semantics may add auto-redirect; for now `ENTITY_GET` is faithful.)

### 4.4 No idempotency

`ENTITY_GET` is a read; it carries no `request_id` and is not cached by the idempotency layer.

## 5. ENTITY_UPDATE (0x0132)

### 5.1 Request body — `EntityUpdateRequest`

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

1. **Replace-not-merge for `aliases` and `attributes_blob`**. The handler reads the current row, replaces these fields, and writes back. Phase 16.7+ may introduce delta-encoded variants; the v1 shape is full-replace for simplicity.
2. If `canonical_name` differs from the current row's, the handler triggers the rename path internally (old name moves into `aliases`, `embedding_version` bumps, exact-name index is rewritten). Equivalent to `ENTITY_RENAME` with `move_to_alias = true`.
3. `entity_type` is **not mutable** via `ENTITY_UPDATE`. A future `RETYPE_ENTITY` opcode (phase 18+) handles type changes.
4. `updated_at_unix_nanos` is set to the server's clock.
5. Idempotency via `request_id`.

### 5.2 Response body — `EntityUpdateResponse`

```rust
pub struct EntityUpdateResponse {
    pub entity: EntityView,         // post-update view (avoids a follow-up GET)
}
```

### 5.3 Error responses

- `ENTITY_NOT_FOUND` (`0x30`)
- `DUPLICATE_CANONICAL_NAME` — if the rename component would collide with an existing entity of the same type.
- `INVALID_ARGUMENT` — empty / oversized fields.
- `IDEMPOTENCY_CONFLICT`.

## 6. ENTITY_RENAME (0x0133)

### 6.1 Request body — `EntityRenameRequest`

```rust
pub struct EntityRenameRequest {
    pub entity_id: WireUuid,
    pub new_canonical_name: String,
    pub move_to_alias: bool,        // spec §18/00 default = true
    pub request_id: WireUuid,
}
```

Semantics:

1. Strictly a name change. Attributes and existing aliases are unchanged.
2. If `move_to_alias = true`, the **old** canonical name is appended to the alias list (deduplicated by normalized form).
3. `embedding_version` is bumped so the embedding worker (phase 21) re-embeds.
4. **Phase 16.7c constraint:** the handler currently rejects `move_to_alias = false` with `INVALID_ARGUMENT`. The flag is wire-stable for forward compat; a "no-trail" rename mode lands in a later phase.

### 6.2 Response body — `EntityRenameResponse`

```rust
pub struct EntityRenameResponse {
    pub entity: EntityView,         // post-rename view
}
```

### 6.3 Error responses

- `ENTITY_NOT_FOUND` (`0x30`)
- `DUPLICATE_CANONICAL_NAME` — `new_canonical_name` collides under the same type.
- `INVALID_ARGUMENT` — empty name, name too long, or `move_to_alias=false` (currently unsupported).

## 7. ENTITY_MERGE (0x0134) — spec-only

### 7.1 Request body — `EntityMergeRequest`

```rust
pub struct EntityMergeRequest {
    pub survivor: WireUuid,         // entity that absorbs the merged
    pub merged: WireUuid,           // entity that gets redirected
    pub confidence: f32,            // [0.0, 1.0]; ≥0.95 = autonomous, [0.7,0.95) = needs review
    pub reason: String,             // human-readable; stored in audit
    pub request_id: WireUuid,
}
```

Spec semantics: see [`../18_entities/00_purpose.md`](../18_entities/00_purpose.md) §"Merging entities" — `merged.merged_into = Some(survivor)`, aliases / attributes folded with conflict rules, all statements / relations re-routed inside one redb transaction, audit record written, `MERGED` event emitted on SUBSCRIBE.

### 7.2 Response body — `EntityMergeResponse`

```rust
pub struct EntityMergeResponse {
    pub audit_id: WireUuid,         // ENTITY_RESOLUTION_AUDIT row id
    pub grace_period_seconds: u64,  // how long UNMERGE can still reverse this
}
```

### 7.3 Error responses

- `ENTITY_NOT_FOUND` — either id missing.
- `ENTITY_MERGE_CONFLICT` (`0x33`) — `survivor` and `merged` are the same entity, or `merged` is already merged into a third entity.
- `INVALID_ARGUMENT` — `confidence` outside `[0.0, 1.0]`, `reason` too long.

### 7.4 Open questions

- Cross-type merges (Person ↔ Organization): forbidden by default, or allowed with attribute drop? See [`./09_open_questions.md`](./09_open_questions.md).
- Should the grace period be returned absolute (unix nanos) or relative (seconds)? Currently relative.

## 8. ENTITY_UNMERGE (0x0135) — spec-only

### 8.1 Request body — `EntityUnmergeRequest`

```rust
pub struct EntityUnmergeRequest {
    pub merged_entity: WireUuid,    // the entity that was merged
    pub request_id: WireUuid,
}
```

Reverses a recent merge by clearing `merged_into`, splitting back the contributed aliases / attributes (from the merge audit's recorded delta), and re-routing statements / relations whose audit trail attributes them to the original merged entity.

Time-bound: only valid within the merge audit's `grace_period_seconds`. After that, the redirect is permanent and `UNMERGE` returns `ENTITY_MERGE_CONFLICT`.

### 8.2 Response body — `EntityUnmergeResponse`

```rust
pub struct EntityUnmergeResponse {
    pub restored_entity_id: WireUuid,
}
```

### 8.3 Error responses

- `ENTITY_NOT_FOUND` — `merged_entity` doesn't exist or was never merged.
- `ENTITY_MERGE_CONFLICT` — grace period expired, or `survivor` has been merged further since.

## 9. ENTITY_RESOLVE (0x0136) — spec-only

Exposes the [phase 16.5 resolver](../../crates/brain-core/src/knowledge/resolver.rs) over the wire so SDK clients can run resolution without re-implementing the tier ladder.

### 9.1 Request body — `EntityResolveRequest`

```rust
pub struct EntityResolveRequest {
    pub candidate_name: String,
    pub context: String,            // surrounding text (≤ 100 chars consumed)
    pub entity_type_hint: u32,      // 0 = no hint; otherwise an EntityTypeId
    pub allow_create: bool,         // if true, tier 5 creates a fresh entity
    pub request_id: WireUuid,
}
```

### 9.2 Response body — `EntityResolveResponse`

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

### 9.3 Error responses

- `INVALID_ARGUMENT` — empty `candidate_name`, oversized `context`.
- `SCHEMA_NOT_DECLARED` (substrate `0x21` for now; §28-specific code possible) — if no schema declared (resolver currently requires the entity_type registry seeded).

## 10. ENTITY_LIST (0x0137) — spec-only

Paginated scan over the entity table. Cheap for small deployments; phase 23's hybrid query is the better path for production-sized graphs.

### 10.1 Request body — `EntityListRequest`

```rust
pub struct EntityListRequest {
    pub entity_type_id: u32,        // 0 = no filter
    pub name_prefix: String,        // "" = no filter; normalized server-side
    pub mention_count_min: u32,     // 0 = no filter
    pub include_tombstoned: bool,
    pub include_merged: bool,
    pub limit: u32,                 // max results (capped at 1000; see §11)
    pub cursor: Vec<u8>,            // opaque continuation token; empty on first page
}
```

### 10.2 Response body — `EntityListResponse`

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

The frame layout mirrors substrate `RECALL_RESP` — see [`../03_wire_protocol/09_streaming.md`](../03_wire_protocol/09_streaming.md).

### 10.3 Error responses

- `INVALID_ARGUMENT` — `limit` > 1000, malformed cursor.

## 11. ENTITY_TOMBSTONE (0x0138) — spec-only

### 11.1 Request body — `EntityTombstoneRequest`

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

### 11.2 Response body — `EntityTombstoneResponse`

```rust
pub struct EntityTombstoneResponse {
    pub tombstoned_at_unix_nanos: u64,
}
```

### 11.3 Error responses

- `ENTITY_NOT_FOUND` (`0x30`)
- already-tombstoned entities return success (idempotent).

## 12. `EntityView` — shared read-side projection

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
    pub merged_into: WireUuid,              // [0;16] when not merged (see §1.2)
    pub embedding_version: u32,
    pub flags: u32,                         // bit 0 = TOMBSTONED, bit 1 = MERGED, …
}
```

Field semantics mirror `brain_core::Entity` ([§18](../18_entities/00_purpose.md) §"Field semantics"). One projection used by `GET`, `UPDATE`, `RENAME`, and `LIST` to avoid divergent response shapes.

### 12.1 What `EntityView` deliberately omits

- The raw embedding bytes. Clients that need the embedding query the entity HNSW directly via `RECALL_HYBRID` (phase 23) or `ADMIN_GET_AUDIT`-style debug paths.
- Reference counts to specific statements / relations. Use `STATEMENT_LIST` / `RELATION_LIST_FROM`.

## 13. Idempotency cache key

For every opcode in this section that carries a `request_id`, the substrate's idempotency layer keys on:

```
(agent_id, opcode_u16, request_id, blake3(payload_bytes))
```

(Same shape as substrate, with the knowledge opcode taking the same `Opcode` slot.) TTL is 24h. Stored responses include the full frame bytes so a duplicate hit is byte-identical, EOS-flag and all.

## 14. Compatibility note

The wire shapes in §3–§6 are **implemented as of phase 16.6c** and have round-trip rkyv tests in `crates/brain-protocol/src/knowledge/entity_req.rs`. The shapes in §7–§11 are **spec-only**; their Rust counterparts land in phases 16.7–16.9 and may be refined during implementation. Refinements must update this file before code lands, per [[spec-first-workflow]].
