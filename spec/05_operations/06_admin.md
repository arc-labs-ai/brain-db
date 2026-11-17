# 05.06 Admin Operations

Brain's administrative primitives — operational, not agent-facing.

## 1. The category

Admin operations are:

- **Privileged**: require admin auth (a higher level than regular agent auth).
- **Operational**: typically called by operators, dashboards, automation — not by typical agent code.
- **Substrate-state-altering or -reporting**: snapshots, stats, maintenance triggers.

They're separate from cognitive primitives because their failure modes, latency, and access patterns are different.

**They are also on a different plane.** Admin is an HTTP listener on its own
port (`[server] admin_addr`), gated by a bearer admin token
(`BRAIN__ADMIN__TOKEN`) — not a family of wire opcodes. A dozen `ADMIN_*`
opcode numbers remain allocated in the wire table for namespace stability, and
the dispatcher rejects every one of them permanently with *"admin operations
are served over the HTTP admin listener, not the wire protocol"*: a refusal
rather than a `NotYetImplemented`, because the plane is wrong, not the version.
The exception is `ADMIN_LIST_PENDING_CONTRADICTIONS`, which is a genuine
per-shard wire op and is dispatched.

This section names operations the way the old wire-opcode design did. Where a
heading still reads `ADMIN_SOMETHING`, the route beneath it is what exists.

## 2. The catalog

The admin plane, as it is routed today. `?shard=N` selects a shard where the
operation is per-shard; most listings fan out across shards.

| Route | Purpose |
|---|---|
| `GET /healthz` | Liveness — is the admin accept loop running |
| `GET /readyz` | Readiness — at least one shard, and every shard's executor still draining |
| `GET /metrics` | Prometheus exposition for the whole process |
| `GET /v1/diagnostics/debug-snapshot?shard=N` | Point-in-time per-shard runtime state |
| `POST /v1/diagnostics/profile` | On-demand profile capture |
| `POST /v1/snapshots` | Take a snapshot |
| `GET /v1/snapshots` | List snapshots across shards |
| `POST /v1/snapshots/{id}/restore` | Restore a shard from a snapshot (destructive) |
| `DELETE /v1/snapshots/{id}` | Delete a snapshot |
| `POST /v1/rebuild-ann` | Rebuild the memory HNSW |
| `POST /v1/rebuild?index=<target>` | Rebuild a named derived index |
| `POST /v1/extract/backfill` | Re-enqueue memories through the extractor tiers |
| `POST /v1/backfill` / `GET /v1/backfill` / `DELETE /v1/backfill/{id}` | Submit, poll and cancel a backfill run |
| `GET /v1/workers` | Worker inventory and state |
| `POST /v1/workers/{name}/{stop\|start\|run-now}` | Pause, resume or trigger one worker |
| `GET /v1/config` | Whole config or a subtree (`?key=a.b.c`) |
| `POST /v1/config/reload` | Re-read the file; apply the live-safe subset, report the rest as `requires_restart` |
| `POST /v1/config?key=…&value=…` | Set the one runtime-settable key |
| `GET /v1/audit` / `GET /v1/audit/export` | Extraction and resolution audit rows, paginated or bulk |
| `GET /v1/schema/review` | Predicate review queue — names the corpus coined that no schema declared |
| `POST /v1/api-keys` / `GET /v1/api-keys` / `DELETE /v1/api-keys/{id}` | Mint, list and revoke scope-bound API keys |
| `GET /v1/spaces` | List spaces |
| `DELETE /v1/spaces/{id}` | Cascade-delete a space and everything under it |
| `POST /v1/memories/{id}/restore?namespace=…` | Undo a soft FORGET within grace |
| `GET /v1/shards` / `POST /v1/shards` / `DELETE /v1/shards/{id}` | List, create and delete shards |

Specified in this section and **not built**:

| Named here | Status |
|---|---|
| `ADMIN_RECOVER_METADATA` | No route, no opcode. Metadata is rebuilt only by WAL replay at boot |
| `ADMIN_CONTEXT_RENAME` | Nothing renames a session or a space, on either plane |
| `ADMIN_AGENT_QUOTA_SET` | No quota surface on the admin plane |
| `ADMIN_FLUSH_CACHES` | No route; caches are dropped by restarting |
| `ADMIN_PROMOTE_TO_CONSOLIDATED` | No route; consolidation is the worker's business only |
| `ADMIN_HEALTH_CHECK`'s detailed report | `/healthz` and `/readyz` answer yes/no; the per-check report below does not exist |
| `ADMIN_STATS`'s per-space scope | `GET /v1/spaces/{id}` answers **501** — it needs a `space_id` secondary index |
| `ADMIN_MOVE_MEMORY`, `ADMIN_RECLASSIFY`, `ADMIN_LIST_TOMBSTONED` | Opcode numbers allocated and rejected; nothing implements them. The closest live paths are `POST /v1/extract/backfill` for re-extraction, and `MEMORY_LIST`, whose `include_tombstoned` flag is honoured at the row level while the timeline index holds no tombstoned rows to surface |
| `ADMIN_TOKENIZE`, `ADMIN_REGISTER_MODEL`, `ADMIN_ABORT_MIGRATION`, `ADMIN_RETIRE_FINGERPRINT` | Opcode *numbers* only: no request type, no decode, no dispatch arm. See [07.06 Migration](../07_embedding/06_migration.md) |

## 3. Statistics — `GET /metrics` and the debug snapshot

There is no single stats call and no `StatsResponse`. Two surfaces, split by
what they are good at:

```
GET /metrics                                  → Prometheus exposition
GET /v1/diagnostics/debug-snapshot?shard=N    → per-shard runtime state
```

`/metrics` carries the counts and latency histograms — per-operation
request families, storage and WAL gauges, HNSW gauges, worker counters, the
derivation backlog and admission families. It is the only surface with
history, because a scrape interval is what makes a rate. The catalog of
families is [17.01 Signals](../17_observability/01_signals.md); families that
are declared but not yet emitted are marked there and in
[17.03 Alerts](../17_observability/03_alerts.md).

The debug snapshot is a point-in-time structure for one shard, and v1
populates four fields: `workers` (the scheduler snapshot), `pending_requests`
(the shard's dispatch-queue depth), `derivation_queue` (the extraction
backlog), and `in_memory_state_summary` (HNSW plus storage stats).
`pending_requests` and `derivation_queue` are deliberately separate: the first
is requests waiting milliseconds for the executor, the second is memories
waiting minutes to hours to become graph, and summing them hides both.
`active_tasks` and `recent_errors` are specified and not populated.

**Scopes.** Per-shard works, through `?shard=N`. Per-space does not:
`GET /v1/spaces/{id}` returns 501 pending a `space_id` secondary index. There
is no per-session stats surface.

## 4. Snapshot create — `POST /v1/snapshots`

```
POST /v1/snapshots[?shard=N]
```

Creates a coherent backup of the shard:

1. Briefly pauses writes.
2. Hardlinks (reflink if available) the arena, WAL segments, metadata.
3. Records the snapshot's metadata (LSN, timestamp).
4. Resumes writes.

The snapshot is hosted in a snapshots directory, with a stable name. It can be copied off-machine for backup.

Detailed in [08.06 Snapshots](../08_storage/06_snapshots.md).

## 5. Snapshot restore — `POST /v1/snapshots/{id}/restore`

```
POST /v1/snapshots/<id>/restore[?shard=N]
```

Restores a shard from a snapshot:

1. Stops the shard.
2. Replaces arena/WAL/metadata files with the snapshot's.
3. Restarts the shard.
4. Replays WAL records since the snapshot's LSN (if any are still available).

This is destructive — the shard's current state is lost. The snapshot becomes the new state.

For DR scenarios where the current shard is corrupted, this is the path back.

## 6. Snapshot list and delete

```
GET    /v1/snapshots                  → every shard's snapshots
DELETE /v1/snapshots/<id>[?shard=N]
```

The listing fans out across shards. Delete removes one (frees the underlying
files via reference counting). All four snapshot routes share one
prefix-registered entry point that dispatches on `(method, path)`.

## 7. Index rebuild — `POST /v1/rebuild-ann` and `POST /v1/rebuild`

```
POST /v1/rebuild-ann[?shard=N]            → 201 {"entries":N,"elapsed_ms":N,"shard":N}
POST /v1/rebuild?index=<target>[&shard=N] → 201 {"index":"…","entries":N,"elapsed_ms":N,"shard":N}
```

`/v1/rebuild-ann` is the memory-HNSW alias, kept for back-compat;
`/v1/rebuild` takes the index to rebuild as `?index=`.

**The call is synchronous**, and the response reports what it did — the entry
count and the elapsed milliseconds. There is no rebuild id and nothing to
poll: a caller that returns from this route is a caller whose rebuild has
finished.

Use cases:
- After bulk deletions, when tombstone ratio is high.
- Before benchmarks, for fresh graph quality.
- For debugging ANN issues.

## 8. Metadata recovery — not an operation

**Not built, on either plane.** There is no route and no opcode that rebuilds
the metadata store on demand.

What exists is the boot path: a shard replays its WAL into the metadata store
every time it spawns, and refuses to spawn when the log holds damage that
would lose an acked write. The deliberate repair path for that case is
`wal::repair`, a library call with a plan-and-token shape, not an admin
operation. See [18.02 Crash recovery](../18_failure_recovery/02_crash_recovery.md).

A deployment whose metadata is corrupt while its WAL is intact therefore
restores from a snapshot (§5) or restarts the shard on an intact log; it does
not call anything.

## 9. Spaces and sessions — data-plane ops, plus one admin route

"Context" was the old name for what is now a **space**, and the tier below it
is a **session** ([20. Tenancy](../20_tenancy/00_purpose.md)). Neither is
administered by an `ADMIN_*` operation: both are first-class wire ops, because
an agent creates and lists its own sessions.

```
SPACE_CREATE / SPACE_LIST / SPACE_DELETE          (wire)
SESSION_CREATE / SESSION_LIST / SESSION_DELETE    (wire)

GET    /v1/spaces            → list spaces
GET    /v1/spaces/{id}       → 501, pending a space_id secondary index
DELETE /v1/spaces/{id}       → cascade delete
```

`DELETE /v1/spaces/{id}` hard-tombstones every memory under
`(namespace, space)`, tears down the backing entity / statement / relation
rows through the FORGET cascade, and drops the space registry row. It reuses
the `SPACE_DELETE` wire path rather than reimplementing it, so the two cannot
drift. There is **no `force` flag**: the cascade is what the operation is.

**Nothing renames a space or a session**, on either plane.

## 10. Identity — the API-key routes

There is no "agent" object to create. Identity enters Brain as a **key**:

```
POST   /v1/api-keys          → mint a key (space_id, namespace, permissions,
                               optional user_id / org_id audit tags)
GET    /v1/api-keys          → list
DELETE /v1/api-keys/{id}     → revoke
```

Minting interns the namespace and binds the space and permissions to the key,
which is why this route is the bootstrap for everything else (§16). Deleting
an identity's data is the space cascade in §9, not a separate operation.

**Quotas are not here.** No admin route sets a per-space memory, session or
rate limit. Brain enforces no quota of its own; the `quota:` response header
is populated by the deployment in front of it.

## 11. Undo a soft FORGET — `POST /v1/memories/{id}/restore`

```
POST /v1/memories/<memory_id>/restore?namespace=<name>
```

The id is the 128-bit `MemoryId` as a `u128` in the path. The owning namespace
is named explicitly in the query string, because the admin plane has no API
key to resolve the `(namespace, space)` pair from.

Undoes a Soft FORGET within the grace period. It un-tombstones the memory (making it searchable again) and replays the forget undo log to reverse the soft cascade: for each dependent statement / relation the cascade touched it re-attaches the stripped evidence, re-adds the reverse-index row, recomputes confidence, and un-tombstones rows still carrying reason `SourceMemoryForgotten`. Replay is idempotent — each consumed undo row is deleted in-txn, so a re-run is a structural no-op. See [`../10_metadata/00_purpose.md`](../10_metadata/00_purpose.md) (the additive undo log) for the mechanism.

Fails if:
- The memory was hard-forgotten (no undo log was written — the data is gone).
- The grace period has expired (slot reclamation already reaped the undo rows; a post-grace forget is irreversible).

This is admin-only because it can resurrect data the agent expected to be gone. Compliance-sensitive.

## 12. Plan inspection — `QUERY_EXPLAIN` and `QUERY_TRACE`

Two **data-plane** opcodes, not an admin operation: `QUERY_EXPLAIN` runs the
planner without executing and returns the plan; `QUERY_TRACE` returns what an
executed query actually did. Both are rendered by `brain_planner::explain`.

They are on the data plane because the caller debugging a slow query is the
caller that issued it, and the plan is scoped to their own request. Useful for
debugging slow queries — and for seeing which of the two the engine is doing,
since a plan and a trace disagree exactly where the estimate was wrong.

## 13. Health — `GET /healthz` and `GET /readyz`

Two probes, and the split is the point:

```
GET /healthz   → is the admin accept loop running  (liveness)
GET /readyz    → can this node serve data-plane traffic now  (readiness)
```

A node is ready **iff** it has at least one shard and every shard's executor
loop is still draining requests. Both answer with a status, not a report.

**The per-check report is not built.** Disk space, WAL fsync latency, HNSW
recall estimate and embedder availability are not rolled up into a
`HealthResponse` with a `Degraded` middle state; a reader wanting those looks
at the corresponding metric families, where the HNSW recall estimate is
itself one of the deferred ones. Per-shard liveness is the only one of the
five that reaches a probe, through `/readyz`.

## 14. Cache flush — not an operation

**Not built.** No route drops the embedding cache or any other transient
state, so a cold-cache measurement means restarting the process (or, for the
durable LLM response cache, waiting out its TTL — see
[15.06](../15_background_workers/06_typed_graph_workers.md)). Benchmarks in
the tree construct their own caches rather than asking a running server to
forget.

## 15. Promotion to Consolidated — not an operation

**Not built.** Nothing on either plane sets `kind: Consolidated` by hand; the
consolidation worker is the only writer of that kind. An operator who wants a
memory to stand as a summary of others writes it as an ordinary memory and
links it.

## 16. Auth model

Every credential (API key) binds a `(namespace, agent, permissions)` scope plus a non-authoritative `user` audit tag (see [`../04_wire_protocol/04_handshake.md`](../04_wire_protocol/04_handshake.md) §10). Namespace is the tenant boundary; agent is the application within it.

- Agent credentials: scoped to one `(namespace, space)`; can only act within that namespace and agent.
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

Admin routes vary widely:

- `GET /metrics`, `GET /readyz`: milliseconds.
- `POST /v1/snapshots`: 1-10 sec, with shard size.
- `POST /v1/snapshots/{id}/restore`: 1-30 sec, with shard size.
- `POST /v1/rebuild-ann`: up to minutes on a large shard — and **the call
  blocks for all of it**, returning the entry count and elapsed time it
  measured. There is no rebuild id to poll.
- `DELETE /v1/spaces/{id}`: minutes for a large space; the cascade runs
  before the response.

**One route is asynchronous, and it is the only one.** `POST /v1/backfill`
returns a run id, `GET /v1/backfill` reports progress, and
`DELETE /v1/backfill/{id}` flags the run for cancellation. Every other admin
route completes its work before it answers, so an operator budgets for the
request duration rather than for a polling loop.

## 19. The "no admin in production" anti-pattern

Some admin routes are useful in development and risky in production — a
rebuild that blocks for minutes, a space delete that cascades, a snapshot
restore that discards the shard's current state.

**There is no per-operation switch.** `[admin]` carries the operator token
and nothing that disables individual routes, so there is no
`OperationDisabled` to return. What the engine does instead is fail closed on
the whole plane: with no token configured (or a whitespace-only one) the admin
listener **refuses to start**, because that listener mints data-plane API keys
and a blank secret is no secret at all.

The split that does exist is by listener. `/healthz`, `/readyz` and
`/metrics` are built into a public router that is safe to expose; everything
under `/v1` lives on the token-gated admin listener, and a `/v1` path on the
public listener 404s. Restricting admin in production is therefore a matter of
who can reach that port, not of which routes are compiled in.

## 20. The future-extension question

The admin set will grow as new operational needs emerge. The distinction from
cognitive primitives is now carried by the **plane** rather than by a name
prefix: admin is HTTP on the operator's port, cognition is the wire protocol
on the data port. A new operational need is a new route under `/v1`, not a new
opcode — the `ADMIN_*` numbers that remain in the wire table are kept for
namespace stability and permanently rejected (§1).

Brain avoids putting agent-facing semantics in admin: there is deliberately no
route that recalls from another space, because reading across the space wall is
exactly what the wall is for. Admin is about substrate operation, not about
cognition.

---

*Continue to [`07_consistency.md`](07_consistency.md) for the consistency model.*
