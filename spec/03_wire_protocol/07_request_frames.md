# 03.07 Request Frame Layouts

This file specifies the payload layout for every client-to-server request frame. Frame headers are documented in [`03_frame_header.md`](03_frame_header.md); payload encoding (rkyv + bytemuck) in [`04_payload_encoding.md`](04_payload_encoding.md). Here we focus on the structured fields per opcode.

## 1. ENCODE_REQ (0x20)

```rust
struct EncodeRequest {
    text: String,
    context_id: ContextId,                   // 8 bytes; 0 for default
    kind: MemoryKind,                        // 1 byte; default Episodic
    salience_hint: f32,                      // [-1, +1]; default 0
    edges: Vec<EdgeRequest>,                 // outgoing edges to add at encode time
    request_id: RequestId,                   // 16 bytes; required for idempotency
    txn_id: Option<TxnId>,                   // 16 bytes; if part of a transaction
    deduplicate: bool,                       // if true, exact-text duplicates return existing memory_id
}

struct EdgeRequest {
    target: MemoryId,
    kind: EdgeKind,
    weight: f32,
}
```

Fields:

- `text` — the content to encode. UTF-8. Maximum length is server-configurable (default: 1 MiB).
- `context_id` — the context to encode into. Use 0 for default.
- `kind` — `Episodic` (0), `Semantic` (1), or `Consolidated` (2; rejected for client encode).
- `salience_hint` — agent-supplied importance signal in [-1, +1].
- `edges` — list of outgoing edges to create from this new memory. May be empty.
- `request_id` — UUIDv7 recommended; required for idempotent retry.
- `txn_id` — if non-None, the encode is buffered as part of the transaction.
- `deduplicate` — if true, the server checks for exact-text duplicates in the same context; if found, returns the existing memory_id without creating a new memory.

## 2. ENCODE_VECTOR_DIRECT_REQ (0x2A)

For power users with their own embedding pipeline.

```rust
struct EncodeVectorDirectRequest {
    text: String,
    vector_offset: u32,                      // offset into raw section
    vector_dim: u16,                         // 384 for bge-small-en-v1.5
    model_fingerprint: [u8; 16],             // BLAKE3-derived
    context_id: ContextId,
    kind: MemoryKind,
    salience_hint: f32,
    edges: Vec<EdgeRequest>,
    request_id: RequestId,
    txn_id: Option<TxnId>,
}
```

Followed in the raw section by the L2-normalized `f32` vector.

The server validates:

- `vector_dim` matches the configured embedding dimensionality.
- `model_fingerprint` matches a known model.
- The vector's L2 norm is approximately 1.0 (within epsilon).

## 3. RECALL_REQ (0x21)

```rust
struct RecallRequest {
    cue_text: String,                        // the query
    cue_vector_offset: u32,                  // 0 if text-only
    cue_vector_dim: u16,                     // 0 if text-only; 384 if vector pre-supplied
    top_k: u32,                              // max results
    confidence_threshold: f32,               // [0, 1]; results below this are excluded
    context_filter: Option<Vec<ContextId>>,  // None = no filter; up to 16 contexts
    age_bound_unix_nanos: Option<u64>,       // results must be newer than this
    kind_filter: Option<Vec<MemoryKind>>,    // None = all kinds
    salience_floor: f32,                     // [0, 1]; default 0
    strategy_hint: Option<RecallStrategy>,   // override planner
    include_vectors: bool,                   // include vectors in results
    include_edges: bool,                     // include edges in results
    request_id: Option<RequestId>,           // optional; for tracing
}

enum RecallStrategy {
    Auto,                // let the planner decide (recommended)
    AnnOnly,
    Attractor,
    GraphWalk,
    Hybrid,
}
```

Fields:

- `top_k` — max results returned. Default 10. Hard cap: 1000.
- `confidence_threshold` — results with confidence below this are filtered out. Default: 0.0.
- `context_filter` — restrict to specific contexts. None means search across all contexts the agent owns. Up to 16 context IDs allowed.
- `age_bound_unix_nanos` — only return memories created after this time.
- `kind_filter` — restrict to specific kinds. None means all kinds.
- `salience_floor` — minimum salience for inclusion. Default 0.0.
- `strategy_hint` — override the planner's strategy choice. `Auto` (default) lets the planner pick.
- `include_vectors` — if true, response carries vector data. Default false (saves bandwidth).
- `include_edges` — if true, response includes each result's outgoing edges. Default false.
- `request_id` — optional; for tracing/logging only, not for idempotency.

## 4. PLAN_REQ (0x22)

```rust
struct PlanRequest {
    start: PlanState,
    goal: PlanState,
    budget: PlanBudget,
    strategy_hint: Option<PlanStrategy>,
    context_filter: Option<Vec<ContextId>>,
    request_id: Option<RequestId>,
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
    Auto,
    AStar,
    Mcts,
    AttractorRollout,
}
```

Notes:

- `start` and `goal` can each be referenced by `MemoryId`, by text (server embeds), or by raw vector.
- `budget` is required; planning is unbounded otherwise.
- `strategy_hint` lets the planner override be controlled; `Auto` is the default.

## 5. REASON_REQ (0x23)

```rust
struct ReasonRequest {
    observation: ObservationInput,
    depth: u32,                              // max graph traversal depth
    confidence_threshold: f32,               // [0, 1]
    context_filter: Option<Vec<ContextId>>,
    max_inferences: u32,                     // max inference steps to emit
    budget_wall_time_ms: u32,                // wall-clock cap
    request_id: Option<RequestId>,
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

## 6. FORGET_REQ (0x24)

```rust
struct ForgetRequest {
    memory_id: MemoryId,
    mode: ForgetMode,                        // Soft or Hard
    request_id: RequestId,                   // required for idempotency
    txn_id: Option<TxnId>,
}

enum ForgetMode {
    Soft,                                    // tombstone; recoverable until reclaim
    Hard,                                    // overwrite; unrecoverable
}
```

Notes:

- `mode = Soft` is the default for `FORGET_REQ`; `Hard` requires the agent to have appropriate permissions.
- `request_id` is mandatory — like `ENCODE`, FORGET is idempotent on the request_id.

## 7. SUBSCRIBE_REQ (0x30)

```rust
struct SubscribeRequest {
    filter: SubscriptionFilter,
    include_history: bool,                   // start with snapshot of matching memories
    from_lsn: Option<u64>,                   // resume from a specific LSN
    max_inflight: u32,                       // server stops sending after this many unacked events
}

struct SubscriptionFilter {
    contexts: Option<Vec<ContextId>>,        // None = all contexts
    kinds: Option<Vec<MemoryKind>>,          // None = all kinds
    similar_to: Option<SimilarityFilter>,    // optional similarity match
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

## 8. UNSUBSCRIBE_REQ (0x31)

```rust
struct UnsubscribeRequest {
    target_stream_id: u32,                   // the stream_id of the SUBSCRIBE_REQ to cancel
}
```

The unsubscribe is sent as its own stream (different stream_id from the subscription it cancels). The server emits the final EOS on the original subscription stream once unsubscribe is complete.

## 9. TXN_BEGIN (0x40)

```rust
struct TxnBeginRequest {
    txn_id: TxnId,                           // client-supplied; 16 bytes
    timeout_seconds: u32,                    // auto-abort after this; default 60, max 300
}
```

Notes:

- The client supplies the `txn_id` (typically UUIDv7).
- The server validates that `txn_id` isn't already in use within the agent.

## 10. TXN_COMMIT (0x41)

```rust
struct TxnCommitRequest {
    txn_id: TxnId,
}
```

The server applies all buffered operations from this transaction atomically. If commit fails (e.g., a conflict), `ERROR` is returned.

## 11. TXN_ABORT (0x42)

```rust
struct TxnAbortRequest {
    txn_id: TxnId,
}
```

All buffered operations are discarded.

## 12. CANCEL_STREAM (0x50)

```rust
struct CancelStreamRequest {
    target_stream_id: u32,                   // the stream to cancel
    reason: CancellationReason,              // optional context
}

enum CancellationReason {
    ClientUnneeded,
    Timeout,
    Other(String),
}
```

The server stops emitting frames on `target_stream_id` and emits a final EOS frame.

## 13. PING (0x10)

```rust
struct PingRequest {
    client_timestamp_unix_nanos: u64,
}
```

## 14. CLIENT_PONG (0x11)

```rust
struct ClientPongResponse {
    server_timestamp_unix_nanos: u64,        // the timestamp from SERVER_PING
    client_timestamp_unix_nanos: u64,        // current client time
}
```

## 15. BYE (0x1F)

```rust
struct ByeRequest {
    reason: Option<String>,                  // optional log context
}
```

## 16. ADMIN_STATS_REQ (0x60)

```rust
struct AdminStatsRequest {
    detail: StatsDetail,
}

enum StatsDetail {
    Summary,                                 // top-level metrics only
    PerShard,                                // include per-shard breakdowns
    PerContext,                              // include per-context breakdowns
    Full,                                    // all of the above
}
```

## 17. ADMIN_SNAPSHOT_REQ (0x61)

```rust
struct AdminSnapshotRequest {
    snapshot_name: String,                   // human-readable label
    target_path: Option<String>,             // server-side path; None = default location
    include_wal: bool,                       // include the WAL in the snapshot
    request_id: RequestId,
}
```

## 18. ADMIN_RESTORE_REQ (0x62)

```rust
struct AdminRestoreRequest {
    snapshot_name: String,
    target_shard: Option<ShardId>,           // None = restore to all shards in snapshot
    request_id: RequestId,
}
```

## 19. ADMIN_INTEGRITY_CHECK_REQ (0x63)

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

## 20. ADMIN_MIGRATE_EMBEDDINGS_REQ (0x64)

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

## 21. ADMIN_CREATE_CONTEXT_REQ (0x65)

```rust
struct AdminCreateContextRequest {
    name: String,
    description: String,
    request_id: RequestId,
}
```

## 22. ADMIN_RENAME_CONTEXT_REQ (0x66)

```rust
struct AdminRenameContextRequest {
    context_id: ContextId,
    new_name: String,
}
```

## 23. ADMIN_MOVE_MEMORY_REQ (0x67)

```rust
struct AdminMoveMemoryRequest {
    memory_id: MemoryId,
    new_context_id: ContextId,
}
```

## 24. ADMIN_RECLASSIFY_REQ (0x68)

```rust
struct AdminReclassifyRequest {
    memory_id: MemoryId,
    new_kind: MemoryKind,
}
```

## 25. ADMIN_LIST_TOMBSTONED_REQ (0x69)

```rust
struct AdminListTombstonedRequest {
    context_id: Option<ContextId>,
    max_age_seconds: u32,                    // tombstoned within this many seconds
    limit: u32,                              // max returned
}
```

Returns a streaming response with one tombstoned memory's details per frame. Used for debugging and emergency recovery scenarios.

---

*Continue to [`08_response_frames.md`](08_response_frames.md) for response frame layouts.*
