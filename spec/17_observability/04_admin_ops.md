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

Health and metrics live on a separate **public listener** (`[server] metrics_addr`, default `127.0.0.1:9091`), which carries exactly three routes and no authentication: `GET /healthz` (liveness — is the accept loop running), `GET /readyz` (readiness — `200` with `{"ready":true,"shards":N,"alive":N}` iff the node has at least one shard and every shard's executor is still draining, `503` otherwise), and `GET /metrics` (Prometheus text exposition). Database-wide counters (shard count, WAL size, HNSW node counts, request rates) come from `GET /metrics`; a dedicated JSON stats-summary route is not implemented.

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

The exposition includes `brain_shards_total`, `brain_wal_size_bytes`, `brain_hnsw_node_count`, `brain_hnsw_tombstone_count` / `brain_hnsw_tombstone_ratio`, and the `brain_request_*` request counters and histograms, among others. For per-shard or per-space detail, use the `GET /v1/shards` and `GET /v1/spaces` routes (see §11 and §10).

### Health

Two probes, answering different questions
(`crates/brain-server/src/admin/handlers/healthz.rs`,
`crates/brain-server/src/admin/handlers/readyz.rs`):

```bash
# Liveness: is the admin accept loop running? Plain text, always 200.
$ curl -s http://127.0.0.1:9091/healthz
ok

# Readiness: can this node serve data-plane traffic right now? JSON.
$ curl -s http://127.0.0.1:9091/readyz
{"ready":true,"shards":4,"alive":4}
```

`/readyz` is the one worth wiring to a load balancer. A node is ready only if it
has at least one shard and **every** shard's executor loop is still draining
requests; it flips to `503` the moment a shard thread dies, so the orchestrator
drains the node instead of routing requests that can never be served. The body
is JSON on both the `200` and the `503`, so a probe can log which shards are
missing.

Shards are recovered and spawned before the admin listener binds, so a fresh
process is ready as soon as `/readyz` answers at all — the probe earns its keep
after startup, not during it.

Neither probe reports per-shard identity or a version string. `/healthz`
returns the two bytes `ok`; the build version is on `brain_build_info` in the
metrics exposition, and per-shard detail is on `GET /v1/shards`.

### Info

Version, build info, and a configuration summary are returned by `GET /v1/config` (see §7), alongside the running configuration.

### Aggregates

`GET /v1/aggregates` sums the live aggregate counters across every shard and tenant: memory count, salience sum / average and access-count sum. Exact as of the last committed write (it reads the counters, not the analytics snapshot). Tenants use `MEMORY_AGGREGATE` on the wire for their own `(namespace, space)`; this route is the operator's cross-tenant view.

```bash
# Grand total of active memories, then per space and kind.
curl -s http://127.0.0.1:9092/v1/aggregates
curl -s 'http://127.0.0.1:9092/v1/aggregates?group_by=space,kind'
# One namespace, active + tombstoned, by status.
curl -s 'http://127.0.0.1:9092/v1/aggregates?namespace=3&status=active,tombstoned&group_by=status'
```

Parameters (all optional; lists are comma-separated): `namespace=<id>`, `space=<uuid>`, `kind=episodic,semantic,consolidated`, `status=active,tombstoned,hard_forgotten` (default `active`), `group_by=namespace,space,kind,status`. Returns `{"groups":[{...,"count":N,"salience_sum":S,"salience_avg":A,"access_count_sum":C}]}`. `503` while a shard is still building its counters after an upgrade (`brain_agg_counters_ready` is 0 there); `400` on an unknown parameter or value.

## 4. Maintenance operations

### Rebuild ANN

```bash
curl -s -X POST http://127.0.0.1:9092/v1/rebuild-ann -d '{"shard":"<shard-id>"}'
```

Triggers an immediate HNSW rebuild on the shard. Async; the call returns a job handle immediately. There is no progress metric — `brain_hnsw_rebuild_*` is a deferred family (`crates/brain-server/src/metrics/mod.rs:31-40`). Watch `brain_worker_cycles_total{worker="hnsw_maintenance"}` and `brain_hnsw_node_count` for the shard, and `brain_hnsw_tombstone_ratio` to confirm the rebuild did its job.

### GC

Triggers immediate garbage collection — pruning expired idempotency entries, deleting eligible WAL segments, or reclaiming eligible slots — rather than waiting for the scheduled worker. This is the generic worker run-now control plane: `POST /v1/workers/<name>/run-now` on the admin listener, where `<name>` is `idempotency_cleanup`, `wal_retention`, or `slot_reclamation`. (No dedicated GC route — the run-now action on the owning worker is the mechanism.)

### Vacuum

Compacts the metadata store (redb); may take minutes for large stores. In-place redb compaction is **not exposed as a dedicated admin route today** (planned, post-v1); the practical reclamation path is a snapshot + restore cycle (`/v1/snapshots`). redb reclaims freed pages within its own file over time, so an explicit compaction route is a convenience, not a correctness requirement.

## 5. Snapshot operations

### Create a snapshot

```bash
# Snapshot shard 3. The shard is a QUERY parameter, not a body field.
curl -s -X POST 'http://127.0.0.1:9092/v1/snapshots?shard=3'
# → {"id":7,"shard":3}
```

Three things to get right here, because each fails quietly:

- The route takes **no request body**. There is no `name` parameter —
  a snapshot is identified by a numeric `id` the shard assigns, returned in the
  response (`crates/brain-server/src/admin/handlers/snapshot/create.rs:22-24`).
- `shard` is a **query parameter** and a **shard index**, parsed as an integer;
  a non-numeric value is rejected with `invalid shard`.
- **Omitting `shard=` snapshots shard 0**, it does not snapshot every shard.
  The parser defaults to zero rather than erroring
  (`crates/brain-server/src/admin/query.rs:11-23`), so a command meant for the
  whole deployment silently covers one shard. Loop over the indices from
  `GET /v1/shards`.

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
  {"shard": 0, "id": 7, "taken_at_unix_nanos": 1763212800000000000, "size_bytes": 13207224320},
  {"shard": 1, "id": 4, "taken_at_unix_nanos": 1763190000000000000, "size_bytes": 12884901888}
]
```

The listing walks every shard, so it is the one snapshot route that is not
scoped by `?shard=`. A single shard failing to list fails the whole request
with a generic `500` — the per-shard error carries filesystem paths and redb
internals, so it is logged rather than returned
(`crates/brain-server/src/admin/handlers/snapshot/list.rs:43-53`).

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
# One shard: curl -s 'http://127.0.0.1:9092/v1/workers?shard=0'
```

`shard` is the **integer index**, not a UUID. The response is an object with a
`workers` array (`crates/brain-server/src/admin/handlers/worker/list.rs:83-100`):

```json
{"workers":[
  {"shard":0,"name":"decay","cycles":41,"processed":1203,"errors":0,"panics":0,
   "pending_work":0,"last_cycle_duration_ms":7,"last_run_unix":1763164800,
   "paused":false}
]}
```

There is no `next_run`: a worker's next tick is its interval from `last_run`,
and the fields that matter for triage are `pending_work` (the backlog estimate),
`errors` / `panics`, and `paused`.

That interval is **fixed in the binary, per worker kind**
(`crates/brain-workers/src/config.rs:152-228`), and is not operator-tunable.
The `[workers] *_interval_sec` keys are parsed and validated, but eleven of
them reach no scheduler at all — a test enumerates that set and asserts its
size so the condition stays deliberate
(`crates/brain-server/src/config/mod.rs:2078-2145`). The scheduler also reads
the interval once before entering its loop
(`crates/brain-workers/src/scheduler.rs:377`, `:462`), so nothing moves a
cadence at runtime either. Reading one of those keys back from
`GET /v1/config` reports what the file says, **not** what the scheduler is
doing.

The levers that do work are the three control actions below.

The fan-out is **partial-tolerant and says so**. If some shards answer and
others fail, the status is `207 Multi-Status` with a top-level `errors` array
naming the shards that dropped out — a short worker list never arrives as a
clean `200`. Only an all-shard failure is a `500`.

### Stop / start / run a worker now

```bash
curl -s -X POST http://127.0.0.1:9092/v1/workers/decay/stop
curl -s -X POST http://127.0.0.1:9092/v1/workers/decay/start
curl -s -X POST http://127.0.0.1:9092/v1/workers/decay/run-now
```

The path is `/v1/workers/{name}/{action}`, with `{action}` one of `stop`,
`start`, `run-now`. It takes **no body and no shard parameter**: each shard runs
its own instance of every worker, and the action applies to all of them. The
reply reports how many shards took it:

```json
{"worker":"decay","action":"stop","applied_shards":4,"errors":[]}
```

`stop` pauses the loop's work, not the loop: the worker keeps ticking on its
fixed interval and can be resumed without restarting the shard. `start` also
kicks the wake channel, so a resumed worker runs its next cycle without waiting
out the current sleep.

`run-now` is the only way to make a worker act off-cadence. For a worker whose
interval is longer than an incident — the snapshot worker's hour, the daily
sweepers — `run-now` is the remedy, not a config change.

#### The C0 guard — four workers that cannot be paused

```bash
$ curl -s -X POST http://127.0.0.1:9092/v1/workers/extractor/stop
worker `extractor` is C0 always-on and cannot be paused, resumed, or run on demand
```

`extractor`, `forget_cascade`, `schema_migration` and `statement_embed` are
refused with `403` for every one of the three actions
(`crates/brain-server/src/admin/handlers/worker/mod.rs:31-36`). They are the
write/read-coherence set: extraction populates the typed graph that
entity-anchored reads fuse over, and pausing it makes those reads quietly answer
from an incomplete graph. A degradation nothing reports is worse than a refusal.

Every other registered worker is controllable, and the controllable set is
derived from the scheduler's **live registration snapshot** rather than a
hand-maintained list — so a newly added maintenance worker is controllable the
day it lands, and no C0 worker becomes pausable by being left off a list. An
unregistered name is a `400`, distinct from the C0 `403`.

## 7. Configuration operations

### Reload configuration

```bash
curl -s -X POST http://127.0.0.1:9092/v1/config/reload
```

Reloads configuration from disk. Some settings reload live; others require restart.

### Get configuration

```bash
curl -s http://127.0.0.1:9092/v1/config
# Single key / subtree: curl -s 'http://127.0.0.1:9092/v1/config?key=workers.consolidation_interval_sec'
```

Returns the current configuration. With a `key` parameter, returns just that key.

### Set configuration — one key

```bash
curl -s -X POST 'http://127.0.0.1:9092/v1/config?key=monitoring.logging.level&value=debug'
```

The route is `POST /v1/config` — the same path as the `GET`, distinguished by
method, and it takes the key and value as **query parameters**, not a JSON
body. There is no `/v1/config/set`.

**Exactly one key is settable at runtime: `monitoring.logging.level`**
(`crates/brain-server/src/admin/handlers/config/set.rs:27`). Every other key
answers `501` with a `restart-required` marker naming the key, rather than a
no-op success — a configuration call that reports success and changes nothing
is worse than a refusal, because the operator stops looking. Change anything
else in the file and restart.

Raising the log level live is the case that has to work without a restart: it
is what an operator reaches for while an incident is in progress, and
restarting the process to see more of it destroys the state they were trying to
observe. `BRAIN_LOG` sets the same filter at boot.

## 8. Audit operations

### Query audit log

```bash
curl -s -G http://127.0.0.1:9092/v1/audit \
  --data-urlencode 'since=2026-05-01' \
  --data-urlencode 'until=2026-05-07' \
  --data-urlencode 'space=<space-id>'
```

Returns audit log entries matching the filter.

### Export audit log

```bash
curl -s -X POST http://127.0.0.1:9092/v1/audit/export \
  -d '{"output":"/backup/audit-2026-05.jsonl"}'
```

Exports audit logs to a file. For long-term archival.

## 9. Diagnostic operations

### Profile — not implemented

```bash
$ curl -s -X POST http://127.0.0.1:9092/v1/diagnostics/profile   # → 501
```

The route is registered and answers `501` with a deferred marker
(`crates/brain-server/src/admin/handlers/diagnostics/profile.rs`): an in-process
CPU profiler for a shard's Glommio executor is not built.

Profile from outside the process instead — `perf record` / `perf top` against
the `brain-shard-<id>` thread, which is named precisely so it can be picked out
(`crates/brain-server/src/shard/mod.rs:3113`). A shard is pinned to one core
when a cpu is assigned, so the thread and the core are interchangeable targets.

### Debug snapshot

```bash
curl -s 'http://127.0.0.1:9092/v1/diagnostics/debug-snapshot?shard=3' > debug.json
```

A **GET** with a query parameter — no body, and no `output` field; the snapshot
comes back in the response, so redirect it to a file yourself
(`crates/brain-server/src/admin/router.rs:270-274`,
`crates/brain-server/src/admin/handlers/diagnostics/debug_snapshot.rs:1-2`).

It captures a **partial** snapshot of one shard's runtime state — pending
requests from the dispatch-queue depth, and the in-memory state summary. As
with the other `?shard=` routes, omitting the parameter reports on shard 0.

For deep debugging, paired with `perf` from outside the process.

## 10. Space operations

### List spaces / space stats — not implemented

```bash
$ curl -s http://127.0.0.1:9092/v1/spaces               # → 501
$ curl -s http://127.0.0.1:9092/v1/spaces/<space-id>    # → 501
```

Both reads answer `501` with a `phase-11/space-index` marker
(`crates/brain-server/src/admin/handlers/space/list.rs`, and the `GET` arm of
`.../space/by_id.rs:58-61`). They need a `space_id` secondary index that does
not exist: every table key is already space-prefixed, which is what makes
isolation structural, but a prefix lets you scan *within* one space, not
enumerate the set of them.

That is the cost of the key layout, and it is the right trade — the alternative
is a second index on the write path for a read only an operator makes. Until
it exists, per-space numbers come from the tenant's own wire reads
(`SPACE_*`, `MEMORY_LIST`) rather than from the admin plane.

A space's own identity is still one-way: `space_id` is the `UUIDv5` derived
from the structured string, so the registry row is the only place the string
survives.

### Delete a space

```bash
# By derived space id (the UUIDv5 that routing and every table key use):
curl -s -X DELETE 'http://127.0.0.1:9092/v1/spaces/<space-uuid>?namespace=acme'

# By the external string ingress hashes, in its canonical hyphenated form:
curl -s -X DELETE 'http://127.0.0.1:9092/v1/spaces/<external-uuid>?namespace=acme&by=external'
```

Deletes every row under that space — the GDPR-erasure path.

**`?namespace=` is required**, and the call is a `400` without it
(`crates/brain-server/src/admin/handlers/space/by_id.rs:99-107`). The namespace
is the outer half of the `(namespace, space)` scope key, and the wire path takes
it from the caller's key — the admin plane has no key to take it from, so it has
to be named. Add `?by=external` when the path segment is the external string
rather than the derived id.

There is **no confirmation field and no dry run**: the route takes no body, and
the only guard is possession of the admin secret. Treat the call itself as the
point of no return.

The tenant-facing equivalent is the `SPACE_DELETE` opcode (`0x0072`), which acts
on the caller's *own* effective space and needs no admin secret.

There is no agent-level admin surface. "Agent" names the client application, not a storage scope; the thing an operator lists, inspects and deletes is a **space**.

## 10a. Extraction and backfill operations

Two distinct routes, easy to confuse:

```bash
# One-shot re-enqueue: push a selector's memories back onto the extraction queue.
curl -s -X POST http://127.0.0.1:9092/v1/extract/backfill -d '{"selector":{"all":{}}}'

# The durable, checkpointed, cancellable BackfillWorker.
curl -s -X POST   http://127.0.0.1:9092/v1/backfill -d '{"scope":{"all":{}},"extractor_ids":[],"dry_run":false}'
curl -s           http://127.0.0.1:9092/v1/backfill
curl -s -X DELETE http://127.0.0.1:9092/v1/backfill/<backfill-id>
```

`POST /v1/extract/backfill` is fire-and-forget: it reports how many memories it enqueued and skipped, and then the normal ExtractorWorker drains them. `POST /v1/backfill` drives the resumable worker — per-`(memory, extractor)` checkpoints, progress accounting, and a cancel verb. Use the first for a small nudge, the second for a real re-extraction run. Both are also reachable over the wire protocol as `ADMIN_BACKFILL` (`0x006E`) / `ADMIN_BACKFILL_CANCEL` (`0x006F`).

Related single-record repair:

```bash
# Un-tombstone a soft-forgotten memory (reverts the FORGET cascade).
curl -s -X POST http://127.0.0.1:9092/v1/memories/<memory-id>/restore

# Open-vocabulary predicates awaiting curation.
curl -s http://127.0.0.1:9092/v1/schema/review

# Rebuild a named index (distinct from /v1/rebuild-ann, which rebuilds the HNSW).
curl -s -X POST 'http://127.0.0.1:9092/v1/rebuild?index=tantivy_memory&shard=3'
```

Both parameters are query parameters. `index` is required — omitting it answers
`400` — and takes one of exactly seven values
(`crates/brain-server/src/admin/handlers/rebuild.rs:89-95`):

```
memory_hnsw, entity_hnsw, hype_hnsw, statement_question_hnsw,
all, tantivy_memory, tantivy_statement
```

An unrecognised name answers `400` with that list, so a typo is caught; a
missing `shard=` is not, and rebuilds shard 0. The response reports what was
done: `{"index":"tantivy_memory","entries":N,"elapsed_ms":M,"shard":3}`.

## 11. Shard operations

### List shards

```bash
$ curl -s http://127.0.0.1:9092/v1/shards
{"shards":[{"index":0,"shard_id":0},{"index":1,"shard_id":1}]}
```

The one implemented shard route (`crates/brain-server/src/admin/handlers/shard/list.rs`).
It reports the shards this process is running and their runtime ids, and nothing
else — no per-shard counters. Per-shard storage numbers come from `/metrics`,
where every storage and HNSW gauge carries a `shard` label.

### Create / delete a shard — not implemented

```bash
$ curl -s -X POST   http://127.0.0.1:9092/v1/shards        # → 501
$ curl -s -X DELETE http://127.0.0.1:9092/v1/shards/0      # → 501
```

Both routes are registered and both answer `501` with a deferred marker
(`crates/brain-server/src/admin/handlers/shard/create.rs`,
`.../shard/delete.rs`). The shard tier is not elastic: `shard_count` is settled
when the data directory is created, recorded in `layout.json`, and checked on
every open — a `MemoryId` encodes its shard, so serving a directory with a
different count would route every existing id at a shard that does not hold it.

The routes exist rather than 404 so that an operator reaching for expansion is
told it is deferred instead of guessing whether they have the path wrong.
Changing the shard count means a new data directory and a re-ingest; see
[`spec/16_sharding/05_rebalancing_and_replication.md`](../16_sharding/05_rebalancing_and_replication.md) §2.

## 12. Authentication

Admin operations require an admin token, passed as a bearer header:

```bash
export BRAIN__ADMIN__TOKEN="..."
curl -s -H "Authorization: Bearer $BRAIN__ADMIN__TOKEN" http://127.0.0.1:9092/v1/shards
```

**One secret, compared whole.** `[admin] token` (or `BRAIN__ADMIN__TOKEN`) is a single string; every `/v1/*` route is gated on a constant-time comparison against it (`crates/brain-server/src/admin/router.rs:64-79`). There is no per-operator token list and no scoping: a request either presents that one secret and may do everything on the admin plane, or it is refused. Fail-closed — with no token configured the listener does not start.

Mint and revoke data-plane keys through the API-key routes, which need the same secret:

```bash
# Mint a key. space_id_hex is required: it is the 16-byte space id as 32 hex
# chars, and it is what every operation issued on this key gets stamped with.
curl -s -X POST http://127.0.0.1:9092/v1/api-keys \
  -H "Authorization: Bearer $BRAIN__ADMIN__TOKEN" \
  -H 'content-type: application/json' \
  -d '{"space_id_hex":"00000000000000000000000000000001",
       "namespace":"acme",
       "permissions":["ENCODE","RECALL"]}'
# Revoke a key, by the hex of its BLAKE3 hash
curl -s -X DELETE http://127.0.0.1:9092/v1/api-keys/<key-hash-hex> \
  -H "Authorization: Bearer $BRAIN__ADMIN__TOKEN"
```

`permissions` is either a list of names or a raw `u32` bitfield. Names are matched case-insensitively and cover the single bits — `ENCODE`, `RECALL`, `FORGET`, `LINK`, `SCHEMA_UPLOAD`, `ADMIN`, `ACT_AS`, `LLM_CREDENTIAL` — plus the bundles `STANDARD_SPACE`, `READ_ONLY`, `READ_WRITE` and `FULL` (`crates/brain-server/src/admin/handlers/api_keys.rs:72-98`). An unknown name is refused rather than silently dropped. `org_id_hex`, `user_id_hex`, `may_act` and `service_principal` are optional. The reply carries the raw secret once and never again.

There is no `agent` field: a key names a space, and the space is the identity.

#### The `ACT_AS` permission and the `may_act` allowlist

`permissions` may include `"ACT_AS"` — the service-principal grant that lets a
key run data-plane ops **on behalf of another identity** via the per-request
`act_as` field (defined in
[`../04_wire_protocol/04_handshake.md`](../04_wire_protocol/04_handshake.md)
§"Per-request identity (`act_as`)"; backed by the permission bit
`ACT_AS = 1 << 6`). It is held only by a trusted service principal — an edge or
gateway — never a normal agent key.

An `ACT_AS` key MUST carry a **`may_act` allowlist**: the set of namespaces the
principal may impersonate within. It is stored as a field on the key's row in
the `api_keys` table (alongside `namespace`, `agent`, `permissions`), and Brain
honors an `act_as` request **only** when the target namespace is in this
allowlist. A principal can never act as a namespace outside its `may_act` set,
and the mint never crosses its own namespace boundary — cross-tenant isolation
stays absolute. Unbounded impersonation, as Kubernetes offers it, is a
God-credential; the `may_act` allowlist is the bound that avoids it.

A single **`"*"` entry** is the **wildcard grant**: the principal may act as
*any* namespace. This is the trusted multi-tenant front-door case — a
gateway/edge that fronts every tenant cannot enumerate an allowlist that grows
with each new signup, so it holds `["*"]` and Brain admits any target namespace
(exactly as a fixed allowlist admits its named members). Named entries and the
wildcard compose: `["*"]` grants all, `["acme", "globex"]` grants those two.
The wildcard is the deliberate, auditable escape hatch for a front-door
principal; it does **not** relax the per-request effective-identity isolation —
each `act_as` op still runs strictly as its own `(namespace, space)`, and the
dual-principal audit records that a `"*"` service key performed it. Grant `"*"`
only to a genuinely trusted service principal, reached over mTLS or a private
transport, never to a tenant key.

```jsonc
// Mint an edge/gateway service principal that may act for two named tenants:
{
  "namespace": "edge",             // the principal's own home namespace
  "agent": "arc-gateway",
  "permissions": ["encode", "recall", "forget", "ACT_AS"],
  "may_act": ["acme", "globex"]    // the namespaces it may impersonate within
}

// Or a front-door principal that may act for ANY tenant (multi-tenant SaaS):
{
  "namespace": "edge",
  "agent": "arc-gateway",
  "permissions": ["encode", "recall", "forget", "ACT_AS"],
  "may_act": ["*"]                 // wildcard: any namespace, resolved per request
}
```

**Observability.** Emit and track `act_as` ops per
`(service_principal → target namespace)`. A sudden fan-out — one principal
acting across many namespaces, or across namespaces it rarely touches — is the
signature of a compromised edge fanning across tenants; alert on it. This is the
analogue of Kubernetes impersonation auditing.

### 12.1 Scope-bound API keys

Authentication is **mandatory** on every data-plane connection, and identity is derived 100% from the key. There is no anonymous mode, no default agent, and no default namespace. The server reads the connection's effective `(namespace, agent, permissions)` — plus a non-authoritative `user` audit tag — from the key resolved at AUTH; it never trusts a client-supplied `space_id` or `namespace` on the wire. A connection with no key, an unresolvable key, or a revoked key is rejected at AUTH (`Unauthenticated`).

Minting binds the scope to the key at issuance time:

```jsonc
{
  "key_id": "...",
  "namespace": "acme",        // tenant boundary; interned (created) if new
  "agent": "support-bot",     // application within the namespace
  "user": "...",              // optional, non-authoritative audit tag
  "permissions": ["encode", "recall", "subscribe"]
}
```

This closes the agent-impersonation surface: an authenticated client at one `agent` cannot construct a request claiming a different `agent` (or another `namespace`) and have the server honor it. Every shard-side handler reads the bound scope from the connection's session state, not from the request payload.

**Provisioning is the only way identity enters the system.** Minting a key creates/interns its namespace and binds its agent; the data plane never creates identity. The mint route itself is gated by the operator admin secret (above), so identity creation is an operator privilege, not an agent one.

The key-issuance flow + the `api_keys` redb table live under `crates/brain-server/src/admin/api_key.rs`; operator UX for key rotation rides on the `/v1/api-keys` routes.

## 13. Dry runs

**The destructive admin routes have no dry-run mode.** `DELETE /v1/spaces/{id}`
takes no body and no `dry_run` parameter; it deletes. Possession of the admin
secret is the whole of the authorization, which is the other reason the listener
stays on loopback.

Where a dry run does exist, it is a worker control, not an HTTP flag. Two
sweepers take `dry_run` in their config sections and log what they would reclaim
instead of reclaiming it — the supersession sweeper
(`crates/brain-server/src/config/mod.rs:664`) and the predicate GC
(`:711`):

```toml
[workers.predicate_gc]
enabled = true
dry_run = true

[workers.supersession_sweeper]
enabled = true
dry_run = true
```

That is where a dry run earns its keep — a sweeper decides what to delete from a
rule, and the rule is what an operator wants to check. An explicit `DELETE` of a
named space has nothing to preview: the operator already named the thing.

## 14. Scriptable output

Responses are JSON — ready for scripts. Pipe through `jq`:

```bash
curl -s -H "Authorization: Bearer $BRAIN__ADMIN__TOKEN" \
  http://127.0.0.1:9092/v1/shards | jq .
# {"shards":[{"index":0,"shard_id":0}, ...]}
```

Note the shape: `/v1/shards` returns an **object** with a `shards` array, not a
bare array, so `jq '.shards[]'` rather than `jq '.[]'`. Per-shard counters are
not in it — those are `/metrics` gauges carrying a `shard` label.

## 15. Output piping

The admin API composes with standard Unix tools. Every call needs the bearer
header, so export it once:

```bash
export BRAIN__ADMIN__TOKEN="..."
auth=(-H "Authorization: Bearer $BRAIN__ADMIN__TOKEN")

# Which workers are registered, and when each last ran:
curl -s "${auth[@]}" http://127.0.0.1:9092/v1/workers | jq .

# Run every reclamation worker now, in order:
for w in idempotency_cleanup wal_retention slot_reclamation; do
  curl -s "${auth[@]}" -X POST "http://127.0.0.1:9092/v1/workers/$w/run-now"
done
```

Fan-out over spaces is not scriptable from here: the space listing is `501`
(§10), so there is no id list to pipe. Per-space numbers come from the tenant's
own wire reads.

## 16. The "background ops" tracking

Async operations (rebuild, restore, etc.) return a job ID in the response body:

```bash
$ curl -s -X POST http://127.0.0.1:9092/v1/rebuild-ann -d '{"shard":"<shard-id>"}'
{"job":"abc-123","status":"started"}
```

The returned job id is not pollable: there is no per-job status route and no rebuild-progress metric. Completion is inferred from `brain_hnsw_tombstone_ratio` dropping for that shard. Per-job status polling is future work.

## 16.5. Identity binding for admin keys

Admin operations authenticate with the operator admin secret, which is a different object from a data-plane API key: the secret is a bearer token on an HTTP listener, a data-plane key is presented in the wire AUTH frame and resolves to `(namespace, space, permissions)`.

The admin secret carries **no scope claims**. It is unscoped by construction — one secret, all of `/v1/*` — which is why the listener stays on loopback and why a leaked admin token is a full deployment compromise rather than a bounded one. Scoped operator credentials would need a second credential store on the admin plane and do not exist.

The consequence for callers: an admin route's `space_id` or `shard_id` argument is not checked against claims, because there are none to check against. It is checked for existence and range only.

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
