# 04.09 Typed-Graph Operation Frames

Request/response body schemas for the typed-graph "operation" opcodes — schema management (`0x0120–0x0126`), SUBSCRIBE event payloads carrying typed-graph deltas, the schema-optional / schemaless dispatch gate, query introspection (`0x0161–0x0162`), and admin operations (`0x0170–0x0177`). These opcodes orchestrate the noun-frame surface defined in [`./08_typed_graph_frames.md`](./08_typed_graph_frames.md).

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
| `0x0120` | `SCHEMA_UPLOAD` | "SCHEMA_UPLOAD" | implemented |
| `0x0121` | `SCHEMA_GET` | "SCHEMA_GET" | implemented |
| `0x0122` | `SCHEMA_LIST` | "SCHEMA_LIST" | implemented |
| `0x0123` | `SCHEMA_VALIDATE` | "SCHEMA_VALIDATE" | implemented |
| `0x0124` | `EXTRACTOR_LIST` | "EXTRACTOR_LIST" | implemented |
| `0x0125` | `SCHEMA_DROP` | "SCHEMA_DROP" | implemented |
| `0x0127` | `SCHEMA_REPLACE` | "SCHEMA_REPLACE" | implemented |

Responses live at `0x01A0–0x01A7` (low byte with high bit set).

All payloads follow the CBOR field-schema conventions in [`./08_typed_graph_frames.md`](./08_typed_graph_frames.md).

### SCHEMA_UPLOAD (0x0120)

#### Request — `SchemaUploadRequest`

```rust
pub struct SchemaUploadRequest {
    pub schema_document: String,       // DSL source text per §21
    pub dry_run: bool,                 // identical to SCHEMA_VALIDATE when true
    pub allow_breaking: bool,          // false = reject if migration would be required
    pub request_id: WireUuid,
    pub act_as: Option<ActAs>,         // effective identity; the namespace checks bind to it
    pub scope: SchemaScope,            // WHERE the declarations land; default Namespace
}

pub enum SchemaScope {
    Namespace = 0,                     // the tenant's base: every space resolves it
    Space = 1,                         // an overlay at the CALLER'S effective space
}
```

`scope` is a two-state selector, **never a space id** — a caller may only
declare an overlay at the space it is already acting as. `Space` with no
effective space is `INVALID_REQUEST`, not a silent downgrade: `SpaceId::NIL`
is byte-equal to the base key, so honouring it would write a declaration
meant to be private straight into the namespace base.

It is `Namespace` by default and omitted from the CBOR map when it holds that
value, so a base upload encodes byte-identically to the pre-field form and
every already-encoded request keeps its meaning. **A request that names a
space in `act_as` but omits `scope` still declares at the base** — the common
case, and the reason the opt-in is explicit rather than inferred from
`act_as`.

Semantics:

1. Parse `schema_document` per the §21 grammar. Syntax errors → `SCHEMA_INVALID` with line/column in `ErrorDetails`.
2. Diff against current schema. If breaking changes are present and `allow_breaking = false`, return `SCHEMA_MIGRATION_REQUIRED`.
3. If `dry_run`, return validation result without persisting.
4. Otherwise commit the new schema version inside a redb transaction: writes to the `schemas` table, allocates a fresh `schema_version`, increments the registry, fires `SCHEMA_UPDATED` event (see §"SUBSCRIBE events" below).

<!-- Scope of the overlay, stated precisely so nobody reads more into it. -->
**What `scope: Space` does and does not isolate.** Today the overlay is an
ENTITY-TYPE overlay: only the entity-type registry is space-keyed, so entity
type *resolution* is what a sibling space cannot see. A space-scoped upload
still bumps the **namespace's** `schema_version`, still merges its document
into the namespace's stored source (so `SCHEMA_GET` and other spaces'
validation contexts see its declarations), and still interns its predicates,
relation types and extractors namespace-wide. `SCHEMA_REPLACE` and
`SCHEMA_DROP` carry no `scope` field for the same reason: their destructive
deltas are keyed by namespace with no space component, so a "space-scoped"
drop would remove vocabulary every sibling space depends on.
5. Existing entities / statements / relations remain valid against the *old* schema version until a migration pass re-validates them. Migration policy is in [`../03_schema/`](../03_schema/00_purpose.md).

#### Response — `SchemaUploadResponse`

```rust
pub struct SchemaUploadResponse {
    pub namespace: String,             // the namespace the upload resolved to
    pub schema_version: u32,           // 0 if the upload was rejected
    pub validation_errors: Vec<SchemaValidationError>,
    pub backward_compatible: bool,
    pub migration_summary_blob: Vec<u8>,   // JSON-encoded migration summary; empty when there is none
}

pub struct SchemaValidationError {
    pub code: String,                  // the stable ValidationErrorCode name, e.g. "UnresolvedTypeRef"
    pub message: String,
    pub line: u32,
    pub column: u32,
    pub length: u32,                   // span length, so a client can underline the offending token
    pub severity: u8,                  // 0=info, 1=warning, 2=error
}
```

Every schema response carries `namespace`, because under `act_as` the namespace a request resolved to is not the one the connection is bound to, and a client that cannot see which namespace it just wrote has no way to confirm the delegation landed where it meant.

`SchemaValidationError` carries a stable `code` alongside the human `message`: the message is for an operator, the code is what a client branches on. The codes are the `ValidationErrorCode` set — `NamespaceMissing`, `NamespaceInvalidIdentifier`, `DuplicateDefinition`, `UnresolvedTypeRef`, `PredicateKindObjectMismatch`, `RelationCardinalitySymmetricInvalid`, `ExtractorMissingRequired`, `ExtractorDuplicateField`, `ExtractorInvalidConfig`, `AttributeUniqueOnRefType`, `DefaultIncompatibleWithType`, `NameInvalidIdentifier`, `NameTooLong`.

The migration summary travels as an opaque JSON **blob** rather than a typed field set. It is diagnostic output for an operator, its shape follows the diff rather than a fixed schema, and freezing it into wire types would mean a protocol change every time the diff gains a category.

#### Errors

- `SCHEMA_INVALID` — parse or validation failure (severity ≥ 2 in any error).
- `SCHEMA_MIGRATION_REQUIRED` — breaking change with `allow_breaking = false`.
- `INVALID_ARGUMENT` — `schema_document` empty or > 1 MiB (per [`./07_error_handling.md`](./07_error_handling.md)).

### SCHEMA_GET (0x0121)

#### Request — `SchemaGetRequest`

```rust
pub struct SchemaGetRequest {
    pub namespace: String,             // empty = the effective caller's own namespace
    pub version: u32,                  // 0 = latest
    pub act_as: Option<ActAs>,
}
```

#### Response — `SchemaGetResponse`

```rust
pub struct SchemaGetResponse {
    pub namespace: String,
    pub schema_version: u32,
    pub schema_document: String,       // verbatim DSL text as uploaded; empty for a programmatic upload
    pub source_blob: Vec<u8>,          // JSON encoding of the parsed Schema AST
    pub uploaded_at_unix_nanos: u64,
    pub validator_version: u32,
}
```

`schema_document` is the **verbatim** upload text, not a canonicalized re-render. A schema uploaded programmatically (as an AST rather than DSL text) has no source text, and the field is then empty — `source_blob` is the authoritative form in both cases, and is always present.

`validator_version` records which validator admitted the document, so a re-validation under a newer validator can be told apart from the original acceptance.

#### Errors

- `SCHEMA_INVALID` — `version_id != 0` and not in registry.

### SCHEMA_LIST (0x0122)

#### Request — `SchemaListRequest`

```rust
pub struct SchemaListRequest {
    pub namespace: String,             // empty = the effective caller's own namespace
    pub limit: u32,                    // 1..=100
    pub cursor: Vec<u8>,               // opaque keyset cursor
    pub act_as: Option<ActAs>,
}
```

#### Response — streaming, per-item `SchemaListItem`

```rust
pub struct SchemaListResponseFrame {
    pub namespace: String,
    pub items: Vec<SchemaListItem>,
    pub total: u32,
    pub next_cursor: Vec<u8>,          // empty when the listing is exhausted
    pub is_final: bool,
}

pub struct SchemaListItem {
    pub schema_version: u32,
    pub uploaded_at_unix_nanos: u64,
    pub validator_version: u32,
    pub has_source_text: bool,         // whether SCHEMA_GET can return verbatim DSL for this version
}
```

Stream contract: same as `ENTITY_LIST` ([`./08_typed_graph_frames.md`](./08_typed_graph_frames.md)) — streaming with EOS on the final frame, `is_final` mirroring the EOS flag.

There is no per-version `change_summary`. A listing is an index, not a changelog: the summary a version carries is the migration blob on the upload that created it, and re-deriving a prose diff at list time would mean re-parsing every stored version on every page.

### SCHEMA_VALIDATE (0x0123)

#### Request — `SchemaValidateRequest`

```rust
pub struct SchemaValidateRequest {
    pub schema_document: String,
    pub act_as: Option<ActAs>,
}
```

#### Response — `SchemaValidateResponse`

```rust
pub struct SchemaValidateResponse {
    pub namespace: String,
    pub would_be_version: u32,         // the version an upload of this document would allocate
    pub validation_errors: Vec<SchemaValidationError>,
}
```

The version field is named `would_be_version`, not `schema_version` set to 0. A validate commits nothing, so reporting "version 0" would be a sentinel the caller has to know to special-case; reporting the version an upload *would* allocate is the answer the caller actually wants, and its name says the upload did not happen.

#### Errors

- `INVALID_ARGUMENT` — empty or oversized document.

### EXTRACTOR_LIST (0x0124)

#### Request — `ExtractorListRequest`

```rust
pub struct ExtractorListRequest {
    pub act_as: Option<ActAs>,         // extractors are listed for the effective namespace
}
```

#### Response — streaming, per-item `ExtractorListItem`

```rust
pub struct ExtractorListResponseFrame {
    pub items: Vec<ExtractorListItem>,
    pub total: u32,
    pub is_final: bool,
}

pub struct ExtractorListItem {
    pub extractor_id: u32,
    pub namespace: String,             // the namespace this extractor was declared in
    pub name: String,                  // e.g. "pattern:role-assignment"
    pub kind: u8,                      // the extractor kind discriminant
    pub schema_version: u32,           // version this extractor binds to
    pub created_at_unix_nanos: u64,
}
```

The listing is registry metadata, not run statistics: there are no lifetime counters or last-run timestamps on it. Per-run outcomes are reported where they happen — on the `StageCompleted` event and in the extraction audit rows — rather than accumulated into a row that a listing would have to keep hot.

Cross-ref: [`../11_extractors/00_purpose.md`](../11_extractors/00_purpose.md) defines the tier model.

Extraction is always-on and cannot be paused: there is no `EXTRACTOR_DISABLE` /
`EXTRACTOR_ENABLE` op. A write with extraction disabled would leave the typed
graph empty and make graph-backed reads silently incoherent, so the toggle was
removed at both deploy-time (no `[extractors.<tier>].enabled` config) and
runtime (no wire op). `EXTRACTOR_LIST` remains for read-only introspection.

### Schema authorization

All schema-namespace opcodes (`0x0120–0x0123`) require **admin** permissions in the agent's `SpacePermissions` (see [`04_handshake.md`](./04_handshake.md)). `SCHEMA_GET`, `SCHEMA_LIST`, `EXTRACTOR_LIST` are readable by any authenticated agent. `SCHEMA_REPLACE` (`0x0127`) is admin-only and destructive; the handler rejects the call unless `force_drop_existing` is exactly `true` (see [`./03_opcodes.md`](./03_opcodes.md) §2.1).

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
| `RELATION_TOMBSTONED` | `RELATION_TOMBSTONE` (0x0153) | Relation |
| `SCHEMA_UPDATED` | `SCHEMA_UPLOAD` (0x0120) / `SCHEMA_REPLACE` (0x0127) / `SCHEMA_DROP` (0x0125) | Schema |
| `STAGE_COMPLETED` | each async write-derivation worker | Write pipeline |
| `EDGE_ADDED` / `EDGE_REMOVED` / `EDGE_SUPERSEDED` | `LINK` / `UNLINK` and the relation ops | Edges |

Extraction does **not** emit its own event pair. It reports through `STAGE_COMPLETED` with `stage_kind = Extractor`, carrying entity/statement/relation counts, a `StageAuditStatus`, and a `StageOutcome` that separates *ran and found nothing* (`Empty`) from *could not run* (`Failed`) — a distinction a completed/failed event pair could not express, since both of those cases produce zero counts.

Substrate event types (`ENCODED`, `FORGOTTEN`, `LINKED`, etc.) remain as defined in [`./05_frame_layouts.md`](./05_frame_layouts.md) and are not duplicated here.

### Event envelope

Every event rides as a `SUBSCRIBE_EVENT` frame (opcode `0x00B0`) with body shape:

```rust
pub struct SubscriptionEvent {
    pub event_type: EventType,
    pub memory_id: WireMemoryId,        // 0 when not memory-scoped
    pub session_id: u64,                // 0 = the default session
    pub text: String,                   // human-readable summary; may be empty
    pub kind: MemoryKindWire,           // for cognitive events; ignored for typed-graph
    pub salience: f32,                  // for cognitive events; 0.0 for typed-graph
    pub timestamp_unix_nanos: u64,      // server clock at emission
    pub lsn: u64,                       // monotonic per-shard LSN; subscriber resumes by LSN
    pub graph_payload: Option<GraphEventPayload>,  // see below
    pub edge_payload: Option<EdgeEventPayload>,    // EdgeAdded / EdgeRemoved / EdgeSuperseded
    pub stage_kind: Option<StageKind>,             // StageCompleted only
    pub stage_outcome: Option<StageOutcome>,       // StageCompleted only
    pub stage_payload: Option<StagePayload>,       // StageCompleted only
}
```

One envelope carries every event family; the four optional sidecars are populated per family and omitted otherwise. For cognitive events all four are `None` and the standard fields carry the data. The envelope and the full `EventType` discriminant table are defined in [`./05_frame_layouts.md`](./05_frame_layouts.md) §43.

### `GraphEventPayload` union

```rust
pub enum GraphEventPayload {
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
    RelationTombstoned(RelationTombstonedEvent),
    SchemaUpdated(SchemaUpdatedEvent),
}
```

Each variant is a CBOR map. The union is named for the layer it describes — the typed graph — and carries no extraction variants; extraction reports through `StageCompleted` (see the event type table above).

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

#### Statement events

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

#### Relation events

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

#### Schema events

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

- The shared `SubscriptionEvent` envelope carries an optional `graph_payload: Option<GraphEventPayload>` field.
- `ENTITY_CREATE` / `_UPDATE` / `_RENAME` handlers emit `EntityCreated` / `EntityUpdated` / `EntityRenamed` events post-commit.
- `ENTITY_MERGE` / `_UNMERGE` / `_TOMBSTONE` handlers emit their respective events.

Events are forward-only from their introduction; there is **no retroactive emission** for entities created before subscribers connect.

### Memory layer ↔ typed graph event correlation

When an extractor processes a freshly encoded memory it emits `STAGE_COMPLETED` with `stage_kind = Extractor`, whose `memory_id` correlates back to the `ENCODED` event for the same memory. Subscribers that want the chain "memory encoded → extractor ran → entities created" subscribe to both event families and join on `memory_id`. A subscription scoped with `filter.memory_ids = [that id]` narrows the stream to exactly one write's derivation.

Brain's `lsn` is shared across substrate and typed-graph events on the same shard, so the chain is replayable in causal order.

### SUBSCRIBE open questions

Notably:

- Whether `SubscriptionEvent` should be a sum type per family (substrate / typed-graph) rather than a struct-with-optional-payload. Currently flat-with-optional to keep one event envelope on the wire.
- Whether the `min_confidence` filter should apply to entity events too (not just statements). Currently no.

## Schema-optional mode

The typed-graph opcodes accept traffic in both modes: with or without a declared schema. When no schema is declared, predicates and relation types are open-vocabulary — they are interned on first use with origin `ImplicitFromWrite` (see [`../02_data_model/07_statement.md`](../02_data_model/07_statement.md) and [`../02_data_model/08_relation.md`](../02_data_model/08_relation.md)). When a schema is declared, it acts as a **strict validator** for that namespace: unknown qnames are rejected with `PredicateNotInSchema` / `RelationTypeNotInSchema`, and declared cardinalities are enforced.

Brain's cognitive primitives (the `0x00xx` opcode namespace) and the retrieval path are unaffected by schema state — retrieval is the default `RECALL` path for every deployment.

Schemaless ("open-vocabulary") and schema-declared ("strict") are both **first-class deployment postures**. A deployment that wants vector-schemaless behavior simply never calls the typed-graph opcodes.

### The schema declaration trigger

A schema is "declared" when a successful (`dry_run = false`) `SCHEMA_UPLOAD` (`0x0120`) commits at least one schema version. The declaration is **per-namespace**, recorded in the `schemas` redb table; it persists across server restarts.

State machine:

```
[open vocabulary] --SCHEMA_UPLOAD success--> [strict schema (version N)]
[strict schema (version N)] --SCHEMA_UPLOAD success--> [strict schema (version N+1)]
```

`SCHEMA_DROP` (`0x0125`, response `0x01A5`, admin-only) narrows the active schema by removing one declared predicate or relation type, gated by an in-use safety check; like `SCHEMA_REPLACE` it is routed through `submit(Write)` so the drop is WAL-durable and replayed on recovery. `SCHEMA_REPLACE` (`0x0127`) remains the destructive whole-schema escape hatch. Neither requires touching the underlying redb file.

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
| `RECALL_REQ` | works normally; runs the retrieval path (semantic + lexical + memory-edge graph) — retrieval is the default in both modes |
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

`RECALL_REQ` (`0x0021`) runs through the retrieval pipeline (semantic + lexical + memory-edge graph, fused via RRF) by default in every deployment. Clients always see these fields populated on `MemoryResult`:

- `contributing_retrievers: Vec<RetrieverNameWire>` — which retrievers ranked this memory.
- `fused_score: f32` — the post-RRF rank score.

Declaring a schema does not change the retrieval mode; it adds typed entity-anchored graph traversal as an additional path that the planner may select for the graph retriever. The `RecallResponseFrame` shape is identical in both modes.

RECALL is one verb with no client-side strategy switch. A request that carries a `txn_id` runs the transactional path internally (read-your-writes requires it); every other RECALL runs the retrieval path. Clients always observe the same `RecallResponseFrame` shape; the `contributing_retrievers` field is populated for retrieval responses and empty for the transactional case. See [`./05_frame_layouts.md`](./05_frame_layouts.md).

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

## Query introspection frames

Request/response body schemas for `0x0161`/`0x0162` — the query-introspection
opcodes. These are the **operator debug surface** over the shared retrieval
engine, not a client read path. They accept traffic regardless of schema state.

There is no client-facing bulk-query opcode. `RECALL_REQ` (`0x0021`) is the sole
primary read; it returns the answer as a membership shape (Single / Many / None
— see [`../05_operations/03_read_pipeline.md`](../05_operations/03_read_pipeline.md)),
not a ranked candidate list. `QUERY_EXPLAIN` renders the planner's execution
plan without running it; `QUERY_TRACE` executes and returns the per-retriever
candidate breakdown for diagnosing "why did the engine rank it this way." Both
accept the internal `QueryRequest` (retriever selection, fusion overrides,
filters) precisely because they are diagnostic tools. Structured typed-graph
lookup is served by the LIST ops (`STATEMENT_LIST`, `RELATION_LIST_FROM/TO`,
`ENTITY_RESOLVE` / `ENTITY_LIST`), not a fused ranked list.

Cross-references:
- [`../13_retrievers/`](../13_retrievers/00_purpose.md) — three retrievers (semantic / lexical / graph) + RRF fusion.
- [`./06_streaming.md`](./06_streaming.md) — substrate streaming model reused here.

### Query opcode index

| Opcode | Name | Section | Status |
|---|---|---|---|
| `0x0161` | `QUERY_EXPLAIN` | "QUERY_EXPLAIN" | implemented |
| `0x0162` | `QUERY_TRACE` | "QUERY_TRACE" | implemented |

Responses live at `0x01E1–0x01E2`.

`RECALL_REQ` (`0x0021`) runs the retrieval path by default in every deployment
(see §"Schema-optional mode" §"RECALL routing"). The engine that
`QUERY_EXPLAIN` / `QUERY_TRACE` introspect is the same one `RECALL` runs on.

### Shared query types

#### `QueryRequest`

```rust
pub struct QueryRequest {
    pub text: String,
    pub entity_anchor: Option<EntityId>,         // anchor the graph lane on one entity
    pub kind_filter: Vec<u8>,                    // StatementKind bytes (0 = Fact, 1 = Preference, 2 = Event); empty = no filter
    /// Predicate filter as canonical `"namespace:name"` qnames. Empty = no
    /// filter. The planner resolves each qname through the predicate
    /// registry per request — an unknown qname yields an empty result set
    /// in open-vocabulary mode and `PredicateNotInSchema` (0x004B) when a
    /// schema is active for the namespace.
    pub predicate_filter: Vec<String>,
    pub session_filter: Option<Vec<SessionId>>,  // None/empty = all sessions in the caller's space
    pub time_filter: Option<TimeRange>,          // { from_unix_ms, to_unix_ms }, each optional
    /// Bi-temporal time-travel anchor (record-time). When `Some(t)`,
    /// statement/relation results are filtered to the state the substrate
    /// believed at `t`, and `t` is the reference point for recency ranking.
    pub as_of_record_time_unix_nanos: Option<u64>,
    pub confidence_min: Option<f32>,
    pub include_tombstoned: bool,
    pub include_superseded: bool,
    pub limit: u32,
    pub retrievers: RetrieverSelection,
    pub fusion_config: Option<FusionConfig>,
    pub request_id: RequestId,
}

pub enum RetrieverSelection {
    Auto,                                        // the planner picks the lanes
    Explicit(Vec<Retriever>),                    // Semantic = 0 | Lexical = 1 | Graph = 2
}

pub struct FusionConfig {
    pub k: u32,                                  // the RRF k constant
    pub semantic_weight: f32,
    pub lexical_weight: f32,
    pub graph_weight: f32,
}
```

`QueryRequest` is carried inside `QueryExplainRequest` / `QueryTraceRequest`, each of which adds the shared `act_as` field:

```rust
pub struct QueryExplainRequest { pub query: QueryRequest, pub act_as: Option<ActAs> }
pub struct QueryTraceRequest   { pub query: QueryRequest, pub act_as: Option<ActAs> }
```

Semantics (this request is the introspection input to `QUERY_EXPLAIN` / `QUERY_TRACE`, never a client read):

- There is **no query DSL**. The input is the same structured filter set the read pipeline already takes — text cue plus entity / predicate / kind / time / confidence conditions — so EXPLAIN and TRACE describe the real planner rather than a parallel language.
- `retrievers` is a sum type, not three booleans: `Auto` lets the planner choose the lanes, `Explicit` pins them. An empty `Explicit` list is `InvalidArgument` — disabling every lane is not a query.
- `fusion_config` overrides the RRF constant and the per-lane weights, for isolating one lane's contribution to the ranking. `None` uses the deployment's configured fusion settings.
- `limit` is the **final fused** cap. Per-retriever depth is derived from it by the planner; `QUERY_EXPLAIN` ignores it (no execution).

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

The `MemoryResult` substrate type ([`./05_frame_layouts.md`](./05_frame_layouts.md)) is reused unchanged. Typed-graph retrieval results that include memories carry that type's existing fields; the post-schema additions (`contributing_retrievers`, `fused_score`) live on the **outer** `QueryResultItem` rather than mutating `MemoryResult`. Schemaless clients ignore the outer fields and consume `MemoryResult` directly.

#### `contributing_retrievers` bit-flag values

```rust
pub const RETRIEVER_SEMANTIC: u8 = 0b001;
pub const RETRIEVER_LEXICAL: u8  = 0b010;
pub const RETRIEVER_GRAPH: u8    = 0b100;
```

A `QueryResultItem.contributing_retrievers = vec![0b011]` means semantic + lexical ranked this result; graph did not. Each contributor contributes one entry to the vector with a separate flag — `vec![0b001, 0b010]` is also valid encoding meaning "two separate retriever hits" with provenance.

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

Executes the plan `QUERY_EXPLAIN` would render and returns the results **plus
per-retriever debug info**. Treats `include_trace = true` internally regardless
of the request's setting. Operator diagnostic only — not a client read.

#### Request

`QueryRequest` directly.

#### Response — streaming with extended tail

Multiple per-item frames sharing `stream_id`, each a `QueryResultItem` plus a
`trace` field, followed by a tail frame carrying the extended trace:

```text
S → C  frame: opcode=0x01E2 stream_id=N        body: QueryResultItem  (intermediate)
...
S → C  frame: opcode=0x01E2 stream_id=N EOS    body: QueryTraceTail   (tail)
```

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

#### Errors

- `QUERY_TIMEOUT` (substrate `Unavailable`) — wall budget exceeded. Tail frame carries whatever results were ready; `QueryResultTail.truncated_by = 2`.
- `QUERY_OVER_BUDGET` (substrate `ResourceExhausted`) — per-shard memory or candidate-pool cap blown. Frame stream ends without an EOS tail; an `ERROR` frame closes the stream.
- `PredicateNotInSchema` (0x004B) — strict mode only; `filters.predicate_filter` contains a qname not declared in the active schema.
- `RelationTypeNotInSchema` (0x004C) — strict mode only; the DSL or graph step referenced an unknown relation type.
- `RetrievalUnavailable` (0x0083) — a required retriever component is not currently servable (e.g. inside a transaction, during index rebuild).
- `INVALID_ARGUMENT` — DSL parse failure, `top_k > 1000`, all retrievers disabled.

#### Cancellation

The operator sends `CANCEL_STREAM` (`0x0050`) with the offending `stream_id`. Server emits a `CANCEL_STREAM_ACK` (`0x00D0`) on a different stream; the trace's frame stream ends with an EOS-flagged empty tail.

#### Performance note

`QUERY_TRACE` is **noticeably slower** than a plain `RECALL` because of the
trace bookkeeping — it exists to diagnose the engine, not to serve reads. The
production read path is `RECALL` (`0x0021`).

### Idempotency for the introspection ops

`QUERY_TRACE` with `request_id != [0;16]` populates the idempotency cache (same
shape as substrate; 24h TTL). Cached responses are byte-identical, **including**
the full streamed result sequence. Re-issuing the same `request_id` replays the
entire stream from cache — useful for reproducibility in test suites. An
operator wanting **fresh** trace output every time passes `request_id = [0;16]`.
`QUERY_EXPLAIN` does not execute, so idempotency is moot.

### Introspection transactions

`QueryRequest.txn_id != [0;16]` makes `QUERY_TRACE` observe a read snapshot that
includes the transaction's pending writes — same semantics as substrate
`RecallRequest.txn_id`. This lets an operator trace how the engine would rank a
transaction's not-yet-committed state.

### Multi-shard fan-out

A `QUERY_TRACE` typically fans out to **all** shards unless filters scope it to
a specific shard (e.g. `filters.entity_type_id` + a subject filter the planner
can route).

Per-shard results are streamed to the coordinator (the caller's bound shard); the
coordinator runs RRF fusion across the union and streams the final fused result.
The per-shard streaming back-pressure handling applies — if one shard is slow,
the coordinator buffers within its budget. `QUERY_TRACE`'s
`RetrieverTrace.debug_notes` may include per-shard breakdown when run on
multi-shard deployments.

### Query introspection open questions

Notably:

- Cross-shard error aggregation: what if 1 of 8 shards fails? Currently the planner returns a partial result with a warning. Should there be a strict-mode flag for "all-or-nothing"?
- Planner cost model exposure: should `QueryPlanStep.cost` be `f32` (current) or a richer structured cost?

## Admin frames

Request/response body schemas for `0x0170–0x0177` — typed-graph admin operations. These are operator-facing: index rebuilds, ambiguity resolution, audit inspection, backfill, job status.

All opcodes in this range require **admin** permissions (per §"Schema authorization" above). Authorization failures return `AdminPermissionRequired` (substrate `Authorization`).

Cross-references:
- [`../11_extractors/00_purpose.md`](../11_extractors/00_purpose.md) — extractor backfill semantics.
- [`../15_background_workers/00_purpose.md`](../15_background_workers/00_purpose.md) — background workers that run jobs.
- [`../02_data_model/06_entity_lifecycle.md`](../02_data_model/06_entity_lifecycle.md) §"Confidence-banded behavior" — audit-driven ambiguity resolution.

### Admin opcode index

| Opcode | Name | Section |
|---|---|---|
| `0x0178` | `ADMIN_LIST_PENDING_CONTRADICTIONS` | "ADMIN_LIST_PENDING_CONTRADICTIONS" |

Responses live at `0x01F0–0x01FF`; this one's is `0x01F8`. `0x0170`–`0x0177` are unallocated.

The typed-graph admin range holds exactly one operation. The operator surface that exists alongside it lives in the **substrate** admin range (`0x00xx`), not here: backfill is `ADMIN_BACKFILL` (`0x006E`) with `ADMIN_BACKFILL_CANCEL` (`0x006F`), and snapshots, restore, integrity check and embedding migration are `0x0061`–`0x0064`. See [`03_opcodes.md`](03_opcodes.md) §1.6 and [`05_frame_layouts.md`](05_frame_layouts.md) §35.

### ADMIN_LIST_PENDING_CONTRADICTIONS (0x0178)

Lists open Fact-vs-Fact contradictions awaiting operator reconciliation
(rows in `statement_contradiction_audit`; see §10/02 §18.6a). Admin-only.

Request body (CBOR map):

```
{ limit: u32 }   // 0 = server default
```

Response (`0x01F8`, single frame — the queue is small, not streamed):

```
{ contradictions: [ ContradictionAuditView ] }

ContradictionAuditView {
    audit_id:                    bytes[16],
    subject_id:                  bytes[16],
    predicate_id:                u32,
    contradicting_statement_ids: [ bytes[16] ],
    detected_at_unix_nanos:      u64,
    outcome:                     u8,   // 0 = Pending (only Pending is returned)
}
```

The server re-checks liveness against `statements` on each call and
lazily resolves rows that no longer hold, so the list reflects only
currently-live contradictions.

### Backfill runs

`ADMIN_BACKFILL` (`0x006E`) is the one long-running admin operation, and it does not use a generic "job" surface. The request names a `BackfillScope` (`All`, or an inclusive `MemoryId` range) and a list of `extractor_ids`, and the response returns a `backfill_id` plus an immediate `BackfillProgress` snapshot. `ADMIN_BACKFILL_CANCEL` (`0x006F`) stops an in-flight run by that id and returns the final snapshot.

```
BackfillProgress {
    running:                            bool,
    completed:                          u64,
    failed:                             u64,
    skipped_already_completed:          u64,
    last_processed_memory_id_present:   bool,
    last_processed_memory_id:           MemoryId,
}
```

Semantics worth knowing:

- **Resumable by construction.** Every `(memory, extractor)` pair has its own row in the shared `worker_checkpoints` table, so a restart mid-run resumes rather than re-extracting what already completed. `skipped_already_completed` is counted apart from `completed` so a re-run over a finished range reports honestly instead of looking like fresh work.
- **Per-item resilience.** An item that fails is retried up to three times and then counted in `failed`; it never aborts the run. One bad memory does not cost the operator the whole backfill.
- **Re-extraction goes through the live path.** Memory text is durable in the `texts` table, so a backfill does not need the original `ENCODE` frame. Each live item is re-enqueued on the same durable `extraction_queue` the write path uses, inside the same write txn as its checkpoint; the per-shard `ExtractorWorker` drains it on its next cycle and re-derived rows flow through the normal supersession path. Backfill itself never deletes prior derivations.
- **`dry_run`** marks items complete without enqueueing — plan validation only.
- A memory already extracted under the current schema is re-run only when the operator clears the `ExtractorWorker`'s `skip_already_extracted` gate. That gate, not backfill, is the forced-re-extraction knob.
