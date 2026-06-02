# 04.03 Opcodes

The opcode is a big-endian `u16` in the frame header (bytes 5–6). The high byte is a **namespace** (`0x00` substrate primitives — connection management, cognitive ops, txn, subscribe, admin; `0x01` typed-graph operations — schema, entity, statement, relation, query, extraction admin; `0x02–0xFF` reserved). Within a namespace the low byte's high bit selects direction: low byte `< 0x80` is server-bound (C → S); low byte `≥ 0x80` is client-bound (S → C).

> The opcode is a `u16` and `flags` is a `u8` (see [`02_wire_format.md`](02_wire_format.md) §1). Substrate opcodes use the low byte; `ENCODE_REQ` is `0x0020`. Typed-graph opcodes use the `0x01xx` namespace; `ENTITY_CREATE` is `0x0130`. The wire version is documented in §"Versioning" below.

## 1. The complete table

### 1.1 Connection management

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x0001 | `HELLO` | C → S | Initial frame; client identity and supported versions |
| 0x0081 | `WELCOME` | S → C | Reply to HELLO; server identity, negotiated version, session_id |
| 0x0002 | `AUTH` | C → S | Authentication credentials |
| 0x0082 | `AUTH_OK` | S → C | Authentication success; bind to agent_id |
| 0x0010 | `PING` | C → S | Keepalive |
| 0x0090 | `PONG` | S → C | Response to PING |
| 0x0091 | `SERVER_PING` | S → C | Server-initiated keepalive |
| 0x0011 | `CLIENT_PONG` | C → S | Response to SERVER_PING |
| 0x001F | `BYE` | bidirectional | Graceful close |

### 1.2 Operations

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x0020 | `ENCODE_REQ` | C → S | Encode a memory |
| 0x00A0 | `ENCODE_RESP` | S → C | Encode result (memory_id) |
| 0x0021 | `RECALL_REQ` | C → S | Recall memories matching a cue |
| 0x00A1 | `RECALL_RESP` | S → C | Recall result (streaming) |
| 0x0022 | `PLAN_REQ` | C → S | Plan from start to goal |
| 0x00A2 | `PLAN_RESP` | S → C | Plan result (streaming) |
| 0x0023 | `REASON_REQ` | C → S | Reason about an observation |
| 0x00A3 | `REASON_RESP` | S → C | Reason result (streaming) |
| 0x0024 | `FORGET_REQ` | C → S | Forget a memory |
| 0x00A4 | `FORGET_RESP` | S → C | Forget result (acknowledgment) |
| 0x0025 | `LINK_REQ` | C → S | Create an edge between two memories |
| 0x00A5 | `LINK_RESP` | S → C | Link acknowledgment |
| 0x0026 | `UNLINK_REQ` | C → S | Remove an edge between two memories |
| 0x00A6 | `UNLINK_RESP` | S → C | Unlink acknowledgment |
| 0x002A | `ENCODE_VECTOR_DIRECT_REQ` | C → S | Power-user encode with pre-supplied vector |
| 0x00AA | `ENCODE_VECTOR_DIRECT_RESP` | S → C | (Same response shape as ENCODE_RESP) |

### 1.3 Subscription

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x0030 | `SUBSCRIBE_REQ` | C → S | Subscribe to memory events |
| 0x00B0 | `SUBSCRIBE_EVENT` | S → C | Push event matching subscription |
| 0x0031 | `UNSUBSCRIBE_REQ` | C → S | Stop a subscription |
| 0x00B1 | `UNSUBSCRIBE_RESP` | S → C | Acknowledgment |

### 1.3a Capability introspection

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x0032 | `GET_CAPABILITIES_REQ` | C → S | Query the shard's enabled features (rerank, extractor tiers, schema namespaces, vector dim) |
| 0x00B2 | `GET_CAPABILITIES_RESP` | S → C | Capability descriptor |

`GET_CAPABILITIES` is available to every authenticated client — not admin-only. Clients call it after `WELCOME` to avoid issuing requests the shard can't serve (e.g. `request.rerank = true` against a shard with `rerank.enabled = false`). The response carries explicit booleans per capability plus the set of currently-active schema namespaces; there is no "feature flag" fallback — capabilities that are disabled by config or unavailable on this shard are surfaced concretely.

### 1.4 Transactions

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x0040 | `TXN_BEGIN` | C → S | Begin transaction |
| 0x00C0 | `TXN_BEGIN_RESP` | S → C | Confirm transaction id |
| 0x0041 | `TXN_COMMIT` | C → S | Commit transaction |
| 0x00C1 | `TXN_COMMIT_RESP` | S → C | Confirm commit |
| 0x0042 | `TXN_ABORT` | C → S | Abort transaction |
| 0x00C2 | `TXN_ABORT_RESP` | S → C | Confirm abort |

### 1.5 Stream control

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x0050 | `CANCEL_STREAM` | C → S | Cancel an in-flight stream |
| 0x00D0 | `CANCEL_STREAM_ACK` | S → C | Acknowledge cancellation |

### 1.6 Admin operations

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x0060 | `ADMIN_STATS_REQ` | C → S | Request stats |
| 0x00E0 | `ADMIN_STATS_RESP` | S → C | Stats response |
| 0x0061 | `ADMIN_SNAPSHOT_REQ` | C → S | Take a snapshot |
| 0x00E1 | `ADMIN_SNAPSHOT_RESP` | S → C | Snapshot result |
| 0x0062 | `ADMIN_RESTORE_REQ` | C → S | Restore from snapshot |
| 0x00E2 | `ADMIN_RESTORE_RESP` | S → C | Restore result |
| 0x0063 | `ADMIN_INTEGRITY_CHECK_REQ` | C → S | Run integrity check |
| 0x00E3 | `ADMIN_INTEGRITY_CHECK_RESP` | S → C | Integrity result |
| 0x0064 | `ADMIN_MIGRATE_EMBEDDINGS_REQ` | C → S | Re-embed all memories |
| 0x00E4 | `ADMIN_MIGRATE_EMBEDDINGS_RESP` | S → C | Migration progress (streaming) |
| 0x0065 | `ADMIN_CREATE_CONTEXT_REQ` | C → S | Create a context with metadata |
| 0x00E5 | `ADMIN_CREATE_CONTEXT_RESP` | S → C | Context creation ack |
| 0x0066 | `ADMIN_RENAME_CONTEXT_REQ` | C → S | Rename a context |
| 0x00E6 | `ADMIN_RENAME_CONTEXT_RESP` | S → C | Rename ack |
| 0x0067 | `ADMIN_MOVE_MEMORY_REQ` | C → S | Move a memory between contexts |
| 0x00E7 | `ADMIN_MOVE_MEMORY_RESP` | S → C | Move ack |
| 0x0068 | `ADMIN_RECLASSIFY_REQ` | C → S | Change a memory's kind |
| 0x00E8 | `ADMIN_RECLASSIFY_RESP` | S → C | Reclassify ack |
| 0x0069 | `ADMIN_LIST_TOMBSTONED_REQ` | C → S | List tombstoned memories (debug) |
| 0x00E9 | `ADMIN_LIST_TOMBSTONED_RESP` | S → C | List response (streaming) |
| 0x006A | `ADMIN_TOKENIZE_REQ` | C → S | Tokenize text for inspection (returns token IDs and surface forms) |
| 0x00EA | `ADMIN_TOKENIZE_RESP` | S → C | Tokenizer output |
| 0x006B | `ADMIN_REGISTER_MODEL_REQ` | C → S | Register an additional embedding-model fingerprint |
| 0x00EB | `ADMIN_REGISTER_MODEL_RESP` | S → C | Registration ack |
| 0x006C | `ADMIN_ABORT_MIGRATION_REQ` | C → S | Abort an in-progress `ADMIN_MIGRATE_EMBEDDINGS` run |
| 0x00EC | `ADMIN_ABORT_MIGRATION_RESP` | S → C | Abort ack |
| 0x006D | `ADMIN_RETIRE_FINGERPRINT_REQ` | C → S | Retire a model fingerprint after a completed migration |
| 0x00ED | `ADMIN_RETIRE_FINGERPRINT_RESP` | S → C | Retire ack |
| 0x006E | `ADMIN_BACKFILL_REQ` | C → S | Submit a backfill run (re-run extractors over a memory range); returns a `BackfillId` |
| 0x00EE | `ADMIN_BACKFILL_RESP` | S → C | Backfill submission ack (id + initial progress) |
| 0x006F | `ADMIN_BACKFILL_CANCEL_REQ` | C → S | Cancel an in-flight backfill run by id |
| 0x00EF | `ADMIN_BACKFILL_CANCEL_RESP` | S → C | Cancel ack (final progress snapshot) |

### 1.7 Errors

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x00FF | `ERROR` | bidirectional | Error frame; can be sent in response to any operation |

The error frame is a single opcode that carries an error code and details. See [`07_error_handling.md`](07_error_handling.md).

## 2. Typed-graph opcodes (`0x01xx` namespace)

All typed-graph opcodes live in the `0x01xx` namespace. Within `0x01xx`, low-byte ranges are partitioned by operation family:

```
0x0100–0x010F   reserved
0x0110–0x011F   reserved future
0x0120–0x012F   schema operations
0x0130–0x013F   entity operations
0x0140–0x014F   statement operations
0x0150–0x015F   relation operations
0x0160–0x016F   query operations (hybrid retrieval)
0x0170–0x017F   admin operations (extraction + index maintenance)
0x0180–0x018F   reserved future
```

The low byte's high bit selects direction within this namespace, mirroring the substrate convention. For example, `ENTITY_CREATE` is `0x0130` (request) and its response `ENTITY_CREATE_RESP` is `0x01B0`.

### 2.1 Schema operations (0x0120–0x012F)

| Opcode | Name | Body | Response |
|---|---|---|---|
| 0x0120 | `SCHEMA_UPLOAD` | schema document (text) | schema_version, validation_errors. Additive-merge: classifies each declared item as Insert / Idempotent / Conflict against the current namespace; one conflict aborts the whole upload. See [`../03_schema/05_versioning.md`](../03_schema/05_versioning.md) §1a. |
| 0x0121 | `SCHEMA_GET` | version_id (latest if 0) | schema document |
| 0x0122 | `SCHEMA_LIST` | (none) | list of versions with timestamps |
| 0x0123 | `SCHEMA_VALIDATE` | schema document | validation_errors (without commit) |
| 0x0124 | `EXTRACTOR_LIST` | (none) | active extractors |
| 0x0125 | `EXTRACTOR_DISABLE` | extractor_id | confirmation |
| 0x0126 | `EXTRACTOR_ENABLE` | extractor_id | confirmation |
| 0x0127 | `SCHEMA_REPLACE` | schema document + `force_drop_existing: true` | namespace, schema_version, dropped_count, validation_errors |

`SCHEMA_REPLACE` (request `0x0127`, response `0x01A7`) is the destructive counterpart to the additive-merge `SCHEMA_UPLOAD`. It tombstones every schema-declared predicate, relation_type, and extractor row in the target namespace and re-runs the apply path against a clean slate inside a single redb wtxn. Entity types are **not** dropped (they are global in v1; see [`../03_schema/05_versioning.md`](../03_schema/05_versioning.md) §1c). Admin-only; the handler rejects the call unless `force_drop_existing` is exactly `true`. See [`../03_schema/05_versioning.md`](../03_schema/05_versioning.md) §9.

### 2.2 Entity operations (0x0130–0x013F)

| Opcode | Name | Body | Response |
|---|---|---|---|
| 0x0130 | `ENTITY_CREATE` | type, canonical_name, attributes | EntityId |
| 0x0131 | `ENTITY_GET` | EntityId | Entity record |
| 0x0132 | `ENTITY_UPDATE` | EntityId, attribute_deltas | confirmation |
| 0x0133 | `ENTITY_RENAME` | EntityId, new_name, move_to_alias | confirmation |
| 0x0134 | `ENTITY_MERGE` | survivor, merged, confidence | merge audit_id |
| 0x0135 | `ENTITY_UNMERGE` | merged_entity | restored EntityId |
| 0x0136 | `ENTITY_RESOLVE` | candidate_name, context, hint | ResolutionOutcome |
| 0x0137 | `ENTITY_LIST` | filter (type, name_prefix, mention_count_min) | EntityIds |
| 0x0138 | `ENTITY_TOMBSTONE` | EntityId, reason | confirmation |

### 2.3 Statement operations (0x0140–0x014F)

| Opcode | Name | Body | Response |
|---|---|---|---|
| 0x0140 | `STATEMENT_CREATE` | kind, subject, predicate, object, evidence, confidence | StatementId |
| 0x0141 | `STATEMENT_GET` | StatementId | Statement record |
| 0x0142 | `STATEMENT_SUPERSEDE` | old_id, new_statement | new StatementId |
| 0x0143 | `STATEMENT_TOMBSTONE` | StatementId, reason | confirmation |
| 0x0144 | `STATEMENT_RETRACT` | StatementId | confirmation |
| 0x0145 | `STATEMENT_HISTORY` | StatementId or chain_root | full chain |
| 0x0146 | `STATEMENT_LIST` | filter (subject, predicate, kind, time, confidence) | StatementIds |

### 2.4 Relation operations (0x0150–0x015F)

| Opcode | Name | Body | Response |
|---|---|---|---|
| 0x0150 | `RELATION_CREATE` | type, from, to, properties, evidence | RelationId |
| 0x0151 | `RELATION_GET` | RelationId | Relation record |
| 0x0152 | `RELATION_SUPERSEDE` | old_id, new_relation | new RelationId |
| 0x0153 | `RELATION_TOMBSTONE` | RelationId, reason | confirmation |
| 0x0154 | `RELATION_LIST_FROM` | EntityId, type_filter, time_filter | RelationIds |
| 0x0155 | `RELATION_LIST_TO` | EntityId, type_filter, time_filter | RelationIds |
| 0x0156 | `RELATION_TRAVERSE` | start, types, depth, direction | path/subgraph |

### 2.5 Query operations (0x0160–0x016F)

| Opcode | Name | Body | Response |
|---|---|---|---|
| 0x0160 | `QUERY` | QueryRequest | QueryResult (streamed if large) |
| 0x0161 | `QUERY_EXPLAIN` | QueryRequest | QueryPlan (no execution) |
| 0x0162 | `QUERY_TRACE` | QueryRequest | QueryResult + per-retriever debug |
| 0x0163 | `RECALL_HYBRID` | text, filters, retriever_selection | RecallResult |
| 0x0164 | `MATERIALIZE_PROCEDURAL` | agent_id, target_predicates | ProceduralBlock (rendered system prompt) |

`QUERY` is the primary structured query opcode. `RECALL_HYBRID` is the simple-text fast path used by clients that just want hybrid text-only retrieval with no entity anchoring.

`MATERIALIZE_PROCEDURAL` (`0x0164` request, `0x01E4` response) renders the agent's procedural-memory predicates (`brain:behavior_*` — see [`../03_schema/06_system_schema.md`](../03_schema/06_system_schema.md)) into a single system-prompt block the agent can re-inject at conversation start. Semantically a read-only structured query under the hood; conceptually a memory primitive because the agent treats the materialised block as a separate handle.

The substrate `RECALL` opcode (`0x0021`) is the primary vector recall. The pipeline (semantic + lexical + memory-edge graph fused by RRF) is the **only** `RECALL` path; every shard runs it on every request. The response always carries `contributing_retrievers` and `fused_score`. Declaring a user schema does not toggle a different retrieval path — it makes typed entity-anchored graph traversal and predicate-vocabulary checking available to callers, and lets extracted typed rows persist; the fan-out, fusion, and filter chain are the same shape regardless.

### 2.6 Admin operations (0x0170–0x017F)

| Opcode | Name | Body | Response |
|---|---|---|---|
| 0x0170 | `ADMIN_REBUILD_INDEX` | index_name, shard_id | job_id |
| 0x0171 | `ADMIN_REINDEX_TANTIVY` | shard_id | job_id |
| 0x0172 | `ADMIN_LIST_PENDING_RESOLUTIONS` | (none) | list of ambiguity audits |
| 0x0173 | `ADMIN_RESOLVE_AMBIGUITY` | audit_id, chosen_entity | confirmation |
| 0x0174 | `ADMIN_GET_AUDIT` | audit_id | AuditEntry |
| 0x0175 | `ADMIN_LIST_STALE_STATEMENTS` | filter | StatementIds |
| 0x0176 | `ADMIN_BACKFILL` | extractor_ids, memory_range | job_id |
| 0x0177 | `ADMIN_JOB_STATUS` | job_id | status, progress, ETA |

### 2.7 SUBSCRIBE event types (typed graph)

The SUBSCRIBE primitive (§1.3) carries event types for the typed graph:

- `ENTITY_CREATED`, `ENTITY_UPDATED`, `ENTITY_MERGED`, `ENTITY_UNMERGED`, `ENTITY_RENAMED`, `ENTITY_TOMBSTONED`
- `STATEMENT_CREATED`, `STATEMENT_SUPERSEDED`, `STATEMENT_TOMBSTONED`
- `RELATION_CREATED`, `RELATION_SUPERSEDED`
- `EXTRACTION_COMPLETED` (with extractor_id, memory_id, output_count)
- `EXTRACTION_FAILED` (with extractor_id, memory_id, error)
- `SCHEMA_UPDATED` (with from_version, to_version)

Subscribers filter by event type, entity_id, predicate, etc. Event-payload schemas and emission semantics are part of the streaming surface defined in [`06_streaming.md`](06_streaming.md).

### 2.8 Schema as a per-type gate

The server operates the same pipeline whether or not a user namespace has been uploaded. Every shard ships with the seeded `brain:` system namespace, so there is always at least one schema active.

What user declarations control is **per-type acceptance** on explicit typed-graph writes:

- `ENTITY_CREATE` with an entity type that isn't in any active namespace → `EntityTypeNotInSchema`.
- `STATEMENT_CREATE` with an undeclared predicate qname → `PredicateNotInSchema` (or open-vocabulary intern with `SchemaOrigin::ImplicitFromWrite` if the deployment runs in open-vocabulary mode; see §07/error-handling).
- `RELATION_CREATE` with an undeclared relation_type qname → `RelationTypeNotInSchema` (same open-vocabulary rule).

Extracted candidates whose types are not in any active schema are dropped silently (extractor best-effort; see [`../11_extractors/00_purpose.md`](../11_extractors/00_purpose.md)). `RECALL`, `QUERY`, `RECALL_HYBRID`, and the fan-out pipeline always run regardless of which user namespaces are present.

There is no `SCHEMA_NOT_DECLARED` error any more — the gate is per-type, not per-namespace.

## 3. Reserved ranges

Within the substrate namespace (`0x00xx`), the following low-byte ranges are reserved:

- 0x70–0x7F (server-bound, `0x0070–0x007F`) — reserved for future C → S substrate operations.
- 0xF0–0xFE (client-bound, `0x00F0–0x00FE`) — reserved for future S → C substrate operations.

Within the typed-graph namespace (`0x01xx`):

- 0x0100–0x010F — reserved.
- 0x0110–0x011F — reserved future.
- 0x0180–0x018F — reserved future.

Receivers MUST treat unknown opcodes as protocol errors (sending `ERROR` with `BadOpcode`) — no silent discarding.

## 4. Symmetry between request and response

For most operations, the request opcode `0x002N` corresponds to the response opcode `0x00AN`. Mnemonic: low byte's high bit set = response, low nibble selects the operation.

For admin operations, the pattern is `0x006N` → `0x00EN`.

Typed-graph opcodes follow the same convention within their `0x01xx` namespace: e.g. `0x0130 ENTITY_CREATE` (request) ↔ `0x01B0 ENTITY_CREATE_RESP`.

For connection management, the pattern is less regular because operations have multiple frames (PING/PONG, BYE bidirectional, etc.).

## 5. Operation dispatch

When the server receives a frame:

1. Validates the header (CRC, magic, version, reserved bytes).
2. Dispatches by opcode and stream_id.
3. For server-bound opcodes (low byte `< 0x80`, in any namespace): processes the operation. Most operations carry a stream_id and the response uses the same stream_id.
4. For client-bound opcodes (low byte `≥ 0x80`): protocol error — clients shouldn't send these. The server responds with `ERROR(InvalidOpcode)`.

The reverse on the client side: the client expects only client-bound opcodes from the server.

## 6. Order of frames per opcode

### 6.1 Single-frame request → single-frame response

Examples: `ENCODE_REQ` → `ENCODE_RESP`, `FORGET_REQ` → `FORGET_RESP`.

```
client: REQ (stream_id=N, EOS)
server: RESP (stream_id=N, EOS)
```

The single frame in each direction carries the entire request/response. The stream is one frame long in each direction.

### 6.2 Single-frame request → streaming response

Examples: `RECALL_REQ` → multiple `RECALL_RESP` frames, similarly for `PLAN`, `REASON`.

```
client: REQ (stream_id=N, EOS)
server: RESP (stream_id=N, no EOS) [first results]
server: RESP (stream_id=N, no EOS) [more results]
...
server: RESP (stream_id=N, EOS)    [final batch or empty terminator]
```

The server emits intermediate frames as results become available; the EOS frame signals end of stream.

### 6.3 Subscription

```
client: SUBSCRIBE_REQ (stream_id=N, EOS)
server: SUBSCRIBE_EVENT (stream_id=N) [ongoing]
server: SUBSCRIBE_EVENT (stream_id=N) [as events occur]
...

(eventually:)
client: UNSUBSCRIBE_REQ (stream_id=M, EOS) referencing stream N
server: UNSUBSCRIBE_RESP (stream_id=M, EOS)
server: SUBSCRIBE_EVENT (stream_id=N, EOS) [final stream-end frame]
```

The unsubscribe is on a different stream; the original stream's EOS frame is sent when the unsubscribe completes.

### 6.4 Transaction

```
client: TXN_BEGIN (stream_id=N, EOS)
server: TXN_BEGIN_RESP (stream_id=N, EOS) [returns txn_id]

client: ENCODE_REQ (stream_id=M, EOS, with txn_id in payload)
server: ENCODE_RESP (stream_id=M, EOS) [memory buffered, not yet visible]

...more operations...

client: TXN_COMMIT (stream_id=K, EOS, txn_id)
server: TXN_COMMIT_RESP (stream_id=K, EOS) [commit applied]
```

Each operation in a transaction is its own stream. The transaction lifecycle has its own streams. The `txn_id` in the operation payload links them.

## 7. Flow examples

### 7.1 Simple ENCODE flow

```
[connection established, AUTH_OK received]

C → S: ENCODE_REQ(stream_id=1, EOS)
       payload: {text: "Hello world", context_id: 0, request_id: <uuid>}
S → C: ENCODE_RESP(stream_id=1, EOS)
       payload: {memory_id: <id>, status: ok}
```

### 7.2 Streaming RECALL flow

```
C → S: RECALL_REQ(stream_id=3, EOS)
       payload: {cue_text: "what about budgets", top_k: 5, ...}

S → C: RECALL_RESP(stream_id=3, !EOS)
       payload: {results: [r1, r2]}  (first batch streamed as ANN finds them)
S → C: RECALL_RESP(stream_id=3, !EOS)
       payload: {results: [r3]}
S → C: RECALL_RESP(stream_id=3, EOS)
       payload: {results: [r4, r5]}  (final batch, EOS)
```

The client may begin processing results as soon as the first frame arrives.

### 7.3 PING/PONG

```
C → S: PING(stream_id=0, EOS)
       payload: {client_timestamp: <ns>}
S → C: PONG(stream_id=0, EOS)
       payload: {client_timestamp: <ns>, server_timestamp: <ns>}
```

The client measures RTT from the timestamp difference.

## 8. Opcode evolution

Adding new opcodes is a wire-protocol-version bump (see §"Versioning" below). The protocol's design accommodates additions:

- Reserved ranges in both namespaces (substrate 0x70–0x7F / 0xF0–0xFE; typed-graph 0x0100–0x011F / 0x0180–0x018F) leave room.
- Existing opcodes are stable; their semantics don't change within a version.
- Negotiation at handshake gives both sides a chance to know what the other supports.

A future major-version bump might add opcodes for replication-related operations, multi-modal operations, etc.

## Versioning

The wire protocol carries a single version field. Clients at any other version are rejected at handshake.

### The version byte

Every frame carries a `wire_version: u8` byte in the frame header. The value is **`1`**. Frames with any other value are protocol errors.

The byte exists so that pre-handshake frames (HELLO, WELCOME) self-identify; a server that reads a HELLO with `wire_version != 1` closes the connection with `WireVersionMismatch` and no further dialog.

### Handshake

The HELLO frame's payload includes:

```
struct HelloPayload {
    wire_version: u8,                   // must be 1
    client_name: String,
    client_version: String,
    feature_flags_requested: Vec<String>,
}
```

The WELCOME frame's payload includes:

```
struct WelcomePayload {
    wire_version: u8,                   // 1
    server_version: String,             // Brain server build version
    feature_flags_enabled: Vec<String>,
    session_id: SessionId,
}
```

A server that receives a HELLO with a wire version different from its own returns `WireVersionMismatch` and closes the connection. There is no negotiation step — the version is fixed.

### Feature flags

The HELLO/WELCOME exchange carries feature flags alongside the version. Feature flags are independent of the version: they govern optional semantic capabilities, not byte layouts.

Examples:

- `gpu_inference` — server supports GPU embedding.
- `txn_isolation_serializable` — transactions support serializable isolation (otherwise read-committed only).

The client requests a set of feature flags in HELLO. The server responds in WELCOME with the subset it actually enables. Operations that depend on a flag check it; if disabled, they fail with `FeatureNotEnabled`.

### Frame-version mismatch after handshake

If, after handshake, the server receives a frame whose `wire_version` differs from the server's, the connection is closed with no error frame; an out-of-band log entry records the issue. A post-handshake mismatch indicates something is so wrong that further communication is meaningless.

### Diagnostic surfaces

- The `ADMIN_STATS` opcode reports the server's wire version.
- Logs record connections that fail handshake with `WireVersionMismatch`.

### Client responsibilities

A conforming client MUST:

- Send a HELLO at the server's wire version.
- Refuse to operate if the server reports a different version.
- Surface version-mismatch errors clearly to the application.

---

*Continue to [`04_handshake.md`](04_handshake.md) for the connection handshake.*
