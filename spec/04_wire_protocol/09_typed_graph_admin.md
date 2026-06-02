# 04.09 Typed-Graph Operation Frames

Request/response body schemas for the typed-graph "operation" opcodes — schema management (`0x0120–0x0126`), SUBSCRIBE event payloads carrying typed-graph deltas, the schema-optional / schemaless dispatch gate, hybrid query (`0x0160–0x0163`), and admin operations (`0x0170–0x0177`). These opcodes orchestrate the noun-frame surface defined in [`./08_typed_graph_frames.md`](./08_typed_graph_frames.md).

Brain's wire protocol — 32-byte header, opcode framing, CRC32C, payload encoding — is covered in [`./02_wire_format.md`](./02_wire_format.md), [`./03_opcodes.md`](./03_opcodes.md), and [`./05_frame_layouts.md`](./05_frame_layouts.md). This file specifies only the CBOR field schemas of the request/response payloads for the operation opcodes.

Cross-references:
- [`./08_typed_graph_frames.md`](./08_typed_graph_frames.md) — entity / statement / relation noun frames.
- [`../03_schema/00_purpose.md`](../03_schema/00_purpose.md) — schema DSL grammar.
- [`../11_extractors/00_purpose.md`](../11_extractors/00_purpose.md) — extractor registry.
- [`../13_retrievers/00_purpose.md`](../13_retrievers/00_purpose.md) — three retrievers and RRF fusion.
- [`./07_error_handling.md`](./07_error_handling.md) — error mapping and field caps.

## Schema frames

Request/response body schemas for every opcode in the `0x0120–0x012F` schema range. Schema operations let clients declare entity types, predicates, and relation types via the schema DSL; they also govern extractor enablement.

### Schema opcode index

| Opcode | Name | Section | Status |
|---|---|---|---|
| `0x0120` | `SCHEMA_UPLOAD` | "SCHEMA_UPLOAD" | spec-only |
| `0x0121` | `SCHEMA_GET` | "SCHEMA_GET" | spec-only |
| `0x0122` | `SCHEMA_LIST` | "SCHEMA_LIST" | spec-only |
| `0x0123` | `SCHEMA_VALIDATE` | "SCHEMA_VALIDATE" | spec-only |
| `0x0124` | `EXTRACTOR_LIST` | "EXTRACTOR_LIST" | spec-only |
| `0x0125` | `EXTRACTOR_DISABLE` | "EXTRACTOR_DISABLE/ENABLE" | spec-only |
| `0x0126` | `EXTRACTOR_ENABLE` | "EXTRACTOR_DISABLE/ENABLE" | spec-only |

Responses live at `0x01A0–0x01A6` (low byte with high bit set).

All payloads follow the CBOR field-schema conventions in [`./08_typed_graph_frames.md`](./08_typed_graph_frames.md).

### SCHEMA_UPLOAD (0x0120)

#### Request — `SchemaUploadRequest`

```rust
pub struct SchemaUploadRequest {
    pub schema_document: String,       // DSL source text per §21
    pub allow_breaking: bool,          // false = reject if migration would be required
    pub dry_run: bool,                 // identical to SCHEMA_VALIDATE when true
    pub request_id: WireUuid,
}
```

Semantics:

1. Parse `schema_document` per the §21 grammar. Syntax errors → `SCHEMA_INVALID` with line/column in `ErrorDetails`.
2. Diff against current schema. If breaking changes are present and `allow_breaking = false`, return `SCHEMA_MIGRATION_REQUIRED`.
3. If `dry_run`, return validation result without persisting.
4. Otherwise commit the new schema version inside a redb transaction: writes to the `schemas` table, allocates a fresh `schema_version`, increments the registry, fires `SCHEMA_UPDATED` event (see §"SUBSCRIBE events" below).
5. Existing entities / statements / relations remain valid against the *old* schema version until a migration pass re-validates them. Migration policy is in [`../03_schema/`](../03_schema/00_purpose.md).

#### Response — `SchemaUploadResponse`

```rust
pub struct SchemaUploadResponse {
    pub schema_version: u32,           // 0 if dry_run rejected the upload
    pub validation_errors: Vec<SchemaValidationError>,
    pub migration_summary: Option<SchemaMigrationSummary>,
}

pub struct SchemaValidationError {
    pub line: u32,
    pub column: u32,
    pub message: String,
    pub severity: u8,                  // 0=info, 1=warning, 2=error
}

pub struct SchemaMigrationSummary {
    pub entity_types_added: Vec<String>,
    pub entity_types_removed: Vec<String>,
    pub predicates_added: Vec<String>,
    pub predicates_removed: Vec<String>,
    pub relation_types_added: Vec<String>,
    pub relation_types_removed: Vec<String>,
    pub estimated_rows_to_revalidate: u64,
}
```

#### Errors

- `SCHEMA_INVALID` — parse or validation failure (severity ≥ 2 in any error).
- `SCHEMA_MIGRATION_REQUIRED` — breaking change with `allow_breaking = false`.
- `INVALID_ARGUMENT` — `schema_document` empty or > 1 MiB (per [`./07_error_handling.md`](./07_error_handling.md)).

### SCHEMA_GET (0x0121)

#### Request — `SchemaGetRequest`

```rust
pub struct SchemaGetRequest {
    pub version_id: u32,               // 0 = latest
}
```

#### Response — `SchemaGetResponse`

```rust
pub struct SchemaGetResponse {
    pub schema_version: u32,
    pub schema_document: String,       // canonicalized DSL form
    pub created_at_unix_nanos: u64,
    pub uploaded_by_agent_id: WireUuid,
}
```

`schema_document` is the **canonicalized** form — comments removed, whitespace normalized — not the original upload text. Clients that want the original can read the audit table via `ADMIN_GET_AUDIT` (see §"Admin frames" below).

#### Errors

- `SCHEMA_INVALID` — `version_id != 0` and not in registry.

### SCHEMA_LIST (0x0122)

#### Request — `SchemaListRequest`

```rust
pub struct SchemaListRequest {
    pub limit: u32,                    // 1..=100
    pub cursor: Vec<u8>,               // opaque
}
```

#### Response — streaming, per-item `SchemaListItem`

```rust
pub struct SchemaListItem {
    pub schema_version: u32,
    pub created_at_unix_nanos: u64,
    pub change_summary: String,        // human-readable, ≤ 4 KiB
}

pub struct SchemaListResponseTail {
    pub next_cursor: Vec<u8>,
    pub total_returned: u32,
}
```

Stream contract: same as `ENTITY_LIST` ([`./08_typed_graph_frames.md`](./08_typed_graph_frames.md)) — substrate streaming with EOS on the tail frame.

### SCHEMA_VALIDATE (0x0123)

#### Request — `SchemaValidateRequest`

```rust
pub struct SchemaValidateRequest {
    pub schema_document: String,
}
```

#### Response — `SchemaValidateResponse`

Same shape as `SchemaUploadResponse`, but `schema_version` is always 0 (no commit).

#### Errors

- `INVALID_ARGUMENT` — empty or oversized document.

### EXTRACTOR_LIST (0x0124)

#### Request — `ExtractorListRequest`

```rust
pub struct ExtractorListRequest {
    pub include_disabled: bool,
}
```

#### Response — streaming, per-item `ExtractorListItem`

```rust
pub struct ExtractorListItem {
    pub extractor_id: u32,
    pub name: String,                  // e.g. "pattern:role-assignment"
    pub tier: u8,                      // 1=pattern, 2=classifier, 3=llm
    pub enabled: bool,
    pub schema_version: u32,           // version this extractor binds to
    pub last_run_unix_nanos: u64,
    pub statements_produced_lifetime: u64,
    pub failures_lifetime: u64,
}

pub struct ExtractorListResponseTail {
    pub total_returned: u32,
}
```

Cross-ref: [`../11_extractors/00_purpose.md`](../11_extractors/00_purpose.md) defines the tier model.

### EXTRACTOR_DISABLE (0x0125) / EXTRACTOR_ENABLE (0x0126)

#### Requests

```rust
pub struct ExtractorDisableRequest {
    pub extractor_id: u32,
    pub reason: String,                // ≤ 4 KiB
    pub request_id: WireUuid,
}

pub struct ExtractorEnableRequest {
    pub extractor_id: u32,
    pub request_id: WireUuid,
}
```

#### Responses

```rust
pub struct ExtractorDisableResponse {
    pub previously_enabled: bool,
    pub disabled_at_unix_nanos: u64,
}

pub struct ExtractorEnableResponse {
    pub previously_disabled: bool,
    pub enabled_at_unix_nanos: u64,
}
```

#### Errors

- `INVALID_ARGUMENT` — `extractor_id` not registered.
- `EXTRACTOR_DISABLED` — `EXTRACTOR_DISABLE` on an already-disabled extractor returns success (idempotent); `EXTRACTOR_ENABLE` on a non-existent id returns this code.

Disabling an extractor takes effect on the *next* ENCODE; in-flight extractions complete. The server emits no `EXTRACTION_FAILED` event for in-flight cancellations — disabling is non-disruptive.

### Schema authorization

All schema-namespace opcodes (`0x0120–0x0123`) and extractor-governance opcodes (`0x0125–0x0126`) require **admin** permissions in the agent's `AgentPermissions` (see [`04_handshake.md`](./04_handshake.md)). `SCHEMA_GET`, `SCHEMA_LIST`, `EXTRACTOR_LIST` are readable by any authenticated agent.

Unauthorized requests return substrate `ErrorCategory::Authorization` with code `AdminPermissionRequired`.

### Schema cross-shard semantics

Schema is a **cluster-wide** concept; `SCHEMA_UPLOAD` on any shard takes effect across all shards. The implementation routes the upload through a coordinated commit on an authoritative shard (see §"Multi-shard schema state" below).

`SCHEMA_LIST` / `SCHEMA_GET` are local reads — every shard holds an identical registry copy. Inconsistencies (a shard with a stale registry) are recovery bugs, not client-facing concerns.

## SUBSCRIBE events (typed-graph)

Brain's `SUBSCRIBE_REQ` (`0x0030`) / `SUBSCRIBE_EVENT` (`0x00B0`) primitive ([`./03_opcodes.md`](./03_opcodes.md), [`./05_frame_layouts.md`](./05_frame_layouts.md)) carries change-feed events for the **substrate** (memory encoded / forgotten / linked). When a schema is declared, the same primitive also carries **typed-graph** events.

This section specifies the typed-graph events: their on-wire shapes, when the server emits them, and how subscribers filter them.

Cross-references:
- [`./05_frame_layouts.md`](./05_frame_layouts.md) §SubscribeRequest — base SUBSCRIBE shape.
- §"Entity frames", §"Statement frames", §"Relation frames" in [`./08_typed_graph_frames.md`](./08_typed_graph_frames.md) — opcodes that *emit* the events below.
- [`../15_background_workers/00_purpose.md`](../15_background_workers/00_purpose.md) — extractor / consolidation workers that emit `EXTRACTION_*` and `SCHEMA_UPDATED`.

### Event type table

| Event type | Emitted by | Family |
|---|---|---|
| `ENTITY_CREATED` | `ENTITY_CREATE` (0x0130) | Entity |
| `ENTITY_UPDATED` | `ENTITY_UPDATE` (0x0132) | Entity |
| `ENTITY_RENAMED` | `ENTITY_RENAME` (0x0133) | Entity |
| `ENTITY_MERGED` | `ENTITY_MERGE` (0x0134) | Entity |
| `ENTITY_UNMERGED` | `ENTITY_UNMERGE` (0x0135) | Entity |
| `ENTITY_TOMBSTONED` | `ENTITY_TOMBSTONE` (0x0138) | Entity |
| `STATEMENT_CREATED` | `STATEMENT_CREATE` (0x0140) | Statement |
| `STATEMENT_SUPERSEDED` | `STATEMENT_SUPERSEDE` (0x0142) | Statement |
| `STATEMENT_TOMBSTONED` | `STATEMENT_TOMBSTONE` (0x0143) | Statement |
| `RELATION_CREATED` | `RELATION_CREATE` (0x0150) | Relation |
| `RELATION_SUPERSEDED` | `RELATION_SUPERSEDE` (0x0152) | Relation |
| `EXTRACTION_COMPLETED` | extractor worker | Extractor |
| `EXTRACTION_FAILED` | extractor worker | Extractor |
| `SCHEMA_UPDATED` | `SCHEMA_UPLOAD` (0x0120) | Schema |

Substrate event types (`ENCODED`, `FORGOTTEN`, `LINKED`, etc.) remain as defined in [`./05_frame_layouts.md`](./05_frame_layouts.md) and are not duplicated here.

### Event envelope

Every event rides as a `SUBSCRIBE_EVENT` frame (opcode `0x00B0`) with body shape:

```rust
pub struct SubscriptionEvent {
    pub event_type: EventTypeWire,
    pub memory_id: WireMemoryId,        // [0;16] when not memory-scoped
    pub context_id: u64,                // 0 = no context
    pub text: String,                   // human-readable summary; may be empty
    pub kind: MemoryKindWire,           // for substrate events; ignored for typed-graph
    pub salience: f32,                  // for substrate; 0.0 for typed-graph
    pub timestamp_unix_nanos: u64,      // server clock at emission
    pub lsn: u64,                       // monotonic per-shard LSN; subscriber resumes by LSN
    pub knowledge_payload: Option<KnowledgeEventPayload>,  // see below
}
```

Brain's `SubscriptionEvent` (defined in [`./05_frame_layouts.md`](./05_frame_layouts.md)) carries an optional `knowledge_payload`. For substrate-emitted events the field is `None`; for typed-graph-emitted events it carries the typed body. Brain's `EventType` enum includes the typed-graph event variants (`EntityCreated` … `SchemaUpdated`) defined in the same section.

### `KnowledgeEventPayload` union

```rust
pub enum KnowledgeEventPayload {
    EntityCreated(EntityCreatedEvent),
    EntityUpdated(EntityUpdatedEvent),
    EntityRenamed(EntityRenamedEvent),
    EntityMerged(EntityMergedEvent),
    EntityUnmerged(EntityUnmergedEvent),
    EntityTombstoned(EntityTombstonedEvent),
    StatementCreated(StatementCreatedEvent),
    StatementSuperseded(StatementSupersededEvent),
    StatementTombstoned(StatementTombstonedEvent),
    RelationCreated(RelationCreatedEvent),
    RelationSuperseded(RelationSupersededEvent),
    ExtractionCompleted(ExtractionCompletedEvent),
    ExtractionFailed(ExtractionFailedEvent),
    SchemaUpdated(SchemaUpdatedEvent),
}
```

Each variant is a CBOR map. Variants for ops not yet implemented carry typed shells; only the entity variants land first (in parallel with the merge / tombstone / etc. opcodes).

#### Entity events

```rust
pub struct EntityCreatedEvent {
    pub entity_id: WireUuid,
    pub entity_type_id: u32,
    pub canonical_name: String,
}

pub struct EntityUpdatedEvent {
    pub entity_id: WireUuid,
    pub entity_type_id: u32,
    pub canonical_name: String,         // post-update
    pub embedding_version_changed: bool,
}

pub struct EntityRenamedEvent {
    pub entity_id: WireUuid,
    pub old_canonical_name: String,
    pub new_canonical_name: String,
    pub old_moved_to_alias: bool,
}

pub struct EntityMergedEvent {
    pub survivor: WireUuid,
    pub merged: WireUuid,
    pub audit_id: WireUuid,
    pub confidence: f32,
    pub statements_rerouted: u32,
    pub relations_rerouted: u32,
}

pub struct EntityUnmergedEvent {
    pub restored_entity_id: WireUuid,
    pub from_survivor: WireUuid,
    pub audit_id: WireUuid,
}

pub struct EntityTombstonedEvent {
    pub entity_id: WireUuid,
    pub reason: String,
}
```

#### Statement events (spec-only)

```rust
pub struct StatementCreatedEvent {
    pub statement_id: WireUuid,
    pub kind: StatementKindWire,        // Fact | Preference | Event
    pub subject: WireUuid,
    pub predicate: String,
    pub confidence: f32,
}

pub struct StatementSupersededEvent {
    pub old_statement_id: WireUuid,
    pub new_statement_id: WireUuid,
    pub chain_root: WireUuid,
}

pub struct StatementTombstonedEvent {
    pub statement_id: WireUuid,
    pub reason: String,
}
```

#### Relation events (spec-only)

```rust
pub struct RelationCreatedEvent {
    pub relation_id: WireUuid,
    pub relation_type: String,
    pub from: WireUuid,
    pub to: WireUuid,
}

pub struct RelationSupersededEvent {
    pub old_relation_id: WireUuid,
    pub new_relation_id: WireUuid,
}
```

#### Extractor events (spec-only)

```rust
pub struct ExtractionCompletedEvent {
    pub extractor_id: u32,
    pub memory_id: WireMemoryId,
    pub statements_produced: u32,
    pub entities_referenced: u32,
    pub wall_time_ms: u32,
}

pub struct ExtractionFailedEvent {
    pub extractor_id: u32,
    pub memory_id: WireMemoryId,
    pub error_code: u8,                 // §03 error code from §10
    pub error_message: String,
}
```

#### Schema events (spec-only)

```rust
pub struct SchemaUpdatedEvent {
    pub from_version: u32,
    pub to_version: u32,
}
```

### Emission semantics

#### Atomicity with the originating write

Events are emitted **after** the originating opcode's redb commit has succeeded. The chain of guarantees:

1. WAL record written + fsynced.
2. Redb transaction committed.
3. Event broadcast to the per-shard subscription registry.
4. ACK to the originating opcode's response stream.

If a subscriber is slow or backpressured, the event is buffered up to `max_inflight` (per the subscriber's `SUBSCRIBE_REQ.max_inflight`). Exceeding the buffer triggers per-subscriber back-pressure handling — not a substrate-wide stall.

#### Ordering within a shard

Events for entities / statements / relations / extractor outcomes on the **same shard** are emitted in the order of their LSN. Subscribers see a single monotonic LSN stream per shard (substrate + typed-graph interleaved).

Cross-shard ordering is **not** guaranteed. Subscribers that need a cross-shard total order use the connection layer's fan-in semantics; the per-shard LSN is the local order signal.

#### Idempotency on resume

Subscribers resume by replaying from `from_lsn` (carried in `SUBSCRIBE_REQ`). Brain's LSN allocator (per shard, monotonic) guarantees that re-delivery matches the original byte stream — same `SubscriptionEvent` body, same `lsn`. Clients should dedupe locally if they reprocess on resume.

#### No "intermediate state" events

Multi-step operations (e.g. `ENTITY_MERGE` performs 7+ redb table writes) emit exactly **one** event for the whole operation. There is no partial-progress event stream. The `EntityMergedEvent.statements_rerouted` / `.relations_rerouted` counts let subscribers see the scope of the change without watching it unfold.

### Subscriber filters

`SUBSCRIBE_REQ` carries a `SubscriptionFilter` struct. For typed-graph events the filter is extended with:

```rust
pub struct KnowledgeSubscriptionFilter {
    pub event_types: Option<Vec<EventTypeWire>>,    // None = all
    pub entity_types: Option<Vec<u32>>,             // None = all (matches EntityTypeId on entity events)
    pub entity_ids: Option<Vec<WireUuid>>,          // None = all
    pub predicates: Option<Vec<String>>,            // None = all (statement / extraction events)
    pub min_confidence: f32,                        // 0.0 = no filter
}
```

The server applies the filter at emission time — non-matching events are discarded before broadcast, not just at the subscriber's edge. This avoids amplifying high-volume events to uninterested subscribers.

### Event emission

All entity event variants are emitted post-commit:

- Substrate `EventEnvelope` carries an optional `knowledge_payload: Option<KnowledgeEventPayload>` field.
- `ENTITY_CREATE` / `_UPDATE` / `_RENAME` handlers emit `EntityCreated` / `EntityUpdated` / `EntityRenamed` events post-commit.
- `ENTITY_MERGE` / `_UNMERGE` / `_TOMBSTONE` handlers emit their respective events.

Events are forward-only from their introduction; there is **no retroactive emission** for entities created before subscribers connect.

### Memory layer ↔ typed graph event correlation

When an extractor processes a freshly encoded memory and emits `EXTRACTION_COMPLETED`, the event's `memory_id` field correlates back to the substrate `ENCODED` event for the same memory. Subscribers that want the chain "memory encoded → extractor ran → entities created" subscribe to both event families and join on `memory_id`.

Brain's `lsn` is shared across substrate and typed-graph events on the same shard, so the chain is replayable in causal order.

### SUBSCRIBE open questions

Notably:

- Whether `SubscriptionEvent` should be a sum type per family (substrate / typed-graph) rather than a struct-with-optional-payload. Currently flat-with-optional to keep one event envelope on the wire.
- Whether `min_confidence` filter should apply to entity events too (not just statements / extractions). Currently no.

## Schema-optional mode

The typed-graph opcodes accept traffic in both modes: with or without a declared schema. When no schema is declared, predicates and relation types are open-vocabulary — they are interned on first use with origin `ImplicitFromWrite` (see [`../02_data_model/07_statement.md`](../02_data_model/07_statement.md) and [`../02_data_model/08_relation.md`](../02_data_model/08_relation.md)). When a schema is declared, it acts as a **strict validator** for that namespace: unknown qnames are rejected with `PredicateNotInSchema` / `RelationTypeNotInSchema`, and declared cardinalities are enforced.

Brain's cognitive primitives (the `0x00xx` opcode namespace) and the hybrid retrieval path are unaffected by schema state — hybrid is the default `RECALL` path for every deployment.

Schemaless ("open-vocabulary") and schema-declared ("strict") are both **first-class deployment postures**. A deployment that wants vector-schemaless behavior simply never calls the typed-graph opcodes.

### The schema declaration trigger

A schema is "declared" when a successful (`dry_run = false`) `SCHEMA_UPLOAD` (`0x0120`) commits at least one schema version. The declaration is **per-namespace**, recorded in the `schemas` redb table; it persists across server restarts.

State machine:

```
[open vocabulary] --SCHEMA_UPLOAD success--> [strict schema (version N)]
[strict schema (version N)] --SCHEMA_UPLOAD success--> [strict schema (version N+1)]
```

There is no `SCHEMA_DROP` opcode currently. Removing a schema entirely requires operator action on the underlying redb file.

### Gate behavior

Typed-graph opcodes (`0x01xx` namespace) dispatch in both modes. Their per-opcode validation rules then branch on schema presence for the target namespace:

- **No schema declared**: predicate / relation-type qnames are interned on first use (`SchemaOrigin::ImplicitFromWrite`, `RelationTypeOrigin::ImplicitFromWrite`); no cardinality contract is enforced; `QUERY.predicate_filter` qnames that resolve to no known predicate produce an empty result set rather than an error.
- **Schema declared for the namespace**: unknown predicate qname → `PredicateNotInSchema` (0x004B). Unknown relation type qname → `RelationTypeNotInSchema` (0x004C). Cardinality violations → `CardinalityViolation` (0x0065). Object-type mismatches → `STATEMENT_OBJECT_TYPE_MISMATCH` (0x41).

No opcode is gated out by schema absence. The `SchemaNotDeclared` error is reserved for explicit schema-introspection opcodes (`SCHEMA_GET` on a namespace that never had one) and is documented in [`./07_error_handling.md`](./07_error_handling.md).

### Substrate opcodes are unaffected

Every opcode in the `0x00xx` namespace works in both modes:

| Substrate opcode | Behavior in schemaless mode |
|---|---|
| `ENCODE_REQ` | works normally; no extractor runs because none are registered |
| `RECALL_REQ` | works normally; runs the hybrid path (semantic + lexical + memory-edge graph) — hybrid is the default in both modes |
| `PLAN_REQ`, `REASON_REQ`, `FORGET_REQ`, `LINK_REQ`, `UNLINK_REQ` | unchanged |
| `SUBSCRIBE_REQ` | works; carries substrate events only (no typed-graph events possible since none can be emitted) |
| `ADMIN_*` | unchanged |
| `TXN_*` | unchanged |

This is Brain's "first-class deployment posture" described in [`../../README.md`](../../README.md) and [`../00_overview/00_index.md`](../00_overview/00_index.md).

### Read-after-declaration behavior

The moment `SCHEMA_UPLOAD` commits, the gate flips. In-flight frames are not retroactively re-evaluated:

- Frames decoded **before** the commit return `SchemaNotDeclared` even if the commit completes mid-processing.
- Frames decoded **after** the commit dispatch normally.

The cutover is the redb commit, not the response emission. The connection layer reads the gate state from a per-shard `ArcSwap<bool>` updated atomically with the commit.

### RECALL routing

`RECALL_REQ` (`0x0021`) runs through the hybrid retriever (semantic + lexical + memory-edge graph, fused via RRF) by default in every deployment. Clients always see these fields populated on `MemoryResult`:

- `contributing_retrievers: Vec<RetrieverNameWire>` — which retrievers ranked this memory.
- `fused_score: f32` — the post-RRF rank score.

Declaring a schema does not change the retrieval mode; it adds typed entity-anchored graph traversal as an additional path that the planner may select for the graph retriever. The `RecallResponseFrame` shape is identical in both modes.

RECALL is one verb with no client-side strategy switch. A request that carries a `txn_id` runs the transactional path internally (read-your-writes requires it); every other RECALL runs the hybrid path. Clients always observe the same `RecallResponseFrame` shape; the `contributing_retrievers` field is populated for hybrid responses and empty for the transactional case. See [`./05_frame_layouts.md`](./05_frame_layouts.md).

### Multi-shard schema state

Schema state is **cluster-wide**. Every shard's `schemas` redb table holds an identical copy. `SCHEMA_UPLOAD` on any shard fans out the registry update to all shards before returning success.

Inconsistency window: between the upload's redb commit on shard 0 and the fan-out completing on shard N, typed-graph ops routed to shard N may return `SchemaNotDeclared`. The fan-out target is ≤ 100ms. Brain uses an authoritative-shard-0 strategy: all `SCHEMA_UPLOAD` ops route to shard 0, which commits the registry update, then writes a fan-out marker; other shards pull-replicate the registry row on the next read against any namespace they have not yet observed at that version.

### Error-code wire shape

`SchemaNotDeclared` enters Brain `ErrorCodeWire` enum (per [`./07_error_handling.md`](./07_error_handling.md) Strategy A). Its `ErrorCategoryWire` is `Conflict` — not `Validation`, because the operation is well-formed but the deployment isn't in the right state.

`ErrorResponse.retry_after_ms` is **always** `None` for `SchemaNotDeclared`. The remedy is an admin action (call `SCHEMA_UPLOAD`), not a backoff-and-retry.

### Capability advertisement

Brain's `WELCOME` frame (see [`04_handshake.md`](./04_handshake.md)) carries a `capabilities` block. `WelcomeCapabilities` includes:

```rust
pub struct WelcomeCapabilities {
    // ...existing substrate fields...
    pub schema_declared: bool,
    pub schema_version: u32,           // 0 if !schema_declared
}
```

Clients use this to decide:

- which schema version to encode typed-graph calls against (pinning); typed derive-macro APIs that depend on declared predicates should be hidden when `!schema_declared`. Untyped (qname-based) typed-graph calls and the cognitive primitives surface in both modes.

The capability is **per-connection**; if a `SCHEMA_UPLOAD` commits mid-connection, existing connections continue with their original `schema_version` view (their `WELCOME`-bound snapshot) until reconnect.

Reconnect after schema change is **client-driven**; the server does not push schema-version-bumped frames to existing connections (other than the `SCHEMA_UPDATED` SUBSCRIBE event, which clients may use as a reconnect signal).

### Migration vs declaration

`SCHEMA_UPLOAD` is used both for:

- **Initial declaration** — transition from "no schema" to "schema declared". The state machine in §"The schema declaration trigger".
- **Schema evolution** — issuing a new `schema_version` against an already-declared deployment.

The wire shape is identical (`SchemaUploadRequest`). The server's behavior diverges only in (a) the migration summary the response carries and (b) whether `SCHEMA_UPDATED` event is emitted (always emitted for evolution; not emitted for initial declaration since no subscribers can have been waiting).

## Query frames

Request/response body schemas for `0x0160–0x0163` — the hybrid-query opcodes. These are the primary read API of the typed graph and accept traffic regardless of schema state.

Cross-references:
- [`../13_retrievers/`](../13_retrievers/00_purpose.md) — three retrievers (semantic / lexical / graph) + RRF fusion.
- [`./06_streaming.md`](./06_streaming.md) — substrate streaming model reused here.

### Query opcode index

| Opcode | Name | Section | Status |
|---|---|---|---|
| `0x0160` | `QUERY` | "QUERY" | spec-only |
| `0x0161` | `QUERY_EXPLAIN` | "QUERY_EXPLAIN" | spec-only |
| `0x0162` | `QUERY_TRACE` | "QUERY_TRACE" | spec-only |
| `0x0163` | `RECALL_HYBRID` | "RECALL_HYBRID" | spec-only |

Responses live at `0x01E0–0x01E3`.

`QUERY` (`0x0160`) is the primary structured query opcode. `RECALL_HYBRID` (`0x0163`) is the simple-text fast path used by clients that just want hybrid text-only retrieval without an explicit query language.

Brain's `RECALL_REQ` (`0x0021`) runs the hybrid path by default in every deployment (see §"Schema-optional mode" §"RECALL routing"). The wire response carries `contributing_retrievers` and `fused_score` populated whether or not a schema has been declared.

### Shared query types

#### `QueryRequest`

```rust
pub struct QueryRequest {
    pub query_dsl: String,                  // structured query DSL
    pub top_k: u32,                         // 1..=1000
    pub filters: QueryFilters,
    pub retriever_selection: RetrieverSelection,
    pub budget_wall_time_ms: u32,           // 1..=60000
    pub include_provenance: bool,
    pub include_trace: bool,                // used by QUERY_TRACE only
    pub schema_version: u32,                // 0 = current
    pub request_id: WireUuid,               // [0;16] = no idempotency cache
    pub txn_id: WireUuid,                   // [0;16] = no transaction
}

pub struct QueryFilters {
    pub entity_type_id: u32,                // 0 = no filter
    /// Predicate filter as canonical `"namespace:name"` qnames.
    /// Empty vec = no filter. The planner resolves each qname through
    /// the predicate registry per request — unknown qnames produce an
    /// empty result set in open-vocabulary mode and `PredicateNotInSchema`
    /// (0x004B) when a schema is active for the namespace.
    pub predicate_filter: Vec<String>,
    pub time_range_start_unix_nanos: u64,
    pub time_range_end_unix_nanos: u64,
    pub min_confidence: f32,
    pub context_ids: Vec<u64>,              // empty = no filter
    pub kind_filter: Vec<StatementKindWire>, // empty = no filter
    pub min_salience: f32,                  // for substrate-side filter
}

pub struct RetrieverSelection {
    pub use_semantic: bool,                 // default true
    pub use_lexical: bool,                  // default true
    pub use_graph: bool,                    // default true
    pub semantic_top_k: u32,                // per-retriever top-K; 0 = retriever default
    pub lexical_top_k: u32,
    pub graph_top_k: u32,
    pub rrf_k_constant: u32,                // RRF k parameter; 0 = default 60 per §23
}
```

Semantics:

- The query DSL is structured — combinations of entity / predicate / time / confidence conditions. For text-only queries the client builds the DSL (or use `RECALL_HYBRID` below).
- `RetrieverSelection` lets clients disable retrievers or override per-retriever depth. Setting all three to false → `INVALID_ARGUMENT`.
- `top_k` is the **final fused** top-K. Per-retriever `top_k`s are typically larger to give RRF a useful candidate pool (default: `4 * top_k`).
- `budget_wall_time_ms` is a soft budget. The server returns whatever it has when exceeded with `QUERY_TIMEOUT` on the final frame.

#### `QueryResult` — streamed per-frame item

```rust
pub struct QueryResultItem {
    pub kind: ResultKind,                   // Entity / Statement / Relation / Memory
    pub entity: EntityView,                 // populated when kind=Entity
    pub statement: StatementView,           // populated when kind=Statement
    pub relation: RelationView,             // populated when kind=Relation
    pub memory: MemoryResult,               // populated when kind=Memory; substrate shape
    pub fused_score: f32,                   // post-RRF rank score
    pub contributing_retrievers: Vec<u8>,   // bit-flag values; see below
    pub explanation: String,                // human-readable why-this-result; empty when !include_provenance
}

#[repr(u8)]
pub enum ResultKind {
    Entity = 1,
    Statement = 2,
    Relation = 3,
    Memory = 4,
}

pub struct QueryResultTail {
    pub total_returned: u32,
    pub fused_from_candidate_pool_size: u32, // pre-fusion candidate count
    pub retriever_timings_ms: RetrieverTimings,
    pub truncated_by: u8,                   // 0=none, 1=top_k, 2=budget
}

pub struct RetrieverTimings {
    pub semantic_ms: u32,
    pub lexical_ms: u32,
    pub graph_ms: u32,
    pub fusion_ms: u32,
    pub total_ms: u32,
}
```

Field discipline: only the field matching `kind` is populated; the others carry zero-filled shapes. This is a tagged-union-by-discriminant shape.

#### `MemoryResult` reuse

The `MemoryResult` substrate type ([`./05_frame_layouts.md`](./05_frame_layouts.md)) is reused unchanged. Typed-graph hybrid-retrieval results that include memories carry that type's existing fields; the post-schema additions (`contributing_retrievers`, `fused_score`) live on the **outer** `QueryResultItem` rather than mutating `MemoryResult`. Schemaless clients ignore the outer fields and consume `MemoryResult` directly.

#### `contributing_retrievers` bit-flag values

```rust
pub const RETRIEVER_SEMANTIC: u8 = 0b001;
pub const RETRIEVER_LEXICAL: u8  = 0b010;
pub const RETRIEVER_GRAPH: u8    = 0b100;
```

A `QueryResultItem.contributing_retrievers = vec![0b011]` means semantic + lexical ranked this result; graph did not. Each contributor contributes one entry to the vector with a separate flag — `vec![0b001, 0b010]` is also valid encoding meaning "two separate retriever hits" with provenance.

### QUERY (0x0160)

#### Request

`QueryRequest` directly.

#### Response — streaming

Multiple `QueryResultItem` frames sharing `stream_id`, followed by a tail frame:

```text
S → C  frame: opcode=0x01E0 stream_id=N        body: QueryResultItem  (intermediate)
S → C  frame: opcode=0x01E0 stream_id=N        body: QueryResultItem  (intermediate)
...
S → C  frame: opcode=0x01E0 stream_id=N EOS    body: QueryResultTail  (tail)
```

Substrate streaming model: per-frame, `EOS` on the tail. The tail body is a different CBOR shape than the per-item bodies — clients dispatch on `is_final` (set when EOS is set) and decode accordingly.

#### Errors

- `QUERY_TIMEOUT` (substrate `Unavailable`) — wall budget exceeded. Tail frame carries whatever results were ready; clients see a partial result with `QueryResultTail.truncated_by = 2`.
- `QUERY_OVER_BUDGET` (substrate `ResourceExhausted`) — per-shard memory or candidate-pool cap blown. Frame stream ends without an EOS tail; an `ERROR` frame closes the stream.
- `PredicateNotInSchema` (0x004B) — strict mode only; `filters.predicate_filter` contains a qname not declared in the active schema.
- `RelationTypeNotInSchema` (0x004C) — strict mode only; the DSL or graph step referenced an unknown relation type.
- `HybridUnavailable` (0x0083) — a required retriever component is not currently servable (e.g. inside a transaction, during index rebuild).
- `INVALID_ARGUMENT` — DSL parse failure, `top_k > 1000`, all retrievers disabled.

#### Cancellation

Clients send `CANCEL_STREAM` (`0x0050`) with the offending `stream_id`. Server emits a `CANCEL_STREAM_ACK` (`0x00D0`) on a different stream; the query's frame stream ends with EOS-flagged empty tail.

### QUERY_EXPLAIN (0x0161)

Returns the planner's execution plan **without running it**. Useful for debugging and cost-bounded clients.

#### Request

`QueryRequest`. `include_trace` and `top_k` are ignored.

#### Response — `QueryExplainResponse`

```rust
pub struct QueryExplainResponse {
    pub plan: QueryPlan,
    pub estimated_cost: PlanCost,
    pub estimated_wall_time_ms: u32,
    pub warnings: Vec<String>,              // planner notes, cost surprises, etc.
}

pub struct QueryPlan {
    pub steps: Vec<QueryPlanStep>,
}

pub struct QueryPlanStep {
    pub step_index: u32,
    pub operation: String,                  // e.g. "SemanticRetrieve(entity_hnsw, k=40)"
    pub input_cardinality_estimate: u32,
    pub output_cardinality_estimate: u32,
    pub cost: f32,                          // matched §07 cost model
}

pub struct PlanCost {
    pub vector_search_ms: u32,
    pub tantivy_query_ms: u32,
    pub graph_walk_ms: u32,
    pub fusion_ms: u32,
    pub total_ms: u32,
}
```

#### Errors

Same as `QUERY` minus the timeout / over-budget set (no execution happens).

### QUERY_TRACE (0x0162)

Identical to `QUERY` but the response carries **per-retriever debug info**. Treats `include_trace = true` internally regardless of the request's setting.

#### Response — streaming with extended tail

Per-item frames are `QueryResultItem` plus a `trace` field. The tail body is extended:

```rust
pub struct QueryTraceTail {
    pub base: QueryResultTail,
    pub per_retriever_traces: Vec<RetrieverTrace>,
    pub planner_log: Vec<String>,
}

pub struct RetrieverTrace {
    pub retriever: u8,                      // bit-flag value
    pub candidate_count: u32,
    pub timing_ms: u32,
    pub top_5_summaries: Vec<String>,       // pre-fusion ranked summaries
    pub debug_notes: Vec<String>,
}
```

#### Performance note

`QUERY_TRACE` is **noticeably slower** than `QUERY` because of the trace bookkeeping. Clients should expose it as a debug-only operation. Production hot paths use `QUERY` (`0x0160`).

### RECALL_HYBRID (0x0163)

Text-only fast path. The server's planner builds a default `QueryRequest` from the text + minimal filters and runs it.

#### Request — `RecallHybridRequest`

```rust
pub struct RecallHybridRequest {
    pub text: String,                       // non-empty; ≤ 4 KiB
    pub top_k: u32,                         // 1..=1000
    pub min_confidence: f32,
    pub context_ids: Vec<u64>,              // empty = no filter
    pub time_range_start_unix_nanos: u64,
    pub time_range_end_unix_nanos: u64,
    pub budget_wall_time_ms: u32,
    pub request_id: WireUuid,
    pub txn_id: WireUuid,
}
```

#### Response

Same shape as `QUERY` (streamed `QueryResultItem` frames + `QueryResultTail`). Clients that want both substrate-style memory results and typed-graph entity / statement / relation results use this opcode.

#### Relationship to substrate `RECALL_REQ`

`RECALL_REQ` (`0x0021`) returns **only** `MemoryResult`s — its substrate contract.

`RECALL_HYBRID` (`0x0163`) returns a mix of `MemoryResult`, `EntityView`, `StatementView`, `RelationView` — leveraging the entity / statement / relation indexes alongside the memory HNSW.

Both deployment postures may use either: the substrate `RECALL_REQ` returns memory results from the hybrid path; `RECALL_HYBRID` additionally surfaces typed entity / statement / relation results that have been populated from prior typed-graph writes (open-vocabulary or schema-declared).

#### Errors

Same as `QUERY`, plus `INVALID_ARGUMENT` for empty `text`.

### Idempotency for queries

Queries with `request_id != [0;16]` populate the idempotency cache (same shape as substrate; 24h TTL). Cached responses are byte-identical, **including** the full streamed result sequence. Re-issuing the same `request_id` replays the entire stream from cache.

Idempotency for queries is unusual but useful for:

- Retries after transient network failure on long-running queries.
- Reproducibility in test suites.

Clients that want **fresh** results every time pass `request_id = [0;16]`.

### Query transactions

`QueryRequest.txn_id != [0;16]` makes the query observe a read snapshot that includes the transaction's pending writes — same semantics as substrate `RecallRequest.txn_id`.

Reads inside a transaction are visible to the same transaction's subsequent writes (read-your-writes).

### Multi-shard fan-out

A `QUERY` typically fans out to **all** shards unless filters scope it to a specific shard (e.g. `filters.entity_type_id` + a subject filter that the planner can route).

Per-shard results are streamed to the coordinator (the agent's bound shard); the coordinator runs RRF fusion across the union and streams the final fused result to the client. The per-shard streaming back-pressure handling applies — if one shard is slow, the coordinator buffers within its budget.

The wire shape doesn't expose which shard contributed which result — that's an internal detail. `QUERY_TRACE`'s `RetrieverTrace.debug_notes` may include per-shard breakdown when run on multi-shard deployments.

### Query open questions

Notably:

- Cross-shard error aggregation: what if 1 of 8 shards fails? Currently the planner returns a partial result with a warning. Should there be a strict-mode flag for "all-or-nothing"?
- Planner cost model exposure: should `QueryPlanStep.cost` be `f32` (current) or a richer structured cost?
- Stable cursor semantics for `QUERY` streaming pagination (current spec assumes single-shot streaming; resumable queries are deferred).

## Admin frames

Request/response body schemas for `0x0170–0x0177` — typed-graph admin operations. These are operator-facing: index rebuilds, ambiguity resolution, audit inspection, backfill, job status.

All opcodes in this range require **admin** permissions (per §"Schema authorization" above). Authorization failures return `AdminPermissionRequired` (substrate `Authorization`).

Cross-references:
- [`../11_extractors/00_purpose.md`](../11_extractors/00_purpose.md) — extractor backfill semantics.
- [`../15_background_workers/00_purpose.md`](../15_background_workers/00_purpose.md) — background workers that run jobs.
- [`../02_data_model/06_entity_lifecycle.md`](../02_data_model/06_entity_lifecycle.md) §"Confidence-banded behavior" — audit-driven ambiguity resolution.

### Admin opcode index

| Opcode | Name | Section | Status |
|---|---|---|---|
| `0x0170` | `ADMIN_REBUILD_INDEX` | "ADMIN_REBUILD_INDEX" | spec-only |
| `0x0171` | `ADMIN_REINDEX_TANTIVY` | "ADMIN_REINDEX_TANTIVY" | spec-only |
| `0x0172` | `ADMIN_LIST_PENDING_RESOLUTIONS` | "ADMIN_LIST_PENDING_RESOLUTIONS" | spec-only |
| `0x0173` | `ADMIN_RESOLVE_AMBIGUITY` | "ADMIN_RESOLVE_AMBIGUITY" | spec-only |
| `0x0174` | `ADMIN_GET_AUDIT` | "ADMIN_GET_AUDIT" | spec-only |
| `0x0175` | `ADMIN_LIST_STALE_STATEMENTS` | "ADMIN_LIST_STALE_STATEMENTS" | spec-only |
| `0x0176` | `ADMIN_BACKFILL` | "ADMIN_BACKFILL" | spec-only |
| `0x0177` | `ADMIN_JOB_STATUS` | "ADMIN_JOB_STATUS" | spec-only |

Responses live at `0x01F0–0x01F7`.

### ADMIN_REBUILD_INDEX (0x0170)

Asynchronously rebuilds one of the per-shard typed-graph indexes (entity HNSW, statement HNSW, entity trigrams, etc.). Returns a `job_id` immediately; client polls via `ADMIN_JOB_STATUS`.

#### Request — `AdminRebuildIndexRequest`

```rust
pub struct AdminRebuildIndexRequest {
    pub index_name: String,            // "entity_hnsw", "statement_hnsw", "entity_trigrams", ...
    pub shard_id: u16,                 // 0..=N-1
    pub request_id: WireUuid,
}
```

#### Response — `AdminRebuildIndexResponse`

```rust
pub struct AdminRebuildIndexResponse {
    pub job_id: WireUuid,              // poll via ADMIN_JOB_STATUS
    pub started_at_unix_nanos: u64,
    pub estimated_wall_time_ms: u32,
}
```

#### Errors

- `INVALID_ARGUMENT` — unknown `index_name`, shard out of range.

### ADMIN_REINDEX_TANTIVY (0x0171)

Rebuilds the tantivy BM25 text index for a shard (memories + statements). Same async pattern as `ADMIN_REBUILD_INDEX`.

#### Request — `AdminReindexTantivyRequest`

```rust
pub struct AdminReindexTantivyRequest {
    pub shard_id: u16,
    pub include_memory_text: bool,
    pub include_statement_text: bool,
    pub request_id: WireUuid,
}
```

#### Response

```rust
pub struct AdminReindexTantivyResponse {
    pub job_id: WireUuid,
    pub started_at_unix_nanos: u64,
    pub estimated_wall_time_ms: u32,
}
```

### ADMIN_LIST_PENDING_RESOLUTIONS (0x0172)

Streams entity-resolution audit rows where `outcome = Pending` — i.e. ambiguous extractions that need operator decision.

#### Request — `AdminListPendingResolutionsRequest`

```rust
pub struct AdminListPendingResolutionsRequest {
    pub limit: u32,                    // 1..=1000
    pub cursor: Vec<u8>,
    pub older_than_unix_nanos: u64,    // 0 = no filter
}
```

#### Response — streaming `PendingResolutionItem`

```rust
pub struct PendingResolutionItem {
    pub audit_id: WireUuid,
    pub candidate_name: String,
    pub context: String,
    pub created_at_unix_nanos: u64,
    pub top_k_candidates: Vec<ResolutionCandidate>,
}

pub struct ResolutionCandidate {
    pub entity_id: WireUuid,
    pub canonical_name: String,
    pub confidence: f32,
    pub tier: u8,                      // which tier ranked this candidate
}

pub struct AdminListPendingResolutionsTail {
    pub next_cursor: Vec<u8>,
    pub total_returned: u32,
    pub total_pending: u32,            // unrelated to pagination; cluster-wide count
}
```

### ADMIN_RESOLVE_AMBIGUITY (0x0173)

Operator decides one pending resolution. Either binds the audit's pending subject to an existing entity, or creates a new one.

#### Request — `AdminResolveAmbiguityRequest`

```rust
pub struct AdminResolveAmbiguityRequest {
    pub audit_id: WireUuid,
    pub action: u8,                    // 1=bind_to_existing, 2=create_new, 3=discard
    pub chosen_entity_id: WireUuid,    // [0;16] unless action=1
    pub new_entity_canonical_name: String,   // empty unless action=2
    pub new_entity_type_id: u32,       // 0 unless action=2
    pub note: String,                  // operator note; logged
    pub request_id: WireUuid,
}
```

#### Response — `AdminResolveAmbiguityResponse`

```rust
pub struct AdminResolveAmbiguityResponse {
    pub resolved_at_unix_nanos: u64,
    pub bound_entity_id: WireUuid,     // the entity statements now point to
    pub statements_rerouted: u32,      // how many pending-subject statements were re-routed
}
```

#### Errors

- `INVALID_ARGUMENT` — bad `action`, missing required field for the action, unknown `chosen_entity_id`.
- `ENTITY_AMBIGUOUS` if a race made the audit already-resolved.

### ADMIN_GET_AUDIT (0x0174)

Read a single audit row (resolution audit, merge audit, schema audit) by id.

#### Request — `AdminGetAuditRequest`

```rust
pub struct AdminGetAuditRequest {
    pub audit_id: WireUuid,
}
```

#### Response — `AdminGetAuditResponse`

```rust
pub struct AdminGetAuditResponse {
    pub audit_kind: u8,                // 1=entity_resolution, 2=entity_merge, 3=schema_upload, 4=extractor_governance
    pub created_at_unix_nanos: u64,
    pub actor_agent_id: WireUuid,
    pub payload: AuditPayload,
}

pub enum AuditPayload {
    EntityResolution(ResolutionAuditView),
    EntityMerge(MergeAuditView),
    SchemaUpload(SchemaAuditView),
    ExtractorGovernance(ExtractorAuditView),
}

pub struct ResolutionAuditView {
    pub candidate_name: String,
    pub context: String,
    pub top_k_candidates: Vec<ResolutionCandidate>,
    pub outcome: u8,                   // 0=Pending, 1=Resolved, 2=Created, 3=Ambiguous_decided, 4=Discarded
    pub bound_entity_id: WireUuid,
    pub resolved_by_agent_id: WireUuid,
}

pub struct MergeAuditView {
    pub survivor: WireUuid,
    pub merged: WireUuid,
    pub confidence: f32,
    pub reason: String,
    pub statements_rerouted: u32,
    pub relations_rerouted: u32,
    pub grace_period_expires_at_unix_nanos: u64,
    pub unmerged_at_unix_nanos: u64,   // 0 if not unmerged
}

pub struct SchemaAuditView {
    pub schema_version: u32,
    pub uploaded_at_unix_nanos: u64,
    pub migration_summary: SchemaMigrationSummary,
}

pub struct ExtractorAuditView {
    pub extractor_id: u32,
    pub event: u8,                     // 1=enabled, 2=disabled, 3=registered, 4=deregistered
    pub reason: String,
}
```

#### Errors

- `INVALID_ARGUMENT` — `audit_id` not found.

### ADMIN_LIST_STALE_STATEMENTS (0x0175)

Streams statements whose source memory has been forgotten / tombstoned but the statement itself is still active. Operator decides whether to tombstone or retract.

#### Request — `AdminListStaleStatementsRequest`

```rust
pub struct AdminListStaleStatementsRequest {
    pub older_than_unix_nanos: u64,    // 0 = no filter
    pub kind_filter: Vec<StatementKindWire>, // empty = all
    pub limit: u32,                    // 1..=1000
    pub cursor: Vec<u8>,
}
```

#### Response — streaming `StatementView`

Same shape as `STATEMENT_LIST` ([`./08_typed_graph_frames.md`](./08_typed_graph_frames.md)). Filter is "statement.evidence references a tombstoned / forgotten memory".

### ADMIN_BACKFILL (0x0176)

Re-runs one or more extractors over a memory range. Used after schema migration or extractor improvements.

#### Request — `AdminBackfillRequest`

```rust
pub struct AdminBackfillRequest {
    pub extractor_ids: Vec<u32>,       // empty = all enabled extractors
    pub memory_range_start_unix_nanos: u64,
    pub memory_range_end_unix_nanos: u64,
    pub dry_run: bool,                 // report estimated work without dispatching
    pub max_parallelism: u32,          // 1..=16; default 4
    pub request_id: WireUuid,
}
```

#### Response — `AdminBackfillResponse`

```rust
pub struct AdminBackfillResponse {
    pub job_id: WireUuid,
    pub memories_in_range: u64,
    pub extractors_dispatched: u32,
    pub estimated_wall_time_ms: u64,
}
```

#### Errors

- `INVALID_ARGUMENT` — unknown `extractor_ids`, `memory_range_start > end`, `max_parallelism > 16`.
- `EXTRACTOR_BUDGET_EXCEEDED` (substrate `ResourceExhausted`) — backfill would exceed configured per-extractor budgets.

### ADMIN_JOB_STATUS (0x0177)

Polls the status of an async job started by `ADMIN_REBUILD_INDEX` / `ADMIN_REINDEX_TANTIVY` / `ADMIN_BACKFILL`.

#### Request — `AdminJobStatusRequest`

```rust
pub struct AdminJobStatusRequest {
    pub job_id: WireUuid,
}
```

#### Response — `AdminJobStatusResponse`

```rust
pub struct AdminJobStatusResponse {
    pub job_id: WireUuid,
    pub state: u8,                     // 1=pending, 2=running, 3=completed, 4=failed, 5=cancelled
    pub started_at_unix_nanos: u64,
    pub updated_at_unix_nanos: u64,
    pub completed_at_unix_nanos: u64,  // 0 if not completed
    pub progress_percent: f32,         // 0.0..=100.0
    pub eta_ms: u32,                   // estimated remaining wall time
    pub error_message: String,         // populated when state=failed
    pub kind: u8,                      // 1=rebuild_index, 2=reindex_tantivy, 3=backfill
    pub stats: JobStats,
}

pub struct JobStats {
    pub items_processed: u64,
    pub items_total: u64,
    pub items_failed: u64,
    pub throughput_per_sec: f32,
}
```

#### Errors

- `INVALID_ARGUMENT` — `job_id` not found.

### Job retention

Job records are kept in a `jobs` redb table for 7 days after `completed_at`. After that they're garbage-collected. Polling an expired job returns `INVALID_ARGUMENT`.

### Cancellation

There is no `ADMIN_CANCEL_JOB` opcode currently. Jobs that need cancellation use:

- For index rebuilds: the underlying worker is interrupt-tolerant. A subsequent `ADMIN_REBUILD_INDEX` for the same index supersedes the prior one (the worker re-checks every chunk).
- For backfill: re-issue with a narrower range, then let the wider one complete or expire.

### Concurrent admin jobs

Per-shard, only **one** index rebuild may run at a time. Concurrent `ADMIN_REBUILD_INDEX` requests for the same index queue behind the first; the response returns the queued `job_id` immediately. The job order is reflected in `ADMIN_JOB_STATUS.state` (`pending` until prior jobs complete).

Per-shard, multiple backfills may run concurrently up to `max_parallelism`. Across shards, jobs are independent.
