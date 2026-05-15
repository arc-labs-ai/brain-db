# 28.11 Design Choices

Why §28 looks the way it does. Each subsection states the alternative considered, the choice made, and the reasoning. Mirrors the substrate's [`../03_wire_protocol/01_design_choices.md`](../03_wire_protocol/01_design_choices.md).

## 1. Wire opcode is `u16`, split into namespace + index

### Alternatives

(a) Keep `u8` opcode, place knowledge ops in the gaps (`0x03–0x09`, `0x32–0x39`, etc.).
(b) Dispatch knowledge ops behind a single substrate opcode (e.g. `0x70 KNOWLEDGE_OP`) with a sub-opcode byte in the body.
(c) Widen the opcode to `u16` and partition by high byte.

### Choice: (c).

### Reasoning

The substrate's §03 opcode table was already dense (`0x20–0x29` cognitive, `0x30–0x31` subscribe, `0x40–0x42` txn, `0x50` cancel, `0x60–0x69` admin, `0x70–0x7F` reserved). The §28 draft assigned `0x20–0x77` to knowledge ops — direct collision with the substrate.

- (a) **renumber to fit gaps** loses §28's mnemonic (`0x3x = entity`) and produces non-contiguous family ranges. ~30+ knowledge opcodes don't fit cleanly in the ~38 free bytes without spreading across `0x03`, `0x0F`, `0x27`, etc.
- (b) **sub-opcode under one substrate byte** adds one byte per knowledge frame and a separate dispatch entry. Workable but always-on overhead even when reading a single `ENTITY_GET`.
- (c) **u16 with namespace prefix** is one-time cost (the `u8 → u16` migration) for permanent clarity. Substrate ops kept their byte values (`ENCODE_REQ = 0x0020`); knowledge ops live at `0x01xx`; future namespaces (statements-only, audit, etc.) have `0x02xx`–`0xFFxx` reserved.

The migration was free because v1.0 hasn't shipped — the [pre-v1.0 compatibility policy](../03_wire_protocol/12_versioning.md) §0 explicitly permits incompatible wire changes before tag.

### Cost paid

- 1 byte per frame.
- All existing substrate code call-sites updated in phase 16.6b (~256 sites across `brain-server`, `brain-sdk-rust`, tests).
- §03's `flags` field shrank from `u16` to `u8` to reclaim the byte. Only EOS / MPL / CMP bits were ever used; the shrink lost nothing.

## 2. Knowledge errors ride the substrate ERROR frame

### Alternatives

(a) Knowledge-namespace ops define their own `KNOWLEDGE_ERROR` (`0x01FF`) opcode with a §28-specific body.
(b) Reuse the substrate's `ERROR` (`0x00FF`) frame, extending `ErrorCodeWire` with knowledge variants.

### Choice: (b).

### Reasoning

Two ERROR shapes mean SDK clients write two error-handling paths. Reuse means one path with new enum variants — the cheapest extension point. The cost is coordinated edits to [`../03_wire_protocol/10_errors.md`](../03_wire_protocol/10_errors.md) when new §28 codes appear; for v1.0 the surface is small enough that this is fine.

Migration path detailed in [`./03_errors.md`](./03_errors.md) §2: Strategy B (interim fallback to existing substrate codes) lets handlers ship before §03/10 is extended; Strategy A (extension) is the long-term goal.

## 3. SUBSCRIBE events extend the substrate event envelope

### Alternatives

(a) Knowledge events use a separate opcode (`KNOWLEDGE_EVENT = 0x01B0`-ish) and parallel SUBSCRIBE channel.
(b) Knowledge events extend the substrate's `SubscriptionEvent` body with an optional `knowledge_payload` field.

### Choice: (b).

### Reasoning

Subscribers that want both substrate and knowledge events would maintain two streams under (a) — twice the LSN tracking, twice the reconnect logic. (b) keeps a single per-shard LSN stream with optional typed payload. Substrate-only subscribers ignore the optional field; knowledge subscribers dispatch on it.

The cost is wire bandwidth — substrate-only frames carry an extra 1-2 bytes for the `Option<KnowledgeEventPayload>::None` tag. Negligible.

## 4. Optional `WireUuid` fields use `[0; 16]` sentinel

### Alternatives

(a) `Option<[u8; 16]>` archived directly via rkyv.
(b) Bare `[u8; 16]` with the all-zeros value as "absent" sentinel.

### Choice: (b).

### Reasoning

rkyv 0.7's `Option<[u8; 16]>` archive shape requires the `check_bytes` derive to recurse into the optional discriminant, which works but produces noisier archive types and slightly larger payloads (one byte for the discriminant per Option). UUIDv7's first 48 bits are a unix-ms timestamp, so the all-zero value is *unreachable* — collision is impossible by construction.

Used uniformly for `EntityId`, `StatementId`, `RelationId`, `AuditId`, etc., across §28's read-side projections. Documented per-struct.

## 5. Replace-not-merge on `ENTITY_UPDATE` / `STATEMENT_*` collection fields

### Alternatives

(a) Wire shape carries deltas (add / remove lists for `aliases`, `properties`, etc.).
(b) Wire shape carries the full new state; server diffs against current.

### Choice: (b) for v1.0.

### Reasoning

Delta encoding lets clients send just the changed parts but doubles the protocol's surface area (every collection field needs add / remove / replace variants). Full-replace is simpler, matches the underlying `brain-metadata::entity_ops` API, and avoids edge cases (what if the delta references an alias the server has already removed?).

The cost is wire bandwidth on updates with many unchanged aliases. Acceptable until profiling shows otherwise. Tracked in [`./09_open_questions.md`](./09_open_questions.md) Q-future if/when needed.

## 6. Streaming reuses substrate per-frame model

### Alternatives

(a) Knowledge-specific stream envelope: `STREAM_START` (metadata frame) → N × `STREAM_ITEM` → `STREAM_END`.
(b) Reuse the substrate's per-frame model: each result is one frame with shared `stream_id`, EOS on the last.

### Choice: (b).

### Reasoning

The substrate already implements (b) for `RECALL_RESP`, `PLAN_RESP`, `REASON_RESP`, `ADMIN_MIGRATE_EMBEDDINGS_RESP`, `ADMIN_LIST_TOMBSTONED_RESP`. Knowledge list / query ops have the same shape (one logical result per frame, EOS terminates). Adding a separate envelope would mean two distinct streaming models in one server.

The original §28 draft mentioned `STREAM_START` / `STREAM_ITEM` / `STREAM_END`; that's obsolete. See [`./09_open_questions.md`](./09_open_questions.md) §"Resolved" R4.

## 7. Opaque attribute / property / evidence blobs

### Alternatives

(a) Wire structs decode attributes / properties into typed `BTreeMap<String, Value>` at the wire layer.
(b) Wire structs carry rkyv-encoded blobs; the schema validator unpacks them in the handler.

### Choice: (b) for v1.0.

### Reasoning

The schema isn't always known at the wire layer (the connection layer doesn't hold the schema registry). Pushing schema-aware decode into the wire forces a circular dependency or eager schema-replica caching at every dispatch point. Opaque blobs let the wire ship without knowing the schema; the handler (which holds `ctx.executor.metadata`) does the typed decode.

The cost is later error reporting on malformed attribute bags. Phase 19+ may revisit; tracked in [`./09_open_questions.md`](./09_open_questions.md) Q7.

## 8. Idempotency keying

### Choice

`(agent_id, opcode_u16, request_id, blake3(payload_bytes))` — substrate's existing scheme with the knowledge opcode taking the same slot. 24h TTL. See [`./01_entity_frames.md`](./01_entity_frames.md) §13.

### Why not just `(agent_id, request_id)`?

Defends against a client recycling a `request_id` for a different operation. The `blake3(payload_bytes)` digest catches the "same id, different params" case as a structured `Conflict` rather than silently returning the stale cached response.

## 9. UUIDv7 everywhere for first-class IDs

### Choice

`EntityId`, `StatementId`, `RelationId`, `AuditId`, `MergeId`, `EvidenceOverflowId`, `request_id` are all UUIDv7. `EntityTypeId`, `RelationTypeId`, `PredicateId`, `ExtractorId` are `u32` interned ids.

### Reasoning

UUIDv7 is timestamp-prefixed → naturally sorts by creation order, which helps redb's b-tree locality. The all-zero sentinel (§4) is reachable only if a UUIDv7 implementation is broken. Interned u32 ids cover small-cardinality registries where the timestamp prefix would waste bytes.

## 10. Schema-optional is a deployment posture, not a degradation

### Choice

Substrate-only deployments are **first-class**, not a "legacy" or "minimal" mode. The substrate works fully without a schema; the knowledge layer is opt-in via `SCHEMA_UPLOAD`.

### Reasoning

Two product lines (vector substrate, knowledge graph) share a single codebase and a single deployment story. Operators that want only vectors don't pay for knowledge features at all (no extractor budget, no LLM cache, no entity tables). Operators that want both flip one switch. The schema-optional design [`./08_schema_optional_mode.md`](./08_schema_optional_mode.md) is what makes this clean.
