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

- [ ] All sub-tasks complete.
- [ ] `just verify` green.
- [ ] Each operation type (encode/recall/plan/reason/forget) has at least one end-to-end planner test.
- [ ] Tag `phase-6-complete`.
