# brain-server

> The Brain cognitive substrate server.

The top crate of **[Brain](../../README.md)** — a memory database for AI agents.
It depends on every other `brain-*` crate and is a library plus a thin
`brain-server` binary: everything lives in the library, and the binary is only
an entry point, so integration tests import the crate the normal way. Not
published to crates.io. Apache-2.0.

## What it does

Wires the whole system together. A Tokio connection layer accepts TCP/TLS and
routes each accepted stream through a per-connection task and the frame
dispatcher; per-shard work runs on Glommio (thread-per-core, io_uring) and owns
the storage, index, embed, ops, rerank, and worker stacks. It also hosts the
operator admin HTTP listener (built on `brain-http`), Prometheus-style metrics
exposition, OpenTelemetry tracing, TOML config loading, and the admin
maintenance routes — among them `POST /v1/extract/backfill`, which re-enqueues
existing memories by walking the redb tables inside the shard executor.

The binary itself takes only `--config <path>`, `--version`, and `--help`;
everything operational is config or an admin route.

**Linux-only.** The shard runtime and the `brain-*` crates that pull io_uring,
mmap, candle, and redb live behind a `cfg(target_os = "linux")` gate. The server
does not run on macOS or Windows — use the dev container (see the project
Dockerfile / `just docker`) to build and run it. On other hosts only the
host-independent modules compile: `config`, `bootstrap`, `machine`, and
`storage_layout`.

## Key modules

- `network/` — Tokio accept loop, connection handling, dispatch, auth, routing,
  subscribe streams, and the connection gate.
- `shard/` — Glommio shard executor, adapters, LLM setup, and tantivy recovery.
- `admin/` — admin HTTP router and handlers (on `brain-http`).
- `bootstrap/` — process startup: logging, tracing, TLS, graceful shutdown.
- `config/` — TOML config schema and loading.
- `metrics/` — counters, gauges, histograms, and exposition.
- `llm/` — summarizer factory plus the two feature-gated adapters the
  consolidation worker can be pointed at: `ResolvedSummarizer`
  (`summarizer-llm`, billed to the namespace it consolidates) and
  `OllamaSummarizer` (`summarizer-ollama`), with a bridge Tokio runtime so
  their HTTP never runs inside a shard's Glommio executor.
- `machine` — how much memory this process may use, asked of the host rather
  than typed by an operator.
- `storage_layout` — what a data directory was created with; `shard_count` is a
  property of the data, not a tuning knob.

## Where it fits

Top of the stack: depends on `brain-core` and `brain-crypto` everywhere, plus
(Linux-gated) `brain-protocol`, `brain-storage`, `brain-metadata`,
`brain-extractors`, `brain-llm`, `brain-index`, `brain-embed`, `brain-planner`,
`brain-ops`, `brain-rerank`, `brain-workers`, and `brain-http`. Tokio drives the
connection layer; Glommio drives the shards.

## Spec

- [`../../spec/01_architecture/04_layers.md`](../../spec/01_architecture/04_layers.md)
- [`../../spec/15_background_workers/01_worker_architecture.md`](../../spec/15_background_workers/01_worker_architecture.md)
- [`../../spec/17_observability/04_admin_ops.md`](../../spec/17_observability/04_admin_ops.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
