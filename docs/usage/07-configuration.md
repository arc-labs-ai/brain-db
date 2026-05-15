# 07 — Configuration

Full reference for `config/dev.toml`. The same schema applies to
production configs at any path passed via `--config`.

The authoritative spec is in `spec/01_system_architecture/`. For
production sizing recommendations, see
[`docs/guides/configure.md`](../guides/configure.md).

## [server]

```toml
[server]
listen_addr = "127.0.0.1:9090"   # data plane TCP (SDK clients)
metrics_addr = "127.0.0.1:9091"  # admin + metrics HTTP (brain CLI, Prometheus)
admin_addr = "127.0.0.1:9092"    # additional admin HTTP
```

**Verify:**

```bash
just cli --output json config get --key server.listen_addr
```

Should echo `"127.0.0.1:9090"`.

## [storage]

```toml
[storage]
data_dir = "./data"    # shard data root
shard_count = 4        # number of shards; each pins a CPU core
```

`shard_count` should typically equal the number of CPU cores on
your host. Each shard runs a single-threaded Glommio executor.

**Verify:**

```bash
ls data/
```

Should show `shard-0/`, `shard-1/`, …, `shard-{N-1}/` after the
first request.

## [shard]

```toml
[shard]
arena_capacity_bytes = "1GiB"      # per-shard mmap arena
wal_segment_size_bytes = "256MiB"  # rotate WAL at this size
wal_retention_segments = 4         # keep last N segments
```

`arena_capacity_bytes` × `shard_count` = total addressable memory
footprint. The arena is mmap'd; growing it costs nothing at startup
until the slots are written.

Size suffixes: `KiB`, `MiB`, `GiB`, `TiB` (binary), or bare bytes.

**Verify:**

```bash
ls -la data/shard-0/arena.bin
```

The file size matches `arena_capacity_bytes`.

## [hnsw]

```toml
[hnsw]
m = 16                  # max edges per node (spec §06/02)
ef_construction = 200   # candidate list during index build
ef_search = 64          # candidate list during recall
```

Spec §06/02 documents why these defaults; tuning trade-offs are:

- Higher `m` → better recall, more memory per node.
- Higher `ef_construction` → better recall, slower writes.
- Higher `ef_search` → better recall, slower reads.

**Verify:**

```bash
just cli --output json config get --key hnsw
```

```json
{"m":16,"ef_construction":200,"ef_search":64}
```

## [embedder]

```toml
[embedder]
model = "bge-small-en-v1.5"  # HuggingFace model id
cache_size = 10000           # in-memory embedding cache entries
batch_size = 32              # max texts per embedding batch
batch_window_ms = 5          # wait up to Nms to form a batch
```

The model is downloaded from HuggingFace on first ENCODE. The
local cache lives at `~/.cache/huggingface/hub/`. Allow a few
minutes for the first download.

To pre-download:

```bash
python3 -c "
from huggingface_hub import snapshot_download
snapshot_download('BAAI/bge-small-en-v1.5')
"
```

## [workers]

```toml
[workers]
decay_interval_sec                 = 3600   # 1 h
access_boost_interval_sec          = 600    # 10 m
consolidation_interval_sec         = 86400  # 24 h
hnsw_maintenance_interval_sec      = 1800   # 30 m
idempotency_cleanup_interval_sec   = 3600
slot_reclamation_interval_sec      = 86400
wal_retention_interval_sec         = 300
edge_scrub_interval_sec            = 86400
counter_reconciliation_interval_sec = 86400
statistics_update_interval_sec     = 3600
embedder_cache_eviction_interval_sec = 86400
snapshot_interval_sec              = 86400
```

A value of `0` disables the worker. Spec §11/03 documents what
each worker does.

**Verify:**

```bash
just cli worker list | head -5
```

After running for ≥ 10 minutes, you should see `cycles > 0` on the
high-frequency workers (`access_boost`, `wal_retention`).

## [logging]

```toml
[logging]
level  = "info"      # error | warn | info | debug | trace
output = "stdout"
format = "json"      # "json" or "compact"
```

`format = "json"` is recommended for production (ingestible by
Loki / Elastic / Splunk). The `BRAIN_LOG` and `RUST_LOG` env vars
override `level` (and accept the same per-target syntax as
`tracing-subscriber`'s `EnvFilter`).

## [tracing]

```toml
[tracing]
enabled       = false                  # opt-in OTel
sampler       = "always_off"           # always_on | always_off | ratio | parent_based
sample_ratio  = 0.01                   # used when sampler = "ratio"
endpoint      = ""                     # OTLP/HTTP, e.g. "http://otel-collector:4318/v1/traces"
service_name  = "brain-server"
```

When `enabled = false` (default), no spans are exported and the
substrate runs unchanged.

## [auth]

```toml
[auth]
mode = "none"    # dev: no auth
```

`mode = "token"` and `mode = "mtls"` are deferred to a follow-up
minor release. v1 only accepts `"none"`. Until token/mTLS land,
**don't expose brain-server to the public internet**.

## TLS

Optional fields under `[server]`:

```toml
[server]
listen_addr   = "0.0.0.0:8443"
tls_cert_file = "/etc/brain/tls/cert.pem"
tls_key_file  = "/etc/brain/tls/key.pem"
```

Omit both → plaintext on the data port.

## Environment variable overrides

Any field can be overridden with `BRAIN__SECTION__FIELD=value`
(double underscores separate nesting):

```bash
BRAIN__SERVER__LISTEN_ADDR=0.0.0.0:9090
BRAIN__STORAGE__SHARD_COUNT=8
BRAIN__SHARD__ARENA_CAPACITY_BYTES=2GiB
BRAIN__HNSW__EF_SEARCH=128
BRAIN__LOGGING__LEVEL=debug
BRAIN__TRACING__ENABLED=true
BRAIN__TRACING__ENDPOINT=http://localhost:4318/v1/traces
```

**Verify:**

```bash
BRAIN__HNSW__EF_SEARCH=128 \
  cargo run --bin brain-server -- --config config/dev.toml &
sleep 3
just cli --output json config get --key hnsw.ef_search
# → 128
```

## Validation

```bash
brain-server --config config/dev.toml --help
```

The server parses the config before printing help. Syntax errors,
missing required fields, or unrecognised keys all surface here as
`config error: ...` on stderr before the runtime spins up.

## Next

[`08-debugging.md`](08-debugging.md) — logs, metrics scraping,
debug-snapshot, backtraces.
