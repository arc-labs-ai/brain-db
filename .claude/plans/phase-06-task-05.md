# Sub-task 6.5 — Plan and Reason planners

Planner side only. The phase doc 6.5 reads "Both queries become traversal plans with depth bounds and edge-kind filters" — no executor work. The orientation plan listed "+ executors" but the phase doc is normative; spec §08/05 says PLAN and REASON compose RECALL + graph traversal, and the **traversal step needs the edge-graph wiring that arrives properly in Phase 7** (cognitive operations).

What we ship in 6.5: a `plan_path` and `plan_reason` that build full `PathPlan` / `ReasonPlan` values from their wire requests, validated, costed, budget-checked. Plus extending the **shells** of those plan structs (from 6.1) into full step-bearing shapes so 6.7's executor and Phase 7+'s real implementation can consume them.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §08/05 §1 | PLAN and REASON are "higher-level" — orchestrate RECALL + graph traversal |
| §08/05 §3 | `PlanPlan { embedding, starting_recall, goal_recall, traversal, scoring, response }` |
| §08/05 §4 | Traversal is bidirectional BFS along `edge_kinds` to `max_depth` |
| §08/05 §5 | Bidirectional BFS gives `b^(d/2)` savings — defends `max_depth ≤ 10` |
| §08/05 §6 | Path scoring: `length_score × edge_score × salience_score` |
| §08/05 §9 | `ReasonPlan { embedding, base_recall, supports_traversal, contradicts_traversal, aggregation, response }` |
| §08/05 §11 | Cost: PLAN 30-100 ms, REASON 30-50 ms |
| §08/05 §13 | `max_results ≤ 100` cap |
| §08/05 §14 | Validation: `max_depth ≤ 10`, edge_kinds valid, `max_results ≤ 100` |
| Phase doc 6.5 | "Both queries become traversal plans with depth bounds and edge-kind filters" |
| Phase doc 6.7 | Executor dispatch lands later; PLAN/REASON executor proper is Phase 7 territory |

The wire `PlanRequest` and `ReasonRequest` from Phase 1 (`brain-protocol`) don't quite match the spec's shape — the wire has `start: PlanState`, `goal: PlanState`, `budget: PlanBudget`, `strategy_hint` for PLAN; and `observation: ObservationInput`, `depth: u32`, `confidence_threshold: f32`, `max_inferences: u32`, `budget_wall_time_ms: u32` for REASON. Phase 6 works with the wire shape we have.

## 1. Scope

**In scope for 6.5:**
- `crates/brain-planner/src/path.rs` — planner side. `plan_path(&PlanRequest, &PlannerContext) -> Result<ExecutionPlan, PlanError>`.
- `crates/brain-planner/src/reason.rs` — planner side. `plan_reason(&ReasonRequest, &PlannerContext) -> Result<ExecutionPlan, PlanError>`.
- Extend `plan/path.rs` and `plan/reason.rs` (the 6.1 shells) into full plans:
  - `PathPlan { embedding_start, embedding_goal, starting_recall, goal_recall, traversal, scoring, response, ... }`
  - `ReasonPlan { embedding, base_recall, supports_traversal, contradicts_traversal, aggregation, response, ... }`
- New step structs in `plan/`: `RecallSubStep`, `TraversalStep`, `ScoringStep`, `AggregationStep`, `EvidenceResponseStep`.
- New `cost::cost_path` and `cost::cost_reason` (replacing 6.2's `cost_path_placeholder` / `cost_reason_placeholder`).
- New `PlannerConfig` knobs: `max_traversal_depth: usize = 10`, `max_plan_results: usize = 100`, `default_edge_kinds_plan: &[EdgeKind]`, `default_edge_kinds_supports: &[EdgeKind]`, `default_edge_kinds_contradicts: &[EdgeKind]` — but const arrays in structs are awkward; we expose them as `fn` returning `Vec` instead.
- Pure planner unit tests for both PLAN and REASON: validation paths, plan shape, defaults, edge-kind filters.

**NOT in scope:**
- No executor side. `execute_path` / `execute_reason` don't ship in 6.5 — they're Phase 7+ work where the edge-graph traversal naturally fits with cognitive-ops scaffolding. (6.7 wires `Executor::execute` dispatch to *return* an `ExecError::Unsupported` for Plan/Reason variants until then.)
- No new `ExecError` variants for traversal.
- No real `cost_path` numeric tuning (spec §11 cites 30-100 ms; we use a coarse estimate based on `max_depth` and `recall_k`).
- No bidirectional-BFS implementation.

## 2. Type sketches (extending 6.1 shells)

### 2.1 `RecallSubStep`

A planner-internal RECALL invocation. Spec §08/05 §3 calls this a `RecallStep`. We reuse 6.1's `ShardSearchStep` / `AnnSearchStep` shapes from the recall plan but wrap them so consumers don't confuse "the request was a RECALL" with "this is the sub-RECALL inside a PLAN".

```rust
// plan/path.rs (extending the existing shell)

#[derive(Debug, Clone)]
pub struct RecallSubStep {
    pub embedding: EmbeddingStep,
    pub shard: ShardSearchStep,    // single shard
    pub merge: MergeStep,
}
```

### 2.2 `PathPlan` (full shape)

```rust
#[derive(Debug, Clone)]
pub struct PathPlan {
    pub start: PlanState,
    pub goal: PlanState,
    pub budget: PlanBudget,
    pub strategy: PlanStrategy,
    pub starting_recall: Option<RecallSubStep>,    // None if start is ByMemoryId
    pub goal_recall: Option<RecallSubStep>,        // None if goal is ByMemoryId
    pub traversal: TraversalStep,
    pub scoring: ScoringStep,
    pub response: EvidenceResponseStep,
    pub estimated_cost_ms: f32,
}
```

Why `Option<RecallSubStep>` — if `start = PlanState::ByMemoryId(id)`, we skip the starting RECALL (we already have the memory). Same for `goal`. The executor checks both and runs only the needed ones.

### 2.3 `TraversalStep`

```rust
#[derive(Debug, Clone)]
pub struct TraversalStep {
    pub edge_kinds: Vec<EdgeKind>,
    pub max_depth: usize,
    pub bidirectional: bool,
    /// Hard cap on candidate paths the traversal will collect.
    pub max_paths: usize,
}
```

### 2.4 `ScoringStep`

Per spec §08/05 §6.

```rust
#[derive(Debug, Clone, Copy)]
pub struct ScoringStep {
    pub include_length_score: bool,
    pub include_edge_weight_score: bool,
    pub include_salience_score: bool,
    pub top_n: usize,    // Final cap on paths returned
}
```

All three weights default to `true`. Future tuning (sub-task or admin knob) can disable any.

### 2.5 `EvidenceResponseStep`

Distinct from the recall `ResponseStep` because PLAN and REASON return *paths* / *evidence*, not flat hit lists.

```rust
#[derive(Debug, Clone, Copy)]
pub struct EvidenceResponseStep {
    pub include_paths: bool,
    pub include_text: bool,
    pub include_metadata: bool,
}
```

### 2.6 `ReasonPlan` (full shape)

```rust
// plan/reason.rs (extending the existing shell)

#[derive(Debug, Clone)]
pub struct ReasonPlan {
    pub observation: ObservationInput,
    pub depth: u32,
    pub confidence_threshold: f32,
    pub max_inferences: u32,
    pub budget_wall_time_ms: u32,
    pub embedding: Option<EmbeddingStep>,         // None if observation is ByMemoryId
    pub base_recall: Option<RecallSubStep>,        // ditto
    pub supports_traversal: TraversalStep,
    pub contradicts_traversal: TraversalStep,
    pub aggregation: AggregationStep,
    pub response: EvidenceResponseStep,
    pub estimated_cost_ms: f32,
}
```

### 2.7 `AggregationStep`

Spec §08/05 §10's `confidence` aggregation.

```rust
#[derive(Debug, Clone, Copy)]
pub struct AggregationStep {
    pub max_supporting: usize,
    pub max_contradicting: usize,
    /// `true` = aggregate confidence = (supporting_weight) /
    /// (supporting_weight + contradicting_weight); `false` = compute
    /// per-evidence only.
    pub include_aggregate_confidence: bool,
}
```

## 3. Validation rules

### 3.1 PLAN

- `req.budget.max_steps == 0` → `InvalidParameters` (must search at least one step).
- `req.budget.max_steps as usize > config.max_traversal_depth` → `InvalidParameters` (cap at 10 per spec §14).
- `req.budget.max_branches_explored == 0` → `InvalidParameters`.
- `req.context_filter.is_some()` AND empty → treat as `None` (forgiving), don't error.

Note: `PlanBudget::max_steps` is u32 in the wire; map to `usize`. We treat `max_steps` as the spec's `max_depth` (the wire is the bind point; phase 1 chose this naming).

### 3.2 REASON

- `req.depth == 0` → `InvalidParameters`.
- `req.depth as usize > config.max_traversal_depth` → `InvalidParameters`.
- `req.confidence_threshold` ∈ [0, 1] — else `InvalidParameters`.
- `req.max_inferences == 0` → `InvalidParameters`.
- `req.max_inferences as usize > config.max_plan_results` → `InvalidParameters`.

## 4. Implementation decisions

### 4.1 Edge-kind defaults

Spec §08/05 §2 says PLAN's default is `[CAUSED, FOLLOWED_BY]`. REASON splits into:
- supports: `[SUPPORTS, DERIVED_FROM]`
- contradicts: `[CONTRADICTS]`

`brain-core::EdgeKind` enumerates `Caused`, `FollowedBy`, `DerivedFrom`, `SimilarTo`, `Contradicts`, `Supports`, `References`, `PartOf`. These match.

`PlannerConfig` doesn't carry the lists (const Vec is awkward) — instead `crate::plan::path::default_plan_edge_kinds() -> Vec<EdgeKind>` etc. as free helpers.

### 4.2 Wire `PlanRequest` has no `edge_kinds` field

The wire shape carries `strategy_hint: Option<PlanStrategy>` but no explicit edge-kind list. Spec §08/05 §2 lists edge_kinds in the *spec* shape; the wire shape didn't include it (Phase 1 chose otherwise). We use `default_plan_edge_kinds()` always for now; a future wire-schema bump can add a field.

Document this divergence.

### 4.3 `PlanState::ByVector` handling

If `start = PlanState::ByVector { offset, dim }`, the wire references a vector somewhere in the request frame's payload. Phase 1 wired the offsets; the planner doesn't materialise the vector itself. For 6.5 we ship the plan shape; the executor (later) handles the deref.

Practical: `plan_path` does NOT consult the vector bytes at planning time. The plan records the `PlanState` enum; the executor reads it.

### 4.4 `cost_path` and `cost_reason` shape

Replace 6.2's placeholders with rough estimators:

```rust
pub fn cost_path(max_depth: usize, max_branches: usize, ctx: &PlannerContext) -> f32 {
    // Two embeddings + two RECALLs + traversal.
    let embed = 2.0 * cost::embedding_cost(false);
    let recall = 2.0 * cost::cost_recall(10, 1.0, false, ctx);
    let traversal = (max_depth as f32) * (max_branches as f32) * METADATA_POINT_LOOKUP_MS * 4.0;
    embed + recall + traversal
}

pub fn cost_reason(depth: usize, max_inferences: usize, ctx: &PlannerContext) -> f32 {
    let embed = cost::embedding_cost(false);
    let recall = cost::cost_recall(20, 1.0, false, ctx);
    let traversal = 2.0 * (depth as f32) * (max_inferences as f32) * METADATA_POINT_LOOKUP_MS * 4.0;
    embed + recall + traversal
}
```

These align with spec §11's 30-100 ms (PLAN) and 30-50 ms (REASON) ranges for typical inputs. The `* 4.0` accounts for the edge-table lookups being slightly heavier than a point fetch.

Pin coefficients via tests that assert the result lies in the spec's range for default inputs.

### 4.5 Where the new step structs live

`plan/common.rs` already exists. Adding 4 new structs there bloats it; instead extend `plan/path.rs` and `plan/reason.rs` (the shells). `RecallSubStep` is shared between PLAN and REASON — it lives in `plan/common.rs` since it's the natural cross-module type.

### 4.6 Public API additions

Re-exports from `lib.rs`:
- `plan_path`, `plan_path_inner`
- `plan_reason`, `plan_reason_inner`
- `AggregationStep`, `EvidenceResponseStep`, `RecallSubStep`, `ScoringStep`, `TraversalStep`

The lib.rs `pub use plan::{...}` block grows by 4 names.

## 5. Test plan

### 5.1 PLAN tests

- `plan_path_default_request_shape` — default `PlanBudget`; check that traversal.max_depth, edge_kinds list, and both recalls are populated when start/goal are `ByText`.
- `plan_path_by_memory_id_skips_recall` — `start = ByMemoryId(...)` ⇒ `starting_recall = None`.
- `plan_path_zero_max_steps_rejected` → `InvalidParameters[budget.max_steps]`.
- `plan_path_depth_over_max_rejected` — `max_steps = 11` (cap 10) → `InvalidParameters`.
- `plan_path_estimated_cost_in_range` — for `max_depth=4, branches=64`, estimated cost ∈ [10, 200] ms (lenient).

### 5.2 REASON tests

- `plan_reason_default_request_shape` — both traversals populated; embedding + base_recall present for `ByText` observation.
- `plan_reason_by_memory_id_skips_embedding` — `observation = ByMemoryId(...)` ⇒ `embedding = None`.
- `plan_reason_zero_depth_rejected`.
- `plan_reason_max_inferences_zero_rejected`.
- `plan_reason_confidence_out_of_range_rejected`.
- `plan_reason_edge_kind_defaults` — `supports_traversal.edge_kinds == [Supports, DerivedFrom]`, `contradicts_traversal.edge_kinds == [Contradicts]`.

### 5.3 Cost-model tests

- `cost_path_grows_with_depth` — monotone in `max_depth`.
- `cost_reason_grows_with_inferences` — monotone in `max_inferences`.

## 6. Files written / changed

```
crates/brain-planner/src/path.rs                     [new — planner side]
crates/brain-planner/src/reason.rs                   [new — planner side]
crates/brain-planner/src/plan/common.rs              [edit: + RecallSubStep]
crates/brain-planner/src/plan/path.rs                [edit: full PathPlan shape]
crates/brain-planner/src/plan/reason.rs              [edit: full ReasonPlan shape]
crates/brain-planner/src/cost.rs                     [edit: replace placeholders with cost_path / cost_reason]
crates/brain-planner/src/config.rs                   [edit: + max_traversal_depth + max_plan_results]
crates/brain-planner/src/lib.rs                      [edit: mod + re-exports]
```

No new external deps. No new files in `executor/`.

## 7. Verify checklist

- `cargo build -p brain-planner` clean (in dev container).
- `cargo test -p brain-planner` — existing 59 + ~12 new (PATH + REASON + cost monotonicity).
- `cargo clippy -p brain-planner --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-planner` no diff.

## 8. Commit message (draft)

```
feat(brain-planner): Plan (path) + Reason planners (sub-task 6.5)

Planner side only — phase doc 6.5 reads "both queries become
traversal plans with depth bounds and edge-kind filters." The
executor lands later (the bidirectional-BFS traversal naturally
fits with Phase 7's cognitive-operations scaffolding).

Planners:
- plan_path(&PlanRequest, &PlannerContext) → PathPlan: validates
  budget.max_steps ∈ (0, 10], builds RecallSubSteps for ByText/
  ByVector starts/goals (skipped for ByMemoryId), assembles
  TraversalStep with default edge_kinds = [Caused, FollowedBy],
  ScoringStep, EvidenceResponseStep.
- plan_reason(&ReasonRequest, &PlannerContext) → ReasonPlan:
  validates depth ∈ (0, 10], confidence ∈ [0, 1], max_inferences > 0;
  builds optional embedding + base_recall (for ByText observation),
  supports_traversal (edge_kinds [Supports, DerivedFrom]),
  contradicts_traversal (edge_kinds [Contradicts]), aggregation.

Plan structs (extending 6.1 shells):
- PathPlan now carries embedding + recalls + traversal + scoring +
  response.
- ReasonPlan now carries embedding + base_recall + two traversals +
  aggregation + response.
- New step types: RecallSubStep, TraversalStep, ScoringStep,
  AggregationStep, EvidenceResponseStep.

Cost model: replaces cost_path_placeholder / cost_reason_placeholder
with depth-/branches-aware estimators. Tests pin the result is
monotone in the depth/inference parameters.

PlannerConfig adds max_traversal_depth (10, spec §05 §14) and
max_plan_results (100, spec §05 §13).

The wire PlanRequest doesn't carry an explicit edge_kinds field —
spec §05 §2 lists it but Phase 1's wire shape omits. The planner uses
default_plan_edge_kinds() = [Caused, FollowedBy] for now; a future
wire bump can plumb explicit kinds through.

No new external deps. No new ExecError. No executor changes.

Verify: cargo build/test/clippy -p brain-planner in dev container.
```

---

PLAN READY.
