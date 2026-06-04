# brain-workers

> Background workers for Brain (decay, consolidation, GC, etc.).

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

Background-worker infrastructure plus the concrete per-shard maintenance
workers. It defines the `Worker` trait, scheduler, per-worker config, context
(handle bag + shutdown signal), and metrics, then ships the workers that keep
each shard healthy: salience decay, consolidation, HNSW maintenance,
idempotency cleanup, slot reclamation, WAL retention, snapshotting, statistics,
plus the typed-graph workers (auto-edge, temporal-edge, causal-edge,
statement-embed, extractor, entity/ambiguity resolution, confidence sweep,
forget cascade, schema migration, …). The scheduler runs on Glommio with a
per-worker run-now wakeup channel.

## Key modules

| Module | Role |
|---|---|
| `worker` | `Worker` trait + `drive_batch` cycle helper. |
| `scheduler` | `WorkerScheduler` + `WorkerHandle` (Glommio per-shard scheduling). |
| `config` | `WorkerConfig`, `WorkerKind`, per-worker knobs/defaults. |
| `context` / `metrics` / `error` | `WorkerContext`, `WorkerMetrics`, `WorkerError`. |
| `summarizer` | `Summarizer` trait + `DisabledSummarizer`. |
| `workers` | The concrete workers: `decay`, `consolidation`, `auto_edge`, `temporal_edge`, `causal_edge`, `statement_embed`, `extractor`, `hnsw_maint`, `slot_reclaim`, `wal_retention`, `snapshot`, `idempotency_cleanup`, `ambiguity_resolver`, `confidence_sweep`, `forget_cascade`, `schema_migration`, `entity_gc`, and more. |

## Where it fits

Depends on `brain-ops` (write path it drives), `brain-planner`, `brain-index`,
`brain-metadata`, `brain-embed`, `brain-extractors`, and `brain-llm`, with
`glommio` + `flume` for scheduling and wakeups. `brain-server` spawns these
workers per shard.

## Spec

- [`../../spec/15_background_workers/00_purpose.md`](../../spec/15_background_workers/00_purpose.md)
- [`../../spec/15_background_workers/01_worker_architecture.md`](../../spec/15_background_workers/01_worker_architecture.md)
- [`../../spec/15_background_workers/02_memory_maintenance.md`](../../spec/15_background_workers/02_memory_maintenance.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
