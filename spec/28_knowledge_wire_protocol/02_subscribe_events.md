# 28.09 Knowledge-Layer SUBSCRIBE Events

The substrate's `SUBSCRIBE_REQ` (`0x0030`) / `SUBSCRIBE_EVENT` (`0x00B0`) primitive ([§03/05 §1.3](../03_wire_protocol/05_opcodes.md), [§07 §SubscribeRequest](../03_wire_protocol/07_request_frames.md)) carries change-feed events for the **substrate** (memory encoded / forgotten / linked). When a schema is declared, the same primitive also carries **knowledge-layer** events.

This file specifies the knowledge events: their on-wire shapes, when the server emits them, and how subscribers filter them.

Cross-references:
- [`../03_wire_protocol/07_request_frames.md`](../03_wire_protocol/07_request_frames.md) §SubscribeRequest — base SUBSCRIBE shape.
- [`./01_entity_frames.md`](./01_entity_frames.md), [`./05_statement_frames.md`](./05_statement_frames.md), [`./06_relation_frames.md`](./06_relation_frames.md) — opcodes that *emit* the events below.
- [`../27_knowledge_workers/00_purpose.md`](../27_knowledge_workers/00_purpose.md) — extractor / consolidation workers that emit `EXTRACTION_*` and `SCHEMA_UPDATED`.

## 1. Event type table

| Event type | Emitted by | Family | Phase |
|---|---|---|---|
| `ENTITY_CREATED` | `ENTITY_CREATE` (0x0130) | Entity | 16.6c (implemented; emission lands phase 16.7) |
| `ENTITY_UPDATED` | `ENTITY_UPDATE` (0x0132) | Entity | 16.7 |
| `ENTITY_RENAMED` | `ENTITY_RENAME` (0x0133) | Entity | 16.7 |
| `ENTITY_MERGED` | `ENTITY_MERGE` (0x0134) | Entity | 16.7 |
| `ENTITY_UNMERGED` | `ENTITY_UNMERGE` (0x0135) | Entity | 16.7 |
| `ENTITY_TOMBSTONED` | `ENTITY_TOMBSTONE` (0x0138) | Entity | 16.7 |
| `STATEMENT_CREATED` | `STATEMENT_CREATE` (0x0140) | Statement | 17 |
| `STATEMENT_SUPERSEDED` | `STATEMENT_SUPERSEDE` (0x0142) | Statement | 17 |
| `STATEMENT_TOMBSTONED` | `STATEMENT_TOMBSTONE` (0x0143) | Statement | 17 |
| `RELATION_CREATED` | `RELATION_CREATE` (0x0150) | Relation | 18 |
| `RELATION_SUPERSEDED` | `RELATION_SUPERSEDE` (0x0152) | Relation | 18 |
| `EXTRACTION_COMPLETED` | extractor worker | Extractor | 22 |
| `EXTRACTION_FAILED` | extractor worker | Extractor | 22 |
| `SCHEMA_UPDATED` | `SCHEMA_UPLOAD` (0x0120) | Schema | 19 |

Substrate event types (`ENCODED`, `FORGOTTEN`, `LINKED`, etc.) remain as defined in [`../03_wire_protocol/07_request_frames.md`](../03_wire_protocol/07_request_frames.md) and are not duplicated here.

## 2. Event envelope

Every event rides as a `SUBSCRIBE_EVENT` frame (opcode `0x00B0`) with body shape:

```rust
pub struct SubscriptionEvent {
    pub event_type: EventTypeWire,
    pub memory_id: WireMemoryId,        // [0;16] when not memory-scoped
    pub context_id: u64,                // 0 = no context
    pub text: String,                   // human-readable summary; may be empty
    pub kind: MemoryKindWire,           // for substrate events; ignored for knowledge
    pub salience: f32,                  // for substrate; 0.0 for knowledge
    pub timestamp_unix_nanos: u64,      // server clock at emission
    pub lsn: u64,                       // monotonic per-shard LSN; subscriber resumes by LSN
    pub knowledge_payload: Option<KnowledgeEventPayload>,  // §3 below
}
```

The substrate's `SubscriptionEvent` (defined in [§07](../03_wire_protocol/07_request_frames.md)) is extended with the optional `knowledge_payload`. For substrate-emitted events the field is `None`; for knowledge-emitted events it carries the typed body.

## 3. `KnowledgeEventPayload` union

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

Each variant is rkyv-archivable. Variants for ops not yet implemented (phase 17+) carry typed shells; only the entity variants land in phase 16.7's code (in parallel with the merge / tombstone / etc. opcodes).

### 3.1 Entity events

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

### 3.2 Statement events (spec-only, phase 17)

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

### 3.3 Relation events (spec-only, phase 18)

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

### 3.4 Extractor events (spec-only, phase 22)

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
    pub error_code: u8,                 // §28 error code from §10
    pub error_message: String,
}
```

### 3.5 Schema events (spec-only, phase 19)

```rust
pub struct SchemaUpdatedEvent {
    pub from_version: u32,
    pub to_version: u32,
    pub backward_compatible: bool,
}
```

## 4. Emission semantics

### 4.1 Atomicity with the originating write

Events are emitted **after** the originating opcode's redb commit has succeeded. The chain of guarantees:

1. WAL record written + fsynced (per [§05/08](../05_storage_arena_wal/08_recovery.md)).
2. Redb transaction committed.
3. Event broadcast to the per-shard subscription registry.
4. ACK to the originating opcode's response stream.

If a subscriber is slow or backpressured, the event is buffered up to `max_inflight` (per the subscriber's `SUBSCRIBE_REQ.max_inflight`). Exceeding the buffer triggers per-subscriber back-pressure handling per [§03/09](../03_wire_protocol/09_streaming.md) — not a substrate-wide stall.

### 4.2 Ordering within a shard

Events for entities / statements / relations / extractor outcomes on the **same shard** are emitted in the order of their LSN. Subscribers see a single monotonic LSN stream per shard (substrate + knowledge interleaved).

Cross-shard ordering is **not** guaranteed. Subscribers that need a cross-shard total order use the connection layer's fan-in semantics; the per-shard LSN is the local order signal.

### 4.3 Idempotency on resume

Subscribers resume by replaying from `from_lsn` (carried in `SUBSCRIBE_REQ`). The substrate's LSN allocator (per shard, monotonic) guarantees that re-delivery matches the original byte stream — same `SubscriptionEvent` body, same `lsn`. Clients should dedupe locally if they reprocess on resume.

### 4.4 No "intermediate state" events

Multi-step operations (e.g. `ENTITY_MERGE` performs 7+ redb table writes) emit exactly **one** event for the whole operation. There is no partial-progress event stream. The `EntityMergedEvent.statements_rerouted` / `.relations_rerouted` counts let subscribers see the scope of the change without watching it unfold.

## 5. Subscriber filters

`SUBSCRIBE_REQ` carries a `SubscriptionFilter` struct ([§03/07 §SubscribeRequest](../03_wire_protocol/07_request_frames.md)). For knowledge events the filter is extended with:

```rust
pub struct KnowledgeSubscriptionFilter {
    pub event_types: Option<Vec<EventTypeWire>>,    // None = all
    pub entity_types: Option<Vec<u32>>,             // None = all (matches EntityTypeId on entity events)
    pub entity_ids: Option<Vec<WireUuid>>,          // None = all
    pub predicates: Option<Vec<String>>,            // None = all (statement / extraction events)
    pub min_confidence: f32,                        // 0.0 = no filter
}
```

The server applies the filter at emission time (per [§03/07 §SubscribeRequest](../03_wire_protocol/07_request_frames.md) §"server-side filtering") — non-matching events are discarded before broadcast, not just at the subscriber's edge. This avoids amplifying high-volume events to uninterested subscribers.

## 6. Phase 16.6c interim state

As of phase 16.6c (entity wire ops landed), the **emission path is not wired**. Handlers commit redb writes but do not call into `ctx.events.broadcast(...)`. This is intentional — phase 16.7 wires the emission path along with merge / unmerge / tombstone, so all entity event variants land together.

Subscribers that subscribe today receive substrate events only. The `KnowledgeEventPayload::None` invariant holds for all current emissions.

Tracked as an open item in [`./09_open_questions.md`](./09_open_questions.md): whether phase 16.7 should also retroactively emit `ENTITY_CREATED` for entities created via 16.6c's `ENTITY_CREATE` — leaning **no** (events are forward-only from their introduction).

## 7. Memory layer ↔ knowledge layer event correlation

When an extractor (phase 20+) processes a freshly encoded memory and emits `EXTRACTION_COMPLETED`, the event's `memory_id` field correlates back to the substrate `ENCODED` event for the same memory. Subscribers that want the chain "memory encoded → extractor ran → entities created" subscribe to both event families and join on `memory_id`.

The substrate's `lsn` is shared across substrate and knowledge events on the same shard, so the chain is replayable in causal order.

## 8. Open questions

See [`./09_open_questions.md`](./09_open_questions.md). Notably:

- Whether `SubscriptionEvent` should be a sum type per family (substrate / knowledge) rather than a struct-with-optional-payload. Currently flat-with-optional for backward compat with substrate clients.
- Whether `min_confidence` filter should apply to entity events too (not just statements / extractions). Currently no.
