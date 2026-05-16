# 18.7 — Relation handlers + event emission

7 wire handlers wiring `RequestBody::Relation*` (18.6) to
`brain_metadata::relation_ops` (18.4) + `relation_traversal::traverse`
(18.5). Replaces the `NotYetImplemented` stubs in dispatch.rs.

Mirrors 17.7 structure exactly.

## Spec refs

- `spec/20_relations/00_purpose.md` — handler invariants.
- `spec/28_knowledge_wire_protocol/07_relation_frames.md` — wire
  semantics + error mapping per opcode.
- `spec/28_knowledge_wire_protocol/02_subscribe_events.md` §3.3 —
  RelationCreated / Superseded / Tombstoned event shapes.

## Reads-only

- `crates/brain-ops/src/ops/knowledge_statement.rs` — closest
  precedent.
- `crates/brain-ops/src/ops/knowledge_entity.rs` — `emit_knowledge_event`
  helper (already `pub(crate)`).
- `crates/brain-protocol/src/knowledge/relation_{req,resp}.rs` —
  wire shapes.
- `crates/brain-metadata/src/relation_ops.rs` — `relation_create` /
  `_supersede` / `_tombstone` / `_list_from` / `_list_to` /
  `_history`.
- `crates/brain-metadata/src/relation_traversal.rs` — `traverse`.

## Key decisions

### D1 — New `ops/knowledge_relation.rs` module

7 handler functions + `map_relation_op_error` + `project_view`
(resolves `RelationTypeId` → canonical `"ns:name"` string).

### D2 — `relation_type_lookup_by_qname_wtxn` helper

Mirrors `predicate_lookup_by_qname_wtxn` from 17.7 — `wtxn`-based
qname → `RelationTypeId` lookup so create/supersede can
validate-and-write atomically inside one txn.

### D3 — Emit `RelationCreated / Superseded / Tombstoned` events

- CREATE: emits `RelationCreated`. When cardinality auto-supersede
  fires (relation_create returns a successor id), also emit
  `RelationSuperseded` for the old → new transition.
- SUPERSEDE: emits `RelationSuperseded`.
- TOMBSTONE: emits `RelationTombstoned`.
- GET / LIST / TRAVERSE: read-only; no events.

### D4 — TRAVERSE wire request → traversal config

Direction byte: 0 = Outgoing / 1 = Incoming / 2 = Both. `max_depth`
+ `max_nodes` clamped server-side.

### D5 — Type-filter resolution at TRAVERSE start

`relation_types: Vec<String>` (qnames) → resolved to
`Vec<RelationTypeId>` via `relation_type_lookup_by_qname` (read
txn). Unknown qname → `InvalidArgument`.

### D6 — View projection

`RelationView::from_relation(r, qname)` from 18.6. Handler resolves
the type qname via `relation_type_get` for every projection.

## Plan

### Step 1 — New module

`crates/brain-ops/src/ops/knowledge_relation.rs`. ~700 lines.

7 handlers:

```rust
pub async fn handle_relation_create(req, ctx) -> Result<RelationCreateResponse, OpError>;
pub async fn handle_relation_get(req, ctx) -> Result<RelationGetResponse, OpError>;
pub async fn handle_relation_supersede(req, ctx) -> Result<RelationSupersedeResponse, OpError>;
pub async fn handle_relation_tombstone(req, ctx) -> Result<RelationTombstoneResponse, OpError>;
pub async fn handle_relation_list_from(req, ctx) -> Result<RelationListFromResponseFrame, OpError>;
pub async fn handle_relation_list_to(req, ctx) -> Result<RelationListToResponseFrame, OpError>;
pub async fn handle_relation_traverse(req, ctx) -> Result<RelationTraverseResponseFrame, OpError>;
```

### Step 2 — `map_relation_op_error`

Mirror `map_statement_op_error`. Mapping:
- `NotFound` → `OpError::NotFound { what: "relation", ... }`.
- `AlreadyExists` → `OpError::Conflict`.
- `UnknownRelationType` → `OpError::InvalidRequest`.
- `UnknownEntity` → `OpError::NotFound { what: "entity", ... }`.
- `InvalidArgument` → `OpError::InvalidRequest`.
- `AlreadySuperseded / AlreadyTombstoned / TypeMismatch /
  EndpointMismatch / CardinalityViolation` → `OpError::Conflict`.
- `DecodeFailed` → `OpError::Internal`.
- `Storage / Table` → `OpError::Internal`.
- `RelationTypeOp / EntityOp` → unwrap and re-map.

### Step 3 — Dispatch routing

Replace 7 `NotYetImplemented` arms with routed calls.

### Step 4 — Re-exports

`brain-ops/src/lib.rs` adds `knowledge_relation` to the `pub use ops::{...}`
list.

## Verify

```
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu -p brain-ops --all-targets -- -D warnings
```

## Out of scope

- Integration tests (18.9a).
- SDK builders (18.8).
- Bench / ROADMAP / phase exit (18.9b).
- Streaming TRAVERSE (phase 23).
