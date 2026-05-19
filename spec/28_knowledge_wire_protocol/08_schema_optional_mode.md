# 28.08 Schema-Optional Mode

The knowledge opcodes accept traffic in both modes: with or without a declared schema. When no schema is declared, predicates and relation types are open-vocabulary — they are interned on first use with origin `ImplicitFromWrite` (see `19_statements/00_purpose.md` and `20_relations/00_purpose.md`). When a schema is declared, it acts as a **strict validator** for that namespace: unknown qnames are rejected with `PredicateNotInSchema` / `RelationTypeNotInSchema`, and declared cardinalities are enforced.

The substrate's cognitive primitives (the `0x00xx` opcode namespace) and the hybrid retrieval path are unaffected by schema state — hybrid is the default `RECALL` path for every deployment.

Schemaless ("open-vocabulary") and schema-declared ("strict") are both **first-class deployment postures**. A deployment that wants vector-substrate-only behavior simply never calls the knowledge opcodes. See [`./00_purpose.md`](./00_purpose.md) §"Schema-optional behavior" and [`../00_master_overview/02_doc_map.md`](../00_master_overview/02_doc_map.md).

## 1. The schema declaration trigger

A schema is "declared" when a successful (`dry_run = false`) `SCHEMA_UPLOAD` (`0x0120`) commits at least one schema version. The declaration is **per-namespace**, recorded in the `schemas` redb table; it persists across server restarts.

State machine:

```
[open vocabulary] --SCHEMA_UPLOAD success--> [strict schema (version N)]
[strict schema (version N)] --SCHEMA_UPLOAD success--> [strict schema (version N+1)]
```

There is no `SCHEMA_DROP` opcode in v1.0. Removing a schema entirely requires operator action on the underlying redb file. Tracked in [`./09_open_questions.md`](./09_open_questions.md).

## 2. Gate behavior

Knowledge opcodes (`0x01xx` namespace) dispatch in both modes. Their per-opcode validation rules then branch on schema presence for the target namespace:

- **No schema declared**: predicate / relation-type qnames are interned on first use (`SchemaOrigin::ImplicitFromWrite`, `RelationTypeOrigin::ImplicitFromWrite`); no cardinality contract is enforced; `QUERY.predicate_filter` qnames that resolve to no known predicate produce an empty result set rather than an error.
- **Schema declared for the namespace**: unknown predicate qname → `PredicateNotInSchema` (0x004B). Unknown relation type qname → `RelationTypeNotInSchema` (0x004C). Cardinality violations → `CardinalityViolation` (0x0065). Object-type mismatches → `STATEMENT_OBJECT_TYPE_MISMATCH` (0x41).

No opcode is gated out by schema absence. The legacy `SchemaNotDeclared` error remains reserved for explicit schema-introspection opcodes (`SCHEMA_GET` on a namespace that never had one) and is documented in [`./03_errors.md`](./03_errors.md).

## 3. Substrate opcodes are unaffected

Every opcode in the `0x00xx` namespace works in both modes:

| Substrate opcode | Behavior in substrate-only mode |
|---|---|
| `ENCODE_REQ` | works normally; no extractor runs because none are registered |
| `RECALL_REQ` | works normally; runs the hybrid path (semantic + lexical + memory-edge graph) — hybrid is the default in both modes |
| `PLAN_REQ`, `REASON_REQ`, `FORGET_REQ`, `LINK_REQ`, `UNLINK_REQ` | unchanged |
| `SUBSCRIBE_REQ` | works; carries substrate events only (no knowledge events possible since none can be emitted) |
| `ADMIN_*` | unchanged |
| `TXN_*` | unchanged |

This is the substrate's "first-class deployment posture" described in [`../../README.md`](../../README.md) and [`../00_master_overview/`](../00_master_overview/).

## 4. Read-after-declaration behavior

The moment `SCHEMA_UPLOAD` commits, the gate flips. In-flight frames are not retroactively re-evaluated:

- Frames decoded **before** the commit return `SchemaNotDeclared` even if the commit completes mid-processing.
- Frames decoded **after** the commit dispatch normally.

The cutover is the redb commit, not the response emission. The connection layer reads the gate state from a per-shard `ArcSwap<bool>` updated atomically with the commit.

## 5. RECALL routing

`RECALL_REQ` (`0x0021`) runs through the hybrid retriever (semantic + lexical + memory-edge graph, fused via RRF — phase 23) by default in every deployment. Clients always see these fields populated on `MemoryResult`:

- `contributing_retrievers: Vec<RetrieverNameWire>` — which retrievers ranked this memory.
- `fused_score: f32` — the post-RRF rank score.

Declaring a schema does not change the retrieval mode; it adds typed entity-anchored graph traversal as an additional path that the planner may select for the graph retriever. The `RecallResponseFrame` shape is identical in both modes.

Clients that want to bypass the hybrid path can pass `RecallStrategy::SubstrateOnly`. Clients that require it can pass `RecallStrategy::HybridOnly`; if a required retriever is not currently servable, the server returns `HybridUnavailable` (0x0083). See [`../03_wire_protocol/07_request_frames.md`](../03_wire_protocol/07_request_frames.md).

## 6. Multi-shard schema state

Schema state is **cluster-wide**. Every shard's `schemas` redb table holds an identical copy. `SCHEMA_UPLOAD` on any shard fans out the registry update to all shards before returning success.

Inconsistency window: between the upload's redb commit on shard 0 and the fan-out completing on shard N, knowledge ops routed to shard N may return `SchemaNotDeclared`. This window is target ≤ 100ms; multi-shard coordination semantics are detailed in [`../25_temporal_model/`](../25_temporal_model/) (TBD).

Phase 19's implementation will choose between two coordination strategies:

1. **Authoritative shard 0** — all `SCHEMA_UPLOAD` ops route to shard 0; other shards pull-replicate on the next read.
2. **2PC across shards** — `SCHEMA_UPLOAD` is a coordinated commit. Simpler in steady state, more complex in failure modes.

Tracked in [`./09_open_questions.md`](./09_open_questions.md).

## 7. Error-code wire shape

`SchemaNotDeclared` enters the substrate `ErrorCodeWire` enum (per [`./03_errors.md`](./03_errors.md) Strategy A). Its `ErrorCategoryWire` is `Conflict` — not `Validation`, because the operation is well-formed but the deployment isn't in the right state.

`ErrorResponse.retry_after_ms` is **always** `None` for `SchemaNotDeclared`. The remedy is an admin action (call `SCHEMA_UPLOAD`), not a backoff-and-retry.

## 8. Capability advertisement

The substrate's `WELCOME` frame ([substrate §06 handshake](../03_wire_protocol/06_handshake.md)) carries a `capabilities` block. Phase 19 extends `WelcomeCapabilities` with:

```rust
pub struct WelcomeCapabilities {
    // ...existing substrate fields...
    pub schema_declared: bool,
    pub schema_version: u32,           // 0 if !schema_declared
}
```

SDKs use this to decide:

- which schema version to encode typed knowledge calls against (pinning); typed derive-macro APIs that depend on declared predicates should be hidden when `!schema_declared`. Untyped (qname-based) knowledge calls and the cognitive primitives surface in both modes.

The capability is **per-connection**; if a `SCHEMA_UPLOAD` commits mid-connection, existing connections continue with their original `schema_version` view (their `WELCOME`-bound snapshot) until reconnect.

Reconnect after schema change is **client-driven**; the server does not push schema-version-bumped frames to existing connections (other than the `SCHEMA_UPDATED` SUBSCRIBE event, which clients may use as a reconnect signal).

## 9. Migration vs declaration

`SCHEMA_UPLOAD` is used both for:

- **Initial declaration** — transition from "no schema" to "schema declared". The state machine in §1.
- **Schema evolution** — issuing a new `schema_version` against an already-declared deployment.

The wire shape is identical (`SchemaUploadRequest`). The server's behavior diverges only in (a) the migration summary the response carries and (b) whether `SCHEMA_UPDATED` event is emitted (always emitted for evolution; not emitted for initial declaration since no subscribers can have been waiting).
