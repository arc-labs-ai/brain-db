# 28.14 Admin Wire Frames

Request/response body schemas for `0x0170–0x0177` — knowledge-layer admin operations. These are operator-facing: index rebuilds, ambiguity resolution, audit inspection, backfill, job status.

All opcodes in this range require **admin** permissions (per [`./05_schema_frames.md`](./05_schema_frames.md) §8). Authorization failures return `AdminPermissionRequired` (substrate `Authorization`).

Cross-references:
- [`../22_extractors/00_purpose.md`](../22_extractors/00_purpose.md) — extractor backfill semantics.
- [`../27_knowledge_workers/00_purpose.md`](../27_knowledge_workers/00_purpose.md) — background workers that run jobs.
- [`../18_entities/00_purpose.md`](../18_entities/00_purpose.md) §"Resolution ambiguity" — audit-driven ambiguity resolution.

## 1. Opcode index

| Opcode | Name | Section | Status |
|---|---|---|---|
| `0x0170` | `ADMIN_REBUILD_INDEX` | §2 | spec-only — phase 22+ |
| `0x0171` | `ADMIN_REINDEX_TANTIVY` | §3 | spec-only — phase 22 |
| `0x0172` | `ADMIN_LIST_PENDING_RESOLUTIONS` | §4 | spec-only — phase 16.7+ |
| `0x0173` | `ADMIN_RESOLVE_AMBIGUITY` | §5 | spec-only — phase 16.7+ |
| `0x0174` | `ADMIN_GET_AUDIT` | §6 | spec-only — phase 16.7+ |
| `0x0175` | `ADMIN_LIST_STALE_STATEMENTS` | §7 | spec-only — phase 17 |
| `0x0176` | `ADMIN_BACKFILL` | §8 | spec-only — phase 22 |
| `0x0177` | `ADMIN_JOB_STATUS` | §9 | spec-only — phase 22 |

Responses live at `0x01F0–0x01F7`.

## 2. ADMIN_REBUILD_INDEX (0x0170)

Asynchronously rebuilds one of the per-shard knowledge indexes (entity HNSW, statement HNSW, entity trigrams, etc.). Returns a `job_id` immediately; client polls via `ADMIN_JOB_STATUS`.

### 2.1 Request — `AdminRebuildIndexRequest`

```rust
pub struct AdminRebuildIndexRequest {
    pub index_name: String,            // "entity_hnsw", "statement_hnsw", "entity_trigrams", ...
    pub shard_id: u16,                 // 0..=N-1
    pub request_id: WireUuid,
}
```

### 2.2 Response — `AdminRebuildIndexResponse`

```rust
pub struct AdminRebuildIndexResponse {
    pub job_id: WireUuid,              // poll via ADMIN_JOB_STATUS
    pub started_at_unix_nanos: u64,
    pub estimated_wall_time_ms: u32,
}
```

### 2.3 Errors

- `INVALID_ARGUMENT` — unknown `index_name`, shard out of range.

## 3. ADMIN_REINDEX_TANTIVY (0x0171)

Rebuilds the tantivy BM25 text index for a shard (memories + statements). Same async pattern as §2.

### 3.1 Request — `AdminReindexTantivyRequest`

```rust
pub struct AdminReindexTantivyRequest {
    pub shard_id: u16,
    pub include_memory_text: bool,
    pub include_statement_text: bool,
    pub request_id: WireUuid,
}
```

### 3.2 Response

```rust
pub struct AdminReindexTantivyResponse {
    pub job_id: WireUuid,
    pub started_at_unix_nanos: u64,
    pub estimated_wall_time_ms: u32,
}
```

## 4. ADMIN_LIST_PENDING_RESOLUTIONS (0x0172)

Streams entity-resolution audit rows where `outcome = Pending` — i.e. ambiguous extractions that need operator decision.

### 4.1 Request — `AdminListPendingResolutionsRequest`

```rust
pub struct AdminListPendingResolutionsRequest {
    pub limit: u32,                    // 1..=1000
    pub cursor: Vec<u8>,
    pub older_than_unix_nanos: u64,    // 0 = no filter
}
```

### 4.2 Response — streaming `PendingResolutionItem`

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

## 5. ADMIN_RESOLVE_AMBIGUITY (0x0173)

Operator decides one pending resolution. Either binds the audit's pending subject to an existing entity, or creates a new one.

### 5.1 Request — `AdminResolveAmbiguityRequest`

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

### 5.2 Response — `AdminResolveAmbiguityResponse`

```rust
pub struct AdminResolveAmbiguityResponse {
    pub resolved_at_unix_nanos: u64,
    pub bound_entity_id: WireUuid,     // the entity statements now point to
    pub statements_rerouted: u32,      // how many pending-subject statements were re-routed
}
```

### 5.3 Errors

- `INVALID_ARGUMENT` — bad `action`, missing required field for the action, unknown `chosen_entity_id`.
- `ENTITY_AMBIGUOUS` if a race made the audit already-resolved.

## 6. ADMIN_GET_AUDIT (0x0174)

Read a single audit row (resolution audit, merge audit, schema audit) by id.

### 6.1 Request — `AdminGetAuditRequest`

```rust
pub struct AdminGetAuditRequest {
    pub audit_id: WireUuid,
}
```

### 6.2 Response — `AdminGetAuditResponse`

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
    pub backward_compatible: bool,
    pub migration_summary: SchemaMigrationSummary,
}

pub struct ExtractorAuditView {
    pub extractor_id: u32,
    pub event: u8,                     // 1=enabled, 2=disabled, 3=registered, 4=deregistered
    pub reason: String,
}
```

### 6.3 Errors

- `INVALID_ARGUMENT` — `audit_id` not found.

## 7. ADMIN_LIST_STALE_STATEMENTS (0x0175)

Streams statements whose source memory has been forgotten / tombstoned but the statement itself is still active. Operator decides whether to tombstone or retract.

### 7.1 Request — `AdminListStaleStatementsRequest`

```rust
pub struct AdminListStaleStatementsRequest {
    pub older_than_unix_nanos: u64,    // 0 = no filter
    pub kind_filter: Vec<StatementKindWire>, // empty = all
    pub limit: u32,                    // 1..=1000
    pub cursor: Vec<u8>,
}
```

### 7.2 Response — streaming `StatementView`

Same shape as `STATEMENT_LIST` ([§06](./06_statement_frames.md) §9). Filter is "statement.evidence references a tombstoned / forgotten memory".

## 8. ADMIN_BACKFILL (0x0176)

Re-runs one or more extractors over a memory range. Used after schema migration or extractor improvements.

### 8.1 Request — `AdminBackfillRequest`

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

### 8.2 Response — `AdminBackfillResponse`

```rust
pub struct AdminBackfillResponse {
    pub job_id: WireUuid,
    pub memories_in_range: u64,
    pub extractors_dispatched: u32,
    pub estimated_wall_time_ms: u64,
}
```

### 8.3 Errors

- `INVALID_ARGUMENT` — unknown `extractor_ids`, `memory_range_start > end`, `max_parallelism > 16`.
- `EXTRACTOR_BUDGET_EXCEEDED` (substrate `ResourceExhausted`) — backfill would exceed configured per-extractor budgets.

## 9. ADMIN_JOB_STATUS (0x0177)

Polls the status of an async job started by §2 / §3 / §8.

### 9.1 Request — `AdminJobStatusRequest`

```rust
pub struct AdminJobStatusRequest {
    pub job_id: WireUuid,
}
```

### 9.2 Response — `AdminJobStatusResponse`

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

### 9.3 Errors

- `INVALID_ARGUMENT` — `job_id` not found.

## 10. Job retention

Job records are kept in a `jobs` redb table for 7 days after `completed_at`. After that they're garbage-collected. Polling an expired job returns `INVALID_ARGUMENT`.

## 11. Cancellation

There is no `ADMIN_CANCEL_JOB` opcode in v1.0. Jobs that need cancellation use:

- For index rebuilds (§2, §3): the underlying worker is interrupt-tolerant. A subsequent `ADMIN_REBUILD_INDEX` for the same index supersedes the prior one (the worker re-checks every chunk).
- For backfill (§8): re-issue with a narrower range, then let the wider one complete or expire.

Tracked in [`./09_open_questions.md`](./09_open_questions.md) for v1.x.

## 12. Concurrent admin jobs

Per-shard, only **one** index rebuild may run at a time. Concurrent `ADMIN_REBUILD_INDEX` requests for the same index queue behind the first; the response returns the queued `job_id` immediately. The job order is reflected in `ADMIN_JOB_STATUS.state` (`pending` until prior jobs complete).

Per-shard, multiple backfills may run concurrently up to `max_parallelism`. Across shards, jobs are independent.
