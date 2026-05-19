# HTTP API reference

Brain exposes one HTTP server on `[server] metrics_addr`
(default `0.0.0.0:9091`). Three families:

- **Liveness** — `/healthz`, `/metrics` for orchestrators and
  scrapers.
- **Admin** — `/v1/*` for snapshots, workers, config, audit,
  agents, shards, diagnostics.

The **data plane** (encode, recall, plan, reason, forget,
subscribe) is the binary `rkyv` wire protocol on `listen_addr`
— not HTTP. See [`wire-protocol/`](wire-protocol/).

Authentication on this surface is **not yet wired**. Routes are
exposed to any caller that can reach the port; the production
posture is "loopback + reverse proxy", and `--token` on
`brain-cli` is parsed for forward compatibility.

Source: `crates/brain-server/src/admin/router.rs`.

---

## Liveness

### `GET /healthz`

Returns `200 OK` with body `ok\n` if the server is up. Used by
the container `HEALTHCHECK` and any orchestrator probe.

Does **not** check:
- LLM-summarizer connectivity (degrades gracefully).
- OTel collector reachability.
- Cross-shard quorum (Brain is single-host in v1).

It **does** require:
- HTTP server listening.
- Per-shard WAL writable.
- Embedder model loaded.

If liveness fails after start, the container restart policy will
recycle the process; see
[`../runbooks/substrate-down.md`](../runbooks/substrate-down.md).

### `GET /metrics`

Prometheus text-exposition format. The full metric catalog is at
[`metrics.md`](metrics.md). Sample scrape job:

```yaml
- job_name: brain
  metrics_path: /metrics
  static_configs:
    - targets: ["brain:9091"]
```

---

## Admin — snapshots

| Method | Path | Action |
|---|---|---|
| `POST` | `/v1/snapshots` | Create a snapshot. Optional body: `{ "shard": N }`. |
| `GET`  | `/v1/snapshots` | List snapshots. |
| `DELETE` | `/v1/snapshots/{id}` | Delete a snapshot by id. |

Snapshots are per-shard, point-in-time captures (arena + WAL
pointer + redb). `POST /v1/snapshots` triggers a one-off; the
periodic snapshot worker honours `[workers] snapshot_interval_sec`.

`restore` from a snapshot is wire-protocol-only in v1
(`AdminRestoreReq`); the admin HTTP surface does not expose it.

---

## Admin — workers

| Method | Path | Action |
|---|---|---|
| `GET`  | `/v1/workers` | List the 12 workers, their intervals, and last-run timestamps. |
| `POST` | `/v1/workers/{name}` | Worker control. Body specifies action: `{ "action": "run-now" }`, `start`, `stop`. |

Worker names: `decay`, `consolidation`, `hnsw_maintenance`,
`idempotency_cleanup`, `slot_reclamation`, `wal_retention`,
`edge_scrub`, `counter_reconciliation`, `statistics_update`,
`embedder_cache_eviction`, `snapshot`. (See
[`configuration.md`](configuration.md#workers-optional).)

`run-now` triggers an immediate execution. `start`/`stop`
toggle the worker's scheduling. Stopped workers can be restarted;
their last-run pointer is preserved.

---

## Admin — config

| Method | Path | Action |
|---|---|---|
| `GET`  | `/v1/config` | Dump the merged effective config as JSON. |
| `GET`  | `/v1/config?key=<dotted.path>` | Read a single field (e.g. `?key=server.listen_addr`). |
| `POST` | `/v1/config/reload` | **Deferred.** Returns `501 Not Implemented`. v1 is restart-only. |
| `POST` | `/v1/config` | **Deferred.** Hot-set a field. Returns `501`. |

For permanent changes: edit the TOML and restart the server.

---

## Admin — audit

| Method | Path | Action |
|---|---|---|
| `GET`  | `/v1/audit` | Query the audit log. **Deferred** — returns `501`. |
| `GET`  | `/v1/audit/export` | Bulk export. **Deferred**. |

The audit pipeline lands in Phase 14+.

---

## Admin — agents

| Method | Path | Action |
|---|---|---|
| `GET`  | `/v1/agents` | List agents. **Deferred**. |
| `GET`  | `/v1/agents/{id}` | Get one agent's stats. **Deferred**. |
| `DELETE` | `/v1/agents/{id}` | Delete an agent (and all its memories). **Deferred**. |

---

## Admin — shards

| Method | Path | Action |
|---|---|---|
| `GET`  | `/v1/shards` | List shards and their state. |
| `POST` | `/v1/shards` | **Deferred.** Online shard add. Body: `{ "logical_id": N }`. |
| `DELETE` | `/v1/shards/{id}?confirm=1` | **Deferred.** Online shard remove. |

Shard add/remove is a v2 feature. v1 ships static `shard_count`
set at startup.

---

## Admin — diagnostics

| Method | Path | Action |
|---|---|---|
| `POST` | `/v1/diagnostics/profile` | **Deferred.** Body: `{ "duration_secs": N, "output_path": "/tmp/p.pb" }`. Captures a CPU profile to the given path. |
| `GET`  | `/v1/diagnostics/debug-snapshot` | Dump internal runtime state for debugging (workers, in-flight RPCs, shard state). Partial schema in v1. |

---

## Response shapes

All wired endpoints return JSON with this envelope:

```json
{
  "ok": true,
  "data": { ... },
  "error": null
}
```

Or on error:

```json
{
  "ok": false,
  "data": null,
  "error": {
    "code": "INVALID_ARGUMENT",
    "message": "shard 42 does not exist",
    "details": {}
  }
}
```

Error codes overlap with the wire-protocol taxonomy at
[`wire-protocol/error-codes.md`](wire-protocol/error-codes.md) —
the HTTP surface uses the same names where applicable, mapped to
the closest HTTP status code:

| Wire category | HTTP status |
|---|---|
| Protocol | 400 |
| Authentication | 401 |
| Authorization | 403 |
| Validation | 400 |
| NotFound | 404 |
| Conflict | 409 |
| ResourceExhausted | 429 |
| Unavailable | 503 |
| Internal | 500 |

`501 Not Implemented` is returned for deferred routes (not in the
wire taxonomy — it's an HTTP-surface signal that the route exists
but isn't wired yet).

---

## See also

- [`cli.md`](cli.md) — `brain-cli` wraps every wired endpoint here.
- [`metrics.md`](metrics.md) — what `/metrics` actually emits.
- [`../guides/observability.md`](../guides/observability.md) —
  wiring the HTTP surface into a monitoring stack.

**Source:** `crates/brain-server/src/admin/router.rs`. **Handlers:**
`crates/brain-server/src/admin/handlers/`.
