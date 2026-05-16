# 18.8 — SDK relation builders

Hand-written fluent builders over the 7 relation wire opcodes
(18.6 + 18.7). Mirrors the statement SDK (17.8) — no derive macro;
that lands phase 19 with `#[derive(BrainRelation)]` + typed
`Relation<ReportsTo>` wrappers.

## Spec refs

- `spec/29_knowledge_sdk/00_purpose.md` §"Typed relation API".
- `spec/28_knowledge_wire_protocol/07_relation_frames.md`.

## API surface (v1)

```rust
// CREATE / SUPERSEDE
let rel = client.relation()
    .relation_type("acme:reports_to")
    .from(bob)
    .to(priya)
    .confidence(0.9)
    .evidence(vec![mem_a])
    .create().await?;

// GET / TOMBSTONE / LIST / TRAVERSE
let r = client.relations().get(rel_id).await?;
let from_b = client.relations().list_from(bob)
    .with_type("acme:reports_to")
    .current_only()
    .send().await?;
let paths = client.relations().traverse(priya)
    .with_types(&["acme:reports_to"])
    .depth(2)
    .send().await?;
client.relations().tombstone(rel_id, "test").await?;
```

## Types

### `RelationHandle`

Uniform read-side projection (no per-type generic in v1):

```rust
pub struct RelationHandle {
    pub id: RelationId,
    pub chain_root: RelationId,
    pub relation_type: String,        // canonical "ns:name"
    pub from_entity: EntityId,
    pub to_entity: EntityId,
    pub properties_blob: Vec<u8>,
    pub evidence: Vec<MemoryId>,
    pub extractor_id: u32,
    pub extracted_at_unix_nanos: u64,
    pub confidence: f32,
    pub valid_from_unix_nanos: Option<u64>,
    pub valid_to_unix_nanos: Option<u64>,
    pub version: u32,
    pub superseded_by: Option<RelationId>,
    pub supersedes: Option<RelationId>,
    pub tombstoned: bool,
    pub tombstoned_at_unix_nanos: Option<u64>,
    pub is_symmetric: bool,
}
```

`from_view(RelationView) -> RelationHandle`.

### `TraversalPath` / `TraversalStep`

Same shape as the wire types but with brain-core ids:

```rust
pub struct TraversalStep {
    pub relation_id: RelationId,
    pub from: EntityId,
    pub to: EntityId,
    pub relation_type: String,
    pub depth: u32,
}

pub struct TraversalPath {
    pub steps: Vec<TraversalStep>,
}
```

### `TraverseDirection`

```rust
pub enum TraverseDirection {
    Outgoing,
    Incoming,
    Both,
}
```

Maps to the wire byte (0/1/2).

## Builders

- `RelationBuilder` — CREATE (with optional `.supersedes(id)` to
  route to SUPERSEDE).
- `RelationListFromBuilder` / `RelationListToBuilder` — filter
  builders.
- `RelationTraverseBuilder`.
- `RelationsClient` entry point: `get / get_current / tombstone /
  list_from / list_to / traverse`.
- `Client::relation()` / `.relations()` entry methods.

## Errors

Extend `crates/brain-sdk-rust/src/knowledge/errors.rs` with
`RelationErrorKind` + `ClientErrorRelationExt`:

- `NotFound`
- `RelationTypeUnknown`
- `EndpointUnknown` (from/to entity missing)
- `CardinalityViolation`
- `ChainConflict` (already-superseded, type/endpoint mismatch on supersede)
- `EvidenceOverflowUnsupported`

Strategy-B message inspection mirrors statement / entity patterns.

## Files written

| Path | Change |
|---|---|
| `crates/brain-sdk-rust/src/knowledge/relation.rs` | New. ~700 lines. |
| `crates/brain-sdk-rust/src/knowledge/mod.rs` | Module + re-exports. |
| `crates/brain-sdk-rust/src/knowledge/errors.rs` | RelationErrorKind + extension trait. |
| `crates/brain-sdk-rust/src/lib.rs` | Top-level re-exports. |

## Tests

~12 builder-logic unit tests:

- `relation_builder_requires_type / from / to`.
- `predicate-like qname validation`.
- `confidence out-of-range rejected`.
- `evidence cap (32 per spec) checked` — actually spec §20/05 §3
  says soft cap 32. v1 SDK rejects > 32 at .create() time.
- `traverse_depth_clamped_at_5`.
- `traverse_max_nodes_zero_rejected`.
- `from_view round-trip`.
- `RelationHandle::is_current logic`.
- `from-impls for traverse direction`.

End-to-end mock-server tests in 18.9a.

## Verify

```
cargo test -p brain-sdk-rust knowledge::relation
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy -p brain-sdk-rust --all-targets -- -D warnings
```

## Out of scope

- `#[derive(BrainRelation)]` macro — phase 19.
- Typed `Relation<T>` wrappers — phase 19.
- Streaming TRAVERSE — phase 23.
- Cross-shard traversal — phase 23.
