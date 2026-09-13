# brain-server

> The Brain cognitive substrate server.

The binary crate of **[Brain](../../README.md)** — a memory database for AI
agents. The top of the workspace: it depends on every other `brain-*` crate and
produces the `brain-server` executable. Not published to crates.io. Apache-2.0.

## What it does

The server binary that wires the whole system together. A Tokio connection layer
accepts TCP/TLS and routes each accepted stream through a per-connection task and
the frame dispatcher; per-shard work runs on Glommio (thread-per-core, io_uring)
and owns the storage, index, embed, ops, rerank, and worker stacks. It also hosts
the operator admin HTTP listener (built on `brain-http`), Prometheus-style metrics
exposition, OpenTelemetry tracing, TOML config loading, and an `extract
--backfill` maintenance path that walks the redb tables inside the shard
executor.

**Linux-only.** The shard runtime and the `brain-*` crates that pull io_uring,
mmap, candle, and redb live behind a `cfg(target_os = "linux")` gate. The binary
does not build natively on macOS or Windows — use the dev container (see the
project Dockerfile / `just docker`) to build and run it. Only config and routing
modules compile on other hosts.

## Key modules

- `network/` — Tokio accept loop, connection handling, dispatch, auth, routing,
  subscribe streams, and the connection gate.
- `shard/` — Glommio shard executor, adapters, LLM setup, and tantivy recovery.
- `admin/` — admin HTTP router and handlers (on `brain-http`).
- `bootstrap/` — process startup: logging, tracing, TLS, graceful shutdown.
- `config/` — TOML config schema and loading.
- `metrics/` — counters, gauges, histograms, and exposition.
- `llm/` — per-shard LLM bridge/factory plus optional OpenAI/Ollama summarizers.

## Where it fits

Top of the stack: depends on `brain-core` everywhere plus (Linux-gated)
`brain-protocol`, `brain-storage`, `brain-metadata`, `brain-extractors`,
`brain-llm`, `brain-index`, `brain-embed`, `brain-planner`, `brain-ops`,
`brain-rerank`, `brain-workers`, and `brain-http`. Tokio drives the connection
layer; Glommio drives the shards.

## Spec

- [`../../spec/01_architecture/04_layers.md`](../../spec/01_architecture/04_layers.md)
- [`../../spec/15_background_workers/01_worker_architecture.md`](../../spec/15_background_workers/01_worker_architecture.md)
- [`../../spec/17_observability/04_admin_ops.md`](../../spec/17_observability/04_admin_ops.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
