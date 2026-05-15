# 08 — Debugging

Logs, metrics, runtime snapshots, backtraces.

## Log level

The server uses `tracing` with JSON output by default per
`config/dev.toml`. Override at runtime via env:

```bash
RUST_LOG=brain_server=debug cargo run --bin brain-server -- --config config/dev.toml
```

```bash
RUST_LOG=brain_storage=trace,brain_server=info \
  cargo run --bin brain-server -- --config config/dev.toml
```

```bash
BRAIN_LOG=info,brain_server::network=debug \
  cargo run --bin brain-server -- --config config/dev.toml
```

Filter precedence: `BRAIN_LOG` > `RUST_LOG` > `[logging] level`.
Valid levels: `error`, `warn`, `info`, `debug`, `trace`.

**Verify the filter took effect:**

```bash
curl -s http://127.0.0.1:9091/metrics | grep "^# HELP" | head -3
```

If `BRAIN_LOG=info,brain_server::network=debug` is set, you should
see `DEBUG` lines from `target = "brain_server::network::..."` in
the server log while serving requests.

## Reading structured logs

Log lines are newline-delimited JSON objects. Pipe through `jq`:

```bash
cargo run --bin brain-server -- --config config/dev.toml 2>&1 \
  | jq 'select(.level == "ERROR")'
```

```bash
cargo run ... 2>&1 \
  | jq 'select(.fields.shard != null) | {shard: .fields.shard, msg: .fields.message}'
```

```bash
cargo run ... 2>&1 \
  | jq 'select(.span.name == "brain.request")'
```

(The third example surfaces every per-request span from the Phase
12.3 OTel instrumentation.)

## Runtime debug snapshot

`debug-snapshot` gives a point-in-time view of one shard's worker
state without stopping the server:

```bash
just cli --output json debug-snapshot --shard 0 \
  | jq '.workers[] | select(.errors > 0)'
```

If any worker has reported errors, this surfaces them.

**Verify:**

A healthy 30-minute-old server should print nothing (all workers
have `errors == 0`).

## Prometheus scraping

The `/metrics` endpoint exposes Prometheus text-format output:

```bash
curl -s http://127.0.0.1:9091/metrics | head -40
```

Key metrics:

```
brain_up                       1 when accepting requests
brain_shards_total             configured shard count
brain_connections_active       in-flight client connections
brain_connections_total        total accepted since startup
brain_connections_closed_total{reason="bye|protocol_error|timeout|eof|fatal"}
brain_frame_send_total         outbound frames since startup
brain_frame_recv_total         inbound frames since startup
brain_request_total{op,status} per-op + per-status counter
brain_request_active{op}       per-op in-flight gauge
brain_request_duration_ms_*    per-op latency histogram
brain_worker_cycles_total      worker run count per worker per shard
brain_worker_errors_total      worker error count per worker per shard
brain_worker_last_run_unixtime unix timestamp of last worker cycle
brain_hnsw_node_count          active HNSW nodes
brain_hnsw_tombstone_count     tombstoned HNSW nodes
brain_hnsw_tombstone_ratio     tombstone / total
process_cpu_seconds_total      cumulative CPU time
process_memory_resident_bytes  resident set size
process_open_fds               open file descriptors
process_uptime_seconds         server uptime
```

Full taxonomy in
[`docs/guides/observability.md`](../guides/observability.md#2-metrics).

**Verify metric counts after an ENCODE:**

```bash
# baseline
curl -s :9091/metrics | grep 'brain_request_total{op="encode",status="success"}'

# run ENCODE via SDK (or example)
cargo run --example store_and_recall -p brain-sdk-rust

# after
curl -s :9091/metrics | grep 'brain_request_total{op="encode",status="success"}'
```

The second value should be higher than the first by exactly the
number of successful encodes.

## Backtrace on panic

The container sets `RUST_BACKTRACE=1` automatically. For full
backtrace:

```bash
RUST_BACKTRACE=full cargo run --bin brain-server -- --config config/dev.toml
```

The panic line + full stack appears on stderr.

## Per-crate test output

```bash
cargo test -p brain-storage --lib -- arena::tests::crc_mismatch_halts --nocapture
```

`--nocapture` shows `println!` / `eprintln!` output that's
suppressed by default. Without `--nocapture` you only see assertion
failures.

## Miri (unsafe memory safety)

`brain-storage` is the only crate allowed to use `unsafe`. Run the
unsafe blocks under Miri to check for UB. Syscall-bound paths
(mmap, pwritev2) are excluded by `#[cfg(miri)]`-gated tests; the
~47 pure-data tests run:

```bash
just miri
```

Failures here are real soundness bugs — surface immediately.

## OpenTelemetry traces

If `[tracing] enabled = true` and the OTLP collector is reachable,
every request emits a `brain.request` span:

```bash
curl -s http://localhost:4318/v1/traces -X POST -H 'Content-Type: application/json' -d '{}'
```

In a Jaeger / Tempo UI, search by service name (`brain-server`) and
sort by duration to find slow operations. See
[`docs/guides/observability.md` §4](../guides/observability.md#4-tracing-opentelemetry).

## Next

[`09-troubleshooting.md`](09-troubleshooting.md) — common issues
and how to resolve them.
