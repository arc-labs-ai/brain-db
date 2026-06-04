# brain-core

> Shared types for the Brain cognitive substrate.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

Defines the foundational value types shared across the entire workspace: the ID
family (`MemoryId`, `EntityId`, `StatementId`, `RelationId`, slot index/version,
…), the node types (`Memory`, `Entity`, `Statement`, `Relation`) and their kinds,
the typed-graph edge types, the stable `Error` taxonomy, the entity-resolution
primitives (resolver, confidence aggregation, trigram similarity), migration
descriptors, and background-worker state. Everything here is a pure value type —
no I/O, no async, no runtime dependency (`#![forbid(unsafe_code)]`).

## Key modules

| Module | Purpose |
|---|---|
| `ids` | All Brain IDs and slot index/version types, including `MemoryId` slot-version encoding. |
| `nodes` | Core record types: `Memory`/`MemoryKind`/`Salience`, `Entity`, `Statement` + evidence, `Relation`, predicates. |
| `edges` | Typed-graph edge model: `Edge`, `EdgeKind`, `NodeRef`, and their refs. |
| `resolution` | Entity resolver, confidence aggregation, and trigram (Jaccard) similarity. |
| `error` | The stable `Error` / `Result` taxonomy used workspace-wide. |
| `migration` | Migration plan/item/summary descriptors. |
| `worker_state` | Background-worker bookkeeping: backfill ranges, progress, priority. |

## Where it fits

Depends only on small leaf crates (`thiserror`, `uuid`, `serde`, `smallvec`,
`tracing`). It is the bottom of the dependency graph — every other `brain-*`
crate depends on it, and it depends on no other `brain-*` crate.

## Spec

- Data model: [`../../spec/02_data_model/00_purpose.md`](../../spec/02_data_model/00_purpose.md)
- Statements & predicates: [`../../spec/02_data_model/07_statement.md`](../../spec/02_data_model/07_statement.md)
- Edges: [`../../spec/02_data_model/05_edges.md`](../../spec/02_data_model/05_edges.md)
- Entity lifecycle & resolution: [`../../spec/02_data_model/06_entity_lifecycle.md`](../../spec/02_data_model/06_entity_lifecycle.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
