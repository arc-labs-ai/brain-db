# 04.05 Frame Layouts

The payload layout for every request and response frame. Frame headers are documented in [`02_wire_format.md`](02_wire_format.md); payload encoding (CBOR + little-endian f32) is in the same file. This file focuses on the structured fields per opcode.

## Request Frames

### Shared request field: `act_as`

Tenant-scoped requests carry one shared, **optional** identity field, `act_as`. When present it names the **effective identity** the op runs as, on behalf of the authenticated connection principal; when absent (the common case) the op runs as the connection's own key-bound identity. The full model — the connection-principal-vs-effective-identity distinction, the `can_act_as` grant, and the six invariants R1–R6 — is defined in [`04_handshake.md`](04_handshake.md) §10a. This file only fixes the wire layout.

```rust
struct ActAs {
    namespace: String,                       // effective namespace; must be within the principal's may_act allowlist
    space: String,                           // structured effective-space selector; "" = the key-bound space
    grant: u32,                              // optional (omitted when 0): delegable bits added for this op
}
```

The selector field is named `space`, not `space_id`. `space_id` is the derived 16-byte storage id (`UUIDv5(namespace, space)`) everywhere else in this protocol; the input string that it is derived from is a different value and carries a different name. The server derives `space_id` at request ingress and never parses the string for sub-scopes.

`act_as: Option<ActAs>` appears on **every tenant-scoped request** — all 53 of them:

| Family | Requests carrying `act_as` |
|---|---|
| Cognitive | `ENCODE`, `ENCODE_VECTOR_DIRECT`, `RECALL`, `PLAN`, `REASON`, `FORGET`, `LINK`, `UNLINK`, `MEMORY_LIST`, `MEMORY_INSPECT` |
| Registry | `SPACE_CREATE`, `SPACE_LIST`, `SPACE_DELETE`, `SESSION_CREATE`, `SESSION_LIST`, `SESSION_DELETE` |
| Schema | `SCHEMA_UPLOAD`, `SCHEMA_GET`, `SCHEMA_LIST`, `SCHEMA_VALIDATE`, `SCHEMA_REPLACE`, `SCHEMA_DROP` |
| Entity | `ENTITY_CREATE`, `ENTITY_GET`, `ENTITY_UPDATE`, `ENTITY_RENAME`, `ENTITY_MERGE`, `ENTITY_UNMERGE`, `ENTITY_RESOLVE`, `ENTITY_LIST`, `ENTITY_TOMBSTONE` |
| Statement | `STATEMENT_CREATE`, `STATEMENT_GET`, `STATEMENT_SUPERSEDE`, `STATEMENT_TOMBSTONE`, `STATEMENT_RETRACT`, `STATEMENT_HISTORY`, `STATEMENT_LIST` |
| Relation | `RELATION_CREATE`, `RELATION_GET`, `RELATION_SUPERSEDE`, `RELATION_TOMBSTONE`, `RELATION_LIST_FROM`, `RELATION_LIST_TO`, `RELATION_TRAVERSE` |
| Query / graph | `QUERY_EXPLAIN`, `QUERY_TRACE`, `GRAPH_FETCH`, `MATERIALIZE_PROCEDURAL` |
| Introspection | `GET_CAPABILITIES`, `EXTRACTOR_LIST` |
| Streaming / txn | `SUBSCRIBE`, `TXN_BEGIN` |

It is honored only if the connection principal holds `can_act_as` and `act_as.namespace` is within its `may_act` allowlist — otherwise the op is rejected with `ActAsDenied` (see [`07_error_handling.md`](07_error_handling.md) §3.3). Every schema namespace check binds to the effective namespace, and `GET_CAPABILITIES.schema_namespaces` is filtered to the (effective) caller's own namespace for every caller.

`act_as` is **absent** from the frames that are not tenant-scoped: the handshake (`HELLO`, `AUTH`), liveness and stream control (`PING`, `CLIENT_PONG`, `BYE`, `CANCEL_STREAM`, `UNSUBSCRIBE`), the mid-transaction frames (`TXN_COMMIT`, `TXN_ABORT` — the txn is already bound to an identity by `TXN_BEGIN`), and every operator admin frame (`ADMIN_STATS`, `ADMIN_SNAPSHOT`, `ADMIN_RESTORE`, `ADMIN_INTEGRITY_CHECK`, `ADMIN_MIGRATE_EMBEDDINGS`, `ADMIN_CREATE_SESSION`, `ADMIN_RENAME_SESSION`, `ADMIN_MOVE_MEMORY`, `ADMIN_RECLASSIFY`, `ADMIN_LIST_TOMBSTONED`, `ADMIN_LIST_PENDING_CONTRADICTIONS`, `ADMIN_BACKFILL`, `ADMIN_BACKFILL_CANCEL`). Admin frames act on the shard, not on a tenant's rows, and are gated by the `ADMIN` permission bit instead.

**Delegated grant.** `ActAs` carries a third, optional key, `grant: u32` (omitted from the CBOR map when `0`, defaulted to `0` when absent, so a two-key selector is byte-identical to the historical form). A delegated op always runs with the fixed `STANDARD_SPACE` mask; `grant` lets the trusted delegator add the **delegable** bits — `SCHEMA_UPLOAD` (`1 << 4`) and `ADMIN` (`1 << 5`) — for that one op: `effective = STANDARD_SPACE | (grant & delegator_permissions & (SCHEMA_UPLOAD | ADMIN))`. A grant naming any other bit (including `ACT_AS`), or a bit the connection principal does not itself hold, is rejected with `ActAsDenied`. Whether a tenant's role allows schema rights is the delegator's decision; Brain only guarantees the delegator can never hand out more than it holds. (`SUBSCRIBE_REQ`'s `act_as` support has a structural history worth knowing — see the note under §7 below.)

### 1. ENCODE_REQ (0x20)

`ENCODE` expresses client **intent only**: the text to remember, where it belongs, and when its content happened. Everything mechanical — the memory `kind`, its `salience`, whether the write deduplicates against an existing row, and which edges get wired — is decided by the write router, server-side. Clients do not (and cannot) dictate that machinery; they say *what* to remember, not *how* to file it.

```rust
struct EncodeRequest {
    text: String,
    session_id: SessionId,                   // 8 bytes / u64; 0 = the default session
    request_id: RequestId,                   // 16 bytes; required for idempotency
    txn_id: Option<TxnId>,                   // 16 bytes; if part of a transaction
    occurred_at_unix_nanos: Option<u64>,     // client event time; None when unknown
    act_as: Option<ActAs>,                   // effective identity; None = the connection's own identity
    wait: WaitMode,                          // Ack (default, omitted) | Derived
    allow_duplicates: bool,                  // default false (omitted); opt out of content dedup
}

enum WaitMode {
    Ack = 0,                                 // return on the durable WAL ack; derivation runs async
    Derived = 1,                             // block for async derivation, then return a populated trace
}
```

Fields:

- `text` — the content to encode. UTF-8. Maximum length is server-configurable (default: 1 MiB). Empty text is rejected with `TextEmpty`.
- `session_id` — the session to encode into, an opaque client-chosen `u64`. `0` is the default session. A session is a **soft grouping within a space**, never an isolation boundary; see [`../02_data_model/03_sessions.md`](../02_data_model/03_sessions.md).
- `request_id` — UUIDv7 recommended; required for idempotent retry.
- `txn_id` — if non-None, the encode is buffered as part of the transaction and applied at `TXN_COMMIT`.
- `occurred_at_unix_nanos` — client-supplied **event time**: when the memory's content actually happened, as opposed to `created_at` (the server's write time). `None` when the client doesn't know it. Lets time-aware clients store the real timeline instead of cramming dates into the text; echoed back on `RECALL` (`MemoryResult.occurred_at_unix_nanos`) and carried durably through the WAL so it survives recovery.
- `allow_duplicates` — opt **out** of content dedup. The default (`false`, omitted from the CBOR map) dedupes on `(space_id, session_id, BLAKE3(text))`: a repeat of byte-identical text returns the existing `memory_id` with `was_deduplicated = true` and writes nothing new. Set `true` when the same text is a genuinely distinct observation that must coexist — e.g. the same fact re-stated at a different `occurred_at`.
- `wait` — the single write-completion knob. `Ack` (the wire default, omitted) returns as soon as the WAL record is durable; the async derivation stages (auto-edge / temporal-edge / extractor / HyPE) run in the background and surface through `SUBSCRIBE`, with `lsn` + `pending_stages` on the response to follow them. `Derived` blocks until that derivation completes and returns a populated `trace: EncodeTrace` (§37.1) — the per-stage timeline plus every artifact the write produced. `Derived` is bounded by the shard's trace drain window so a stalled worker cannot hang the call; stragglers are marked `Timeout` and still land in `MEMORY_INSPECT` later.

**API convention:** writes take `wait`; reads take `trace: bool`. They are never both on one op. On a write, waiting and the trace payload are one decision — the only reason to wait for async derivation is to observe it — so one knob controls both. On a read there is no async to wait for, so `trace` is a pure observability toggle with no timing effect.

There is no `kind`, `salience_hint`, `edges` or `deduplicate` field. Kind and salience are router-owned (today every text encode files as `Episodic` at a salience floor of 0.5); edges are derived by the auto-edge and temporal-edge stages; dedup is on by default and controlled by `allow_duplicates` above. Explicit edges are written with `LINK`.

### 2. ENCODE_VECTOR_DIRECT_REQ (0x2A)

Admin / bulk-import path — **not** a primary client verb. Brain owns the embedding model; ordinary clients send text via `ENCODE` and let the server embed it. This op exists for bulk or administrative import from deployments running their own (often domain-specific or multi-modal) embedder outside Brain. Because the caller supplies the vector, it also supplies the mechanical fields `ENCODE` no longer takes.

```rust
struct EncodeVectorDirectRequest {
    text: String,                            // may be empty for multi-modal upstreams
    // vector: Vec<f32>                      — NOT in the CBOR map; see below
    model_fingerprint: [u8; 16],             // BLAKE3-derived
    session_id: SessionId,
    kind: MemoryKind,
    salience_hint: f32,
    edges: Vec<EdgeRequest>,
    request_id: RequestId,
    txn_id: Option<TxnId>,
    deduplicate: bool,
    act_as: Option<ActAs>,
}

struct EdgeRequest {
    target: MemoryId,
    kind: EdgeKind,
    weight: f32,
}
```

The embedding is **excluded from the CBOR map** and rides the trailing raw little-endian `f32` section of the payload at full precision — CBOR would tag each float and round to half-precision. The encode arm appends the floats after the CBOR bytes; the decode arm reads the CBOR prefix and repopulates the vector from whatever trails it. There are therefore no `vector_offset` / `vector_dim` fields: the offset is the length of the CBOR prefix and the dimension is `trailing_len / 4`.

The server validates:

- The decoded vector's length matches the configured embedding dimensionality (384 for the BGE-small v1 fingerprint Brain ships with).
- `model_fingerprint` matches the shard's loaded model fingerprint. A mismatch fails the write — the resulting memory would be unsearchable against future text-cued recalls.
- The vector's L2 norm is approximately 1.0 (within epsilon).

When `text` is empty, the content-hash dedup path hashes the vector bytes instead.

### 3. RECALL_REQ (0x21)

```rust
struct RecallRequest {
    cue_text: String,                        // the query
    subject_name: String,                    // optional explicit subject hint; "" = mine subjects from the cue
    max_results: u32,                        // safety cap on returned items; 0 = server default
    confidence_threshold: f32,               // [0, 1]; results below this are excluded
    session_filter: Option<Vec<SessionId>>,  // None = no filter
    age_bound_unix_nanos: Option<u64>,       // results must be newer than this (event time)
    as_of_record_time_unix_nanos: Option<u64>, // bi-temporal time-travel anchor; also the recency reference
    kind_filter: Option<Vec<MemoryKind>>,    // None = all kinds
    salience_floor: f32,                     // [0, 1]; default 0
    include_edges: bool,                     // include each result's memory edges
    include_graph: bool,                     // include per-result typed-graph enrichment
    include_text: bool,                      // include the stored text
    request_id: Option<RequestId>,           // optional; for tracing
    txn_id: Option<TxnId>,                   // read-your-writes against a txn's pending buffer
    trace: bool,                             // opt-in structured RecallTrace on the final frame
    act_as: Option<ActAs>,                   // effective identity; None = the connection's own identity
    scope: RecallScope,                      // Space (default) | Namespace
}

enum RecallScope { Space, Namespace }
```

RECALL is one verb with one server-side path-selection rule: a request that carries a `txn_id` runs the txn path (read-your-writes requires the per-txn buffer overlay, which the lexical and graph retrievers do not see); every other request runs the retrieval path (semantic + lexical + memory-edge graph, fused via RRF). The client cannot select between paths.

There is likewise **no read "mode"**. Recall always runs the episodic associative path and overlays a precise grounded value when one is named exactly; `subject_name` only adds an explicit subject candidate to that overlay instead of mining subjects from the cue.

Fields:

- `scope` — `Space` (wire default) or `Namespace`. `Space` serves the request on the single shard the caller's `(namespace, space)` hashes to. `Namespace` fans the request out across every shard, widening each shard's scope filter from `(namespace, space)` to `namespace_id` only, and shapes once over the globally-merged pool — spanning every space in the caller's own namespace and never another tenant's. Reuses the `RECALL` permission. Full semantics in [`../05_operations/03_read_pipeline.md`](../05_operations/03_read_pipeline.md) §"Recall scope"; the cross-shard RRF merge is noted in [`../13_retrievers/01_rrf_fusion.md`](../13_retrievers/01_rrf_fusion.md).
- `max_results` — a **safety cap on returned items**, not a ranking knob. The answer's shape comes from the data and the answer kind, never from a caller-supplied count. `0` means the server default.
- `confidence_threshold` — results with confidence below this are filtered out. Default: 0.0.
- `session_filter` — restrict to specific sessions. `None` means every session in the caller's space.
- `age_bound_unix_nanos` — only return memories whose event time is after this.
- `as_of_record_time_unix_nanos` — bi-temporal **time-travel** anchor. When set, statement/relation results are resolved against the state the substrate believed at record-time `t` (`extracted_at <= t AND (record_invalidated_at IS NULL OR record_invalidated_at > t)`), and `t` also becomes the reference point for the recency-ranking decay. `None` is the current-state default. Distinct from `age_bound_unix_nanos` (an event-time lower bound).
- `kind_filter` — restrict to specific kinds. None means all kinds.
- `salience_floor` — minimum salience for inclusion. Default 0.0.
- `include_edges` — if true, each result carries its memory edges. Default false.
- `include_graph` — if true, each `MemoryResult` carries a populated `graph: GraphEnrichment` listing entities the memory mentions, statements sourced from it, and typed relations incident to those entities. If the memory was never extracted (no schema declared, no extractors registered, or a mention-less memory), the field stays `None` even with the flag set.
- `include_text` — if true, each result carries the memory's stored UTF-8 text, at the cost of one batched read against the per-shard `texts` table. When unset, `MemoryResult.text` is the empty string.
- `trace` — opt-in structured observability. When true the **final** response frame carries a populated `trace: RecallTrace` (retriever lanes, filter-chain survivor counts, fusion scores, rerank outcome, total wall-time). Zero-cost when false. Distinct from the debug-only `QUERY_TRACE` op, which returns rendered text.
- `request_id` — optional; for tracing/logging only, not for idempotency.

RECALL does **not** accept a pre-computed cue vector, and has no `include_vectors`: the server embeds `cue_text` itself, and results never carry raw vectors. The raw-vector ingress path is `ENCODE_VECTOR_DIRECT` (§2).

### 4. PLAN_REQ (0x22)

```rust
struct PlanRequest {
    start: PlanState,
    goal: PlanState,
    budget: PlanBudget,
    strategy_hint: Option<PlanStrategy>,
    session_filter: Option<Vec<SessionId>>,
    request_id: Option<RequestId>,
    txn_id: Option<TxnId>,                   // read-your-writes against a txn's pending buffer
    trace: bool,                             // opt-in structured PlanTrace on the final frame
    act_as: Option<ActAs>,                   // effective identity; None = the connection's own identity
}

enum PlanState {
    ByMemoryId(MemoryId),
    ByText(String),                          // server embeds and finds the closest memory
    ByVector { offset: u32, dim: u16 },     // raw section
}

struct PlanBudget {
    max_steps: u32,                          // max plan length
    max_wall_time_ms: u32,                   // wall-clock cap
    max_branches_explored: u32,              // search-space cap
}

enum PlanStrategy {
    Auto = 0,
    AStar = 1,
    Mcts = 2,
    AttractorRollout = 3,
}
```

Notes:

- `start` and `goal` can each be referenced by `MemoryId`, by text (server embeds), or by raw vector.
- `budget` is required; planning is unbounded otherwise.
- `strategy_hint` lets the planner override be controlled; `Auto` is the default.
- `trace` — opt-in structured observability, the read-side counterpart to `ENCODE`'s `wait: derived`. When true the final frame carries a populated `trace: PlanTrace` (every explored node with its direction/depth/parent edge, and the meeting points of the bidirectional search).

### 5. REASON_REQ (0x23)

```rust
struct ReasonRequest {
    observation: ObservationInput,
    depth: u32,                              // max graph traversal depth
    confidence_threshold: f32,               // [0, 1]
    session_filter: Option<Vec<SessionId>>,
    max_inferences: u32,                     // max inference steps to emit
    budget_wall_time_ms: u32,                // wall-clock cap
    request_id: Option<RequestId>,
    txn_id: Option<TxnId>,                   // read-your-writes against a txn's pending buffer
    trace: bool,                             // opt-in structured ReasonTrace on the final frame
    act_as: Option<ActAs>,                   // effective identity; None = the connection's own identity
}

enum ObservationInput {
    ByMemoryId(MemoryId),
    ByText(String),
}
```

Fields:

- `observation` — what to reason about. Either an existing memory or new text.
- `depth` — graph traversal depth limit. Default 5.
- `confidence_threshold` — only emit inferences above this confidence.
- `max_inferences` — limit on inference steps. Default 50.
- `budget_wall_time_ms` — wall-clock cap. Default 5000 (5 seconds).
- `trace` — opt-in structured observability. When true the final frame carries a populated `trace: ReasonTrace`: the base candidate set, the walk (considered edges plus each drop reason — edge kind, tombstone, already-visited, confidence, supporting/contradicting caps), the per-memory score breakdown, and whether the centroid was computed.

### 6. FORGET_REQ (0x24)

```rust
struct ForgetRequest {
    memory_id: MemoryId,
    mode: ForgetMode,                        // Soft or Hard
    request_id: RequestId,                   // required for idempotency
    txn_id: Option<TxnId>,
    act_as: Option<ActAs>,                   // effective identity; None = the connection's own identity
}

enum ForgetMode {
    Soft,                                    // tombstone; recoverable until reclaim
    Hard,                                    // overwrite; unrecoverable
}
```

Notes:

- `mode = Soft` is the default for `FORGET_REQ`; `Hard` requires the agent to have appropriate permissions.
- `request_id` is mandatory — like `ENCODE`, FORGET is idempotent on the request_id.

### 7. SUBSCRIBE_REQ (0x30)

```rust
struct SubscribeRequest {
    filter: SubscriptionFilter,
    include_history: bool,                   // start with snapshot of matching memories
    from_lsn: Option<u64>,                   // resume from a specific LSN
    max_inflight: u32,                       // server stops sending after this many unacked events
    act_as: Option<ActAs>,                   // effective identity; None = the connection's own identity
}

struct SubscriptionFilter {
    session_filter: Option<Vec<SessionId>>,  // None = all sessions
    kinds: Option<Vec<MemoryKind>>,          // None = all kinds
    similar_to: Option<SimilarityFilter>,    // optional similarity match
    spaces: Option<Vec<SpaceId>>,            // scopes the stream to specific spaces; see note below
    memory_ids: Option<Vec<MemoryId>>,       // None = no memory-id restriction; scopes the stream to specific memories
}

struct SimilarityFilter {
    reference_memory_id: MemoryId,
    threshold: f32,                          // cosine similarity threshold
}
```

Notes:

- The subscription's `stream_id` is the one the client allocates for the SUBSCRIBE_REQ frame; all SUBSCRIBE_EVENT frames use the same stream_id.
- `from_lsn` resumes a previously-disconnected subscription (per-shard LSN).
- `max_inflight` is per-subscription rate control. The server stops sending events after this many unacked.
- `spaces` is also an authorization boundary, not just a filter dimension: the server requires it to resolve to exactly the caller's own effective space (the connection principal's space, or — with `act_as` present — the effective identity's space named by `act_as`). Subscribing across spaces is not supported.
- `memory_ids` scopes the stream to specific memories — e.g. watching one write's async derivation as it happens. This is the mechanism behind the live write-progress pattern: `ENCODE` with `wait: ack`, then `SUBSCRIBE` with `filter.memory_ids: [that memory_id]`, to receive that write's `StageCompleted` events (§43.2) as `auto_edge`/`temporal_edge`/`extractor`/`hype` each complete. See [05. Operations](../05_operations/02_write_pipeline.md) §17e.
- `act_as` follows the standard mechanism (§10a of [`04_handshake.md`](04_handshake.md)), with one structural note: `SUBSCRIBE`/`UNSUBSCRIBE`/`CANCEL_STREAM` historically dispatched through a code path separate from the normal `act_as`-aware dispatch, so support here required resolving the effective identity explicitly in that branch rather than falling out of the shared mechanism for free. Behavior when `act_as` is absent is unchanged from before this support existed.

### 8. UNSUBSCRIBE_REQ (0x31)

```rust
struct UnsubscribeRequest {
    target_stream_id: u32,                   // the stream_id of the SUBSCRIBE_REQ to cancel
}
```

The unsubscribe is sent as its own stream (different stream_id from the subscription it cancels). The server emits the final EOS on the original subscription stream once unsubscribe is complete.

### 9. TXN_BEGIN (0x40)

```rust
struct TxnBeginRequest {
    txn_id: TxnId,                           // client-supplied; 16 bytes
    timeout_seconds: u32,                    // auto-abort after this; default 60, max 300
    act_as: Option<ActAs>,                   // effective identity; binds the whole txn
}
```

Notes:

- The client supplies the `txn_id` (typically UUIDv7).
- The server validates that `txn_id` isn't already in use within the space.
- `act_as` is accepted here and **only** here in the transaction family: `TXN_BEGIN` binds the transaction to an effective identity, and `TXN_COMMIT` / `TXN_ABORT` inherit it from the open transaction rather than re-asserting it.

### 10. TXN_COMMIT (0x41)

```rust
struct TxnCommitRequest {
    txn_id: TxnId,
}
```

The server applies all buffered operations from this transaction atomically. If commit fails (e.g., a conflict), `ERROR` is returned.

### 11. TXN_ABORT (0x42)

```rust
struct TxnAbortRequest {
    txn_id: TxnId,
}
```

All buffered operations are discarded.

### 12. CANCEL_STREAM (0x50)

```rust
struct CancelStreamRequest {
    target_stream_id: u32,                   // the stream to cancel
    reason: CancellationReason,              // optional session
}

enum CancellationReason {
    ClientUnneeded,
    Timeout,
    Other(String),
}
```

The server stops emitting frames on `target_stream_id` and emits a final EOS frame.

### 13. PING (0x10)

```rust
struct PingRequest {
    client_timestamp_unix_nanos: u64,
}
```

### 14. CLIENT_PONG (0x11)

```rust
struct ClientPongRequest {
    server_timestamp_unix_nanos: u64,        // the timestamp from SERVER_PING
    client_timestamp_unix_nanos: u64,        // current client time
}
```

### 15. BYE (0x1F)

```rust
struct ByeRequest {
    reason: Option<String>,                  // optional log session
}
```

### 16. ADMIN_STATS_REQ (0x60)

```rust
struct AdminStatsRequest {
    detail: StatsDetail,
}

enum StatsDetail {
    Summary = 0,                             // top-level metrics only
    PerShard = 1,                            // include per-shard breakdowns
    PerSession = 2,                          // include per-session breakdowns
    Full = 3,                                // all of the above
}
```

### 17. ADMIN_SNAPSHOT_REQ (0x61)

```rust
struct AdminSnapshotRequest {
    snapshot_name: String,                   // human-readable label
    target_path: Option<String>,             // server-side path; None = default location
    include_wal: bool,                       // include the WAL in the snapshot
    request_id: RequestId,
}
```

### 18. ADMIN_RESTORE_REQ (0x62)

```rust
struct AdminRestoreRequest {
    snapshot_name: String,
    target_shard: Option<ShardId>,           // None = restore to all shards in snapshot
    request_id: RequestId,
}
```

### 19. ADMIN_INTEGRITY_CHECK_REQ (0x63)

```rust
struct AdminIntegrityCheckRequest {
    scope: CheckScope,
    repair_if_possible: bool,
}

enum CheckScope {
    QuickSample,                             // check a sample, fast
    PerShard(Vec<ShardId>),                  // specific shards
    Full,                                    // all data, slow
}
```

### 20. ADMIN_MIGRATE_EMBEDDINGS_REQ (0x64)

```rust
struct AdminMigrateEmbeddingsRequest {
    target_model: ModelIdentifier,           // the model to migrate to
    batch_size: u32,                         // memories per batch; default 100
    rate_limit_qps: u32,                     // throttle, 0 = unthrottled
}

struct ModelIdentifier {
    name: String,                            // e.g., "bge-large-en-v1.5"
    fingerprint: [u8; 16],
}
```

### 21. ADMIN_CREATE_SESSION_REQ (0x65)

```rust
struct AdminCreateSessionRequest {
    name: String,
    description: String,
    request_id: RequestId,
}
```

The operator-facing counterpart to the tenant-facing `SESSION_CREATE` (§33). This one is admin-gated and names the session; `SESSION_CREATE` is non-admin, takes the client's own `session_id`, and is `act_as`-aware.

### 22. ADMIN_RENAME_SESSION_REQ (0x66)

```rust
struct AdminRenameSessionRequest {
    session_id: SessionId,
    new_name: String,
}
```

### 23. ADMIN_MOVE_MEMORY_REQ (0x67)

```rust
struct AdminMoveMemoryRequest {
    memory_id: MemoryId,
    new_session_id: SessionId,
}
```

### 24. ADMIN_RECLASSIFY_REQ (0x68)

```rust
struct AdminReclassifyRequest {
    memory_id: MemoryId,
    new_kind: MemoryKind,
}
```

### 25. ADMIN_LIST_TOMBSTONED_REQ (0x69)

```rust
struct AdminListTombstonedRequest {
    session_id: Option<SessionId>,
    max_age_seconds: u32,                    // tombstoned within this many seconds
    limit: u32,                              // max returned
}
```

Returns a streaming response with one tombstoned memory's details per frame. Used for debugging and emergency recovery scenarios.

### 26. LINK_REQ (0x25) / UNLINK_REQ (0x26)

The explicit edge verbs. `ENCODE` no longer carries an `edges` list; a client that wants a specific edge writes it with `LINK`.

```rust
struct LinkRequest {
    source: MemoryId,
    target: MemoryId,
    kind: EdgeKind,
    weight: f32,
    request_id: RequestId,                   // required for idempotency
    txn_id: Option<TxnId>,
    act_as: Option<ActAs>,
}

struct UnlinkRequest {
    source: MemoryId,
    target: MemoryId,
    kind: EdgeKind,                          // (source, target, kind) identifies the edge
    request_id: RequestId,
    txn_id: Option<TxnId>,
    act_as: Option<ActAs>,
}
```

Both endpoints must resolve within the caller's effective `(namespace, space)`. Both ops publish an `EdgeAdded` / `EdgeRemoved` event on the unified edge feed (§43.3).

### 27. MEMORY_LIST_REQ (0x27)

Keyset-paginated enumeration of the caller's memories. Unlike `RECALL` this is not a ranked retrieval — it is an ordered scan with filters, for operator tooling, exports and UI listings.

```rust
struct MemoryListRequest {
    sort: MemoryListSort,                    // Created = 0 | Salience = 1 | Occurred = 2 | LastAccessed = 3
    dir: MemoryListDir,                      // Asc = 0 | Desc = 1
    limit: u32,
    cursor: Vec<u8>,                         // opaque keyset cursor; empty for the first page
    kinds: Vec<MemoryKind>,                  // empty = all kinds
    include_tombstoned: bool,                // default false
    time_axis: MemoryListTimeAxis,           // Created = 0 | Occurred = 1 — which clock the range applies to
    from_unix_nanos: u64,
    to_unix_nanos: u64,
    salience_min: f32,
    salience_max: f32,
    text_contains: String,                   // empty = no substring filter
    act_as: Option<ActAs>,
}
```

`time_axis` is what makes the time range unambiguous: a range over `Created` filters on the server's write time, a range over `Occurred` filters on the client-supplied event time. The two are genuinely different questions and the request says which one it is asking.

The cursor is opaque and keyset-based (not an offset), so a page is stable under concurrent writes. Clients MUST treat it as bytes and MUST NOT construct one.

### 28. MEMORY_INSPECT_REQ (0x28)

```rust
struct MemoryInspectRequest {
    memory_id: MemoryId,
    act_as: Option<ActAs>,
}
```

Returns the durable write-artifact bundle for one memory, so **any** memory can be inspected in the per-stage view — not only the one just written via `ENCODE(wait: derived)`. Read-only.

Three outcomes, deliberately kept apart:

- **The memory exists and its bundle is populated** — success, with the artifact filled in.
- **The memory exists but no bundle has been written yet** — success, with an *empty* artifact. The bundle is populated incrementally by the write path and the async workers, so a freshly-acked write legitimately has nothing in it yet.
- **The memory does not exist, belongs to another `(namespace, space)`, or has been forgotten** — `MemoryNotFound`. A tombstoned memory is gone for this verb exactly as it is for `RECALL` and for `MEMORY_LIST`; answering a forgotten memory with an empty-artifact success would collide with the second case above, which is the one condition this response shape exists to express.

A foreign-tenant id is deliberately indistinguishable from a missing one.

### 29. GET_CAPABILITIES_REQ (0x32)

```rust
struct GetCapabilitiesRequest {
    act_as: Option<ActAs>,
}
```

Available to every authenticated client; **not** admin-only. Returns the per-shard feature flags so clients can avoid issuing requests the shard cannot serve.

### 30. GRAPH_FETCH_REQ (0x163)

Paginated export of the caller's whole typed graph — nodes and edges.

```rust
struct GraphFetchRequest {
    limit: u32,
    cursor: Vec<u8>,                         // opaque; empty for the first page
    include_statements: bool,
    include_memories: bool,
    include_memory_edges: bool,
    include_tombstoned: bool,
    act_as: Option<ActAs>,
}
```

`include_memories` and `include_memory_edges` are what let one export carry both layers: the typed graph (entities, statements, relations) and the cognitive layer (memories and the edges between them) in a single node/edge vocabulary.

### 31. MATERIALIZE_PROCEDURAL_REQ (0x164)

Renders a space's stored `brain:behavior_*` Preference statements into a system block ready for LLM prompt injection.

```rust
struct MaterializeProceduralRequest {
    space_id: SpaceId,
    session_filter: Option<Vec<SessionId>>,
    top_k: u32,
    min_confidence: f32,
    categories: Vec<String>,                 // empty = every behaviour category
    request_id: RequestId,
    act_as: Option<ActAs>,
}
```

### 32. SPACE_CREATE_REQ (0x70) / SPACE_LIST_REQ (0x71) / SPACE_DELETE_REQ (0x72)

The space registry. **Non-admin**, scoped to the caller's `(namespace, space)`. `CREATE` and `DELETE` act on the caller's *effective* space (selected by `act_as`, or the key's bound space); `LIST` enumerates the caller's namespace's spaces.

```rust
struct SpaceCreateRequest {
    metadata: Option<Vec<u8>>,               // opaque caller blob (quota hints, labels)
    request_id: RequestId,
    act_as: Option<ActAs>,
}

struct SpaceListRequest {
    limit: u32,                              // 0 = no cap
    act_as: Option<ActAs>,
}

struct SpaceDeleteRequest {
    request_id: RequestId,
    act_as: Option<ActAs>,
}
```

`SPACE_CREATE` is idempotent: a create for an existing space returns the existing row with `created = false`. Spaces are also created lazily by the first write that names one, so an explicit create is for provisioning metadata ahead of time, not a precondition for writing.

`SPACE_DELETE` is GDPR erasure of the caller's effective space: every row under `(namespace, space)` is removed, hard and immediately. There is no soft mode.

### 33. SESSION_CREATE_REQ (0x73) / SESSION_LIST_REQ (0x74) / SESSION_DELETE_REQ (0x75)

The session registry. **Non-admin**, scoped to the caller's `(namespace, space)`. A session is a conversation/run grouping within a space — soft grouping, never an isolation boundary.

```rust
struct SessionCreateRequest {
    session_id: SessionId,                   // the client's opaque u64
    title: Option<String>,
    request_id: RequestId,
    act_as: Option<ActAs>,
}

struct SessionListRequest {
    limit: u32,                              // 0 = no cap
    act_as: Option<ActAs>,
}

struct SessionDeleteRequest {
    session_id: SessionId,
    hard: bool,                              // false (default) = soft, 7-day grace; true = zero immediately
    request_id: RequestId,
    act_as: Option<ActAs>,
}
```

`SESSION_CREATE` is idempotent. `SESSION_LIST` returns one space's sessions newest-first by `last_active`. The default session (`session_id = 0`) is **non-deletable**.

These are the tenant-facing registry ops. The admin-gated `ADMIN_CREATE_SESSION` / `ADMIN_RENAME_SESSION` (§21, §22) are a separate operator surface: they name sessions and are not `act_as`-aware.

### 34. EXTRACTOR_LIST_REQ (0x124)

```rust
struct ExtractorListRequest {
    act_as: Option<ActAs>,
}
```

Read-only introspection of the extractors registered for the effective caller's namespace.

### 35. ADMIN_BACKFILL_REQ (0x6E) / ADMIN_BACKFILL_CANCEL_REQ (0x6F)

Operator control surface for the per-shard backfill worker: re-run extractors over a `(memory_range × extractor_ids)` grid, and cancel an in-flight run by id.

```rust
struct AdminBackfillRequest {
    scope: BackfillScope,
    extractor_ids: Vec<u32>,                 // empty = every registered extractor
    dry_run: bool,
    request_id: RequestId,
}

enum BackfillScope {
    All,
    MemoryRange { start: MemoryId, end_inclusive: MemoryId },
}

struct AdminBackfillCancelRequest {
    backfill_id: [u8; 16],
    request_id: RequestId,
}
```

Admin-gated, and therefore **not** `act_as`-aware: a backfill acts on the shard, not on one tenant's rows.

### 36. ADMIN_LIST_PENDING_CONTRADICTIONS_REQ (0x178)

```rust
struct AdminListPendingContradictionsRequest {
    limit: u32,
}
```

The one implemented member of the typed-graph admin range. Lists contradiction audit rows the substrate detected but did not auto-resolve.

## Response Frames

### 37. ENCODE_RESP (0xA0)

```rust
struct EncodeResponse {
    memory_id: MemoryId,                     // the new memory's id (or the existing one, on dedup)
    was_deduplicated: bool,
    salience: f32,                           // the router-assigned salience
    auto_edges_added: u32,
    lsn: u64,                                // WAL LSN; `0` is the "no LSN" sentinel (see below)
    space_id: SpaceId,                       // 16 bytes; echoes the effective space
    session_id: SessionId,                   // echoes the request's session_id
    kind: MemoryKind,                        // the router-assigned kind
    created_at_unix_nanos: u64,              // server write time
    edges_out_count: u32,                    // outgoing edges that actually landed
    embedding_model_fp: [u8; 16],
    pending_stages: Vec<StageKind>,          // background stages this write queued
    has_active_schema: bool,
    trace: Option<EncodeTrace>,              // populated only when the request set wait: Derived
}
```

Fields:

- `memory_id` — always returned, even on dedup (returns the existing one's id).
- `was_deduplicated` — true when the write collapsed onto an existing row instead of creating one. Dedup is on by default; `allow_duplicates` opts out.
- `salience` — the salience the write router assigned. The client no longer supplies a hint, so this is the only place the value appears.
- `kind` — the kind the write router assigned.
- `auto_edges_added` — count of edges derived during encode. Doesn't include explicit edges written with `LINK`.
- `space_id` — the space the row was attributed to, echoed so the client can verify routing (particularly under `act_as`).
- `edges_out_count` — outgoing edges that actually landed. Edges whose targets are missing are dropped silently, so this counts the survivors.
- `embedding_model_fp` — the embedding-model fingerprint stamped on the row. Lets the client detect when a model migration would change the vector.
- `pending_stages` — the background stages this write queued. Each entry will emit a `SUBSCRIBE_EVENT` with `event_type = StageCompleted` once the owning worker commits (§43.2). Empty when the write triggered no background work — a dedup hit, or a deployment with those workers disabled.
- `has_active_schema` — whether a user schema is declared on the shard the write landed on. This is what separates two structurally identical "0 statements, 0 relations" outcomes: *no schema declares matching predicates* (a deployment-configuration story the client can report up front) from *a schema is declared and the extractor found no matching sentence* (a per-memory content story).
- `trace` — the full synchronous write-analysis trace, populated only when the request set `wait: Derived`, and omitted from the wire map otherwise so the default path pays nothing. The same per-stage data is available live over `SUBSCRIBE` keyed by `lsn` + `pending_stages`; `trace` is the alternative for a caller that wants the whole timeline back in one response without opening a stream.
- `lsn` — the WAL log-sequence-number at which this ENCODE was committed, suitable for chaining `encode → subscribe --start-lsn lsn+1`. **A value of `0` is a sentinel meaning "no LSN"** — returned when the request hit the fingerprint dedup index (no fresh WAL record was appended), when an idempotency replay returned a cached response that originated from a dedup hit, or on a deployment with no WAL sink. Clients chaining onto a subscription MUST treat `0` as "subscribe from tail" rather than "subscribe from position 0." Clients SHOULD expose this as an optional, treating `0` as absent.

#### 37.1 `EncodeTrace`

```rust
struct EncodeTrace {
    stages: Vec<EncodeTraceStage>,
    artifacts: EncodeTraceArtifacts,
    total_latency_us: u64,
}

struct EncodeTraceStage {
    name: String,                            // "embed", "wal", "auto_edge", "temporal_edge", "extractor", "hype", ...
    status: EncodeTraceStageStatus,
    latency_us: u64,
    detail: String,
    artifact: Option<EncodeStageArtifact>,
}

enum EncodeTraceStageStatus {
    Ok = 0,
    Skipped = 1,
    Failed = 2,
    Timeout = 3,                             // the stage outran the shard's trace drain window
}

struct EncodeStageArtifact {
    vector: Vec<f32>,
    record: Option<EncodeStageRecord>,
    hype_questions: Vec<String>,
    keyword_fields: Vec<EncodeStageKeywordField>,
    graph: Option<EncodeStageGraph>,
}

struct EncodeStageRecord {
    memory_id: MemoryId,
    kind: u8,
    salience: f32,
    created_at_unix_nanos: u64,
    occurred_at_unix_nanos: u64,
    vector_dim: u32,
    text_len: u32,
    lsn: u64,
}

struct EncodeStageKeywordField { field: String, terms: Vec<String> }

struct EncodeStageGraph {
    nodes: Vec<EncodeGraphNode>,             // { id, name, kind, type_qname }
    edges: Vec<EncodeGraphEdge>,             // { source, target, predicate, kind, confidence, event_at_unix_nanos }
}

struct EncodeTraceArtifacts {
    entities: Vec<EncodeTraceEntity>,        // { id, name, type_qname }
    statements: Vec<EncodeTraceStatement>,   // { id, subject_name, predicate, object_name, confidence, event_at_unix_nanos }
    relations: Vec<EncodeTraceRelation>,     // { source_name, predicate, target_name }
    indexes: Vec<EncodeTraceIndex>,          // { name, status } — e.g. memory_hnsw, memory_text, statement_text
    dedup: EncodeTraceDedup,                 // { was_deduplicated, matched_memory_id }
}
```

`Timeout` is not a failure: it means the stage had not reported by the time the shard's trace drain window closed. The stage keeps running, and its result is visible later through `MEMORY_INSPECT` (§63) or a `StageCompleted` event.

### 38. ENCODE_VECTOR_DIRECT_RESP (0xAA)

Same structure as `ENCODE_RESP`. The server doesn't distinguish: a successful direct-vector encode returns the same response shape as a text-only encode. `trace` is always `None` — `ENCODE_VECTOR_DIRECT` has no `wait` field.

### 39. RECALL_RESP (0xA1)

Streaming. Multiple frames per query.

```rust
struct RecallResponseFrame {
    answer_kind: AnswerKind,                 // the shape of the answer
    memories: Vec<MemoryResult>,
    is_final: bool,                          // matches the EOS flag, redundantly
    cumulative_count: u32,                   // total results emitted so far
    estimated_remaining: Option<u32>,         // server's estimate; may be None
    trace: Option<RecallTrace>,              // populated on the FINAL frame when the request set trace = true
}

enum AnswerKind {
    Single,                                  // one memory is the answer; `memories` has exactly one entry
    Many,                                    // several memories together are the answer; 2+ entries
    None,                                    // the substrate has no memory that answers the cue; `memories` is empty
}
```

`answer_kind` is **pure cardinality**, decided by the server's router from the stored data. There is deliberately no retrieval-mechanism vocabulary in it — no "episodic", no "grounded" — because how the router found the memories is an internal concern the caller never sees. `None` is a first-class answer: absence is reported explicitly, never as a fabricated guess or an arbitrary low-scoring hit.

```rust
struct MemoryResult {
    memory_id: MemoryId,
    text: String,                            // empty unless include_text = true
    similarity_score: f32,                   // raw cosine similarity
    confidence: f32,                         // calibrated confidence in [0, 1]
    salience: f32,                           // current salience
    kind: MemoryKind,
    space_id: SpaceId,
    session_id: SessionId,
    created_at_unix_nanos: u64,              // server write time
    last_accessed_at_unix_nanos: u64,
    edges: Option<Vec<EdgeView>>,            // None unless include_edges = true
    contributing_retrievers: Vec<RetrieverName>,  // which lanes surfaced this item
    fused_score: f32,                        // the post-fusion score it was ranked on
    rerank_score: Option<f32>,               // None when rerank is disabled (the default)
    salience_initial: f32,
    access_count: u32,
    lsn: u64,
    flags: u32,
    consolidated_at_unix_nanos: Option<u64>,
    occurred_at_unix_nanos: Option<u64>,     // client event time, echoed from ENCODE; None if not supplied
    edges_out_count: u32,
    edges_in_count: u32,
    graph: Option<GraphEnrichment>,          // None unless include_graph = true (and the memory was extracted)
}

struct EdgeView { target: MemoryId, kind: EdgeKind, weight: f32 }

struct GraphEnrichment {
    entities: Vec<EnrichedEntity>,           // { id, name, type_qname }
    statements: Vec<EnrichedStatement>,      // { id, subject_name, predicate, object_label, object_entity_id, confidence, event_at_unix_nanos }
    relations: Vec<EnrichedRelation>,        // { from_name, predicate, to_name }
}
```

Results never carry raw vectors: there is no `include_vectors` flag, no `vector_offset` / `vector_dim`, and no raw section on a `RECALL_RESP` frame.

#### 39.1 `RecallTrace`

Returned on the final frame when the request set `trace = true`. It is the read-side counterpart to `ENCODE(wait: derived)`'s `EncodeTrace`, and reports the data the read pipeline already computes internally rather than re-running anything.

```rust
struct RecallTrace {
    retrievers: Vec<RecallTraceRetriever>,
    filter_chain: RecallTraceFilterChain,
    rerank: Option<RecallTraceRerank>,
    total_latency_ms: f64,
    fusion: Option<RecallTraceFusion>,
}

struct RecallTraceRetriever {
    name: RetrieverName,                     // Semantic | Lexical | Graph
    status: RecallTraceRetrieverStatus,      // Success | Skipped | Timeout | Failure
    status_detail: String,                   // why it was skipped or how it failed
    latency_ms: f64,
    candidate_count: u32,
    candidates: Vec<RecallTraceCandidate>,   // { item_id, kind, text, score }
}

struct RecallTraceFilterChain {
    before: u32,
    after_type: u32, after_temporal: u32, after_confidence: u32,
    after_tombstone: u32, after_supersession: u32, after_as_of: u32, after_limit: u32,
    dropped_by_type: Vec<MemoryId>,
    dropped_by_temporal: Vec<MemoryId>,
    dropped_by_confidence: Vec<MemoryId>,
    dropped_by_tombstone: Vec<MemoryId>,
    dropped_by_supersession: Vec<RecallTraceDroppedId>,
    dropped_by_as_of: Vec<RecallTraceDroppedId>,
    dropped_by_limit: Vec<RecallTraceDroppedId>,
}

struct RecallTraceFusion { items: Vec<RecallTraceFusionItem> }  // { memory_id, rrf_score, lane_scores }
struct RecallTraceRerank { applied: bool, candidates: u32, latency_ms: f64, before_order: Vec<MemoryId>, after_order: Vec<MemoryId> }
```

The filter chain reports both survivor counts **and** the ids dropped at each stage, so "why is this memory not in my results?" is answerable from one response instead of a bisect.

### 40. PLAN_RESP (0xA2)

Streaming. Each frame carries one or more plan steps.

```rust
struct PlanResponseFrame {
    steps: Vec<PlanStep>,
    is_final: bool,
    plan_status: Option<PlanStatus>,         // set on final frame
    trace: Option<PlanTrace>,                // set on final frame when the request set trace = true
}

struct PlanStep {
    step_index: u32,
    memory_id: MemoryId,
    text: String,                            // brief description
    transition_kind: TransitionKind,         // how this step relates to the prior
    confidence: f32,
    estimated_distance_to_goal: f32,
}

enum TransitionKind {
    Initial,                                 // start state
    Causal,                                  // followed a CAUSED edge
    Temporal,                                // followed a FOLLOWED_BY edge
    Similarity,                              // similarity-driven
    Other(String),
}

enum PlanStatus {
    GoalReached,
    BudgetExhausted,
    NoPathFound,
    Cancelled,
}
```

The plan status is set only on the final frame; intermediate frames have `plan_status = None`.

### 41. REASON_RESP (0xA3)

Streaming. Each frame carries one inference step.

```rust
struct ReasonResponseFrame {
    inferences: Vec<InferenceStep>,
    is_final: bool,
    reason_status: Option<ReasonStatus>,
    trace: Option<ReasonTrace>,              // set on final frame when the request set trace = true
}

struct InferenceStep {
    step_index: u32,
    claim: String,                           // natural-language description
    supporting_memories: Vec<MemoryId>,
    contradicting_memories: Vec<MemoryId>,
    confidence: f32,
    inference_kind: InferenceKind,
}

enum InferenceKind {
    CausalExplanation,                       // walked CAUSED edges
    EvidenceAccumulation,                    // gathered SUPPORTS/CONTRADICTS
    AnalogicalInference,                     // VSA-based
    Other(String),
}

enum ReasonStatus {
    Complete,
    BudgetExhausted,
    DepthLimitReached,
    Cancelled,
}
```

### 42. FORGET_RESP (0xA4)

```rust
struct ForgetResponse {
    memory_id: MemoryId,
    was_already_forgotten: bool,             // idempotent retry case
    edges_removed: u32,                      // outgoing edges that were cleaned up
}
```

### 43. SUBSCRIBE_EVENT (0xB0)

A push event matching the subscription. May arrive at any time after `SUBSCRIBE_REQ`.

```rust
struct SubscriptionEvent {
    event_type: EventType,
    memory_id: MemoryId,
    session_id: SessionId,
    text: String,                            // present for Encoded events
    kind: MemoryKind,
    salience: f32,
    timestamp_unix_nanos: u64,
    lsn: u64,                                // log sequence number; for resumption
    graph_payload: Option<GraphEventPayload>, // typed-graph body; None for cognitive events
    edge_payload: Option<EdgeEventPayload>,   // edge-change body; set for EdgeAdded/Removed/Superseded
    stage_kind: Option<StageKind>,            // populated only for StageCompleted
    stage_outcome: Option<StageOutcome>,      // populated only for StageCompleted
    stage_payload: Option<StagePayload>,      // populated only for StageCompleted
}

#[repr(u8)]
enum EventType {
    // Cognitive events.
    Encoded = 0,
    Forgotten = 1,
    Reclaimed = 2,                           // slot reclaimed (rare; only seen with low filter sensitivity)
    KindChanged = 3,

    // Typed-graph events; graph_payload is populated.
    EntityCreated = 16,
    EntityUpdated = 17,
    EntityRenamed = 18,
    EntityMerged = 19,
    EntityUnmerged = 20,
    EntityTombstoned = 21,
    StatementCreated = 22,
    StatementSuperseded = 23,
    StatementTombstoned = 24,
    RelationCreated = 25,
    RelationSuperseded = 26,

    // Write-pipeline observability; see §43.2.
    StageCompleted = 27,

    SchemaUpdated = 29,
    RelationTombstoned = 30,                 // appended here to keep prior discriminants stable

    // Unified edge change feed; see §43.3.
    EdgeAdded = 31,
    EdgeRemoved = 32,
    EdgeSuperseded = 33,
}
```

Discriminants are explicit and stable: `28` is unassigned, and `RelationTombstoned` sits at `30` rather than beside the other relation events, because both were appended after the earlier variants shipped. Clients MUST treat an unknown discriminant as "ignore this event", not as an error.

When the subscription is unsubscribed, a final frame with the EOS flag is sent. The final frame may carry no events; it just signals end-of-stream.

#### 43.1 Typed-graph events

For cognitive events (`Encoded`, `Forgotten`, `Reclaimed`, `KindChanged`), `graph_payload` is `None`. The standard fields (`memory_id`, `session_id`, `kind`, `salience`, `text`) carry the event-specific data.

For typed-graph events, the standard fields are zero-filled where they don't apply (e.g. `memory_id = MemoryId::zero()` for `EntityCreated`), and the typed `GraphEventPayload` carries the event-specific data:

```rust
enum GraphEventPayload {
    EntityCreated(EntityCreatedEvent),       // { entity_id, entity_type_id, canonical_name }
    EntityUpdated(EntityUpdatedEvent),       // + embedding_version_changed
    EntityRenamed(EntityRenamedEvent),       // { entity_id, old_canonical_name, new_canonical_name, old_moved_to_alias }
    EntityMerged(EntityMergedEvent),         // { survivor, merged, audit_id, confidence, statements_rerouted, relations_rerouted }
    EntityUnmerged(EntityUnmergedEvent),     // { restored_entity_id, from_survivor, audit_id }
    EntityTombstoned(EntityTombstonedEvent), // { entity_id, reason }
    StatementCreated(StatementCreatedEvent), // { statement_id, kind, subject, predicate, confidence }
    StatementSuperseded(StatementSupersededEvent), // { old_statement_id, new_statement_id, chain_root }
    StatementTombstoned(StatementTombstonedEvent), // { statement_id, reason }
    RelationCreated(RelationCreatedEvent),   // { relation_id, relation_type, from, to }
    RelationSuperseded(RelationSupersededEvent),   // { old_relation_id, new_relation_id }
    RelationTombstoned(RelationTombstonedEvent),   // { relation_id, reason }
    SchemaUpdated(SchemaUpdatedEvent),       // { namespace, from_version, to_version, backward_compatible }
}
```

Cognitive event types and typed-graph event types share one envelope on the wire; the optional payload field carries the graph body inline.

#### 43.2 `StageCompleted` events (write-pipeline observability)

For `EventType::StageCompleted`, `stage_kind`, `stage_outcome` and `stage_payload` are populated and the standard fields (`text`, `salience`, `kind`, ...) are zero-filled the same way typed-graph events zero-fill them; `memory_id` and `lsn` are always meaningful (they identify which write the stage belongs to and its WAL position).

```rust
#[repr(u8)]
enum StageKind {
    AutoEdge = 0,      // `SimilarTo` edges derived from HNSW k-NN of the new memory
    TemporalEdge = 1,  // `FollowedBy` edges derived from session adjacency
    Extractor = 2,     // entities/statements/relations extracted via the three-tier pipeline
    Hype = 3,          // write-time hypothetical questions generated, embedded, and indexed
}

#[repr(u8)]
enum StageOutcome {
    Ok = 0,            // the stage ran and produced output
    Empty = 1,         // the stage ran to completion and found nothing
    Failed = 2,        // the stage could not run, or ran and errored
}

enum StagePayload {
    AutoEdge(StageAutoEdgePayload),
    TemporalEdge(StageTemporalEdgePayload),
    Extractor(StageExtractorPayload),
    Hype(StageHypePayload),
}

struct StageAutoEdgePayload { edges_written: u32 }
struct StageTemporalEdgePayload { edges_written: u32 }

struct StageHypePayload {
    questions_written: u32,
    cost_micro_usd: u64,                     // LLM cost; 0 on a cache hit
    error_message: String,                   // why the stage failed; empty unless outcome = Failed
}

struct StageExtractorPayload {
    entity_count: u32,
    statement_count: u32,
    relation_count: u32,
    audit_status: StageAuditStatus,          // Succeeded = 0 | PartiallyApplied = 1 | Failed = 2 | Skipped = 3
    error_message: String,                   // why the stage failed; empty unless it failed
}
```

**`Empty` and `Failed` are different answers and MUST NOT be collapsed.** A stage that ran and found nothing to write (`Empty`, zero counts) and a stage that could not run at all (`Failed`, also zero counts) produce identical counters, and the difference is the whole story: the first is a property of the memory's content, the second is a broken or unconfigured deployment. `stage_outcome` carries the distinction and `error_message` carries the reason — this is how a client sees that HyPE was skipped because the namespace has no registered LLM credential (see [`07_error_handling.md`](07_error_handling.md) §3.9) rather than because the text yielded no questions.

A write triggers at most one `StageCompleted` event per `StageKind` — up to four per `ENCODE` (`AutoEdge`, `TemporalEdge`, `Extractor`, `Hype`), matching the `pending_stages` list on the `ENCODE_RESP` — published by the background worker that owns that stage as soon as it commits, in whatever order the workers actually finish (they are independent; a subscriber MUST NOT assume a fixed sequence). This is the live counterpart to the `EncodeTrace` returned by `ENCODE(wait: derived)` (see [05. Operations](../05_operations/02_write_pipeline.md) §17): `wait: derived` blocks for the same events and returns one populated snapshot afterward, while a `memory_ids`-scoped SUBSCRIBE (§7) surfaces each one live as it happens, without blocking the write. See §17e there for the recommended pattern combining `wait: ack` with a scoped SUBSCRIBE.

#### 43.3 Edge-change events

`EdgeAdded`, `EdgeRemoved` and `EdgeSuperseded` are a **unified** change feed: cognitive `LINK` / `UNLINK` and typed-relation create / supersede / tombstone all flow through these three variants, so a subscriber watching "what edges exist" needs one handler rather than two parallel sets. The `edge_payload` sidecar carries the discriminating detail.

```rust
struct EdgeEventPayload {
    from_kind: u8,                           // node kind of the source
    from_id: Uuid,
    to_kind: u8,
    to_id: Uuid,
    edge_kind_tag: u8,                       // which edge family this is (cognitive edge vs typed relation)
    edge_kind_byte: u8,                      // the EdgeKind discriminant, for cognitive edges
    relation_type_id: Option<u32>,           // the typed relation's type, for relation edges
    weight: f32,
    relation_id: Option<Uuid>,
    superseded_relation_id: Option<Uuid>,    // set on EdgeSuperseded
    origin: u8,                              // what produced the edge (client LINK vs a derivation worker)
}
```

### 44. UNSUBSCRIBE_RESP (0xB1)

```rust
struct UnsubscribeResponse {
    target_stream_id: u32,                   // the stream that was unsubscribed
    final_lsn: u64,                          // LSN of the last event delivered
}
```

The response confirms the unsubscribe; the targeted subscription's stream gets a separate EOS frame.

### 45. TXN_BEGIN_RESP (0xC0)

```rust
struct TxnBeginResponse {
    txn_id: TxnId,                           // confirms the client's id
    timeout_seconds: u32,                    // server's chosen timeout (may be different from request)
    started_at_unix_nanos: u64,
}
```

### 46. TXN_COMMIT_RESP (0xC1)

```rust
struct TxnCommitResponse {
    txn_id: TxnId,
    committed_at_unix_nanos: u64,
    operations_applied: u32,                 // count of ops in the txn
}
```

If commit fails, `ERROR` is returned instead, with `code = TransactionConflict` or another specific code.

### 47. TXN_ABORT_RESP (0xC2)

```rust
struct TxnAbortResponse {
    txn_id: TxnId,
    operations_discarded: u32,
}
```

### 48. CANCEL_STREAM_ACK (0xD0)

```rust
struct CancelStreamAck {
    target_stream_id: u32,
    cancelled_at_unix_nanos: u64,
}
```

After this ack, the server emits a final EOS frame on `target_stream_id` if it hasn't already.

### 49. PONG (0x90)

```rust
struct PongResponse {
    client_timestamp_unix_nanos: u64,        // echoed from PING
    server_timestamp_unix_nanos: u64,
}
```

### 50. SERVER_PING (0x91)

Server-initiated keepalive.

```rust
struct ServerPingResponse {
    server_timestamp_unix_nanos: u64,
}
```

The client responds with `CLIENT_PONG`.

### 51. ADMIN_STATS_RESP (0xE0)

```rust
struct AdminStatsResponse {
    summary: StatsSummary,
    per_shard: Option<Vec<ShardStats>>,
    per_session: Option<Vec<SessionStats>>,
    server_uptime_seconds: u64,
    server_version: String,
}

struct StatsSummary {
    total_memories: u64,
    total_active_memories: u64,
    total_tombstoned_memories: u64,
    total_sessions: u32,
    encode_qps: f32,                         // recent average
    recall_qps: f32,
    p99_encode_latency_ms: f32,
    p99_recall_latency_ms: f32,
    resident_memory_bytes: u64,
    disk_used_bytes: u64,
}

struct ShardStats {
    shard_id: u16,
    memory_count: u64,
    salience_distribution: SalienceHistogram,
    wal_segment_count: u32,
    last_checkpoint_lsn: u64,
    arena_used_bytes: u64,
}

struct SalienceHistogram {
    buckets: [u32; 10],                      // bucket counts for [0,0.1), [0.1,0.2), ..., [0.9,1.0]
}

struct SessionStats {
    session_id: SessionId,
    name: String,
    memory_count: u64,
    last_encoded_at_unix_nanos: u64,
    last_recalled_at_unix_nanos: u64,
}
```

### 52. ADMIN_SNAPSHOT_RESP (0xE1)

```rust
struct AdminSnapshotResponse {
    snapshot_id: [u8; 16],
    snapshot_name: String,
    snapshot_path: String,
    started_at_unix_nanos: u64,
    completed_at_unix_nanos: u64,
    bytes_written: u64,
    used_reflink: bool,                      // true if reflink was used (vs full copy)
}
```

### 53. ADMIN_RESTORE_RESP (0xE2)

```rust
struct AdminRestoreResponse {
    snapshot_name: String,
    shards_restored: Vec<ShardId>,
    completed_at_unix_nanos: u64,
    memories_restored: u64,
}
```

### 54. ADMIN_INTEGRITY_CHECK_RESP (0xE3)

```rust
struct AdminIntegrityCheckResponse {
    scope: CheckScope,
    issues_found: Vec<IntegrityIssue>,
    issues_repaired: u32,
    completed_at_unix_nanos: u64,
}

struct IntegrityIssue {
    issue_type: IntegrityIssueType,
    affected_memory_id: Option<MemoryId>,
    affected_shard_id: Option<u16>,
    description: String,
    repaired: bool,
}

enum IntegrityIssueType {
    VectorCorruption,
    TextCorruption,
    StaleEdge,
    OrphanIndex,
    SchemaVersionMismatch,
    Other(String),
}
```

### 55. ADMIN_MIGRATE_EMBEDDINGS_RESP (0xE4)

Streaming. Multiple frames per migration.

```rust
struct AdminMigrateEmbeddingsResponseFrame {
    is_final: bool,
    progress: MigrationProgress,
    status: Option<MigrationStatus>,
}

struct MigrationProgress {
    total_memories: u64,
    migrated_so_far: u64,
    failed_so_far: u64,
    current_qps: f32,
    estimated_remaining_seconds: u32,
}

enum MigrationStatus {
    InProgress,
    Completed,
    Failed(String),
    Cancelled,
}
```

Intermediate frames carry progress updates (every few seconds). The final frame carries `status = Completed | Failed | Cancelled`.

### 56. ADMIN_CREATE_SESSION_RESP (0xE5)

```rust
struct AdminCreateSessionResponse {
    session_id: SessionId,
    name: String,
}
```

### 57. ADMIN_RENAME_SESSION_RESP (0xE6)

```rust
struct AdminRenameSessionResponse {
    session_id: SessionId,
    new_name: String,
    old_name: String,
}
```

### 58. ADMIN_MOVE_MEMORY_RESP (0xE7)

```rust
struct AdminMoveMemoryResponse {
    memory_id: MemoryId,
    new_session_id: SessionId,
    old_session_id: SessionId,
}
```

### 59. ADMIN_RECLASSIFY_RESP (0xE8)

```rust
struct AdminReclassifyResponse {
    memory_id: MemoryId,
    new_kind: MemoryKind,
    old_kind: MemoryKind,
}
```

### 60. ADMIN_LIST_TOMBSTONED_RESP (0xE9)

Streaming. Each frame carries one tombstoned memory.

```rust
struct AdminListTombstonedResponseFrame {
    memory: TombstonedMemoryInfo,
    is_final: bool,
}

struct TombstonedMemoryInfo {
    memory_id: MemoryId,
    text: String,                            // preserved for soft-forgotten
    forgot_at_unix_nanos: u64,
    forget_mode: ForgetMode,
    age_seconds: u32,
    eligible_for_reclaim: bool,
}
```

### 61. LINK_RESP (0xA5) / UNLINK_RESP (0xA6)

```rust
struct LinkResponse {
    source: MemoryId,
    target: MemoryId,
    kind: EdgeKind,
    weight: f32,
    created_at_unix_nanos: u64,
    already_existed: bool,                   // idempotent retry case
}

struct UnlinkResponse {
    source: MemoryId,
    target: MemoryId,
    kind: EdgeKind,
    removed: bool,                           // false when there was no such edge
}
```

### 62. MEMORY_LIST_RESP (0xA7)

Streaming, keyset-paginated.

```rust
struct MemoryListResponseFrame {
    items: Vec<MemoryListItem>,
    next_cursor: Vec<u8>,                    // empty when the listing is exhausted
    cumulative_count: u32,
    is_final: bool,
}

struct MemoryListItem {
    memory_id: MemoryId,
    space_id: SpaceId,
    session_id: SessionId,
    text: String,
    kind: u8,
    state: u8,                               // live / tombstoned
    created_at_unix_nanos: u64,
    occurred_at_unix_nanos: u64,
    last_accessed_at_unix_nanos: u64,
    salience: f32,
    access_count: u32,
    source_request_id: [u8; 16],             // the ENCODE request that produced the row
    statement_count: u32,
    entity_count: u32,
    relation_count: u32,
}
```

The three derivation counts let a listing show at a glance which memories were actually grounded into the typed graph and which landed as bare text.

### 63. MEMORY_INSPECT_RESP (0xA8)

```rust
struct MemoryInspectResponse {
    memory_id: MemoryId,
    text: String,
    artifact: EncodeStageArtifact,           // the same bundle shape as EncodeTrace's per-stage artifact (§37.1)
    edges: Vec<MemoryEdgeWire>,
}

struct MemoryEdgeWire {
    kind: EdgeKind,
    direction: EdgeDirection,                // Outgoing = 0 | Incoming = 1
    peer: MemoryId,
    weight: f32,
    created_at_unix_nanos: u64,
    annotation: Option<String>,
}
```

`edges` carries **both directions**, each tagged. An inspect that returned only outgoing edges would not answer "what points at this memory?", which is most of what the edge view is for.

There is no `found: bool` on this response. An absent, foreign or forgotten memory is `MemoryNotFound`; an empty `artifact` means the bundle has not been written yet.

### 64. GET_CAPABILITIES_RESP (0xB2)

```rust
struct GetCapabilitiesResponse {
    capabilities: Capabilities,
}

struct Capabilities {
    rerank: bool,                            // false by default — rerank ships disabled
    llm_extractor: bool,
    classifier_extractor: bool,
    pattern_extractor: bool,
    schema_namespaces: Vec<String>,          // filtered to the effective caller's own namespace
    vector_dim: u16,
}
```

`schema_namespaces` is filtered to the effective caller's namespace for **every** caller, including an `act_as`-delegated one. It is an introspection surface, not a directory of other tenants.

### 65. GRAPH_FETCH_RESP (0x1E3)

Streaming, keyset-paginated.

```rust
struct GraphFetchResponseFrame {
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    next_cursor: Vec<u8>,
    is_final: bool,
}

struct GraphNode { id: [u8; 16], kind: u8, label: String, type_qname: String }
struct GraphEdge { from_id: [u8; 16], to_id: [u8; 16], kind: u8, label: String }

#[repr(u8)]
enum GraphNodeKind { Entity = 0, Statement = 1, Memory = 2 }

#[repr(u8)]
enum GraphEdgeKind {
    Relation = 0, Fact = 1, HasStatement = 2, Mentions = 3,
    // the cognitive edge kinds, carried in the same vocabulary
    Caused = 4, FollowedBy = 5, DerivedFrom = 6, SimilarTo = 7,
    Contradicts = 8, Supports = 9, References = 10, PartOf = 11,
}
```

An entity the export reaches through a statement is emitted as a node even when no statement **names** it — otherwise the edge referencing it would dangle, and a client reconstructing the graph would drop the edge or invent a placeholder node.

### 66. MATERIALIZE_PROCEDURAL_RESP (0x1E4)

```rust
struct MaterializeProceduralResponse {
    system_block: String,                    // rendered, ready for prompt injection
    statement_ids: Vec<StatementId>,         // provenance: which statements were rendered
    total_candidates: u32,
    trimmed_by_budget: bool,
}
```

`statement_ids` is the provenance trail: every line in `system_block` traces back to a stored statement, so a surprising behaviour in the prompt is attributable to a specific memory.

### 67. SPACE_CREATE_RESP (0xF0) / SPACE_LIST_RESP (0xF1) / SPACE_DELETE_RESP (0xF2)

```rust
struct SpaceCreateResponse {
    space: String,                           // the human-readable structured space string, not the derived id
    created: bool,                           // false on an idempotent replay
    created_at_unix_nanos: u64,
    last_active_unix_nanos: u64,
    memory_count: u64,
    session_count: u32,
}

struct SpaceListResponse {
    spaces: Vec<SpaceView>,                  // { space, created_at, last_active, memory_count, session_count }
    cross_shard_complete: bool,
}

struct SpaceDeleteResponse {
    space: String,
    existed: bool,
    memories_forgotten: u64,
}
```

These responses echo the **space string**, not the derived 16-byte `space_id`: the string is what the caller passed and what it can act on again.

`cross_shard_complete` is `false` whenever the listing covers only the caller-shard's spaces, which is the case in v1. Clients MUST treat `false` as "this listing is partial" rather than as an error or as a complete answer.

### 68. SESSION_CREATE_RESP (0xF3) / SESSION_LIST_RESP (0xF4) / SESSION_DELETE_RESP (0xF5)

```rust
struct SessionCreateResponse {
    space_id: SpaceId,
    session_id: SessionId,
    created: bool,                           // false on an idempotent replay
    created_at_unix_nanos: u64,
    last_active_unix_nanos: u64,
    memory_count: u32,
}

struct SessionListResponse {
    space_id: SpaceId,
    sessions: Vec<SessionView>,              // { session_id, created_at, last_active, title, memory_count }
}

struct SessionDeleteResponse {
    space_id: SpaceId,
    session_id: SessionId,
    existed: bool,
    memories_forgotten: u64,
}
```

### 69. EXTRACTOR_LIST_RESP (0x1A4)

Streaming.

```rust
struct ExtractorListResponseFrame {
    items: Vec<ExtractorListItem>,           // { extractor_id, namespace, name, kind, schema_version, created_at_unix_nanos }
    total: u32,
    is_final: bool,
}
```

### 70. ADMIN_BACKFILL_RESP (0xEE) / ADMIN_BACKFILL_CANCEL_RESP (0xEF)

```rust
struct AdminBackfillResponse {
    backfill_id: [u8; 16],
    progress: BackfillProgress,
}

struct AdminBackfillCancelResponse {
    backfill_id: [u8; 16],
    cancelled: bool,
    progress: BackfillProgress,
}

struct BackfillProgress {
    running: bool,
    completed: u64,
    failed: u64,
    skipped_already_completed: u64,
    last_processed_memory_id_present: bool,
    last_processed_memory_id: MemoryId,
}
```

`skipped_already_completed` is counted separately from `completed` so a re-run over an already-backfilled range reports honestly instead of looking like fresh work.

### 71. ADMIN_LIST_PENDING_CONTRADICTIONS_RESP (0x1F8)

```rust
struct AdminListPendingContradictionsResponse {
    contradictions: Vec<ContradictionAuditView>,
}

struct ContradictionAuditView {
    audit_id: Uuid,
    subject_id: Uuid,
    predicate_id: u32,
    contradicting_statement_ids: Vec<StatementId>,
    detected_at_unix_nanos: u64,
    outcome: u8,
}
```

### 72. ERROR (0xFF)

```rust
struct ErrorResponse {
    code: ErrorCode,
    category: ErrorCategory,
    message: String,
    details: Option<ErrorDetails>,
    retry_after_ms: Option<u32>,             // suggested retry delay
}

enum ErrorCategory {
    Protocol,                                // bad frame, version mismatch, etc.
    Authentication,                          // auth failed
    Authorization,                           // permissions denied
    Validation,                              // invalid argument
    NotFound,                                // memory_id, session_id missing
    Conflict,                                // idempotency, transaction conflict
    ResourceExhausted,                       // out of slots, out of disk, rate-limited
    Internal,                                // server bug
    Unavailable,                             // shard unavailable, server overloaded
}

struct ErrorDetails {
    field: Option<String>,                   // which field was invalid (for validation errors)
    expected: Option<String>,                // expected value (for validation)
    actual: Option<String>,                  // actual value
}
```

The full list of error codes is in [`07_error_handling.md`](07_error_handling.md).

---

*Continue to [`06_streaming.md`](06_streaming.md) for the streaming model.*
