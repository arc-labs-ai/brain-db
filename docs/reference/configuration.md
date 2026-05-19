# Configuration reference

Full TOML schema for `brain-server`. Every field, every default,
every override pattern. The authoritative struct lives at
`crates/brain-server/src/config/mod.rs`.

The server reads its config from the path passed via
`--config <path>` (default `config/dev.toml`). The shipped
in-image default for the Docker image is at
[`../../config/docker.toml`](../../config/docker.toml).

## How configuration loads

1. The TOML file at `--config <path>` is read and parsed.
2. Every environment variable prefixed `BRAIN__` is interpreted
   as a double-underscore-separated path into the TOML tree and
   merged on top. Path components are lowercased. Leaf values are
   coerced to `bool`, `i64`, or `f64` if they parse cleanly;
   otherwise they stay as strings. Byte sizes (e.g. `"4GiB"`)
   stay as strings — the `deserialize_human_bytes` helper parses
   them on the typed-struct side.
3. The merged TOML is `serde`-deserialised into `Config` with
   `deny_unknown_fields`. Any unknown key, missing required
   field, or invalid value produces a `config error: …` on
   stderr and aborts startup.

Config is **restart-only** in v1 — there is no hot reload (spec
§01/04 §15). The admin endpoint `/v1/config/reload` returns
`501 Not Implemented` for now.

## Byte-size syntax

Wherever a field is documented as a *byte size*, the value is a
string with one of these suffixes (binary or SI):

| Suffix | Multiplier |
|---|---|
| *(none)* or `B` | 1 |
| `KiB` | 1024 |
| `MiB` | 1024² |
| `GiB` | 1024³ |
| `TiB` | 1024⁴ |
| `KB` | 1 000 |
| `MB` | 1 000 000 |
| `GB` | 1 000 000 000 |

Bare integers without a suffix are bytes.

---

## `[server]`

Required. Defines the three listen sockets.

| Field | Type | Default | Notes |
|---|---|---|---|
| `listen_addr` | socket addr | — | Data plane (rkyv wire protocol). Bind to `0.0.0.0:8080` in container deployments; private interface otherwise. |
| `metrics_addr` | socket addr | — | Public HTTP — serves `/healthz` + `/metrics`. Typically `0.0.0.0:9091`. Safe to expose to load balancers and Prometheus scrapers. |
| `admin_addr` | socket addr | — | Admin HTTP — serves `/v1/*` (snapshots, audit, agents, workers, config, diagnostics). **Default loopback** (`127.0.0.1:9092`); v1 has no built-in admin auth. Front with mTLS or a token-checking reverse proxy if you bind to a public interface. |

### `[server.tls]` (optional)

| Field | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `false` | Terminate TLS on `listen_addr`. |
| `cert` | path | — | PEM cert file. Required when `enabled = true`. |
| `key` | path | — | PEM key file. Required when `enabled = true`. |

## `[storage]`

Required.

| Field | Type | Default | Notes |
|---|---|---|---|
| `data_dir` | path | — | Where Brain keeps WAL, arena, and redb. Must be writable by the running user. |
| `shard_count` | usize | — | Number of shards. Each shard pins to one core. Plan = number of physical cores you want to dedicate to Brain. |

## `[shard]`

Required. Per-shard storage knobs (every shard uses these values).

| Field | Type | Default | Notes |
|---|---|---|---|
| `arena_capacity_bytes` | byte size | — | Pre-allocated arena size per shard. At 1600 B/slot, 1 GiB ≈ 660 K vectors. |
| `wal_segment_size_bytes` | byte size | — | Size of each WAL segment file. Typical: 64-256 MiB. Smaller = more frequent rollover; larger = slower recovery. |
| `wal_retention_segments` | u32 | — | Segments kept before reclamation. Total retained WAL = `wal_segment_size_bytes × wal_retention_segments` per shard. |

## `[hnsw]`

Required.

| Field | Type | Default | Notes |
|---|---|---|---|
| `m` | usize | — | HNSW out-degree. Spec §06/02 default = 16. Range [8, 64]. Higher = better recall, slower build, more RAM. |
| `ef_construction` | usize | — | Build-time candidate set size. Spec default = 200. Higher = better graph quality, slower ENCODE. |
| `ef_search` | usize | — | Query-time candidate set size. Spec default = 64. Higher = better recall, slower RECALL. |

See [`../guides/tuning/hnsw-parameters.md`](../guides/tuning/hnsw-parameters.md).

## `[embedder]`

Required.

| Field | Type | Default | Notes |
|---|---|---|---|
| `model` | string | — | Embedding model identifier. Currently only `bge-small-en-v1.5` (384-dim) is wired. |
| `cache_size` | usize | — | LRU cache of (text → vector) entries. Typical: 1 000-10 000. |
| `batch_size` | usize | — | Max texts batched into one model call. Typical: 32. |
| `batch_window_ms` | u32 | — | Wait window for batching. Typical: 5 ms. Higher = better throughput, worse p50 latency. |

## `[auth]` *(optional, defaults to `mode = "none"`)*

| Field | Type | Default | Notes |
|---|---|---|---|
| `mode` | enum | `"none"` | One of `"none"`, `"api_key"`. `api_key` is parsed but not yet fully wired (spec §03/06). |

Until tokens/mTLS ship, the only fully-functional mode is `none`.
See [`../guides/security/auth-modes.md`](../guides/security/auth-modes.md).

## `[logging]` *(optional)*

| Field | Type | Default | Notes |
|---|---|---|---|
| `level` | string | `"info"` | `error` / `warn` / `info` / `debug` / `trace`. |
| `output` | string | `"stdout"` | `stdout` or `stderr`. Brain does not write log files directly. |
| `format` | string | `"compact"` | `compact` (human) or `json` (structured). Use `json` in production. |

## `[tracing]` *(optional)*

OpenTelemetry exporter for distributed tracing. Disabled by default.

| Field | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `false` | Master switch. Off = no spans exported. |
| `sampler` | string | `"always_off"` | `always_on`, `always_off`, `ratio`, or `parent_based`. |
| `sample_ratio` | f64 | `0.0` | Used only when `sampler = "ratio"`. Clamped to `[0.0, 1.0]`. |
| `endpoint` | string | `""` | OTLP/HTTP collector. Empty + `enabled = true` falls back to a stdout exporter. |
| `service_name` | string | `"brain-server"` | `service.name` attribute on every span. |

## `[workers]` *(optional)*

Every value is an interval in seconds. `0` disables the worker.
Omitted fields take spec defaults.

| Field | Spec default | Notes |
|---|---|---|
| `decay_interval_sec` | 3 600 | Recency-based salience decay. |
| `consolidation_interval_sec` | 86 400 | Summarises related memories. No-op unless `[summarizer]` is configured. |
| `hnsw_maintenance_interval_sec` | 1 800 | Periodic graph repair. |
| `idempotency_cleanup_interval_sec` | 3 600 | Reaps 24-h-stale idempotency entries. |
| `slot_reclamation_interval_sec` | 86 400 | Reuses slots past tombstone grace. |
| `wal_retention_interval_sec` | 300 | Closes + reclaims old WAL segments. |
| `edge_scrub_interval_sec` | 86 400 | Verifies edge consistency. |
| `counter_reconciliation_interval_sec` | 86 400 | Syncs in-memory counters vs persisted state. |
| `statistics_update_interval_sec` | 3 600 | Recomputes shard-level stats. |
| `embedder_cache_eviction_interval_sec` | 86 400 | Reaps cold embedder cache entries. |
| `snapshot_interval_sec` | 86 400 | Background snapshot to `data_dir/snapshots/`. |

## `[summarizer]` *(optional)*

Drives the consolidation worker. Defaults to disabled.

| Field | Type | Default | Notes |
|---|---|---|---|
| `backend` | enum | `"disabled"` | `disabled`, `openai`, or `ollama`. Non-disabled values require the matching cargo feature (`summarizer-openai` / `summarizer-ollama`). |
| `request_timeout_sec` | u32 | `30` | HTTP timeout per LLM round-trip. |
| `max_summary_chars` | u32 | `4 096` | Soft cap on summary output length. |
| `openai_api_base` | string | `"https://api.openai.com/v1"` | OpenAI-compatible endpoint. |
| `openai_api_key_env` | string | — | **Name of an env var** holding the key. Brain never reads the key from TOML directly. |
| `openai_model` | string | `"gpt-4o-mini"` | Model id. |
| `openai_temperature` | f32 | `0.3` | Sampling temperature. |
| `ollama_base` | string | `"http://localhost:11434"` | Ollama HTTP endpoint. |
| `ollama_model` | string | `"llama3.1:8b"` | Model tag. |

## Environment overrides

Pattern: `BRAIN__SECTION__FIELD=value` — double underscore
separates nesting. Examples:

```bash
BRAIN__SERVER__LISTEN_ADDR=0.0.0.0:8080
BRAIN__STORAGE__SHARD_COUNT=8
BRAIN__SHARD__ARENA_CAPACITY_BYTES=4GiB
BRAIN__HNSW__EF_SEARCH=128
BRAIN__TRACING__ENABLED=true
BRAIN__TRACING__SAMPLE_RATIO=0.05
BRAIN__WORKERS__DECAY_INTERVAL_SEC=7200
```

Nested tables work the same way:

```bash
BRAIN__SERVER__TLS__ENABLED=true
BRAIN__SERVER__TLS__CERT=/etc/brain/tls/cert.pem
BRAIN__SERVER__TLS__KEY=/etc/brain/tls/key.pem
```

The leaf value is coerced: `"true"` / `"false"` → bool, valid
integers → `i64`, things with `.`/`e`/`E` → `f64`, everything
else → string. Byte-size strings stay strings and are parsed
later.

## Validation errors

Brain validates the merged config before binding any port. Common
errors:

| Error | Meaning |
|---|---|
| `config error: config file not found or unreadable at …` | `--config` path doesn't exist or isn't readable. |
| `config error: config TOML parse error at …` | Malformed TOML — unbalanced quotes, missing equals, etc. |
| `config error: config validation error at …` | Parsed cleanly but missed a required field or used an unknown key (`deny_unknown_fields`). |
| `config error: bad byte suffix: …` | Used a suffix that isn't `B/KiB/MiB/GiB/TiB/KB/MB/GB`. |
| `config error: byte value '...' is not a valid unsigned integer` | The numeric portion of a byte-size string didn't parse. |
| `config error: byte value overflows u64` | The byte-size string would exceed 2⁶⁴. |
| `config error: invalid config: …` | Cross-field invariant violation (e.g. shard count zero). |

## See also

- [`../guides/configure.md`](../guides/configure.md) — how-to with
  worked examples for common deployment shapes.
- [`../guides/tuning/`](../guides/tuning/) — when and how to move
  these values for specific workload problems.
- [`../../config/dev.toml`](../../config/dev.toml) — the
  development-mode default.
- [`../../config/docker.toml`](../../config/docker.toml) — the
  in-image production-shaped default.

**Spec:** §01/04 (system architecture — config), §06/02 (HNSW
parameters), §11/03 (worker intervals), §14/03 (tracing). Source:
`crates/brain-server/src/config/mod.rs`.
