# 18.6 — Wire structs + opcode dispatch (0x0150–0x0156)

7 relation opcodes per spec §28/07. Mirrors 17.6's structure exactly:

- Opcode enum entries (14: req + resp).
- `relation_req.rs` (7 request structs + shared `RelationViewWire`-helpers).
- `relation_resp.rs` (7 response structs + `RelationView` + conversion
  helpers).
- `RequestBody` / `ResponseBody` variants + dispatch arms.
- `brain-ops` dispatch returns `NotYetImplemented("relation op —
  Phase 18.7")` until 18.7 lands real handlers.

## Key decisions

### D1 — `RelationView` as 16-field projection

Mirrors `EntityView` / `StatementView`. Fields per spec §28/07 §2.2.

### D2 — `relation_type` over the wire as canonical string

Caller sends `"namespace:name"`. Handler resolves to `RelationTypeId`
via `relation_type_lookup_by_qname`. Same pattern as predicate in
statement wire.

### D3 — `LIST_FROM / _TO / TRAVERSE` use single-frame snapshot

Per spec §28/07 §9.2 traversal ships streaming `RelationTraverseFrame`.
v1 collapses to one `RelationTraverseResponseFrame` carrying
`Vec<TraversalPathWire>` + `total_paths` + `is_final`. Streaming +
cursor pagination land in phase 23.

### D4 — `TraversalPathWire` shape

Simpler than spec's `RelationTraverseFrame` (which wraps `EntityView`).
For v1:

```rust
pub struct TraversalStepWire {
    pub relation_id: WireUuid,
    pub from: WireUuid,
    pub to: WireUuid,
    pub relation_type: String,    // resolved by handler
    pub depth: u32,
}

pub struct TraversalPathWire {
    pub steps: Vec<TraversalStepWire>,
}
```

Phase 23 wraps `EntityView` for richer client-side state; v1 keeps
the wire shape compact.

### D5 — `EvidenceRefWire` reuse

Same enum from `brain-protocol::knowledge::statement_req`. Relations
use the same `Inline / Overflow` shape on the wire even though
storage carries flat `Vec<MemoryId>`. The handler (18.7) translates
between the wire `EvidenceRefWire::Inline` and brain-core
`Relation.evidence: Vec<MemoryId>` (no overflow path for relations
in v1 per spec §20/05 §3).

## Plan

### Step 1 — Extend `Opcode` enum

Add 14 entries:

```rust
RelationCreateReq      = 0x0150,  RelationCreateResp      = 0x01D0,
RelationGetReq         = 0x0151,  RelationGetResp         = 0x01D1,
RelationSupersedeReq   = 0x0152,  RelationSupersedeResp   = 0x01D2,
RelationTombstoneReq   = 0x0153,  RelationTombstoneResp   = 0x01D3,
RelationListFromReq    = 0x0154,  RelationListFromResp    = 0x01D4,
RelationListToReq      = 0x0155,  RelationListToResp      = 0x01D5,
RelationTraverseReq    = 0x0156,  RelationTraverseResp    = 0x01D6,
```

Plus `from_u16` arms.

### Step 2 — `relation_req.rs` + `relation_resp.rs`

Following 17.6 layout precisely.

### Step 3 — `RequestBody` / `ResponseBody`

Add 7 variants each + matching dispatch arms (opcode / encode /
decode).

### Step 4 — `brain-ops` dispatch

Replace the not-yet-implemented stub for these 7 opcodes with
`NotYetImplemented("relation op — Phase 18.7")` (handlers land next).

### Step 5 — Tests

`relation_req.rs::tests` — ~12 round-trip tests + opcode byte
assertions, mirroring 17.6.

## Files written

| Path | Change |
|---|---|
| `crates/brain-protocol/src/opcode.rs` | +14 enum + `from_u16` arms. |
| `crates/brain-protocol/src/request.rs` | +7 `RequestBody` variants + dispatch. |
| `crates/brain-protocol/src/response.rs` | +7 `ResponseBody` variants + dispatch. |
| `crates/brain-protocol/src/knowledge/mod.rs` | +2 sub-modules + re-exports. |
| `crates/brain-protocol/src/knowledge/relation_req.rs` | New. |
| `crates/brain-protocol/src/knowledge/relation_resp.rs` | New. |
| `crates/brain-ops/src/dispatch.rs` | NotYetImplemented arms for the 7 variants. |

## Verify

```
cargo test -p brain-protocol relation
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu -p brain-protocol -p brain-ops --all-targets -- -D warnings
```
