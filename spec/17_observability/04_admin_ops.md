# 17.04 Admin Operations

The operational commands available to operators.

## 1. The admin API

Admin operations are exposed as HTTP routes under `/v1/*` on a dedicated **loopback admin listener** (default `127.0.0.1:9092`), kept off the public data-plane port. Any HTTP client works; the examples below use `curl`. Brain ships no CLI — operators administer the database directly over this API.

Authentication is required: admin keys are distinct from agent keys (see §12). Bind the listener to loopback or front it with mTLS; the surface is operationally sensitive.

```bash
curl -s http://127.0.0.1:9092/v1/shards
curl -s -X POST http://127.0.0.1:9092/v1/rebuild-ann -d '{"shard":"<shard-id>"}'
curl -s -X POST http://127.0.0.1:9092/v1/snapshots -d '{"name":"my-snapshot"}'
```

Health and metrics live on a separate **metrics listener** (default port `9091`): `GET /healthz` for liveness and `GET /metrics` for the Prometheus scrape. Database-wide counters (shard count, WAL size, HNSW node counts, request rates) are exposed by `GET /metrics`; a dedicated JSON stats-summary route is not yet implemented.

## 2. The categories

- **Status**: shards, agents, health, info, metrics scrape.
- **Maintenance**: rebuild-ann, gc, vacuum.
- **Snapshots**: create, list, restore.
- **Workers**: list, stop, start, run-now.
- **Configuration**: reload, get, set.
- **Audit**: query, export.
- **Diagnostics**: profile, debug-snapshot.

## 3. Status operations

### Stats

Database-wide counters are exposed via `GET /metrics` on the metrics listener (Prometheus text exposition); a dedicated JSON stats-summary route is not yet implemented. Scrape and filter for the counters you need:

```bash
curl -s http://127.0.0.1:9091/metrics | grep '^brain_'
```

The exposition includes `brain_shards_total`, `brain_wal_size_bytes`, `brain_hnsw_node_count`, `brain_hnsw_tombstone_count` / `brain_hnsw_tombstone_ratio`, and the `brain_request_*` request counters and histograms, among others. For per-shard or per-agent detail, use the real `GET /v1/shards` and `GET /v1/agents` routes (see §11 and §10).

### Health

```bash
curl -s http://127.0.0.1:9091/healthz
```

Returns overall health, including per-shard state:

```json
{
  "status": "healthy",
  "shards": {
    "<uuid_1>": "healthy",
    "<uuid_2>": "healthy"
  },
  "version": "1.0.0"
}
```

### Info

Version, build info, and a configuration summary are returned by `GET /v1/config` (see §7), alongside the running configuration.

## 4. Maintenance operations

### Rebuild ANN

```bash
curl -s -X POST http://127.0.0.1:9092/v1/rebuild-ann -d '{"shard":"<shard-id>"}'
```

Triggers an immediate HNSW rebuild on the shard. Async; the call returns a job handle immediately. Track progress via the `brain_hnsw_rebuild_progress_pct` metric on the metrics listener.

### GC

Triggers immediate garbage collection — pruning expired idempotency entries, deleting eligible WAL segments, or reclaiming eligible slots. Administer via the admin HTTP API (`/v1/*` on the admin listener). (Operator action: force an immediate `idempotency` / `wal` / `slots` collection cycle rather than waiting for the scheduled worker. Route name TBD.)

### Vacuum

Compacts the metadata store (redb); may take minutes for large stores. Administer via the admin HTTP API (`/v1/*` on the admin listener). (Operator action: compact the redb metadata file in place. Route name TBD.)

## 5. Snapshot operations

### Create a snapshot

```bash
curl -s -X POST http://127.0.0.1:9092/v1/snapshots -d '{"name":"my-snapshot"}'
# Per-shard: add "shard":"<uuid>" to the body.
```

Creates a consistent snapshot. Brain:

1. Triggers a checkpoint.
2. Copies (or reflinks, if supported) the storage files.
3. Records the snapshot's metadata.

### List snapshots

```bash
curl -s http://127.0.0.1:9092/v1/snapshots
```

Lists snapshots:

```json
[
  {"name": "pre-upgrade-2026-05-07", "shard": "<uuid>", "size_gb": 12.3, "created_at": "..."},
  {"name": "scheduled-daily-001", "shard": "<uuid>", "size_gb": 12.3, "created_at": "..."}
]
```

### Restore a snapshot

```bash
curl -s -X POST http://127.0.0.1:9092/v1/snapshots/pre-upgrade-2026-05-07/restore
```

Restores from a snapshot. **Destructive** — current data is lost. The restore is gated server-side; send the confirmation field the server requires (e.g. `-d '{"confirm":true}'`).

### Delete a snapshot

```bash
curl -s -X DELETE http://127.0.0.1:9092/v1/snapshots/my-snapshot
```

Removes a snapshot.

## 6. Worker operations

### List workers

```bash
curl -s http://127.0.0.1:9092/v1/workers
# Per-shard: curl -s 'http://127.0.0.1:9092/v1/workers?shard=<uuid>'
```

Returns worker status:

```json
[
  {"shard": "...", "name": "decay", "status": "running", "last_run": "...", "next_run": "..."},
  {"shard": "...", "name": "consolidation", "status": "running", "last_run": "...", "next_run": "..."}
]
```

### Stop / start a worker

```bash
curl -s -X POST http://127.0.0.1:9092/v1/workers/decay/stop  -d '{"shard":"<uuid>"}'
curl -s -X POST http://127.0.0.1:9092/v1/workers/decay/start -d '{"shard":"<uuid>"}'
```

Pauses or resumes a specific worker. The path is `/v1/workers/{name}/{action}`.

### Run a worker now

```bash
curl -s -X POST http://127.0.0.1:9092/v1/workers/decay/run-now -d '{"shard":"<uuid>"}'
```

Triggers an immediate cycle.

## 7. Configuration operations

### Reload configuration

```bash
curl -s -X POST http://127.0.0.1:9092/v1/config/reload
```

Reloads configuration from disk. Some settings reload live; others require restart.

### Get configuration

```bash
curl -s http://127.0.0.1:9092/v1/config
# Single key: curl -s 'http://127.0.0.1:9092/v1/config?key=workers.decay.interval'
```

Returns the current configuration. With a `key` parameter, returns just that key.

### Set configuration

```bash
curl -s -X POST http://127.0.0.1:9092/v1/config/set \
  -d '{"key":"workers.decay.interval","value":"30m"}'
```

Updates a setting. Live reload if supported; otherwise persisted for next restart.

Not all settings are runtime-tunable. Brain logs which take effect immediately vs which need restart.

## 8. Audit operations

### Query audit log

```bash
curl -s -G http://127.0.0.1:9092/v1/audit \
  --data-urlencode 'since=2026-05-01' \
  --data-urlencode 'until=2026-05-07' \
  --data-urlencode 'agent=agent-001'
```

Returns audit log entries matching the filter.

### Export audit log

```bash
curl -s -X POST http://127.0.0.1:9092/v1/audit/export \
  -d '{"output":"/backup/audit-2026-05.jsonl"}'
```

Exports audit logs to a file. For long-term archival.

## 9. Diagnostic operations

### Profile

```bash
curl -s -X POST http://127.0.0.1:9092/v1/diagnostics/profile \
  -d '{"shard":"<uuid>","duration":"30s","output":"/tmp/profile.pb"}'
```

Captures a CPU profile of the shard's executor. Output is pprof-compatible.

### Debug snapshot

```bash
curl -s -X POST http://127.0.0.1:9092/v1/diagnostics/debug-snapshot \
  -d '{"shard":"<uuid>","output":"/tmp/debug.json"}'
```

Captures a detailed runtime snapshot:

- Active tasks.
- Pending requests.
- In-memory state summary.
- Recent errors.
- Worker statuses.

For deep debugging.

## 10. Agent operations

### List agents

```bash
curl -s http://127.0.0.1:9092/v1/agents
# Per-shard: curl -s 'http://127.0.0.1:9092/v1/agents?shard=<uuid>'
```

Lists agents on the shard.

### Agent stats

```bash
curl -s http://127.0.0.1:9092/v1/agents/<agent-id>
```

Per-agent stats:

```json
{
  "agent_id": "...",
  "shard": "...",
  "memory_count": 12345,
  "context_count": 5,
  "oldest_memory_age": "30d"
}
```

### Delete an agent

```bash
curl -s -X DELETE http://127.0.0.1:9092/v1/agents/<agent-id> -d '{"confirm":true}'
```

Deletes all of an agent's data. Destructive; the server requires the confirmation field.

## 11. Shard operations

### List shards

```bash
curl -s http://127.0.0.1:9092/v1/shards
```

Lists shards.

### Create a shard (rare)

```bash
curl -s -X POST http://127.0.0.1:9092/v1/shards -d '{"logical_id":16}'
```

Creates a new shard. Used during expansion.

### Delete a shard (rare)

```bash
curl -s -X DELETE http://127.0.0.1:9092/v1/shards/<shard-id> -d '{"confirm":true}'
```

Deletes a shard. All its data is gone. Used during decommission.

## 12. Authentication

Admin operations require an admin token, passed as a bearer header:

```bash
export BRAIN_ADMIN_TOKEN="..."
curl -s -H "Authorization: Bearer $BRAIN_ADMIN_TOKEN" http://127.0.0.1:9092/v1/shards
```

Tokens are configured at deployment. Multiple tokens supported (per-operator). Issue and revoke them via the API-key routes:

```bash
# Issue a scoped key
curl -s -X POST http://127.0.0.1:9092/v1/api-keys -d '{"permissions":["encode","recall"]}'
# Revoke a key
curl -s -X DELETE http://127.0.0.1:9092/v1/api-keys/<key-id>
```

### 12.1 Scope-bound API keys

For agent-facing (non-admin) traffic, API keys are issued with an explicit scope claim bound to the key at issuance time. The server derives the connection's effective `(org_id, user_id, namespace_id, agent_id, permissions)` from the AUTH header rather than trusting any client-supplied `agent_id` on the wire:

```jsonc
{
  "key_id": "...",
  "org_id": "...",
  "user_id": "...",
  "namespace_id": "...",
  "agent_id": "...",
  "permissions": ["encode", "recall", "subscribe"]
}
```

This closes the agent-impersonation surface: an authenticated client at one `agent_id` cannot construct a request claiming a different `agent_id` and have the server honor it. Every shard-side handler reads the bound `agent_id` from the connection's session state, not from the request payload.

**v1.0 rollout posture: opt-in via env flag.** Existing deployments don't break — the default-permissive code path is preserved behind `BRAIN_AUTH_SCOPE_BINDING_REQUIRED=false`. Operators opt in by flipping the flag once their key-issuance pipeline emits scope claims.

**v1.1: default-deny.** The flag inverts; operators opt out explicitly if they have a reason to. This is the security target.

The key-issuance flow + the `api_keys` redb table live under `crates/brain-server/src/admin/api_key.rs`; operator UX for key rotation rides on the `/v1/api-keys` routes.

## 13. The dry-run mode

For destructive operations, pass a `dry_run` field:

```bash
curl -s -X DELETE http://127.0.0.1:9092/v1/agents/<agent-id> -d '{"dry_run":true}'
# Would delete: 12345 memories, 5 contexts, 67890 edges
```

Shows what would happen without doing it.

## 14. Scriptable output

Responses are JSON by default — ready for scripts. Pipe through `jq` for human-readable or tabular shaping:

```bash
curl -s http://127.0.0.1:9092/v1/shards | jq .
curl -s http://127.0.0.1:9092/v1/shards | jq -r '.[] | [.shard, .memory_count] | @tsv'
```

## 15. Output piping

The admin API composes with standard Unix tools:

```bash
curl -s http://127.0.0.1:9092/v1/agents \
  | jq -r '.[].agent_id' \
  | xargs -I{} curl -s http://127.0.0.1:9092/v1/agents/{}
```

Standard Unix tool composition.

## 16. The "background ops" tracking

Async operations (rebuild, restore, etc.) return a job ID in the response body:

```bash
$ curl -s -X POST http://127.0.0.1:9092/v1/rebuild-ann -d '{"shard":"<shard-id>"}'
{"job":"abc-123","status":"started"}
```

Job progress for rebuilds is exposed through the `brain_hnsw_rebuild_progress_pct` metric on the metrics listener; richer per-job status polling is future work.

## 16.5. Identity binding for admin keys

Admin operations authenticate with admin keys, which are separate from client agent keys. Each admin key carries explicit scope claims — which agents it may inspect, which shards it may rebuild, whether it may snapshot, whether it may restore. The server derives the operator's permissions from the key at AUTH time; admin calls do not accept an arbitrary `agent_id` or `shard_id` override that bypasses the claims.

This means an admin key issued for "inspect agent A only" cannot rebuild agent B's HNSW even if the request syntactically allows it; the server rejects the operation at dispatch with an authorization error. Audit logs record the key's claims alongside the operation, so post-incident review can verify that an action was authorized at the moment it ran, not just that the operator had access at some point.

Key rotation, revocation, and claim updates flow through the `/v1/api-keys` routes; changes take effect on subsequent AUTH attempts.

## 17. The "audit of admin ops"

All admin operations are audit-logged:

```json
{
  "ts": "...",
  "actor": "operator-name",
  "operation": "snapshot_create",
  "params": {"name": "pre-upgrade-2026-05-07"},
  "result": "success"
}
```

This documents who did what, when. Important for compliance.

---

*Continue to [`05_runbooks.md`](05_runbooks.md) for runbooks.*
