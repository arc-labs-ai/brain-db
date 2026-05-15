# 03.08 Response Frame Layouts

This file specifies the payload layout for every server-to-client response frame. The layouts complement the request frames in [`07_request_frames.md`](07_request_frames.md).

## 1. ENCODE_RESP (0xA0)

```rust
struct EncodeResponse {
    memory_id: MemoryId,                     // the new memory's id
    was_deduplicated: bool,                  // true if the memory was an existing duplicate
    salience: f32,                           // initial salience computed
    auto_edges_added: u32,                   // count of auto-derived edges added at encode
}
```

Fields:

- `memory_id` — always returned, even on dedup (returns the existing one's id).
- `was_deduplicated` — true when `deduplicate=true` was set in the request and the memory matched an existing one.
- `salience` — the initial salience the substrate assigned. Useful for the client to know how the agent's hint translated into a number.
- `auto_edges_added` — count of edges (e.g., `SIMILAR_TO`) the substrate derived during encode. Doesn't include explicit edges from the request.

## 2. ENCODE_VECTOR_DIRECT_RESP (0xAA)

Same structure as `ENCODE_RESP`. The server doesn't distinguish: a successful direct-vector encode returns the same response shape as a text-only encode.

## 3. RECALL_RESP (0xA1)

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

## 4. PLAN_RESP (0xA2)

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

## 5. REASON_RESP (0xA3)

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

## 6. FORGET_RESP (0xA4)

```rust
struct ForgetResponse {
    memory_id: MemoryId,
    was_already_forgotten: bool,             // idempotent retry case
    edges_removed: u32,                      // outgoing edges that were cleaned up
}
```

## 7. SUBSCRIBE_EVENT (0xB0)

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
    knowledge_payload: Option<KnowledgeEventPayload>,  // §28/02 §3
}

enum EventType {
    // Substrate events
    Encoded,                                 // new memory created
    Forgotten,                               // memory forgotten
    Reclaimed,                               // slot reclaimed (rare; only seen with low filter sensitivity)
    KindChanged,                             // memory's kind changed

    // Knowledge-layer events (§28/02). knowledge_payload is populated.
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

### 7.1 Knowledge-layer events

For substrate events (`Encoded`, `Forgotten`, `Reclaimed`, `KindChanged`), `knowledge_payload` is `None`. The substrate fields (`memory_id`, `context_id`, `kind`, `salience`, `text`) carry the event-specific data.

For knowledge-layer events, the substrate fields are zero-filled where they don't apply (e.g. `memory_id = MemoryId::zero()` for `EntityCreated`), and the typed `KnowledgeEventPayload` carries the event-specific data. The payload's shape per event type is defined in [`../28_knowledge_wire_protocol/02_subscribe_events.md`](../28_knowledge_wire_protocol/02_subscribe_events.md) §3.

The optional payload field is **forward-compatible** — pre-knowledge-layer SDKs that don't decode `knowledge_payload` silently drop knowledge events (or surface them as opaque `event_type` codes). The extension was made in phase 16.7 under the pre-v1.0 compatibility policy ([`12_versioning.md`](12_versioning.md) §0).

## 8. UNSUBSCRIBE_RESP (0xB1)

```rust
struct UnsubscribeResponse {
    target_stream_id: u32,                   // the stream that was unsubscribed
    final_lsn: u64,                          // LSN of the last event delivered
}
```

The response confirms the unsubscribe; the targeted subscription's stream gets a separate EOS frame.

## 9. TXN_BEGIN_RESP (0xC0)

```rust
struct TxnBeginResponse {
    txn_id: TxnId,                           // confirms the client's id
    timeout_seconds: u32,                    // server's chosen timeout (may be different from request)
    started_at_unix_nanos: u64,
}
```

## 10. TXN_COMMIT_RESP (0xC1)

```rust
struct TxnCommitResponse {
    txn_id: TxnId,
    committed_at_unix_nanos: u64,
    operations_applied: u32,                 // count of ops in the txn
}
```

If commit fails, `ERROR` is returned instead, with `code = TransactionConflict` or another specific code.

## 11. TXN_ABORT_RESP (0xC2)

```rust
struct TxnAbortResponse {
    txn_id: TxnId,
    operations_discarded: u32,
}
```

## 12. CANCEL_STREAM_ACK (0xD0)

```rust
struct CancelStreamAck {
    target_stream_id: u32,
    cancelled_at_unix_nanos: u64,
}
```

After this ack, the server emits a final EOS frame on `target_stream_id` if it hasn't already.

## 13. PONG (0x90)

```rust
struct PongResponse {
    client_timestamp_unix_nanos: u64,        // echoed from PING
    server_timestamp_unix_nanos: u64,
}
```

## 14. SERVER_PING (0x91)

Server-initiated keepalive.

```rust
struct ServerPingRequest {
    server_timestamp_unix_nanos: u64,
}
```

The client responds with `CLIENT_PONG`.

## 15. ADMIN_STATS_RESP (0xE0)

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

## 16. ADMIN_SNAPSHOT_RESP (0xE1)

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

## 17. ADMIN_RESTORE_RESP (0xE2)

```rust
struct AdminRestoreResponse {
    snapshot_name: String,
    shards_restored: Vec<ShardId>,
    completed_at_unix_nanos: u64,
    memories_restored: u64,
}
```

## 18. ADMIN_INTEGRITY_CHECK_RESP (0xE3)

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

## 19. ADMIN_MIGRATE_EMBEDDINGS_RESP (0xE4)

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

## 20. ADMIN_CREATE_CONTEXT_RESP (0xE5)

```rust
struct AdminCreateContextResponse {
    context_id: ContextId,
    name: String,
}
```

## 21. ADMIN_RENAME_CONTEXT_RESP (0xE6)

```rust
struct AdminRenameContextResponse {
    context_id: ContextId,
    new_name: String,
    old_name: String,
}
```

## 22. ADMIN_MOVE_MEMORY_RESP (0xE7)

```rust
struct AdminMoveMemoryResponse {
    memory_id: MemoryId,
    new_context_id: ContextId,
    old_context_id: ContextId,
}
```

## 23. ADMIN_RECLASSIFY_RESP (0xE8)

```rust
struct AdminReclassifyResponse {
    memory_id: MemoryId,
    new_kind: MemoryKind,
    old_kind: MemoryKind,
}
```

## 24. ADMIN_LIST_TOMBSTONED_RESP (0xE9)

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

## 25. ERROR (0xFF)

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

The full list of error codes is in [`10_errors.md`](10_errors.md).

---

*Continue to [`09_streaming.md`](09_streaming.md) for the streaming model.*
