# Phase 6 — Query Planner & Executor

Orientation plan. Surfaces the spec-grounded decisions before sub-task 6.1 lands. Implementation lives in `crates/brain-planner/` (currently a 25-line stub).

## 0. Goal

Convert a typed `Request` from `brain-protocol` (Phase 1) into an `ExecutionPlan` value, then run it through an executor that orchestrates `brain-embed` (Phase 5), `brain-index` (Phase 4), `brain-metadata` (Phase 3), and `brain-storage` (Phase 2). After Phase 6 lands:

- Every cognitive operation (ENCODE, RECALL, PLAN, REASON, FORGET) has a planner function and an executor path.
- A `Recall` request runs end-to-end against the real stack with results matching expected ordering.
- Plans are inspectable via `Debug` (the basis for a future `ADMIN_EXPLAIN_PLAN`).

Tag: `phase-6-complete`.

## 1. Spec grounding (13 files)

| Spec § | Topic | Sub-task anchor |
|---|---|---|
| 00 Purpose | bridges wire ↔ storage; planner is pure, executor is async | read first |
| **01 Overview** | **plan = immutable value; sync planning, async exec; rules-not-search; <50 µs** | 6.1 |
| 02 Lifecycle | 9-phase request flow (frame → validate → plan → execute → respond) | informs 6.7 |
| **03 RECALL** | **agent-scoped routing; pick ef from K + selectivity; over_factor; PostFilter** | 6.3 |
| **04 ENCODE** | **idempotency → embed → context → alloc → WAL append → apply → edges → ack** | 6.4 |
| 05 PLAN + REASON | edge-traversal plans; BFS depth bounds; path scoring | 6.5 |
| 06 FORGET | soft vs hard; per-shard tombstone; cascade options | 6.6 |
| **07 Cost** | **table of per-op costs; rules-based ef pick; over-budget detection** | 6.2 |
| **08 Executor** | **async; sequential-vs-parallel steps; cooperative yields; writer-task channel** | 6.7 |
| 09 Concurrency | per-shard executor task; cross-task discipline | informs 6.7 |
| 10 Failure | structured errors; cancellation; backpressure | inline in 6.7 |

## 2. Crate-level structure

```
crates/brain-planner/
├── Cargo.toml          (+ brain-protocol, brain-embed, brain-index,
│                         brain-metadata, brain-storage; tracing)
└── src/
    ├── lib.rs          (re-exports + module wiring)
    ├── error.rs        (PlanError, ExecError variants)
    ├── plan.rs         (PlanNode enum + per-request Plan structs +
    │                     Debug pretty-printer for 6.8)
    ├── cost.rs         (cost model: per-op coefficients + estimators)
    ├── shard_stats.rs  (ShardStats: counts, tombstone ratio, used by
    │                     ef-picker; fed by metadata for now)
    ├── context.rs      (PlannerContext + ExecutorContext: bags of
    │                     handles passed to plan_*/execute_*)
    ├── recall.rs       (plan_recall + execute_recall)
    ├── encode.rs       (plan_encode + execute_encode)
    ├── plan_reason.rs  (plan_plan / plan_reason + executors)
    ├── forget.rs       (plan_forget + execute_forget)
    ├── executor.rs     (top-level Executor::execute dispatch)
    └── inspect.rs      (impl Debug for Plan: tree pretty-printer)
```

Plus integration tests in `tests/`:
- `tests/recall_end_to_end.rs`
- `tests/encode_end_to_end.rs`
- `tests/explain.rs` (Debug round-trip)

## 3. Cross-crate boundaries

`brain-planner` is the **integration crate** for Phases 2–5. It depends on:

- **`brain-protocol`** — `RequestBody`, `EncodeRequest`, `RecallRequest`, etc. (Phase 1 already shipped these.)
- **`brain-embed`** — the `Dispatcher` trait (Phase 5.4); planner holds `Arc<dyn Dispatcher>` so any embedder implementation works.
- **`brain-index`** — `SharedHnsw` + `Writer` (Phase 4.8) for ANN search / insert.
- **`brain-metadata`** — `MetadataDb`, `MetadataSink` (Phase 3); read transactions for lookups, sink for writes.
- **`brain-storage`** — `Arena` (slot read/write), `Wal` (append-fsync-ack).
- **`brain-core`** — `MemoryId`, `AgentId`, `ContextId`, `RequestId`, `Error`, etc.

It is **not** consumed by anything yet (Phase 7 will build cognitive ops on top, Phase 9 wires the server above it).

The crate is the first place where all storage components meet. Until now they've been independently testable; Phase 6 forces them to compose.

## 4. Design decisions to surface before 6.1

### 4.1 Sync planner / async executor split

Spec §01 §5 and §08 §1 are explicit: **planning is synchronous and pure**; **execution is async**. The split has two concrete implications:

- `Planner::plan(...)` returns `Result<ExecutionPlan, PlanError>` synchronously — no `.await`, no I/O.
- `Executor::execute(plan)` is `async fn` returning `Result<Response, ExecError>`.

Two error types because the failure modes are disjoint: a `PlanError` is "your request is malformed or pathologically expensive"; an `ExecError` is "the underlying storage/embed/index failed mid-flight".

### 4.2 Runtime: Tokio vs Glommio?

CLAUDE.md says the server uses **Glommio per shard** + Tokio for the connection layer. But `brain-planner` is a library, not a binary — it doesn't pick a runtime; it returns futures.

**Decision:** make `Executor::execute` runtime-agnostic — return `impl Future<Output = ...>` and use only `std::future` + `futures` crate combinators (`try_join!`, etc.). Phase 9's server then drives the futures on Glommio. This keeps tests easy (we can `tokio::test` against the planner) without picking a runtime in the crate.

Risk: parts of the spec (e.g. "writer task is per-shard; only one") imply a runtime-specific channel pattern. For Phase 6 we model the writer-task interaction as a *trait* (`WriterHandle::submit(op) -> impl Future<Output = WriteAck>`) and let Phase 9 wire the concrete implementation. The Phase 6 executor tests use an in-process synchronous fake.

### 4.3 `PlanNode` enum: single or per-request shapes?

Two competing shapes in the spec:

- **§01 §2** shows `enum ExecutionPlan { Encode(EncodePlan), Recall(RecallPlan), … }` — one struct per request kind.
- **Phase doc 6.1** says "`PlanNode` enum with operators: EmbedText, IndexSearch, MetadataFetch, …" — a generic operator tree.

These describe different layers. The phase doc is closer to a SQL-style logical plan; the spec is closer to a fixed shape per request. Spec §01 §6 + §12 says "fixed rules and lookup tables, not search" — so we don't need a general optimizer over operators.

**Decision:** follow the spec. `ExecutionPlan` is an enum with one variant per request kind. Each variant is a struct of *steps* (per the spec's `RecallPlan { embedding, shards, merge, response }` shape). The phase-doc `PlanNode`-tree idea is preserved only inside the `Debug` pretty-printer (6.8), which walks the steps and emits a tree-like rendering for inspection.

This is a meaningful deviation from the phase doc. Document it.

### 4.4 What about the writer task?

Spec §08 §10 describes a per-shard writer task that batches writes, group-commits to WAL, and acks executors via a channel. This is the spec's answer to "single-writer-per-shard" (CLAUDE.md §5 invariant 2).

For Phase 6 we **don't** build the writer task itself — that's a Phase 8 (background workers) or Phase 9 (server wiring) concern. Phase 6 ships a `WriterHandle` trait the executor calls into:

```rust
#[async_trait]
trait WriterHandle: Send + Sync {
    async fn submit_encode(&self, op: EncodeOp) -> Result<WriteAck, ExecError>;
    async fn submit_forget(&self, op: ForgetOp) -> Result<WriteAck, ExecError>;
    async fn submit_link(&self, op: LinkOp) -> Result<WriteAck, ExecError>;
}
```

Tests use an in-process implementation that calls `Wal::append` + `Arena::write` + `MetadataSink::apply` synchronously (no batching, no channel). The server in Phase 9 replaces it with the real channel-fed task.

This keeps Phase 6 self-contained and unblocks 6.7's end-to-end test.

### 4.5 The fast path

Spec §01 §7 and §07 §9 describe a fast path that bypasses full planning for simple requests. For 6.3 (Recall) we implement the simple-case fast path inline (single-shard, K ≤ 20, no filter, eventual consistency); other shapes go through full planning. The fast path saves ~50 µs.

For 6.4 (Encode) the spec only describes "single ENCODE on a healthy shard" as fast-path-able; we collapse it into the regular planner since encodes don't have multiple shapes — every encode is the same sequence.

### 4.6 ShardStats: where does it come from?

Spec §07 §10 says `ShardStats { memory_count, tombstone_count, tombstone_ratio, last_rebuild_at, avg_search_latency_ms, avg_encode_latency_ms }` is updated by the observability layer (Phase 11+).

For Phase 6 we need *some* `ShardStats` so `pick_ef` and cost estimation work. **Decision:** ship a `ShardStats` struct backed by what we *can* compute now:

- `memory_count`: from `brain-metadata`'s `memories` table count (or a cached `next_lsn` proxy).
- `tombstone_count` + `tombstone_ratio`: from the index's tombstone bitmap (Phase 4) and the count.
- `avg_*_latency`: zero for now; populated by Phase 11.

A `ShardStatsProvider` trait lets the executor inject these. Tests pass a fixed `ShardStats { memory_count: 1000, tombstone_ratio: 0.0, ... }`.

### 4.7 Cross-shard: deferred?

Spec §03 §8 covers cross-shard fan-out; spec §03 §2 says "for most agents, this returns one shard". Sharding itself is Phase 12.

**Decision:** Phase 6 ships **single-shard** planning + execution only. The `RecallPlan` carries `shards: Vec<ShardSearchStep>` exactly as the spec writes, but for v1 the vec always has length 1. Cross-shard fan-out lands when Phase 12 (sharding) lands.

This is forward-compatible — the spec's plan structure already accommodates fan-out, we just don't exercise the branch.

### 4.8 Idempotency: where does the lookup happen?

Spec §04 §4: idempotency check is **Phase 1** of encode. It hits `brain-metadata`'s `idempotency` table (Phase 3.5). The planner builds an `IdempotencyCheckStep`; the executor runs it.

**Important detail:** if the idempotency cache hits, the executor returns the cached response and **skips the rest of the plan**. We model this as an `Either`-shaped return from `execute_idempotency_check`:

```rust
enum IdempotencyResult {
    CachedResponse(Response),
    Proceed,
}
```

The executor's `execute_encode` branches on this and either short-circuits or runs the rest.

### 4.9 Async-trait crate? Or hand-rolled futures?

`async fn` in traits is stable in Rust 1.75+; our MSRV is 1.95 (Phase 3.x bump). We use bare `async fn` in traits — no `async-trait` crate needed.

Caveat: bare `async fn` in traits has limitations around `dyn Trait`. For traits we want as trait objects (`Box<dyn WriterHandle>`), we either:

- Use `async-trait` (adds a workspace dep), or
- Return `Pin<Box<dyn Future<...>>>` explicitly.

**Decision:** use bare `async fn` in traits where possible; for the `WriterHandle` (which we *will* box for runtime injection), return `Pin<Box<dyn Future>>`. Avoids the dep; the boxing is at one point in the codebase.

### 4.10 The `Context` parameter

Phase doc says "`Context` carries `&Wal`, `&Arena`, `&MetadataDb`, `&HnswIndex`, `&Embedder`."

Spec §08 §7 says "Handles are cheap to clone (Arc-based). Each executor task gets its own handles; no contention."

**Decision:** `ExecutorContext` is a struct of `Arc`-wrapped handles:

```rust
pub struct ExecutorContext {
    pub embedder: Arc<dyn Dispatcher>,
    pub index: SharedHnsw,             // Phase 4.8 — already Arc<RwLock<…>>
    pub metadata: Arc<MetadataDb>,
    pub arena: Arc<Arena>,             // brain-storage handle
    pub writer: Arc<dyn WriterHandle>, // per spec §08 §10
    pub stats: Arc<dyn ShardStatsProvider>,
}
```

`PlannerContext` is smaller — no async, no runtime handles — just config + stats snapshot:

```rust
pub struct PlannerContext {
    pub config: PlannerConfig,
    pub stats: ShardStats,
}
```

The split keeps the planner pure (per spec §01 §9).

### 4.11 Test strategy

Three layers:

1. **Pure planner tests** (per planner module): given a `Request` + `PlannerContext`, the planned `ExecutionPlan` matches expectations. No I/O, no async; pure data assertions. ~10 µs per test.

2. **Executor tests with fakes**: a fake `WriterHandle` + fake embedder (returns deterministic vectors per text) + real in-memory `MetadataDb` (via `tempfile`) + real `HnswIndex` + real `Arena` (via `tempdir`). End-to-end Recall and Encode runs; results compared to known-good outputs. Heavier — ~100 ms per test.

3. **Inspection / Debug tests**: format a plan, assert the tree-pretty-printed string contains the expected operators.

Tests gated on `BRAIN_EMBED_MODEL_DIR` use the real `CpuDispatcher` for the embedder; the rest are pure-Rust and always run.

### 4.12 The `EXPLAIN` facility (6.8)

Phase doc says `impl Debug for Plan` should be a tree pretty-printer "similar to `EXPLAIN` in SQL". Spec §01 §15 mentions `ADMIN_EXPLAIN_PLAN` — admin opcode is Phase 9.

**Decision:** 6.8 ships **only** the Debug impl + a test asserting human-readable output. The opcode wrapper is Phase 9. Output format:

```
RecallPlan
├─ idempotency_check { request_id: ... }
├─ embedding { text_hash: ..., cache_lookup: true }
├─ shard [1 shard]
│  └─ ShardSearchStep { shard_id: 0 }
│     ├─ ann_search { ef: 64, k: 80, filter: PreFilter }
│     ├─ metadata_lookup { include_extra: false }
│     └─ filter_apply { stage: PostFilter, rules: [] }
├─ merge { sort_by: Score, final_top: 10, confidence_min: None }
├─ text_fetch: None
└─ response { include_text: false, include_metadata: false }
```

ASCII box-drawing for readability. The output should round-trip through manual inspection but isn't a parseable format.

## 5. The 8 sub-tasks (re-ordered for dependency)

The phase doc lists 6.1–6.8 but the natural dependency order is slightly different — `Context` and the writer-handle trait need to land *before* the planners that depend on them, and the cost model is consumed by 6.3 onwards.

| # | Title | Spec anchor | Notes |
|---|---|---|---|
| 6.1 | `PlanNode` types + `ExecutionPlan` enum | §01 §2; §03 §12 (example) | Foundation: data types only. Compiles + serialises in `Debug`. No logic |
| 6.2 | Cost model + `pick_ef` + budget check | §07 §1–§5 | Used by 6.3+. Pure functions + unit tests |
| 6.3 | Recall planner + Recall executor | §03 | Single-shard. Cross-shard branch present but always 1-element |
| 6.4 | Encode planner + Encode executor | §04 | Idempotency → embed → context → alloc → WAL → apply → response |
| 6.5 | Plan + Reason planners + executors | §05 | BFS depth-bounded edge traversal; path scoring |
| 6.6 | Forget planner + Forget executor | §06 | Soft / hard; bulk cap |
| 6.7 | `Executor::execute` dispatch + cancellation/backpressure | §08 §1 + §11 + §14 | Pulls the per-op pieces together |
| 6.8 | `Debug` pretty-printer for plans | §01 §8 + §15 | The EXPLAIN-style tree |

Each sub-task gets its own plan file per the plan-first workflow.

**Spec deviations expected:**
- **Phase doc's "generic operator tree" → spec's "fixed shape per request"** (decision §4.3). Reasoned, not a workaround — we follow spec. Document in sub-task 6.1's plan, no SD entry needed (spec wins by definition).
- **Single-shard only** (decision §4.7). Document; lands properly with Phase 12.
- **In-process `WriterHandle` fake instead of the real writer task** (decision §4.4). Not an SD — the trait is the production API, the fake is test infrastructure.

## 6. New dependencies

All already in workspace `[workspace.dependencies]`:
- `futures = "0.3"` (already used by brain-storage? — check; if not, add)
- `tracing` (already in workspace)
- `tokio` (only for `#[tokio::test]` in dev-deps; runtime not used in the library)
- workspace internal: `brain-protocol`, `brain-embed`, `brain-index`, `brain-metadata`, `brain-storage`, `brain-core`

Possibly new:
- **`futures`** if not yet at workspace level — for `try_join!`, `FuturesUnordered`. Check during 6.1.

If `tempfile` or other helpers aren't in `brain-planner`'s dev-deps yet, declare them.

## 7. Phase exit criteria

- [ ] Sub-tasks 6.1–6.8 ✅.
- [ ] `cargo test -p brain-planner` green.
- [ ] Each of `ENCODE`, `RECALL`, `PLAN`, `REASON`, `FORGET` has at least one end-to-end planner-+-executor test against the fake-writer + real-storage harness.
- [ ] One integration test that walks a real RECALL through `CpuDispatcher` + real HNSW + real metadata + real arena, gated on `BRAIN_EMBED_MODEL_DIR`.
- [ ] `Debug` of every plan variant renders a readable tree (asserted via snapshot-style string checks).
- [ ] Tag `phase-6-complete`.

## 8. Open items for the user before 6.1

Three calls worth confirming up front:

1. **Plan shape:** spec-style `enum ExecutionPlan { Encode(EncodePlan), Recall(RecallPlan), … }` (recommended; follows spec §01 §2) **or** phase-doc-style generic operator tree?
2. **Writer task:** in-process synchronous fake for Phase 6 (recommended; trait-based), real channel-fed task lands in Phase 8/9 **or** build the writer task here?
3. **Cross-shard scope:** Phase 6 ships single-shard only; structure preserved for future fan-out (recommended) **or** stub the multi-shard branch now?

After confirmation, sub-task 6.1's plan goes in next.

---

PLAN READY.
