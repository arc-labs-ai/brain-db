# 04 — Admin CLI

`brain` (the admin CLI) connects to the admin HTTP server (port
9091 by default). Every command supports JSON or table output.

## Global flags

```
--server <host:port>     Admin endpoint (default 127.0.0.1:9091)
--output <json|table>    Output format (default table)
--shard <N>              Target a specific shard (0-indexed)
```

The CLI is invoked via `just cli ...` (which forwards to
`target/debug/brain` inside the container) or directly:

```bash
cargo run --bin brain -- <subcommand> ...
```

Examples below use the `just cli` form.

---

## health

Pings the admin server and reports liveness.

**Input:**

```bash
just cli health
```

**Expected (table):**

```
status            healthy
admin_endpoint    127.0.0.1:9091
probe             /healthz
```

**Expected (JSON):**

```bash
just cli --output json health
```

```json
{
  "status": "healthy",
  "admin_endpoint": "127.0.0.1:9091",
  "probe": "/healthz"
}
```

**Verify:**

Exit code 0 = server is responsive. Non-zero = couldn't reach
admin endpoint; check [`09-troubleshooting.md`](09-troubleshooting.md).

---

## stats

Snapshot of Prometheus metrics.

**Input:**

```bash
just cli stats
```

**Expected:**

```
brain_up                       1
brain_shards_total             4
brain_connections_active       0
brain_connections_total        12
brain_request_total{op="encode",status="success"}   42
process_uptime_seconds         847
process_memory_resident_bytes  131072000
process_open_fds               48
...
```

**Verify:**

- `brain_up` should be `1`.
- `brain_shards_total` matches `[storage] shard_count` in your
  config.
- `process_uptime_seconds` grows between calls.

For the full taxonomy, see
[`docs/guides/observability.md`](../guides/observability.md#2-metrics).

---

## shard list

Lists configured shards.

**Input:**

```bash
just cli shard list
```

**Expected (table):**

```
index 0    shard_id=0
index 1    shard_id=1
index 2    shard_id=2
index 3    shard_id=3
```

**Expected (JSON):**

```bash
just cli --output json shard list
```

```json
{"shards":[{"index":0,"shard_id":0},{"index":1,"shard_id":1},{"index":2,"shard_id":2},{"index":3,"shard_id":3}]}
```

**Verify:**

Number of entries matches `[storage] shard_count`.

---

## worker list

Each shard runs 12 background workers (decay, consolidation,
hnsw_maintenance, etc.). List them all or filter to one shard.

**Input (all shards):**

```bash
just cli worker list
```

**Expected:**

```
shard 0 / decay              cycles=14  processed=0   errors=0  last_run_unix=1715682000
shard 0 / access_boost       cycles=14  processed=0   errors=0  last_run_unix=1715682000
shard 0 / consolidation      cycles=2   processed=0   errors=0  last_run_unix=1715681400
...
shard 3 / snapshot           cycles=0   processed=0   errors=0  last_run_unix=0
```

**Input (single shard):**

```bash
just cli --shard 0 worker list
```

**Verify:**

- `cycles` should be `> 0` for the always-running workers (decay,
  access_boost) within a minute of starting the server.
- `errors` should be `0` across the board.
- `last_run_unix` of `0` means the worker hasn't run yet (first
  cycle scheduled in the future per `[workers]` intervals).

---

## config get

Read the loaded config, optionally by dotted key path.

**Input (full config):**

```bash
just cli --output json config get
```

**Expected:**

```json
{
  "server": {"listen_addr": "127.0.0.1:9090", "metrics_addr": "127.0.0.1:9091", ...},
  "storage": {"data_dir": "./data", "shard_count": 4},
  "hnsw": {"m": 16, "ef_construction": 200, "ef_search": 64},
  ...
}
```

**Input (single key):**

```bash
just cli --output json config get --key hnsw.m
```

**Expected:**

```json
16
```

```bash
just cli --output json config get --key workers.decay_interval_sec
```

```json
3600
```

**Verify:**

Values match what's in `config/dev.toml` (or your env overrides).

`POST /v1/config` (live mutation) is currently 501 (tracker
`phase-11/runtime-config-set`). To change config, edit the TOML
and restart.

---

## snapshot create

Take a snapshot of one shard. Snapshots are full point-in-time
copies; the daily background worker prunes to retention.

**Input:**

```bash
just cli --shard 0 snapshot create
```

**Expected:**

```
id       1715682345
shard    0
```

**Verify:**

```bash
just cli snapshot list
```

The newly-created snapshot ID appears in the listing.

---

## snapshot list

List snapshots across all shards (or filter).

**Input:**

```bash
just cli snapshot list
```

**Expected:**

```
shard 0 / snapshot 1715682345    1048576 bytes, taken_at_unix_nanos=1715682345000000000
```

---

## snapshot delete

Delete a snapshot by id.

**Input:**

```bash
just cli --shard 0 snapshot delete 1715682345
```

**Expected:**

Exit code 0; no stdout output (HTTP 204 No Content).

**Verify:**

```bash
just cli snapshot list
```

The deleted id is gone from the list.

---

## rebuild-ann

Forces an immediate out-of-schedule rebuild of the HNSW index for
one shard. Used when the tombstone ratio climbs (see
[`docs/runbooks/recall-degraded.md`](../runbooks/recall-degraded.md)).

**Input:**

```bash
just cli --shard 0 rebuild-ann
```

**Expected:**

```
shard       0
entries     42891
elapsed_ms  3241
```

**Verify:**

`entries` matches the number of non-tombstoned memories on that
shard. `elapsed_ms` should be tens of seconds for 1 M memories on
reference hardware; for an empty-shard rebuild expect milliseconds.

---

## debug-snapshot

Captures the current runtime state of a shard. v1 schema is
partial — worker statuses are populated; other fields are listed
in `deferred[]` per the spec deferred-set.

**Input:**

```bash
just cli --output json debug-snapshot --shard 0
```

**Expected:**

```json
{
  "shard": 0,
  "captured_at_unix": 1715682400,
  "partial": true,
  "deferred": ["active_tasks","pending_requests","recent_errors","in_memory_state_summary"],
  "workers": [
    {"name":"decay","cycles":14,"processed":0,"errors":0,"last_run_unix":1715682000},
    {"name":"hnsw_maintenance","cycles":2,"processed":0,"errors":0,"last_run_unix":1715681800}
  ]
}
```

**Write to a file:**

```bash
just cli --output json debug-snapshot --shard 0 --value /tmp/snap.json
cat /tmp/snap.json
```

**Verify:**

`workers[]` should have one entry per worker (12 total per shard).
`deferred[]` is the static list above until the corresponding
primitives land.

---

## Using a remote server

All commands accept `--server`:

```bash
just cli --server 10.0.0.5:9091 health
just cli --server 10.0.0.5:9091 --output json shard list
```

## Next

[`05-sdk.md`](05-sdk.md) — talking to the data plane via the Rust
SDK.
