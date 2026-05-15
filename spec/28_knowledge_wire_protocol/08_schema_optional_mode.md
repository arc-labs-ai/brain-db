# 28.08 Schema-Optional Mode

The knowledge layer activates only when a schema has been declared. Without a schema, knowledge opcodes return a structured `SchemaNotDeclared` error. The substrate's cognitive primitives (the `0x00xx` opcode namespace) work normally either way.

This is a **deployment posture**, not a compatibility mode. A deployment that wants vector-substrate-only behavior simply never calls `SCHEMA_UPLOAD`. See [`./00_purpose.md`](./00_purpose.md) §"Schema-optional behavior" and [`../00_master_overview/02_doc_map.md`](../00_master_overview/02_doc_map.md).

## 1. The schema declaration trigger

A schema is "declared" when a successful (`dry_run = false`) `SCHEMA_UPLOAD` (`0x0120`) commits at least one schema version. The declaration is **per-deployment**, recorded in the `schemas` redb table; it persists across server restarts.

State machine:

```
[no schema] --SCHEMA_UPLOAD success--> [schema declared]
[schema declared] --(no opcode reverses this)--> [schema declared]
```

There is no `SCHEMA_DROP` opcode in v1.0. Removing a schema entirely requires operator action on the underlying redb file. Tracked in [`./09_open_questions.md`](./09_open_questions.md).

## 2. Gate behavior

When a frame arrives with an opcode in the `0x01xx` namespace:

```
1. Decode opcode and body.
2. If schema declared OR opcode == SCHEMA_UPLOAD:
       dispatch normally.
3. Else:
       reply with ERROR frame, code=SchemaNotDeclared, category=Conflict,
       message="operation requires a schema; call SCHEMA_UPLOAD first".
```

`SCHEMA_UPLOAD` is the **only** knowledge opcode allowed before declaration. Even read-side ops (`SCHEMA_GET`, `EXTRACTOR_LIST`) error out — they have nothing to return.

## 3. Substrate opcodes are unaffected

Every opcode in the `0x00xx` namespace works in both modes:

| Substrate opcode | Behavior in substrate-only mode |
|---|---|
| `ENCODE_REQ` | works normally; no extractor runs because none are registered |
| `RECALL_REQ` | works normally; vector-only retrieval (no hybrid retriever) |
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

When a schema is declared, the substrate's `RECALL_REQ` (`0x0021`) opcode **transparently routes** through the hybrid retriever (semantic + lexical + graph with RRF fusion — phase 23). Clients see the same `RecallResponseFrame` shape with these additional fields populated:

- `MemoryResult.contributing_retrievers: Vec<RetrieverNameWire>` — which retrievers ranked this memory (empty pre-schema, populated post-schema).
- `MemoryResult.fused_score: f32` — the post-RRF rank score (0.0 pre-schema, ≥0.0 post-schema).

These fields exist in the wire shape from v1.0 onward; pre-schema clients receive zeros and an empty vec. This is **forward-compatible** — old SDK builds work in both modes; new SDK builds get the metadata when present.

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

- whether to surface knowledge-namespace APIs at all (hide them if `!schema_declared`).
- which schema version to encode against (pinning).

The capability is **per-connection**; if a `SCHEMA_UPLOAD` commits mid-connection, existing connections continue with their original `schema_version` view (their `WELCOME`-bound snapshot) until reconnect.

Reconnect after schema change is **client-driven**; the server does not push schema-version-bumped frames to existing connections (other than the `SCHEMA_UPDATED` SUBSCRIBE event, which clients may use as a reconnect signal).

## 9. Migration vs declaration

`SCHEMA_UPLOAD` is used both for:

- **Initial declaration** — transition from "no schema" to "schema declared". The state machine in §1.
- **Schema evolution** — issuing a new `schema_version` against an already-declared deployment.

The wire shape is identical (`SchemaUploadRequest`). The server's behavior diverges only in (a) the migration summary the response carries and (b) whether `SCHEMA_UPDATED` event is emitted (always emitted for evolution; not emitted for initial declaration since no subscribers can have been waiting).
