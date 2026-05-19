# `brain-cli` reference

> **Looking for the interactive shell?** That's the `brain` binary
> (interactive REPL + one-shot cognitive ops, like `psql` for
> PostgreSQL). See [`brain-shell.md`](brain-shell.md). `brain-cli`
> documented here is the **admin** CLI — HTTP routes only.

Admin CLI for `brain-server`. Talks to two HTTP endpoints:

- **`--server`** (default `127.0.0.1:9092`) — the admin HTTP
  server, serving `/v1/*` routes. Every command except `health`
  and `stats` uses this. Match `[server] admin_addr` in your TOML.
- **`--metrics-addr`** (default `127.0.0.1:9091`) — the public
  HTTP server, serving `/healthz` + `/metrics`. Used by `health`
  and `stats` only. Match `[server] metrics_addr` in your TOML.

The wire protocol on `listen_addr` is for SDK use, not for the CLI.

Source: `crates/brain-cli/src/main.rs`. Authoritative command
surface: spec §14/06.

## Invocation

```
brain-cli [OPTIONS] <COMMAND>
```

## Global options

| Option | Default | Notes |
|---|---|---|
| `--server <host:port>` | `127.0.0.1:9092` | Admin HTTP server (/v1/* routes). Match `[server] admin_addr`. |
| `--metrics-addr <host:port>` | `127.0.0.1:9091` | Public HTTP server (/healthz + /metrics). Used by `health` and `stats`. Match `[server] metrics_addr`. |
| `--output <json\|table>` | `table` | Output format. `json` for piping into `jq`. |
| `--token <value>` | — | Admin token. Parsed for forward compatibility; auth wiring lands in Phase 14+. |
| `--shard <N>` | — | Target a specific shard for the subset of commands that accept it. |
| `--name <worker>` | — | Worker name. One of `decay`, `consolidation`, `hnsw_maintenance`, `idempotency_cleanup`, `slot_reclamation`, `wal_retention`, `edge_scrub`, `counter_reconciliation`, `statistics_update`, `embedder_cache_eviction`, `snapshot`. |
| `--key <dotted.path>` | — | Config key for `config get` / `config set`. |
| `--value <v>` | — | Config value for `config set`, or output path for `profile` / `debug-snapshot`. |
| `--since`, `--until`, `--agent` | — | Audit query filters. |
| `--logical-id <N>` | — | Shard create. |
| `--confirm` | — | Required for destructive commands. |
| `--duration-secs <N>` | `30` | Profile capture duration. |
| `--version`, `-V` | — | Print version and exit. |
| `--help`, `-h` | — | Print help and exit. |

Exit codes: `0` success, `2` invocation or server error.

---

## Commands

### `health`

Probes `/healthz`.

```
brain-cli health
```

Returns `status: ok` and basic uptime / shard-count info.

### `stats`

Snapshots `/metrics` and renders selected counters.

```
brain-cli stats
brain-cli stats --output json
```

Use for spot checks. For continuous monitoring, scrape `/metrics`
into Prometheus.

### `rebuild-ann [--shard N]`

Forces an HNSW rebuild for one shard (or all if `--shard` omitted).
Expensive — runs the graph build from scratch.

```
brain-cli rebuild-ann --shard 3
```

### `snapshot create|list|delete|restore`

Snapshot family. Snapshots are point-in-time captures of a shard's
state (arena + WAL pointer + redb).

| Subcommand | Required flags | Notes |
|---|---|---|
| `snapshot create [--shard N]` | — | Trigger an on-demand snapshot. |
| `snapshot list` | — | List existing snapshots. |
| `snapshot delete --shard N --value <id>` *(uses `--value` as ID)* | id | Delete a specific snapshot. |
| `snapshot restore --value <id>` *(uses `--value` as ID)* | id, `--confirm` | Restore the shard from a snapshot. Destructive. |

### `worker list|stop|start|run-now`

Worker control. Some subcommands deferred to later phases.

```
brain-cli worker list
brain-cli worker run-now --name decay
brain-cli worker stop --name consolidation        # deferred
```

### `config get|reload|set`

| Subcommand | Status | Notes |
|---|---|---|
| `config get --key <dotted.path>` | wired | Read a config field by dotted path (`server.listen_addr`). |
| `config get` *(no key)* | wired | Dump the full effective config as JSON. |
| `config reload` | **deferred** | Returns `501 Not Implemented`. Brain v1 is restart-only. |
| `config set --key … --value …` | **deferred** | Hot-set a field. Deferred. |

### `audit query|export`

Audit log access. **Deferred** in v1.0 — both subcommands return
`501 Not Implemented`. The audit pipeline lands in Phase 14+.

### `agent list|stats|delete`

Agent operations. **Deferred** in v1.0.

### `shard list|create|delete`

| Subcommand | Status | Notes |
|---|---|---|
| `shard list` | wired | Lists shards and their state. |
| `shard create --logical-id N` | **deferred** | Online shard add. |
| `shard delete --shard N --confirm` | **deferred** | Online shard remove. |

### `profile [--duration-secs N] [--value PATH]`

CPU profile capture. **Deferred** — returns a stub. The mechanism
exists; the back-end profiler integration lands later.

### `debug-snapshot [--value PATH]`

Runtime snapshot — dumps internal state for debugging (shard
state, worker last-runs, in-flight RPCs). Partial schema in v1;
expect changes in v2.

---

## Worked examples

### See worker last-runs

```bash
brain-cli worker list --output json | jq '.workers[] | {name, last_run_iso}'
```

### Force a one-off snapshot of shard 0 and list snapshots

```bash
brain-cli snapshot create --shard 0
brain-cli snapshot list
```

### Read a single config field

```bash
brain-cli config get --key shard.arena_capacity_bytes
```

### Inside a container

The admin port (`9092` — `/v1/*` routes) is loopback-only by
config and not published to the host. `docker exec` reaches it
from inside:

```bash
docker exec brain brain-cli worker list
docker exec brain brain-cli snapshot list
```

The public port (`9091` — `/healthz` + `/metrics`) is published
and can be hit from the host too:

```bash
brain-cli health         # uses --metrics-addr default 127.0.0.1:9091
brain-cli stats --output json | jq
```

---

## See also

- [`http-api.md`](http-api.md) — the HTTP endpoints the CLI talks
  to. Useful when scripting against the server directly.
- [`../guides/operate.md`](../guides/operate.md) — day-to-day
  operating workflow that uses these commands in context.
- [`../runbooks/`](../runbooks/) — incident response playbooks
  that invoke specific CLI subcommands.

**Spec:** §14/06 (admin operations). **Source:**
`crates/brain-cli/src/main.rs` and `crates/brain-cli/src/commands/`.
