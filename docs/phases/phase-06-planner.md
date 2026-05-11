# Phase 6 — Query Planner & Executor

## Goal

A logical plan tree, cost model, and pull-based executor that drives the lower layers. After this phase, a `Recall` request transforms into a plan: `EmbedCue → IndexSearch(filtered) → MetadataFetch → Score → Sort → Trim`.

## Prerequisites

- [x] Phase 5 complete (`brain-embed` exists).
- `brain-storage`, `brain-metadata`, `brain-index` are usable handles.

## Reading list

1. [`spec/08_query_planner/00_purpose.md`](../../spec/08_query_planner/00_purpose.md)
2. [`spec/08_query_planner/01_planner_overview.md`](../../spec/08_query_planner/01_planner_overview.md)
3. [`spec/08_query_planner/02_request_lifecycle.md`](../../spec/08_query_planner/02_request_lifecycle.md)
4. [`spec/08_query_planner/03_recall_planning.md`](../../spec/08_query_planner/03_recall_planning.md)
5. [`spec/08_query_planner/04_encode_planning.md`](../../spec/08_query_planner/04_encode_planning.md)
6. [`spec/08_query_planner/05_plan_reason_planning.md`](../../spec/08_query_planner/05_plan_reason_planning.md)
7. [`spec/08_query_planner/06_forget_planning.md`](../../spec/08_query_planner/06_forget_planning.md)
8. [`spec/08_query_planner/07_cost_estimation.md`](../../spec/08_query_planner/07_cost_estimation.md)
9. [`spec/08_query_planner/08_executor.md`](../../spec/08_query_planner/08_executor.md)

## Outputs

- `crates/brain-planner` exports `Plan`, `PlanNode`, `Executor`, `Context` (the bag of handles passed down).
- Tag: `phase-6-complete`.

## Sub-tasks

### Task 6.1 — `PlanNode` enum
**Reads:** `spec/08_query_planner/01_planner_overview.md`
**Writes:** `crates/brain-planner/src/plan.rs`
**What to build:**
- Each operator: `EmbedText`, `IndexSearch`, `MetadataFetch`, `EdgeTraverse`, `FilterByAgent`, `Score`, `Sort`, `Trim`, `WalAppend`, `ArenaWrite`, etc.
- Each variant carries its parameters.

### Task 6.2 — Cost model
**Reads:** `spec/08_query_planner/07_cost_estimation.md`
**Writes:** `crates/brain-planner/src/cost.rs`
**Done when:** Per-node cost = f(estimated cardinality, op cost coefficient). Total plan cost is sum of nodes. Tested with known shapes.

### Task 6.3 — Recall planner
**Reads:** `spec/08_query_planner/03_recall_planning.md`
**Writes:** `crates/brain-planner/src/recall.rs`
**Done when:** Recall request → plan tree per spec. Supports: cue text, filters (agent, context, kind, salience, time), K, with/without text body.

### Task 6.4 — Encode planner
**Reads:** `spec/08_query_planner/04_encode_planning.md`
**Writes:** `crates/brain-planner/src/encode.rs`
**Done when:** Encode request → plan: Embed → AllocSlot → ArenaWrite → MetadataWrite → IndexInsert → WalAppend (with WAL-before-ack semantics).

### Task 6.5 — Plan and Reason planners
**Reads:** `spec/08_query_planner/05_plan_reason_planning.md`
**Writes:** `crates/brain-planner/src/plan_reason.rs`
**Done when:** Both queries become traversal plans with depth bounds and edge-kind filters.

### Task 6.6 — Forget planner
**Reads:** `spec/08_query_planner/06_forget_planning.md`
**Writes:** `crates/brain-planner/src/forget.rs`
**Done when:** Soft and hard forget plans differ as spec'd; force_reclaim flag respected.

### Task 6.7 — `Executor`
**Reads:** `spec/08_query_planner/08_executor.md`
**Writes:** `crates/brain-planner/src/executor.rs`
**What to build:**
- Pull-based iterator model.
- Each `PlanNode` has `execute(self, ctx: &Context) -> impl Iterator<Item = Row>` (or async equivalent).
- `Context` carries `&Wal`, `&Arena`, `&MetadataDb`, `&HnswIndex`, `&Embedder`.
**Done when:** Recall plan executes end-to-end with faked storage; results match expected ordering.

### Task 6.8 — Plan inspection (debug)
**Reads:** `spec/08_query_planner/01_planner_overview.md`
**Writes:** extend `plan.rs`
**What to build:** `impl Debug for Plan` with a tree pretty-printer (similar to `EXPLAIN` in SQL).
**Done when:** Plans round-trip through `Debug` readably; useful for diagnostics.

## Phase exit checklist

- [x] All sub-tasks complete.
- [x] `cargo test -p brain-planner` green (101 tests passing in the Linux dev container).
- [x] Each operation type has at least one end-to-end planner test (recall/encode/forget end-to-end via the executor; PLAN + REASON planner shape tests; the dispatch smoke test exercises three ops through one fixture).
- [x] Tag `phase-6-complete`.

Phase 6 ships **single-shard, single-memory** planning + execution. Cross-shard fan-out and bulk / filter targets need a wire bump and land later (Phase 12 sharding for cross-shard, future wire schema for bulk / filter).

PLAN and REASON ship the **planner side only** — `plan_path` and `plan_reason` build full plans with depth bounds, edge-kind filters, and cost estimates, but `execute(Plan|Reason)` returns `ExecError::Unsupported("…— Phase 7")`. Bidirectional-BFS edge traversal needs the cognitive-ops scaffolding that lands with Phase 7 alongside `LINK` / `UNLINK`.

The `WriterHandle` trait introduced in 6.4 is the design slot the real channel-fed group-commit writer (spec §08/08 §10) plugs into in Phase 8 / Phase 9. Phase 6 ships test-only `FakeWriterHandle` impls that drive the test `MetadataDb` + `SharedHnsw` synchronously without WAL — enough to exercise the interface but not the durability story.

The 6.8 plan inspection ships as `Display` (not `Debug`) — the derive-generated `Debug` is preserved for test panic messages. Phase 9's `ADMIN_EXPLAIN_PLAN` opcode just wraps `format!("{plan}")`.
