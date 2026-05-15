# Configure Brain

Brain reads a TOML config at the path passed via `--config`
(default `config/dev.toml`). Every section is required except
`[workers]`, `[logging]`, `[tracing]`, `[summarizer]`, and `[auth]`.

## Minimal production config

```toml
[server]
listen_addr  = "0.0.0.0:8080"      # data plane (clients + SDK)
metrics_addr = "0.0.0.0:9091"      # /metrics, /healthz, /v1/*
admin_addr   = "127.0.0.1:9090"    # admin CLI surface (loopback)

[storage]
data_dir    = "/var/lib/brain/data"
shard_count = 16

[shard]
arena_capacity_bytes    = "10GiB"   # ≈ 1.6 KB × 6.6M slots per shard
wal_segment_size_bytes  = "64MiB"
wal_retention_segments  = 64        # 4 GiB WAL retention per shard

[hnsw]
m              = 16
ef_construction = 200
ef_search      = 64

[embedder]
model           = "bge-small-en-v1.5"
cache_size      = 10_000
batch_size      = 32
batch_window_ms = 5

[auth]
mode = "none"                       # see "Auth modes" below

[logging]
level  = "info"
output = "stdout"
format = "json"                     # production — ingestible by Loki / Elastic

[tracing]
enabled       = true                # OTel OTLP/HTTP
sampler       = "ratio"
sample_ratio  = 0.01                # 1 %
endpoint      = "http://otel-collector:4318/v1/traces"
service_name  = "brain-server"
```

## Sizing reference

| Workload | Shard count | Arena per shard | RAM per host |
|---|---|---|---|
| Dev / smoke | 1 | 1 GiB | 4 GiB |
| Single team (~100K memories) | 1 | 4 GiB | 16 GiB |
| Org (~1M memories) | 4-8 | 4 GiB | 32 GiB |
| Production at scale (~10M+) | 16+ | 10 GiB | 64+ GiB |

Spec §16/04 §3-§4 documents the per-shard memory budget
(~500-1000 MB at 1M memories). Plan for 1.5-2× headroom.

## Auth modes

`[auth] mode` accepts:

- `none` — dev / single-tenant private network only. No
  authentication; any client that can reach the data port can
  read/write everything.
- `token` *(deferred — Phase 14+ planned)* — bearer token via the
  HELLO/AUTH handshake.
- `mtls` *(deferred — Phase 14+ planned)* — TLS client certs.

Until token / mTLS land, brain-server's network policy is
**"don't expose it to the public internet"**. Bind to private
interfaces; use a reverse proxy with auth in front for any non-
trivial deployment.

## TLS termination

Brain supports terminating TLS directly on the data port:

```toml
[server]
listen_addr     = "0.0.0.0:8443"
tls_cert_file   = "/etc/brain/tls/cert.pem"
tls_key_file    = "/etc/brain/tls/key.pem"
```

The cert / key are loaded once at startup. For rotation, restart
the substrate after replacing the files.

For deployments using an L7 proxy (Envoy / nginx) for TLS, omit
the cert/key fields and bind to loopback.

## Background workers

Spec §11 ships 12 background workers. Their intervals default
sensibly; override only when needed:

```toml
[workers]
decay_interval_sec                 = 3600   # 1h
access_boost_interval_sec          = 600    # 10m
consolidation_interval_sec         = 86400  # 24h
hnsw_maintenance_interval_sec      = 1800   # 30m
idempotency_cleanup_interval_sec   = 3600   # 1h
slot_reclamation_interval_sec      = 86400  # 24h
wal_retention_interval_sec         = 300    # 5m
edge_scrub_interval_sec            = 86400  # 24h
counter_reconciliation_interval_sec = 86400
statistics_update_interval_sec     = 3600
embedder_cache_eviction_interval_sec = 86400
snapshot_interval_sec              = 86400
```

A value of `0` disables the worker. Spec §11/03 documents what
each worker does.

## Env overrides

Every config field can be overridden via env. The form is
`BRAIN__SECTION__FIELD=value` (double underscore separates
nesting):

```bash
BRAIN__SERVER__LISTEN_ADDR=0.0.0.0:8080 \
BRAIN__STORAGE__SHARD_COUNT=8 \
BRAIN__SHARD__ARENA_CAPACITY_BYTES=2GiB \
brain-server --config /etc/brain/config.toml
```

Use this for per-environment differences (dev / staging / prod)
without forking the TOML.

## Validating

The substrate validates the config at startup; syntax errors and
missing required fields surface as `config error: ...` on stderr
before the runtime spins up. See [RB-1](../runbooks/substrate-down.md)
for the "won't start" runbook.
