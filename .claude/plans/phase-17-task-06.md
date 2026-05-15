# 17.6 — Wire structs + opcode dispatch (0x0140–0x0146)

Lands the rkyv-archived request / response payloads for the seven
statement opcodes, plus the `Opcode` enum entries and `RequestBody`
dispatch wiring. Spec §28/06 (334 lines) is fully detailed; this
sub-task is a careful translation, not a design step.

Mirrors the structure of phase 16.6c (entity CRUD wire) + 16.7.3
(merge / unmerge / list / tombstone wire); no new patterns invented.

## Spec refs

- `spec/28_knowledge_wire_protocol/06_statement_frames.md` — the
  spec for every struct + opcode in this sub-task.
- `spec/28_knowledge_wire_protocol/03_errors.md` — error code
  mapping (already extended by 16.7.4 with statement-layer codes).
- `spec/28_knowledge_wire_protocol/04_validation.md` — field caps:
  evidence ≤ 8 inline, predicate qname ≤ 96 chars, blob ≤ 64 KiB.
- `spec/19_statements/00_purpose.md` — value-side semantics behind
  each wire field.

## Reads-only files

- `crates/brain-protocol/src/opcode.rs` — extend with 14 new enum
  variants + 14 new `from_u16` arms.
- `crates/brain-protocol/src/request.rs` — extend `RequestBody`.
- `crates/brain-protocol/src/knowledge/entity_req.rs` — entity-side
  precedent (the closest pattern).
- `crates/brain-protocol/src/knowledge/entity_resp.rs` — entity-side
  view-struct precedent.
- `crates/brain-core/src/knowledge/statement.rs` — value types from
  17.2 that the wire types convert to/from.

## Key design decisions

### D1 — Two new files, mirroring entity layout

- `crates/brain-protocol/src/knowledge/statement_req.rs` — 7 request
  structs + 4 shared types (`StatementKindWire`,
  `StatementObjectWire`, `StatementValueWire`, `EvidenceRefWire`).
- `crates/brain-protocol/src/knowledge/statement_resp.rs` —
  `StatementView` + 7 response structs + the streaming-frame shape
  for `STATEMENT_LIST` / `STATEMENT_HISTORY`.

### D2 — Shared types live in `statement_req.rs`

`StatementKindWire`, `StatementObjectWire`, `StatementValueWire`,
`EvidenceRefWire` are used by both request structs (in CREATE) and
the response `StatementView`. Putting them in `statement_req.rs` keeps
the request side self-contained and matches how `EntityListItem`
(used by responses) lives next to other response types — locality
beats theoretical reuse here. Re-exports surface them at the
`crate::knowledge` level so consumers needn't care about the file
split.

### D3 — `StatementListResponseFrame` mirrors `EntityListResponseFrame`

Single-frame snapshot per the master phase-17 plan (cursor pagination +
true streaming deferred to phase 23 per §28/06 §9.2 — same convention
as `ENTITY_LIST`). The frame carries:

```rust
pub struct StatementListResponseFrame {
    pub items: Vec<StatementView>,
    pub next_cursor: Vec<u8>,
    pub cumulative_count: u32,
    pub is_final: bool,
}
```

Phase 17.7 emits a single frame with `is_final = true`. Phase 23
splits into per-batch streaming.

### D4 — `STATEMENT_HISTORY` shares the same response shape

Per spec §28/06 §8.2 the wire defines `StatementHistoryItem` +
`StatementHistoryTail`. In line with 17.7's deferral to single-frame
snapshot, we collapse to one `StatementHistoryResponseFrame` carrying:

```rust
pub struct StatementHistoryResponseFrame {
    pub items: Vec<StatementView>,    // chain entries in version order
    pub chain_root: WireUuid,
    pub total_versions: u32,
    pub is_final: bool,
}
```

The two-shape distinction (Item vs Tail) is preserved at the spec
level but conflated for v1 single-frame implementations. Phase 23
adopts the split when it streams.

### D5 — Conversion helpers stay in `statement_resp.rs`

`StatementView` ↔ `brain_core::knowledge::Statement` projection lives
next to `StatementView` itself. The conversion needs `StatementObject`
→ `StatementObjectWire` (and vice-versa); those helpers live alongside
the wire enum.

`predicate` over the wire is the canonical `"namespace:name"` string;
the server-side handler (17.7) calls `predicate_lookup_by_qname` to
resolve to `PredicateId` before delegating to `statement_create`.
This wire-only sub-task does NOT introduce a new dependency from
`brain-protocol` on `brain-metadata`.

### D6 — `StatementObjectWire` discriminants are stable

Spec §28/06 §2.2 lists Entity / Value / Memory / Statement (rkyv
auto-numbers, but we make discriminants explicit with `#[repr(u8)]`
on the value-blob variants for forward compat). Helpers:

```rust
impl StatementObjectWire {
    pub fn discriminant(&self) -> u8 { ... }
}
```

Same indirection inside `decode_object` (the brain-metadata side)
uses `discriminant + 1` for "0 = unset / 1..=4 = variant". Wire
discriminants stay `0..=3` (rkyv tags). Conversion helpers handle
the offset.

### D7 — Caller-supplied vs server-allocated `StatementId`

Spec §28/06 §3.1: `STATEMENT_CREATE` request **does not** carry the
StatementId — the server allocates. The handler (17.7) calls
`StatementId::new()` after validation. Wire struct therefore omits
this field.

This differs from `brain_core::knowledge::Statement` which requires
the id to be set (the metadata layer allocates via `StatementId::new()`
in the handler). 17.6 documents the asymmetry.

### D8 — Sentinel zero pattern for optional `WireUuid` / timestamps

Same convention as `EntityView` (16.6c): `[0; 16]` means "absent".
`StatementView.subject_pending_audit_id == [0; 16]` ⇔ subject is a
resolved entity. `StatementView.superseded_by == [0; 16]` ⇔ root of
chain. All `_unix_nanos` fields use `0` for absent (no risk of
collision — UNIX nanos = 0 is 1970-01-01T00:00:00Z, not a realistic
value for this system).

## Plan

### Step 1 — Extend `Opcode` enum

Add 14 entries to `crates/brain-protocol/src/opcode.rs` (7 requests +
7 responses) under the `0x0140–0x0146` / `0x01C0–0x01C6` ranges:

```rust
StatementCreateReq    = 0x0140,  StatementCreateResp    = 0x01C0,
StatementGetReq       = 0x0141,  StatementGetResp       = 0x01C1,
StatementSupersedeReq = 0x0142,  StatementSupersedeResp = 0x01C2,
StatementTombstoneReq = 0x0143,  StatementTombstoneResp = 0x01C3,
StatementRetractReq   = 0x0144,  StatementRetractResp   = 0x01C4,
StatementHistoryReq   = 0x0145,  StatementHistoryResp   = 0x01C5,
StatementListReq      = 0x0146,  StatementListResp      = 0x01C6,
```

Plus the `from_u16` arms. Keep the namespace comments accurate.

### Step 2 — Write `statement_req.rs`

```rust
//! Statement-op request payloads. Spec §28/06.

use rkyv::{Archive, Deserialize, Serialize};
use crate::request::WireUuid;

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum StatementKindWire {
    Fact = 1,
    Preference = 2,
    Event = 3,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum StatementValueWire {
    Text(String),
    Integer(i64),
    Float(f64),
    Bool(bool),
    UnixNanos(u64),
    Blob(Vec<u8>),
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum StatementObjectWire {
    EntityRef(WireUuid),
    Value(StatementValueWire),
    MemoryRef([u8; 16]),  // raw 128-bit MemoryId
    StatementRef(WireUuid),
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum EvidenceRefWire {
    Inline(Vec<[u8; 16]>),    // up to 8 MemoryIds; reject otherwise
    Overflow(WireUuid),       // EvidenceOverflowId
}
```

Then the 7 request structs:

- `StatementCreateRequest` per spec §28/06 §3.1 (no statement_id;
  server-allocated).
- `StatementGetRequest` per §4.1.
- `StatementSupersedeRequest` per §5.1 — carries `old_statement_id`
  + an **embedded** `StatementCreateRequest` for the new statement.
- `StatementTombstoneRequest` per §6.1.
- `StatementRetractRequest` per §7.1.
- `StatementHistoryRequest` per §8.1.
- `StatementListRequest` per §9.1.

Each carries a `request_id: WireUuid` for idempotency (substrate
convention).

### Step 3 — Write `statement_resp.rs`

```rust
//! Statement-op response payloads. Spec §28/06.

pub struct StatementView { /* per §2.4 — 19 fields */ }

pub struct StatementCreateResponse {
    pub statement_id: WireUuid,
    pub auto_superseded: WireUuid,  // [0;16] unless auto-supersede fired
    pub chain_root: WireUuid,
}

pub struct StatementGetResponse {
    pub statement: StatementView,
    pub returned_via_supersession: bool,
}

pub struct StatementSupersedeResponse {
    pub new_statement_id: WireUuid,
    pub chain_root: WireUuid,
    pub version: u32,
}

pub struct StatementTombstoneResponse {
    pub tombstoned_at_unix_nanos: u64,
}

pub struct StatementRetractResponse {
    pub retracted_at_unix_nanos: u64,
    pub will_zero_at_unix_nanos: u64,
}

pub struct StatementHistoryResponseFrame {
    pub items: Vec<StatementView>,
    pub chain_root: WireUuid,
    pub total_versions: u32,
    pub is_final: bool,
}

pub struct StatementListResponseFrame {
    pub items: Vec<StatementView>,
    pub next_cursor: Vec<u8>,
    pub cumulative_count: u32,
    pub is_final: bool,
}
```

### Step 4 — Conversion helpers (alongside `StatementView`)

In `statement_resp.rs`:

```rust
impl StatementView {
    pub fn from_statement(s: &Statement, predicate_qname: &str) -> Self;
    pub fn to_statement(&self) -> Result<Statement, WireToStatementError>;
}

pub fn statement_object_to_wire(o: &StatementObject) -> StatementObjectWire;
pub fn statement_object_from_wire(w: &StatementObjectWire) -> StatementObject;

pub fn evidence_ref_to_wire(e: &EvidenceRef) -> EvidenceRefWire;
pub fn evidence_ref_from_wire(w: &EvidenceRefWire) -> EvidenceRef;
```

`predicate` enters the wire as `"namespace:name"` (a string), so
`StatementView::to_statement` needs an extra arg — the resolved
`PredicateId`. Defer the resolution to the handler (17.7):

```rust
pub fn to_statement(&self, predicate: PredicateId) -> Result<Statement, WireToStatementError>;
```

Errors covered: object/value variant mismatch, evidence cap exceeded.

### Step 5 — Add to `RequestBody` enum + dispatch

In `crates/brain-protocol/src/request.rs`:

```rust
StatementCreate(crate::knowledge::StatementCreateRequest),
StatementGet(crate::knowledge::StatementGetRequest),
StatementSupersede(crate::knowledge::StatementSupersedeRequest),
StatementTombstone(crate::knowledge::StatementTombstoneRequest),
StatementRetract(crate::knowledge::StatementRetractRequest),
StatementHistory(crate::knowledge::StatementHistoryRequest),
StatementList(crate::knowledge::StatementListRequest),
```

Plus matching arms in `opcode()`, `encode()`, `decode()` — same
pattern as the 9 entity variants already present.

### Step 6 — Re-exports

`crates/brain-protocol/src/knowledge/mod.rs`:

```rust
pub mod statement_req;
pub mod statement_resp;

pub use statement_req::{
    EvidenceRefWire, StatementCreateRequest, StatementGetRequest,
    StatementHistoryRequest, StatementKindWire, StatementListRequest,
    StatementObjectWire, StatementRetractRequest, StatementSupersedeRequest,
    StatementTombstoneRequest, StatementValueWire,
};
pub use statement_resp::{
    statement_object_from_wire, statement_object_to_wire,
    evidence_ref_from_wire, evidence_ref_to_wire,
    StatementCreateResponse, StatementGetResponse, StatementHistoryResponseFrame,
    StatementListResponseFrame, StatementRetractResponse, StatementSupersedeResponse,
    StatementTombstoneResponse, StatementView, WireToStatementError,
};
```

### Step 7 — Tests

`crates/brain-protocol/tests/knowledge_statement_wire.rs` (new). Per-
op round-trip tests using `request_body_round_trips`:

- `statement_create_round_trip` — all object variants (Entity, Value
  × 6 sub-variants, Memory, Statement).
- `statement_get_round_trip` (with / without follow_supersession).
- `statement_supersede_round_trip` — embedded create payload.
- `statement_tombstone_round_trip`.
- `statement_retract_round_trip`.
- `statement_history_round_trip`.
- `statement_list_round_trip` — all filter fields populated +
  empty-filter case.

Plus conversion-helper tests in `statement_resp.rs::tests`:

- `view_from_statement_round_trips` — every `StatementObject` variant.
- `view_to_statement_preserves_chain_fields`.
- `wire_to_value_rejects_unknown_variant`.

### Step 8 — Verify

```
cargo test -p brain-protocol knowledge_statement
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy -p brain-protocol --all-targets -- -D warnings
```

## Files written

| Path | Change |
|---|---|
| `crates/brain-protocol/src/opcode.rs` | +14 enum variants, +14 `from_u16` arms. |
| `crates/brain-protocol/src/request.rs` | +7 `RequestBody` variants + dispatch. |
| `crates/brain-protocol/src/knowledge/mod.rs` | +2 sub-modules + re-exports. |
| `crates/brain-protocol/src/knowledge/statement_req.rs` | New. 7 requests + 4 shared types. |
| `crates/brain-protocol/src/knowledge/statement_resp.rs` | New. View + 7 responses + conversion helpers. |
| `crates/brain-protocol/tests/knowledge_statement_wire.rs` | New. ~10 round-trip tests. |

## Files NOT written this sub-task

- Handlers (17.7) — wire structs only.
- Event payload additions — `StatementCreatedEvent` etc. already
  live in `events.rs` from 16.7.4.
- SDK builders (17.8).
- Multi-frame streaming for LIST/HISTORY — phase 23.
- Predicate-string parse helpers in handler (lives in handler).

## Verification gate

- `cargo test -p brain-protocol` all green (round-trip + conversion).
- `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests`
  clean (workspace consumers don't see the new variants but link).
- `cargo clippy -p brain-protocol --all-targets -- -D warnings` clean.

## Commit message draft

```
feat(brain-protocol): statement wire ops 0x0140-0x0146 (17.6)

Adds 7 statement opcodes per spec §28/06:
- STATEMENT_CREATE / GET / SUPERSEDE / TOMBSTONE / RETRACT / HISTORY
  / LIST (request + response, 14 new enum variants).

Two new files in crates/brain-protocol/src/knowledge:
- statement_req.rs: 7 request structs + 4 shared types
  (StatementKindWire, StatementObjectWire, StatementValueWire,
  EvidenceRefWire). All rkyv-archivable with check_bytes.
- statement_resp.rs: 19-field StatementView + 7 response structs +
  conversion helpers between brain-core's Statement and the wire
  shape. LIST + HISTORY use single-frame snapshot shapes mirroring
  the entity-side EntityListResponseFrame (cursor pagination + true
  streaming deferred to phase 23 per §28/06 §9.2).

Spec-side caveats made explicit:
- StatementCreateRequest does NOT carry statement_id; the server
  allocates per §28/06 §3.1.
- predicate enters the wire as canonical "namespace:name" string;
  handler (17.7) resolves to PredicateId via predicate_lookup_by_qname.
- StatementSupersedeRequest embeds the new StatementCreateRequest;
  server runs create + link in one atomic txn.

10 round-trip tests cover every request, every StatementObject /
StatementValue variant, and Statement ↔ StatementView projection.

Plan: .claude/plans/phase-17-task-06.md.
```

## Risks

- **rkyv enum with check_bytes** — the entity-side `RecallView`
  pattern (substrate `RecallResponseFrame`) demonstrated this works
  cleanly. `StatementObjectWire` is a 4-variant enum with payloads;
  rkyv handles it via discriminant byte + per-variant layout. If
  archive derive fails on `Vec<u8>` blob > 64 KiB, we cap at the
  validation layer (handler), not the wire layer.
- **`StatementSupersedeRequest` embeds `StatementCreateRequest`** —
  archives a struct inside a struct. Works (rkyv archives are
  composable); already done in `EntityMergeRequest`-style patterns.
  Round-trip tests verify.
- **`StatementView`'s 19 fields** make it the largest wire struct in
  the project. Field order matches spec §28/06 §2.4 for review
  ergonomics; rkyv archive layout is alignment-driven anyway so
  source order doesn't affect on-wire bytes.
- **Predicate qname over the wire is a copy** — adds ~20 bytes per
  StatementView vs sending the u32 id. Acceptable for v1; phase 22's
  schema upload step lets clients cache the predicate registry.

## Out of scope (this sub-task)

- Handler implementation (17.7).
- SDK builders (17.8).
- Event payload additions (already done in 16.7.4).
- Statement-Linked HNSW search wire opcode (phase 23 hybrid query).
- Confidence aggregation (17.9).
- Cursor pagination format for LIST (phase 23).
