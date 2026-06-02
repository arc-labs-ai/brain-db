# 04.05 Frame Layouts

The payload layout for every request and response frame. Frame headers are documented in [`02_wire_format.md`](02_wire_format.md); payload encoding (CBOR + little-endian f32) is in the same file. This file focuses on the structured fields per opcode.

## Request Frames

### 1. ENCODE_REQ (0x20)

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

### 2. ENCODE_VECTOR_DIRECT_REQ (0x2A)

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

### 3. RECALL_REQ (0x21)

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
    include_vectors: bool,                   // include vectors in results
    include_edges: bool,                     // include edges in results
    request_id: Option<RequestId>,           // optional; for tracing
}
```

RECALL is one verb with one server-side path-selection rule: a request that
carries a `txn_id` runs the txn path (read-your-writes requires the
per-txn buffer overlay, which the lexical and graph retrievers do not see);
every other request runs the hybrid path (semantic + lexical + memory-edge
graph, fused via RRF). The client cannot select between paths.

Fields:

- `top_k` — max results returned. Default 10. Hard cap: 1000.
- `confidence_threshold` — results with confidence below this are filtered out. Default: 0.0.
- `context_filter` — restrict to specific contexts. None means search across all contexts the agent owns. Up to 16 context IDs allowed.
- `age_bound_unix_nanos` — only return memories created after this time.
- `kind_filter` — restrict to specific kinds. None means all kinds.
- `salience_floor` — minimum salience for inclusion. Default 0.0.
- `include_vectors` — if true, response carries vector data. Default false (saves bandwidth).
- `include_edges` — if true, response includes each result's outgoing edges. Default false.
- `request_id` — optional; for tracing/logging only, not for idempotency.

### 4. PLAN_REQ (0x22)

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
}
```

Notes:

- `start` and `goal` can each be referenced by `MemoryId`, by text (server embeds), or by raw vector.
- `budget` is required; planning is unbounded otherwise.
- `strategy_hint` lets the planner override be controlled; `Auto` is the default.

### 5. REASON_REQ (0x23)

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

### 6. FORGET_REQ (0x24)

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

### 7. SUBSCRIBE_REQ (0x30)

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
}
```

Notes:

- The client supplies the `txn_id` (typically UUIDv7).
- The server validates that `txn_id` isn't already in use within the agent.

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
    reason: CancellationReason,              // optional context
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
struct ClientPongResponse {
    server_timestamp_unix_nanos: u64,        // the timestamp from SERVER_PING
    client_timestamp_unix_nanos: u64,        // current client time
}
```

### 15. BYE (0x1F)

```rust
struct ByeRequest {
    reason: Option<String>,                  // optional log context
}
```

### 16. ADMIN_STATS_REQ (0x60)

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

### 21. ADMIN_CREATE_CONTEXT_REQ (0x65)

```rust
struct AdminCreateContextRequest {
    name: String,
    description: String,
    request_id: RequestId,
}
```

### 22. ADMIN_RENAME_CONTEXT_REQ (0x66)

```rust
struct AdminRenameContextRequest {
    context_id: ContextId,
    new_name: String,
}
```

### 23. ADMIN_MOVE_MEMORY_REQ (0x67)

```rust
struct AdminMoveMemoryRequest {
    memory_id: MemoryId,
    new_context_id: ContextId,
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
    context_id: Option<ContextId>,
    max_age_seconds: u32,                    // tombstoned within this many seconds
    limit: u32,                              // max returned
}
```

Returns a streaming response with one tombstoned memory's details per frame. Used for debugging and emergency recovery scenarios.

## Response Frames

### 26. ENCODE_RESP (0xA0)

```rust
struct EncodeResponse {
    memory_id: MemoryId,                     // the new memory's id
    was_deduplicated: bool,                  // true if the memory was an existing duplicate
    salience: f32,                           // initial salience computed
    auto_edges_added: u32,                   // count of auto-derived edges added at encode
    lsn: u64,                                // WAL LSN; `0` is the "no LSN" sentinel (see below)
}
```

Fields:

- `memory_id` — always returned, even on dedup (returns the existing one's id).
- `was_deduplicated` — true when `deduplicate=true` was set in the request and the memory matched an existing one.
- `salience` — the initial salience assigned. Useful for the client to know how the agent's hint translated into a number.
- `auto_edges_added` — count of edges (e.g., `SIMILAR_TO`) derived during encode. Doesn't include explicit edges from the request.
- `lsn` — the WAL log-sequence-number at which this ENCODE was committed, suitable for chaining `encode → subscribe --start-lsn lsn+1`. **A value of `0` is a sentinel meaning "no LSN"** — returned when the request hit the fingerprint dedup index (no fresh WAL record was appended) or when an idempotency replay returned a cached response that originated from a dedup hit. Clients chaining onto a subscription MUST treat `0` as "subscribe from tail" rather than "subscribe from position 0." Clients SHOULD expose this as an optional, treating `0` as absent.

### 27. ENCODE_VECTOR_DIRECT_RESP (0xAA)

Same structure as `ENCODE_RESP`. The server doesn't distinguish: a successful direct-vector encode returns the same response shape as a text-only encode.

### 28. RECALL_RESP (0xA1)

Streaming. Multiple frames per query.

```rust
struct RecallResponseFrame {
    results: Vec<MemoryResult>,
    is_final: bool,                          // matches the EOS flag, redundantly
    cumulative_count: u32,                   // total results emitted so far
    estimated_remaining: Option<u32>,        // server's estimate; may be None
}

struct MemoryResult {
    memory_id: MemoryId,
    text: String,
    similarity_score: f32,                   // raw cosine similarity
    confidence: f32,                         // calibrated confidence in [0, 1]
    salience: f32,                           // current salience
    kind: MemoryKind,
    context_id: ContextId,
    created_at_unix_nanos: u64,
    last_accessed_at_unix_nanos: u64,
    vector_offset: u32,                      // 0 if vectors not requested
    vector_dim: u16,
    edges: Option<Vec<EdgeView>>,            // None if include_edges=false
}

struct EdgeView {
    target: MemoryId,
    kind: EdgeKind,
    weight: f32,
}
```

If `include_vectors = true` in the request, each result has its 1536-byte `f32` vector in the raw section.

### 29. PLAN_RESP (0xA2)

Streaming. Each frame carries one or more plan steps.

```rust
struct PlanResponseFrame {
    steps: Vec<PlanStep>,
    is_final: bool,
    plan_status: Option<PlanStatus>,         // set on final frame
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

### 30. REASON_RESP (0xA3)

Streaming. Each frame carries one inference step.

```rust
struct ReasonResponseFrame {
    inferences: Vec<InferenceStep>,
    is_final: bool,
    reason_status: Option<ReasonStatus>,
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

### 31. FORGET_RESP (0xA4)

```rust
struct ForgetResponse {
    memory_id: MemoryId,
    was_already_forgotten: bool,             // idempotent retry case
    edges_removed: u32,                      // outgoing edges that were cleaned up
}
```

### 32. SUBSCRIBE_EVENT (0xB0)

A push event matching the subscription. May arrive at any time after `SUBSCRIBE_REQ`.

```rust
struct SubscriptionEvent {
    event_type: EventType,
    memory_id: MemoryId,
    context_id: ContextId,
    text: String,                            // present for ENCODED events
    kind: MemoryKind,
    salience: f32,
    timestamp_unix_nanos: u64,
    lsn: u64,                                // log sequence number; for resumption
    knowledge_payload: Option<KnowledgeEventPayload>,  // typed-graph body; None for substrate events
}

enum EventType {
    // Substrate events
    Encoded,                                 // new memory created
    Forgotten,                               // memory forgotten
    Reclaimed,                               // slot reclaimed (rare; only seen with low filter sensitivity)
    KindChanged,                             // memory's kind changed

    // typed-graph events; knowledge_payload is populated.
    EntityCreated,
    EntityUpdated,
    EntityRenamed,
    EntityMerged,
    EntityUnmerged,
    EntityTombstoned,
    StatementCreated,
    StatementSuperseded,
    StatementTombstoned,
    RelationCreated,
    RelationSuperseded,
    ExtractionCompleted,
    ExtractionFailed,
    SchemaUpdated,
}
```

When the subscription is unsubscribed, a final frame with EOS flag is sent. The final frame may carry no events; it just signals end-of-stream.

#### 32.1 Typed-graph events

For substrate events (`Encoded`, `Forgotten`, `Reclaimed`, `KindChanged`), `knowledge_payload` is `None`. The standard fields (`memory_id`, `context_id`, `kind`, `salience`, `text`) carry the event-specific data.

For typed-graph events, the standard fields are zero-filled where they don't apply (e.g. `memory_id = MemoryId::zero()` for `EntityCreated`), and the typed `KnowledgeEventPayload` carries the event-specific data. The payload's shape per event type is defined in [`09_typed_graph_admin.md`](09_typed_graph_admin.md) §3.

The optional payload field carries the knowledge body inline; substrate event types and the knowledge event types share one envelope on the wire.

### 33. UNSUBSCRIBE_RESP (0xB1)

```rust
struct UnsubscribeResponse {
    target_stream_id: u32,                   // the stream that was unsubscribed
    final_lsn: u64,                          // LSN of the last event delivered
}
```

The response confirms the unsubscribe; the targeted subscription's stream gets a separate EOS frame.

### 34. TXN_BEGIN_RESP (0xC0)

```rust
struct TxnBeginResponse {
    txn_id: TxnId,                           // confirms the client's id
    timeout_seconds: u32,                    // server's chosen timeout (may be different from request)
    started_at_unix_nanos: u64,
}
```

### 35. TXN_COMMIT_RESP (0xC1)

```rust
struct TxnCommitResponse {
    txn_id: TxnId,
    committed_at_unix_nanos: u64,
    operations_applied: u32,                 // count of ops in the txn
}
```

If commit fails, `ERROR` is returned instead, with `code = TransactionConflict` or another specific code.

### 36. TXN_ABORT_RESP (0xC2)

```rust
struct TxnAbortResponse {
    txn_id: TxnId,
    operations_discarded: u32,
}
```

### 37. CANCEL_STREAM_ACK (0xD0)

```rust
struct CancelStreamAck {
    target_stream_id: u32,
    cancelled_at_unix_nanos: u64,
}
```

After this ack, the server emits a final EOS frame on `target_stream_id` if it hasn't already.

### 38. PONG (0x90)

```rust
struct PongResponse {
    client_timestamp_unix_nanos: u64,        // echoed from PING
    server_timestamp_unix_nanos: u64,
}
```

### 39. SERVER_PING (0x91)

Server-initiated keepalive.

```rust
struct ServerPingRequest {
    server_timestamp_unix_nanos: u64,
}
```

The client responds with `CLIENT_PONG`.

### 40. ADMIN_STATS_RESP (0xE0)

```rust
struct AdminStatsResponse {
    summary: StatsSummary,
    per_shard: Option<Vec<ShardStats>>,
    per_context: Option<Vec<ContextStats>>,
    server_uptime_seconds: u64,
    server_version: String,
}

struct StatsSummary {
    total_memories: u64,
    total_active_memories: u64,
    total_tombstoned_memories: u64,
    total_contexts: u32,
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

struct ContextStats {
    context_id: ContextId,
    name: String,
    memory_count: u64,
    last_encoded_at_unix_nanos: u64,
    last_recalled_at_unix_nanos: u64,
}
```

### 41. ADMIN_SNAPSHOT_RESP (0xE1)

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

### 42. ADMIN_RESTORE_RESP (0xE2)

```rust
struct AdminRestoreResponse {
    snapshot_name: String,
    shards_restored: Vec<ShardId>,
    completed_at_unix_nanos: u64,
    memories_restored: u64,
}
```

### 43. ADMIN_INTEGRITY_CHECK_RESP (0xE3)

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

### 44. ADMIN_MIGRATE_EMBEDDINGS_RESP (0xE4)

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

### 45. ADMIN_CREATE_CONTEXT_RESP (0xE5)

```rust
struct AdminCreateContextResponse {
    context_id: ContextId,
    name: String,
}
```

### 46. ADMIN_RENAME_CONTEXT_RESP (0xE6)

```rust
struct AdminRenameContextResponse {
    context_id: ContextId,
    new_name: String,
    old_name: String,
}
```

### 47. ADMIN_MOVE_MEMORY_RESP (0xE7)

```rust
struct AdminMoveMemoryResponse {
    memory_id: MemoryId,
    new_context_id: ContextId,
    old_context_id: ContextId,
}
```

### 48. ADMIN_RECLASSIFY_RESP (0xE8)

```rust
struct AdminReclassifyResponse {
    memory_id: MemoryId,
    new_kind: MemoryKind,
    old_kind: MemoryKind,
}
```

### 49. ADMIN_LIST_TOMBSTONED_RESP (0xE9)

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

### 50. ERROR (0xFF)

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
    NotFound,                                // memory_id, context_id missing
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
