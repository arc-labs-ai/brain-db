# brain-ops

> Cognitive operations: encode, recall, plan, reason, forget.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

The one write path plus the five cognitive primitives (encode / recall / plan /
reason / forget), LINK / UNLINK, and transactions. It wires the planner,
storage, metadata, embedder, and index together; idempotency lives at this
layer. A single async `dispatch()` entry exhaustively matches over
`RequestBody` and routes to per-opcode handlers. Writes flow through one
`Write { phases }` model: handlers build phases, the `apply` layer dispatches
each phase variant per table, and the `writer`/`submit` path commits durably.
The `index` module hosts the retriever-feeding indexers (semantic, graph, text).

## Key modules

| Module | Role |
|---|---|
| `dispatch` | Top-level async `dispatch()` + `RequestCaller`/`DispatchOutcome`. |
| `handlers` | Per-opcode handlers: `encode`, `recall`, `plan`, `reason`, `forget`, `link`, `query`, `entity`, `statement`, `relation`, `schema`, `subscribe`, `txn`, … |
| `apply` | Per-table apply: `memory`, `entity`, `statement`, `relation`, `edge`, `schema`, `reclaim`. |
| `write` | The `Write` / `Phase` model + `WriteAck`, ids, transaction shapes. |
| `writer` | `submit` commit path, WAL mapping/sink, `extractor_writes`, edge/extractor/forget-cascade enqueues. |
| `index` | Semantic / graph retrievers + tantivy text indexer drain task. |
| `state` | Access buffer, idempotency, ack codec, txn lens. |
| `context` / `error` / `metrics` | `OpsContext`, `OpError`/`ErrorCode`, worker/writer metrics snapshots. |

## Where it fits

Depends on `brain-planner` (execution), `brain-storage` (WAL/arena),
`brain-metadata`, `brain-embed`, `brain-index`, `brain-rerank`, and
`brain-extractors`, with `tantivy` + `flume` + `glommio` (Linux) for the
per-shard text-indexer drain. It is the operation layer `brain-server` calls per
request and `brain-workers` builds on.

## Spec

- [`../../spec/05_operations/00_purpose.md`](../../spec/05_operations/00_purpose.md)
- [`../../spec/05_operations/02_write_pipeline.md`](../../spec/05_operations/02_write_pipeline.md)
- [`../../spec/05_operations/03_read_pipeline.md`](../../spec/05_operations/03_read_pipeline.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
