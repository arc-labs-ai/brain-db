# 17.7 — Statement handlers + event emission

7 wire handlers wiring `RequestBody::Statement*` (17.6) to
`brain_metadata::statement_ops` (17.4), plus subscription-event
emission per spec §28/02 §3.2. Replaces the
`NotYetImplemented("statement op — Phase 17.7")` stubs in
`brain-ops/src/dispatch.rs`.

## Spec refs

- `spec/19_statements/00_purpose.md` — handler invariants.
- `spec/28_knowledge_wire_protocol/06_statement_frames.md` — wire
  semantics + error mapping per opcode.
- `spec/28_knowledge_wire_protocol/02_subscribe_events.md` §3.2 —
  StatementCreated / Superseded / Tombstoned event shapes.
- `spec/28_knowledge_wire_protocol/03_errors.md` Strategy B —
  StatementOpError → OpError mapping.

## Reads-only files (patterns to clone)

- `crates/brain-ops/src/ops/knowledge_entity.rs` — per-handler
  shape, `emit_knowledge_event` helper, `map_entity_op_error`
  precedent.
- `crates/brain-metadata/src/statement_ops.rs` — the underlying ops.
- `crates/brain-protocol/src/knowledge/statement_{req,resp}.rs` —
  request / response shapes + conversion helpers.

## Plan

### Step 1 — New module `ops/knowledge_statement.rs`

7 handler functions + `map_statement_op_error` + `statement_to_view`
helper that calls `predicate_get` to resolve `PredicateId → "ns:name"`
canonical string for the wire view.

```rust
pub async fn handle_statement_create(req, ctx) -> Result<StatementCreateResponse, OpError>;
pub async fn handle_statement_get(req, ctx) -> Result<StatementGetResponse, OpError>;
pub async fn handle_statement_supersede(req, ctx) -> Result<StatementSupersedeResponse, OpError>;
pub async fn handle_statement_tombstone(req, ctx) -> Result<StatementTombstoneResponse, OpError>;
pub async fn handle_statement_retract(req, ctx) -> Result<StatementRetractResponse, OpError>;
pub async fn handle_statement_history(req, ctx) -> Result<StatementHistoryResponseFrame, OpError>;
pub async fn handle_statement_list(req, ctx) -> Result<StatementListResponseFrame, OpError>;
```

Each handler:
1. Wire-side validation (predicate non-empty, blob caps, limit ≤ 1000).
2. Acquire `MetadataDb` lock; open txn (read for GET/HISTORY/LIST,
   write for the others).
3. Resolve predicate qname → `PredicateId` via
   `predicate_lookup_by_qname`. Empty/unknown → `INVALID_ARGUMENT`.
4. Project wire request → brain-core `Statement` (CREATE / SUPERSEDE).
5. Call into `statement_ops::*`.
6. Commit (write txns).
7. Emit post-commit subscription event (CREATE / SUPERSEDE / TOMBSTONE
   only; GET / RETRACT / HISTORY / LIST don't emit events in v1).
8. Project storage result → wire response.

### Step 2 — Move/share `emit_knowledge_event`

The helper lives in `knowledge_entity.rs` as a `fn` (private). Pull
it up to `ops/mod.rs` or a small `ops/knowledge_common.rs` so both
handler modules call the same function. Tracked as a small refactor;
no behaviour change.

### Step 3 — `map_statement_op_error`

Mirror `map_entity_op_error`. Mapping:
- `NotFound` → `OpError::NotFound { what: "statement", ... }`.
- `AlreadyExists` → `OpError::Conflict`.
- `UnknownPredicate / UnknownSubject` → `OpError::NotFound`.
- `InvalidArgument` → `OpError::InvalidRequest`.
- `AlreadySuperseded / AlreadyTombstoned / EventCannotSupersede /
  KindMismatch / SubjectMismatch / PredicateMismatch` → `OpError::Conflict`.
- `DecodeFailed` → `OpError::Internal`.
- `Storage / Table` → `OpError::Internal`.
- `PredicateOp / EntityOp` → unwrap and re-map.

### Step 4 — Dispatch routing

Replace the `NotYetImplemented` arm in `brain-ops/src/dispatch.rs`
with 7 `match` arms routing each `Statement*` variant to its handler
and mapping into the matching `ResponseBody::Statement*`.

### Step 5 — Tests

New file `crates/brain-protocol/tests/...` — actually
`crates/brain-server/tests/knowledge_statement_wire.rs`. Mirrors
`knowledge_entity_wire.rs`. Smoke tests over the dispatch:

- `create_fact_round_trips` — create + get returns the same view.
- `create_event_requires_event_at` — `INVALID_ARGUMENT`.
- `create_preference_auto_supersedes` — second create on same
  `(subject, predicate)` returns `auto_superseded != [0;16]`.
- `create_unknown_predicate_rejected`.
- `supersede_explicit_fact` — chain version bumps.
- `tombstone_then_get_shows_tombstoned`.
- `history_walks_chain`.
- `list_subject_filter`.

Subscribe-event assertion lives in a follow-up integration test
(deferred to 17.10).

## Files written

| Path | Change |
|---|---|
| `crates/brain-ops/src/ops/knowledge_statement.rs` | New. 7 handlers + helpers + ~10 unit tests. |
| `crates/brain-ops/src/ops/mod.rs` | Register module. |
| `crates/brain-ops/src/lib.rs` | Re-export. |
| `crates/brain-ops/src/dispatch.rs` | Replace `NotYetImplemented` stub with 7 routed arms. |
| `crates/brain-ops/src/ops/knowledge_entity.rs` | Maybe extract `emit_knowledge_event` to a shared location. |
| `crates/brain-server/tests/knowledge_statement_wire.rs` | New. End-to-end smoke tests. |

## Verification gate

```
cargo test -p brain-ops knowledge_statement
cargo zigbuild --target x86_64-unknown-linux-gnu -p brain-server --tests
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu -p brain-ops --all-targets -- -D warnings
```

## Out of scope

- HNSW population (phase 21 embedding worker).
- SDK builders (17.8).
- Confidence aggregation (17.9).
- Full lifecycle integration test + bench (17.10).
- Statement events on `STATEMENT_RETRACT` — v1 emits `StatementTombstoned`
  only; the eventual retract-specific event is a phase-22 follow-up.
