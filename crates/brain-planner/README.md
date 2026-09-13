# brain-planner

> Query planner and executor for Brain.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

Turns `brain-protocol`'s typed requests into a logical plan tree, costs each
plan against per-shard statistics, and executes it over the storage stack
(`brain-storage`, `brain-metadata`, `brain-index`, `brain-embed`). There is one
`ExecutionPlan` variant per cognitive operation. The `retrieval` module is the
fused read pipeline: it fans out the three always-wired retrievers (semantic /
lexical / graph), fuses ranks via RRF, applies the filter chain, and runs the
optional cross-encoder rerank stage. The executor is runtime-agnostic (Glommio
in production, `block_on` in tests); every plan type is `Send + Sync`.

## Key modules

| Module | Role |
|---|---|
| `plan` | Plan tree: `ExecutionPlan` + per-op plan structs (`EncodePlan`, `RecallPlan`, `PathPlan`, `ReasonPlan`, `ForgetPlan`) and step types. |
| `planner` | Per-op planners (`plan_encode`, `plan_recall`, `plan_path`, `plan_reason`, `plan_forget`). |
| `cost` | Cost model consulting `ShardStats`; enforces the cost budget. |
| `executor` | Runs plans: `execute_recall`/`execute_reason`/`execute_path` (+ streaming variants), writer handoff, result types. |
| `retrieval` | Fused read pipeline — fusion, filters, router, rerank stage. |
| `vsa` | Vector Symbolic Architecture: HRR bind/unbind via `rustfft`, codebook, analogy. |
| `config` / `context` / `stats` / `explain` / `error` | `PlannerConfig`, `PlannerContext`, `ShardStats`, `explain`, `PlanError`. |

## Where it fits

Depends on `brain-embed` (cue embedding), `brain-index` (ANN), `brain-metadata`
(filters/lookups), and `brain-rerank` (post-fusion rerank), plus `regex` for
query-router heuristics and `rustfft` for VSA. It is the engine `brain-ops`
drives for every read and write operation.

## Spec

- [`../../spec/13_retrievers/05_retrieval_query.md`](../../spec/13_retrievers/05_retrieval_query.md)
- [`../../spec/13_retrievers/00_purpose.md`](../../spec/13_retrievers/00_purpose.md)
- [`../../spec/05_operations/03_read_pipeline.md`](../../spec/05_operations/03_read_pipeline.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
