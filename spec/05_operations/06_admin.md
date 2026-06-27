# 05.06 Admin Operations

Brain's administrative primitives — operational, not agent-facing.

## 1. The category

Admin operations are:

- **Privileged**: require admin auth (a higher level than regular agent auth).
- **Operational**: typically called by operators, dashboards, automation — not by typical agent code.
- **Substrate-state-altering or -reporting**: snapshots, stats, maintenance triggers.

They're separate from cognitive primitives because their failure modes, latency, and access patterns are different.

## 2. The catalog

| Operation | Purpose |
|---|---|
| ADMIN_STATS | Get per-shard or per-agent statistics |
| ADMIN_SNAPSHOT_CREATE | Create a point-in-time backup |
| ADMIN_SNAPSHOT_RESTORE | Restore from a snapshot |
| ADMIN_SNAPSHOT_LIST | List available snapshots |
| ADMIN_SNAPSHOT_DELETE | Remove a snapshot |
| ADMIN_REBUILD_ANN | Trigger immediate HNSW rebuild |
| ADMIN_RECOVER_METADATA | Rebuild metadata from WAL |
| ADMIN_CONTEXT_CREATE | Create a context with explicit settings |
| ADMIN_CONTEXT_DELETE | Delete a context (and its memories) |
| ADMIN_CONTEXT_RENAME | Rename a context |
| ADMIN_AGENT_CREATE | Create an agent |
| ADMIN_AGENT_DELETE | Delete all data for an agent |
| ADMIN_AGENT_QUOTA_SET | Set per-agent quotas |
| ADMIN_RESTORE_FORGOTTEN | Undo a soft-FORGET within grace |
| ADMIN_EXPLAIN_PLAN | Get the planner's plan for a request |
| ADMIN_HEALTH_CHECK | Detailed health report |
| ADMIN_FLUSH_CACHES | Drop in-memory caches (for testing) |
| ADMIN_PROMOTE_TO_CONSOLIDATED | Promote a Semantic memory to Consolidated |

## 3. ADMIN_STATS

```
ADMIN_STATS(scope, scope_id?) → StatsResponse
```

Where scope is one of:

- `Substrate`: global stats for the running substrate.
- `Shard`: per-shard stats (requires shard_id).
- `Agent`: per-agent stats.
- `Context`: per-context stats.

Returns:

```rust
struct StatsResponse {
    timestamp: u64,
    counts: BTreeMap<String, u64>,
    latencies: BTreeMap<String, LatencyHistogram>,
    storage: StorageStats,
    indexes: IndexStats,
}
```

Counts include: memory_count, edge_count, encodes_per_min, recalls_per_min, etc.

Latencies include: p50, p99 for each operation.

Storage: arena size, WAL size, metadata size, tombstone ratio.

Indexes: HNSW size, recall estimate, last rebuild.

## 4. ADMIN_SNAPSHOT_CREATE

```
ADMIN_SNAPSHOT_CREATE(shard_id, name?) → SnapshotResponse
```

Creates a coherent backup of the shard:

1. Briefly pauses writes.
2. Hardlinks (reflink if available) the arena, WAL segments, metadata.
3. Records the snapshot's metadata (LSN, timestamp).
4. Resumes writes.

The snapshot is hosted in a snapshots directory, with a stable name. It can be copied off-machine for backup.

Detailed in [08.06 Snapshots](../08_storage/06_snapshots.md).

Returns:

```rust
struct SnapshotResponse {
    snapshot_id: SnapshotId,
    name: String,
    lsn: u64,
    created_at: u64,
    size_bytes: u64,
    location: String,
}
```

## 5. ADMIN_SNAPSHOT_RESTORE

```
ADMIN_SNAPSHOT_RESTORE(shard_id, snapshot_id) → RestoreResponse
```

Restores a shard from a snapshot:

1. Stops the shard.
2. Replaces arena/WAL/metadata files with the snapshot's.
3. Restarts the shard.
4. Replays WAL records since the snapshot's LSN (if any are still available).

This is destructive — the shard's current state is lost. The snapshot becomes the new state.

For DR scenarios where the current shard is corrupted, this is the path back.

## 6. ADMIN_SNAPSHOT_LIST and DELETE

```
ADMIN_SNAPSHOT_LIST(shard_id) → Vec<SnapshotInfo>
ADMIN_SNAPSHOT_DELETE(snapshot_id) → ()
```

List shows all snapshots for a shard. Delete removes one (frees the underlying files via reference counting).

## 7. ADMIN_REBUILD_ANN

```
ADMIN_REBUILD_ANN(shard_id) → RebuildResponse
```

Triggers an immediate HNSW rebuild on the shard. The rebuild runs in the background:

```rust
struct RebuildResponse {
    rebuild_id: u64,
    started_at: u64,
    estimated_duration_sec: u32,
}
```

The client can poll status via `ADMIN_STATS`.

Use cases:
- After bulk deletions, when tombstone ratio is high.
- Before benchmarks, for fresh graph quality.
- For debugging ANN issues.

## 8. ADMIN_RECOVER_METADATA

```
ADMIN_RECOVER_METADATA(shard_id) → RecoverResponse
```

Rebuilds the metadata store from the WAL (if the metadata is corrupted but the WAL is intact). Heavy and rarely needed.

The shard is offline during recovery. For a 1M-memory shard: ~30 sec to a few minutes.

## 9. ADMIN_CONTEXT_*

```
ADMIN_CONTEXT_CREATE(agent_id, name, settings) → ContextResponse
ADMIN_CONTEXT_DELETE(context_id, force?) → DeleteResponse
ADMIN_CONTEXT_RENAME(context_id, new_name) → RenameResponse
```

CREATE: explicit context creation with settings (rather than the implicit creation at first ENCODE).

DELETE: removes a context and (optionally) all its memories. With `force: true`, all memories in the context are FORGOTTEN as part of the delete. Without force, the delete fails if the context has memories.

RENAME: change the context's name. The ContextId is unchanged.

## 10. ADMIN_AGENT_*

```
ADMIN_AGENT_CREATE(name, settings) → AgentResponse
ADMIN_AGENT_DELETE(agent_id, mode) → DeleteResponse
ADMIN_AGENT_QUOTA_SET(agent_id, quotas) → QuotaResponse
```

CREATE: creates a new agent. Returns the AgentId.

DELETE: removes the agent and all its memories, contexts, edges. Heavy. With `mode: HardDelete`, vectors and texts are zeroed immediately.

QUOTA_SET: configures per-agent limits (max memories, max contexts, max RPS).

## 11. ADMIN_RESTORE_FORGOTTEN

```
ADMIN_RESTORE_FORGOTTEN(memory_id) → RestoreResponse
```

Undoes a Soft FORGET within the grace period. The memory's tombstone flag is cleared; it becomes searchable again.

Fails if:
- The memory was hard-forgotten (data is gone).
- The grace period has expired (reclamation already happened).

This is admin-only because it can resurrect data the agent expected to be gone. Compliance-sensitive.

## 12. ADMIN_EXPLAIN_PLAN

```
ADMIN_EXPLAIN_PLAN(request) → PlanResponse
```

Runs the planner without executing. Returns the plan for inspection:

```rust
struct PlanResponse {
    plan_text: String,        // Human-readable plan
    estimated_cost_ms: f32,
    parameters: BTreeMap<String, String>,
}
```

Useful for debugging slow queries.

## 13. ADMIN_HEALTH_CHECK

```
ADMIN_HEALTH_CHECK() → HealthResponse
```

Returns:

```rust
struct HealthResponse {
    overall: HealthStatus,        // Healthy / Degraded / Unhealthy
    checks: Vec<HealthCheck>,
}

struct HealthCheck {
    name: String,
    status: HealthStatus,
    detail: Option<String>,
}
```

Checks include:
- Disk space.
- WAL fsync latency.
- HNSW recall estimate.
- Embedder availability.
- Per-shard liveness.

## 14. ADMIN_FLUSH_CACHES

```
ADMIN_FLUSH_CACHES(scope) → ()
```

Drops in-memory caches:

- Embedding cache.
- Other transient state.

Used for benchmarking (cold-cache measurements) and testing.

## 15. ADMIN_PROMOTE_TO_CONSOLIDATED

```
ADMIN_PROMOTE_TO_CONSOLIDATED(memory_id) → PromoteResponse
```

Manually promotes a Semantic memory to Consolidated. Sets `kind: Consolidated` and `consolidated_at`.

Used to mark a memory as a "summary of others" for organizational purposes. Doesn't auto-create DERIVED_FROM edges; the operator manages those.

## 16. Auth model

Every credential (API key) binds a `(namespace, agent, permissions)` scope plus a non-authoritative `user` audit tag (see [`../04_wire_protocol/04_handshake.md`](../04_wire_protocol/04_handshake.md) §10). Namespace is the tenant boundary; agent is the application within it.

- Agent credentials: scoped to one `(namespace, agent)`; can only act within that namespace and agent.
- Admin credentials: may act cross-**agent** *within their own namespace* (operational work). **No credential — admin or otherwise — can act across namespaces**; cross-namespace access is rejected at the boundary. Cross-tenant operations require separate provisioning, not a wider key.

**Namespace-scoped operations.** Key-mint provisions a tenant: minting a key for a namespace interns that namespace (creating the tenant). `SCHEMA_UPLOAD` / `SCHEMA_REPLACE` may target only the caller's own namespace; the reserved `brain` namespace is immutable (read-only system vocabulary). `GET_CAPABILITIES.schema_namespaces` returns only the caller's namespace (plus `brain`).

The wire protocol carries the scope + credential type in the connection's session state, derived from the key at AUTH (never client-supplied). Admin requests on a non-admin session return `Unauthorized`; a write that resolves to no provisioned namespace is rejected fail-closed (`NamespaceRequired` / `NamespaceUnknown`).

### 16.1 Key provisioning is the bootstrap

Identity enters Brain in exactly one place: the admin HTTP surface mints API keys, and minting a key is what creates identity. There is no anonymous access and no self-service identity on the data plane — a data-plane connection can only *prove* a key, never *create* one.

Minting a key:

- **Creates / interns its namespace.** The first key minted for a namespace name brings that tenant into existence; later keys for the same name join it. There is no implicit or default namespace.
- **Binds its agent** within that namespace, and **binds its permissions** (including whether the key is admin).
- Returns the opaque key material once, out-of-band, for the operator to hand to the application.

The admin HTTP surface is itself gated by an **operator admin secret**, presented as a request header (the bearer admin token). Without that secret, the provisioning routes reject the request — so the ability to create identity is held by the operator, not by any agent. Keys are revoked the same way; a revoked key fails AUTH (`Unauthenticated`).

The data plane never creates identity: no opcode mints, names, or elevates a key, a namespace, or an agent. The full provisioning flow and the header-based admin-secret gate are specified in [`../17_observability/04_admin_ops.md`](../17_observability/04_admin_ops.md).

## 17. Audit logging

All admin operations are logged with:

- Admin identifier (who).
- Operation and parameters (what).
- Timestamp (when).
- Result (success/failure).

This is in addition to the regular WAL records. The audit log is separate; queryable for compliance.

## 18. Latency

Admin operations vary widely:

- ADMIN_STATS: < 10 ms.
- ADMIN_SNAPSHOT_CREATE: 1-10 sec (depending on shard size).
- ADMIN_SNAPSHOT_RESTORE: 1-30 sec (depending on shard size).
- ADMIN_REBUILD_ANN: minutes (background; the call returns immediately).
- ADMIN_AGENT_DELETE: minutes for large agents (background).

For long-running admin operations, Brain returns a job ID; the client polls for completion.

## 19. The "no admin in production" anti-pattern

Some admin operations (e.g., FLUSH_CACHES, REBUILD_ANN) are useful in development but risky in production. Brain's deployment configuration can disable them:

```
[admin]
allow_flush_caches = false
allow_immediate_rebuild = false
```

Disabled operations return `OperationDisabled`.

## 20. The future-extension question

The admin set will grow over time as new operational needs emerge. The naming convention (`ADMIN_*` prefix) makes them clearly distinct from cognitive primitives.

Brain avoids putting agent-facing semantics in admin (e.g., Brain does not have `ADMIN_RECALL_FROM_OTHER_AGENT`). Admin is about substrate operation, not about cognition.

---

*Continue to [`07_consistency.md`](07_consistency.md) for the consistency model.*
