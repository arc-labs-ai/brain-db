# Sub-task 6.1 — `ExecutionPlan` enum + per-request plan types

Foundation for Phase 6. Ships **data types only** — every other 6.x sub-task consumes the types declared here. No planner logic, no executor logic, no I/O, no async. The deliverable is a tree of structs/enums that:

1. Faithfully mirror spec §08 §01 §2 + §03 §12 (the per-request `RecallPlan`-shape) + §04 §3 (the `EncodePlan` shape) + §05/§06 equivalents.
2. Compile cleanly against the existing `brain-protocol` request types and `brain-core` IDs.
3. Carry enough payload that 6.3–6.6's planners can populate them and 6.7's executor can consume them.

Per the orientation plan, this follows **spec §01 §2's per-request struct shape** (`enum ExecutionPlan { Encode(EncodePlan), Recall(RecallPlan), … }`), *not* the phase-doc's generic operator tree. The orientation plan §4.3 records this decision.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §08/01 §2 | `enum ExecutionPlan { Encode(EncodePlan), Recall(RecallPlan), … }` is the planner's output |
| §08/01 §5 | Plan time < 100 µs; plan size < 4 KB; deterministic |
| §08/01 §8 | Plan is an immutable value passed planner → executor — testable in isolation, loggable |
| §08/03 §12 | Full `RecallPlan` example with all sub-steps |
| §08/04 §3 | `EncodePlan { shard, idempotency_check, embedding, context_resolution, allocation, wal_append, apply, edges, response }` |
| §08/05 §3+§9 | `PlanPlan` / `ReasonPlan` shapes |
| §08/06 §2 | `ForgetPlan` distinguishes soft / hard / bulk |
| §08/01 §11 | Plan is *above* transport — `ShardId` is a logical reference, not a network address |

## 1. Scope

**In scope for 6.1:**
- `ExecutionPlan` enum with 5 variants (one per cognitive operation). Admin / Txn / Subscribe deferred to their own future sub-tasks.
- Per-variant plan structs:
  - `EncodePlan` + `IdempotencyCheckStep` + `EmbeddingStep` + `ContextResolutionStep` + `SlotAllocationStep` + `WalAppendStep` + `ApplyStep` + `EdgeStep` + `ResponseStep`.
  - `RecallPlan` + `ShardSearchStep` + `AnnSearchStep` + `MetadataLookupStep` + `FilterStep` + `MergeStep` + `TextFetchStep`.
  - `PlanPlan` (traversal + scoring) — the spec name clashes with the enum; we use `PathPlan` as the struct name to avoid `Plan::Plan(PlanPlan)` confusion. The variant is `ExecutionPlan::Plan(PathPlan)`.
  - `ReasonPlan` (BFS + path scoring + observation-input handling).
  - `ForgetPlan` (soft / hard / bulk dispatch).
- Common support types: `ShardId`, `FilterStage`, `SortKey`, `FilterRule`, `EdgeSpec` (planner-side; not the wire `EdgeRequest`).
- `PlannerConfig` struct (default-able): `default_ef_search: 64`, `max_ef_search: 500`, `cost_budget_ms: 1000.0`, etc. (spec §07 §5 + §03 §4 numbers).
- `PlannerContext` struct: `config: PlannerConfig`, `stats: ShardStats`.
- `ShardStats` struct (spec §07 §10 shape): `memory_count`, `tombstone_count`, `tombstone_ratio`, `last_rebuild_at`, `avg_search_latency_ms`, `avg_encode_latency_ms`. All `Default::default()`-able for tests.
- `PlanError` enum: variants from spec §07 §5 (`QueryTooExpensive`, `InvalidParameters { field, reason }`, plus a catch-all for malformed requests).
- A minimal stub of `Debug` on every type (derived) — pretty-tree printing is 6.8's job; we just need *something* readable so tests can print plans.
- Unit tests asserting:
  - Each variant constructs cleanly with reasonable default values.
  - `ExecutionPlan` and all sub-structs are `Send + Sync` (compile-time check).
  - `PlanError` round-trips through `Display`.
  - A pinned size sanity: `size_of::<RecallPlan>() < 4096` per spec §01 §5.

**NOT in scope (deferred):**
- Any *logic* that fills these structs — that's 6.3–6.6.
- The executor side / `ExecutorContext` — 6.7.
- Cost numbers as code — 6.2 owns the cost model; 6.1 only declares the placeholder fields (`estimated_cost_ms: f32`).
- The pretty-tree `Debug` impl — 6.8.
- Admin/Txn/Subscribe plans — separate phase or one-off later sub-task.
- `WriterHandle` trait — that's a runtime concept, lives in 6.7's executor module.

## 2. Module layout

```
crates/brain-planner/src/
├── lib.rs              [edit: declare modules + re-exports]
├── error.rs            [new: PlanError]
├── config.rs           [new: PlannerConfig + defaults from spec]
├── stats.rs            [new: ShardStats]
├── context.rs          [new: PlannerContext]
└── plan/               [new directory]
    ├── mod.rs          (re-exports ExecutionPlan + ShardId + common types)
    ├── common.rs       (ShardId, FilterStage, SortKey, FilterRule, EdgeSpec)
    ├── encode.rs       (EncodePlan + step structs)
    ├── recall.rs       (RecallPlan + step structs)
    ├── path.rs         (PathPlan — the spec's "PlanPlan")
    ├── reason.rs       (ReasonPlan + step structs)
    └── forget.rs       (ForgetPlan + step structs)
```

Splitting by request keeps each plan focused and matches the sub-task split (6.3 owns recall.rs internals, etc.). `plan/mod.rs` re-exports everything; consumers do `use brain_planner::{ExecutionPlan, RecallPlan, ...}`.

## 3. Type sketches (binding the spec to Rust)

These are the shapes to land in 6.1. Detailed field types are pinned here so 6.2 onwards can write planner code without re-deciding.

### 3.1 Top-level

```rust
// plan/mod.rs

#[derive(Debug, Clone)]
pub enum ExecutionPlan {
    Encode(EncodePlan),
    Recall(RecallPlan),
    Plan(PathPlan),        // Variant named after the request, struct renamed
    Reason(ReasonPlan),
    Forget(ForgetPlan),
}

// plan/common.rs

/// Logical shard reference; the executor maps to a physical handle.
/// Spec §08/01 §11.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShardId(pub u32);

/// Spec §08/03 §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterStage {
    PreFilter,
    PostFilter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Score,
    Salience,
    InsertedAt,
}

/// Concrete filter predicate the executor applies. Spec §08/03 §6 only
/// names categories; we'll refine in 6.3 when the recall planner is
/// implemented. For now: an opaque `Vec<FilterRule>` carried inside
/// `FilterStep`.
#[derive(Debug, Clone)]
pub enum FilterRule {
    KindIn(Vec<brain_core::MemoryKind>),
    ContextIn(Vec<brain_core::ContextId>),
    SalienceFloor(f32),
    AgeBound { not_older_than_unix_nanos: u64 },
    ConfidenceFloor(f32),
}

/// Planner-side edge spec (not the wire `EdgeRequest`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeSpec {
    pub target: brain_core::MemoryId,
    pub kind: brain_core::EdgeKind,
    pub weight: f32,
}
```

### 3.2 RecallPlan (spec §03 §12)

```rust
// plan/recall.rs

#[derive(Debug, Clone)]
pub struct RecallPlan {
    pub embedding: EmbeddingStep,
    pub shards: Vec<ShardSearchStep>,
    pub merge: MergeStep,
    pub text_fetch: Option<TextFetchStep>,
    pub response: ResponseStep,
    pub estimated_cost_ms: f32,
}

#[derive(Debug, Clone)]
pub struct EmbeddingStep {
    pub text: String,
    pub cache_lookup: bool,
}

#[derive(Debug, Clone)]
pub struct ShardSearchStep {
    pub shard_id: ShardId,
    pub ann_search: AnnSearchStep,
    pub metadata_lookup: MetadataLookupStep,
    pub filter_apply: FilterStep,
}

#[derive(Debug, Clone)]
pub struct AnnSearchStep {
    pub ef: usize,
    pub candidates_to_request: usize,    // K * over_factor; spec §03 §5
    pub pre_filter: Vec<FilterRule>,
}

#[derive(Debug, Clone)]
pub struct MetadataLookupStep {
    pub include_extra: bool,
}

#[derive(Debug, Clone)]
pub struct FilterStep {
    pub stage: FilterStage,
    pub rules: Vec<FilterRule>,
}

#[derive(Debug, Clone)]
pub struct MergeStep {
    pub sort_by: SortKey,
    pub final_top: usize,
    pub confidence_min: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct TextFetchStep {
    pub memory_ids: Vec<brain_core::MemoryId>,
    pub parallel: bool,
}

#[derive(Debug, Clone)]
pub struct ResponseStep {
    pub include_text: bool,
    pub include_metadata: bool,
}
```

### 3.3 EncodePlan (spec §04 §3)

```rust
// plan/encode.rs

#[derive(Debug, Clone)]
pub struct EncodePlan {
    pub shard: ShardId,
    pub idempotency_check: IdempotencyCheckStep,
    pub embedding: EmbeddingStep,
    pub context_resolution: ContextResolutionStep,
    pub allocation: SlotAllocationStep,
    pub wal_append: WalAppendStep,
    pub apply: ApplyStep,
    pub edges: Vec<EdgeStep>,
    pub response: EncodeResponseStep,
    pub estimated_cost_ms: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct IdempotencyCheckStep {
    pub request_id: brain_core::RequestId,
}

/// Context resolution by name → `ContextId`. If the request gave an
/// explicit `ContextId`, this step still runs but is a no-op
/// (resolver returns the passed id).
#[derive(Debug, Clone)]
pub enum ContextResolutionStep {
    Explicit(brain_core::ContextId),
    GetOrCreate {
        agent_id: brain_core::AgentId,
        name: String,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct SlotAllocationStep {
    pub arena_grow_if_needed: bool,
}

#[derive(Debug, Clone)]
pub struct WalAppendStep {
    pub kind: brain_core::MemoryKind,
    pub salience_initial: f32,
    // The vector + text live transiently in the planner output. We do
    // NOT serialise plans to disk, so storing them here is fine. If
    // 6.8's Debug printer needs to skip them for readability, it'll
    // elide.
    pub fsync: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ApplyStep {
    pub arena_write: bool,
    pub metadata_write: bool,
    pub hnsw_insert: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct EdgeStep {
    pub edge: EdgeSpec,
    pub insert_in_metadata: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct EncodeResponseStep {
    pub persistent_id: bool,
}
```

### 3.4 PathPlan / ReasonPlan / ForgetPlan

Sketches only; field-level pinning happens in 6.5 / 6.6. For 6.1 we ship the *shells* so the enum compiles. Each carries `estimated_cost_ms: f32` and a small placeholder body. 6.5/6.6 fill in the details.

```rust
// plan/path.rs
#[derive(Debug, Clone)]
pub struct PathPlan {
    pub start: brain_protocol::PlanState,
    pub goal: brain_protocol::PlanState,
    pub budget: brain_protocol::PlanBudget,
    pub strategy: brain_protocol::PlanStrategy,
    pub estimated_cost_ms: f32,
    // Details filled in by 6.5.
}

// plan/reason.rs
#[derive(Debug, Clone)]
pub struct ReasonPlan {
    pub depth: u32,
    pub confidence_threshold: f32,
    pub max_inferences: u32,
    pub estimated_cost_ms: f32,
    // Details filled in by 6.5.
}

// plan/forget.rs
#[derive(Debug, Clone)]
pub struct ForgetPlan {
    pub mode: brain_protocol::ForgetMode,
    pub memory_id: brain_core::MemoryId,
    pub estimated_cost_ms: f32,
    // Details filled in by 6.6.
}
```

These are placeholders. **Important:** the orientation plan §5 + the phase doc both say 6.5 and 6.6 will *extend* these shapes; we don't have to predict every field now. The point of 6.1 is the `ExecutionPlan` enum compiles and we can write `Executor::execute(plan: ExecutionPlan)` against it.

### 3.5 `PlannerConfig` + `ShardStats` + `PlannerContext`

```rust
// config.rs
#[derive(Debug, Clone, Copy)]
pub struct PlannerConfig {
    pub default_ef_search: usize,    // 64 (spec §03 §4)
    pub max_ef_search: usize,        // 500 (spec §03 §4)
    pub max_candidates_per_search: usize, // 1000 (spec §03 §5)
    pub cost_budget_ms: f32,         // 1000.0 (spec §07 §5)
    pub max_k: usize,                // 1000 (spec §03 §1)
    pub max_edges_per_encode: usize, // 64 (spec §04 §12)
}

impl Default for PlannerConfig { /* fills with spec defaults */ }

// stats.rs
#[derive(Debug, Clone, Copy, Default)]
pub struct ShardStats {
    pub memory_count: u64,
    pub tombstone_count: u64,
    pub tombstone_ratio: f32,
    pub last_rebuild_at_unix_nanos: u64,
    pub avg_search_latency_ms: f32,
    pub avg_encode_latency_ms: f32,
}

// context.rs
#[derive(Debug, Clone, Copy, Default)]
pub struct PlannerContext {
    pub config: PlannerConfig,
    pub stats: ShardStats,
}
```

### 3.6 `PlanError`

```rust
// error.rs
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PlanError {
    #[error("query too expensive: estimated {estimated_ms:.1} ms > budget {budget_ms:.1} ms")]
    QueryTooExpensive { estimated_ms: f32, budget_ms: f32 },

    #[error("invalid parameter {field}: {reason}")]
    InvalidParameters { field: &'static str, reason: String },

    #[error("unsupported request shape: {0}")]
    Unsupported(&'static str),
}
```

`Unsupported` is the catch-all for "Phase 6 doesn't yet plan this" (e.g. cross-shard, plan-cache miss, subscribe). Lets us land a partial planner without panic.

## 4. Implementation decisions

### 4.1 No serde / no rkyv on plan types

Spec §01 §8 says plans are passed planner → executor in-process; they're not wire-serialised. So we skip `serde::Serialize` / `rkyv::Archive` derives. Reduces compile time and dep surface. The `Debug` derive is enough for the EXPLAIN facility (6.8 builds a manual pretty-tree on top).

### 4.2 Plan types own their data (not borrow)

`EmbeddingStep { text: String }` — not `&'a str`. Plans live across `.await` boundaries (executor is async), so borrows would force `'static` lifetimes everywhere or `Cow`. Owning is simpler and the cost is one `String::clone` per plan, negligible vs the work the executor does.

### 4.3 `estimated_cost_ms: f32` on every plan, not in a wrapper

The cost is per-plan-variant, set by 6.2's cost model when the planner builds the plan. Putting it on each plan struct keeps `ExecutionPlan` flat (no `(Plan, Cost)` tuples flying around). 6.2 wires `pick_ef` and `cost_recall` etc. to *write* this field.

### 4.4 The `Plan` variant naming clash

`ExecutionPlan::Plan(PathPlan)` reads weirdly but matches the request name. Alternative: rename the request from `Plan` to something else — but `brain-protocol` already shipped `PlanRequest` (Phase 1) and we don't change shipped wire types. Decision: keep the variant name `Plan`, name the struct `PathPlan` so call sites read as `ExecutionPlan::Plan(PathPlan { ... })`. The struct's docstring explains the rename.

### 4.5 Send + Sync test

Compile-time:
```rust
const _: fn() = || {
    fn require<T: Send + Sync>() {}
    require::<ExecutionPlan>();
};
```

The executor will likely move plans across thread boundaries (Glommio per-shard tasks). `Send + Sync` is non-negotiable.

### 4.6 No new dependencies

`thiserror` already in workspace + brain-planner. `brain-core` + `brain-protocol` already declared. No new crates.

### 4.7 `Debug` only — no `Display` yet

`Debug` derive on every plan type is enough for tests. `Display` (human-readable, EXPLAIN-style) is 6.8. Keeping that boundary clean.

## 5. Files written / changed

```
crates/brain-planner/Cargo.toml         [edit: add brain-protocol; declare anyhow/tracing for upcoming phases — actually defer until needed; only add what 6.1 imports]
crates/brain-planner/src/lib.rs         [edit: module decls + re-exports]
crates/brain-planner/src/error.rs       [new]
crates/brain-planner/src/config.rs      [new]
crates/brain-planner/src/stats.rs       [new]
crates/brain-planner/src/context.rs     [new]
crates/brain-planner/src/plan/mod.rs    [new]
crates/brain-planner/src/plan/common.rs [new]
crates/brain-planner/src/plan/encode.rs [new]
crates/brain-planner/src/plan/recall.rs [new]
crates/brain-planner/src/plan/path.rs   [new]
crates/brain-planner/src/plan/reason.rs [new]
crates/brain-planner/src/plan/forget.rs [new]
```

Cargo.toml addition (minimal):
```toml
[dependencies]
brain-core = { path = "../brain-core" }
brain-protocol = { path = "../brain-protocol" }
thiserror.workspace = true
```

No dev-deps change.

## 6. Verify checklist

- `cargo build -p brain-planner` clean.
- `cargo test -p brain-planner` — existing 1 stub test + ~10 new (one per struct constructor, plus the Send+Sync compile check + PlanError display + size sanity).
- `cargo clippy -p brain-planner --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-planner` no diff.

## 7. Commit message (draft)

```
feat(brain-planner): ExecutionPlan enum + per-request plan types (sub-task 6.1)

Foundation for Phase 6. Data types only — no planner logic, no
executor logic, no I/O. Per spec §08/01 §2 + §03 §12 + §04 §3,
ExecutionPlan is a per-request enum, each variant carrying a struct
of steps. (Phase doc's "generic operator tree" idea was at the wrong
abstraction level for our rule-based planner; orientation plan §4.3
records the divergence.)

- ExecutionPlan { Encode, Recall, Plan, Reason, Forget }. Admin /
  Txn / Subscribe deferred to later sub-tasks.
- Per-variant plans + step structs (RecallPlan/EncodePlan fully
  fleshed out; PathPlan/ReasonPlan/ForgetPlan shipped as shells for
  6.5/6.6 to extend).
- PlannerConfig with spec defaults (ef=64, max_ef=500, budget=1s,
  max_k=1000, max_edges=64).
- ShardStats per spec §07 §10 — Default-able for tests.
- PlannerContext = (config, stats).
- PlanError { QueryTooExpensive, InvalidParameters, Unsupported }.
- Send + Sync compile-time assertion on ExecutionPlan.

No serde / rkyv on plan types — plans don't cross the wire
(spec §01 §8). Plan types own their data (String, not &str) so they
live across .await boundaries in the executor (6.7).

New dep declared in brain-planner Cargo.toml: brain-protocol (already
a workspace member).

Verify: cargo build/test/clippy -p brain-planner.
```

## 8. Out-of-scope flags

- No planner logic. 6.3–6.6 fill `plan_recall`, `plan_encode`, etc.
- No executor. 6.7 dispatches.
- No cost numbers as code. 6.2 owns coefficients; 6.1 declares the field only.
- No pretty-tree `Debug`. 6.8 lands the EXPLAIN-style printer; 6.1 uses derived `Debug`.
- No serde / wire codec.
- No Admin/Txn/Subscribe plans.
- No real `EdgeKind` / `MemoryKind` referenced if `brain-core` doesn't export them yet — verify during implementation. If missing, declare on the planner side temporarily.

## 9. Risks

- **`brain-core` may not yet export `MemoryKind` / `EdgeKind`**: verified during implementation. If missing, we declare them in `plan/common.rs` and reconcile to `brain-core` when those types land. Track as a follow-up.
- **`brain-protocol`'s `PlanState` is not `Clone`**: we use it in `PathPlan`. If it isn't `Clone`, we either derive it (edit brain-protocol) or wrap in `Arc`. Verify during implementation; the simpler fix wins.
- **4 KB plan size limit (spec §01 §5)** — the size-sanity test should catch any accidental bloat (e.g. embedding the cue text in the plan with too much overhead). If a plan exceeds 4 KB, that's a design problem to revisit at 6.3+.

---

PLAN READY.
