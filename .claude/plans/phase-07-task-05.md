# Sub-task 7.5 — PLAN handler (first BFS executor)

This is the larger of the Phase-6-deferred handlers. The planner-side
`PathPlan` (6.5) exists, but the executor doesn't — `execute()`
currently returns `ExecError::Unsupported("PLAN execution — Phase 7")`.
This sub-task ships the bidirectional-BFS executor **and** wires the
brain-ops handler in one go.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §09/04 §1 | PLAN: embed start + goal → RECALL on each → bi-BFS → return paths |
| §09/04 §5 | Bidirectional BFS halves the cost vs unidirectional |
| §09/04 §6 | "No paths found" is not an error — empty paths + confidence=0 |
| §09/04 §9 | Forward traversal follows edge direction; backward goes against |
| §09/04 §10 | `score = length × edge_weight × salience` (geometric blend) |
| §09/04 §16 | Self-loop guard: a path visits each node at most once |
| §09/04 §17 | `QueryTooExpensive` / `Timeout` are the only hard failures |
| §03/08/4 | Wire `PlanResponseFrame { steps, is_final, plan_status }` |
| §03/08/4 | Wire `PlanStep { step_index, memory_id, text, transition_kind, confidence, estimated_distance_to_goal }` |
| §07/04 | `edges_out` + `edges_in` redb tables (already implemented) |

## 1. Scope

**In scope for 7.5:**

- Add `brain-planner::executor::path::execute_path(plan: PathPlan,
  ctx: &ExecutorContext) -> Result<PathResult, ExecError>`.
- Add `PathResult` to `executor::result` (Rust-side type; multiple
  paths + status).
- Extend `ExecutionResult` with a `Plan(PathResult)` variant and wire
  the `execute()` dispatch arm.
- Replace `crates/brain-ops/src/plan.rs::handle_plan` stub: call
  `plan_path_inner` → `execute_path` → map to wire
  `PlanResponseFrame`.
- Integration tests for the executor (in brain-planner) + the handler
  (in brain-ops).

**NOT in scope:**

- Multi-path response framing. Spec §09/04 §3 returns
  `Vec<Path>`; the **wire** frame is `Vec<PlanStep>` (a single
  linear path). v1 returns the **single best path** as one frame.
  Multi-path streaming is Phase 9 server work.
- Explain mode (§13).
- Implicit-start fallback (§7's "use recent high-salience").
  `PathPlan` already requires explicit endpoints.
- "Goal not encoded yet" weak-anchor heuristics (§8). We use whatever
  the start/goal recall returns.
- Path scoring beyond a simple length-edge-salience blend.

## 2. Implementation decisions

### 2.1 Endpoint resolution

For each endpoint (`start`, `goal`), `PathPlan` carries an optional
`starting_recall` / `goal_recall` (`Option<RecallSubStep>`). The
executor:

- `PlanState::ByMemoryId(id)` → endpoint set = `{id}`. No recall.
- `PlanState::ByText` / `ByVector` → run a lightweight RECALL using
  the sub-step's params (cue text / vector, top_k = a small constant
  like `5`). Endpoint set = resulting `MemoryId`s.

If either endpoint set is empty after recall, return
`PathResult { paths: vec![], status: NoPathFound }`.

### 2.2 Bidirectional BFS

```rust
struct PathNode { memory_id: MemoryId, parent: Option<MemoryId>, edge: Option<EdgeKind>, depth: usize }

fn bidirectional_bfs(
    starts: &HashSet<MemoryId>,
    goals: &HashSet<MemoryId>,
    max_depth: usize,
    edge_kinds: &[EdgeKind],
    metadata: &SharedMetadataDb,
) -> Result<Vec<Path>, ExecError> {
    // Visited maps: id -> PathNode (parent pointer + depth)
    // Two frontiers: forward (from starts), backward (from goals).
    // Alternate expansion of the smaller frontier (classic bi-BFS).
    // On any node added to one side that exists in the other side's
    // visited map → reconstruct the meeting path.
    // Cap total nodes-explored at plan.budget.max_branches_explored.
    // Hard wall-clock cap at plan.budget.max_wall_time_ms.
}
```

**Edge traversal:** forward uses `list_edges_from(EDGES_OUT_TABLE,
node, None)` filtered to `edge_kinds`. Backward uses
`list_edges_to(EDGES_IN_TABLE, node, None)` filtered the same way.
One read txn opened once and reused for all lookups (spec §07/06 §2
on read-txn cost amortization).

**Self-loop guard (§16):** visited maps reject re-entry. A path
reconstruction also walks parents and asserts no repeated nodes (the
visited maps make that automatic, but assert anyway).

**Budget enforcement:**
- `max_branches_explored` is the total node count summed across both
  visited maps. When it trips, return current best paths +
  `PlanStatus::BudgetExhausted`.
- `max_wall_time_ms` checked once per outer BFS iteration via
  `Instant::elapsed()`. On trip → same outcome.

### 2.3 Path scoring

Spec §09/04 §10:

```
length_score = 1 / (1 + path_length)
edge_score   = geometric_mean(edge_weights)   # 1.0 if no edge weights
salience_score = geometric_mean(node.salience for nodes in path)
score = length_score * edge_score * salience_score
```

If `ScoringStep.include_*` flags are off, drop that factor (= 1.0).
Final paths sorted by score desc; truncated to `scoring.top_n`.

For v1, return only the top-1 path on the wire (spec §03/08/4 frame
is one `Vec<PlanStep>`). The Rust-side `PathResult` carries all
top_n; Phase 9 framing will fan them out.

### 2.4 `PathResult` shape

```rust
#[derive(Debug, Clone)]
pub struct PathResult {
    pub paths: Vec<Path>,
    pub status: PlanStatus,
}

#[derive(Debug, Clone)]
pub struct Path {
    pub nodes: Vec<MemoryId>,
    pub edges: Vec<EdgeKind>,  // edges[i] connects nodes[i] → nodes[i+1]
    pub score: f32,
    pub node_salience: Vec<f32>,
    pub node_text: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum PlanStatus { GoalReached, BudgetExhausted, NoPathFound, Timeout }
```

`PlanStatus` reuses the wire enum's variants for clarity.

### 2.5 Wire mapping (handler)

```rust
fn path_to_steps(path: &Path) -> Vec<PlanStep> {
    let n = path.nodes.len();
    path.nodes.iter().enumerate().map(|(i, &id)| {
        let transition_kind = if i == 0 {
            TransitionKind::Initial
        } else {
            edge_to_transition(path.edges[i - 1])
        };
        let estimated_distance_to_goal = (n - 1 - i) as f32;
        PlanStep {
            step_index: i as u32,
            memory_id: id.into(),
            text: path.node_text.get(i).cloned().unwrap_or_default(),
            transition_kind,
            confidence: path.score,
            estimated_distance_to_goal,
        }
    }).collect()
}

fn edge_to_transition(kind: EdgeKind) -> TransitionKind {
    use brain_core::EdgeKind::*;
    match kind {
        Caused => TransitionKind::Causal,
        FollowedBy => TransitionKind::Temporal,
        SimilarTo => TransitionKind::Similarity,
        other => TransitionKind::Other(format!("{other:?}")),
    }
}
```

Handler body returns:
- Empty paths → `PlanResponseFrame { steps: vec![], is_final: true,
  plan_status: Some(NoPathFound) }`.
- At least one path → `steps = path_to_steps(top)`, `is_final: true`,
  `plan_status: Some(result.status)`.

### 2.6 No new error variants

Same error story as 7.3/7.4: planner validation → `PlanError`;
executor failures → `ExecError`; mapping is already in `OpError`'s
`#[from]`.

## 3. Files written / changed

```
crates/brain-planner/src/executor/path.rs        [new — execute_path + bi-BFS]
crates/brain-planner/src/executor/result.rs      [edit: + PathResult + Path + PlanStatus]
crates/brain-planner/src/executor/dispatch.rs    [edit: ExecutionPlan::Plan arm]
crates/brain-planner/src/executor/mod.rs         [edit: pub use path::execute_path]
crates/brain-planner/src/lib.rs                  [edit: re-export PathResult, Path, PlanStatus, execute_path]
crates/brain-planner/tests/path_executor.rs      [new — 5 unit/integration tests for the BFS]
crates/brain-ops/src/plan.rs                     [edit: real handler body]
crates/brain-ops/tests/plan.rs                   [new — 5 integration tests]
```

No new external deps.

## 4. Test plan

### 4.1 brain-planner BFS tests (5)

1. `bfs_finds_direct_edge` — A --CAUSED--> B; start=A, goal=B,
   max_depth=2; one path of length 1.
2. `bfs_finds_two_hop_path` — A → B → C; start=A, goal=C,
   max_depth=3; one path of length 2.
3. `bfs_no_path_returns_empty_status` — two disconnected components;
   `status=NoPathFound`, paths empty.
4. `bfs_respects_edge_kind_filter` — A --REFERENCES--> B exists but
   plan asks for `[CAUSED]`; no path.
5. `bfs_self_loop_guard` — A --CAUSED--> A; start=A, goal=A,
   max_depth=2; allowed (zero-length path or no path — picks the
   first match in spec §16 sense; test pins behaviour).

### 4.2 brain-ops handler tests (5)

1. `plan_full_pipeline_returns_path` — encode 3 memories, link them
   A→B→C with CAUSED, plan from A's text to C's text, assert one
   `PlanResponseFrame` with 3 steps and `plan_status = GoalReached`.
2. `plan_no_path_returns_no_path_status` — two unlinked memories;
   `plan_status = NoPathFound`, `steps.is_empty()`.
3. `plan_step_transitions_map_correctly` — CAUSED → Causal,
   FollowedBy → Temporal; verify the wire mapping.
4. `plan_invalid_budget_returns_plan_error` — `max_depth = 0` (or
   wherever the planner validates) → `OpError::PlanError`.
5. `plan_by_memory_id_skips_recall` — `start = ByMemoryId(known)`,
   `goal = ByMemoryId(other)`, edge between them → 1-hop path.

LINK isn't wired yet (sub-task 7.8), so tests use a small helper that
inserts edges directly via `brain-metadata::tables::edge::link`
inside the test fixture.

## 5. Verify checklist

- `cargo build -p brain-planner -p brain-ops` clean.
- `cargo test -p brain-planner -p brain-ops` — 101 + 5 + 38 + 5 = 149.
- `cargo clippy -p brain-planner -p brain-ops --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-planner -p brain-ops -- --check` no diff.

## 6. Commit message (draft)

```
feat(brain-planner,brain-ops): PLAN executor + handler (sub-task 7.5)

Ships the first BFS executor (Phase 6.5 deferred) and wires the
brain-ops PLAN handler in one commit.

brain-planner:
- executor::path::execute_path: bidirectional BFS along configured
  edge kinds, capped by PathPlan.budget. Resolves ByText / ByVector
  endpoints via lightweight RECALL; ByMemoryId is used directly.
  Self-loop guard via per-side visited maps. Scoring per spec
  §09/04 §10: length × edge-weight × salience (geometric mean).
- ExecutionResult gains a Plan(PathResult) variant; the dispatch
  arm replaces ExecError::Unsupported.
- Result types: Path { nodes, edges, score, node_salience,
  node_text }; PlanStatus { GoalReached, BudgetExhausted,
  NoPathFound, Timeout }.

brain-ops:
- handle_plan: plan_path_inner → execute_path → map top-1 path
  into PlanResponseFrame with TransitionKind translation (CAUSED
  → Causal, FollowedBy → Temporal, SimilarTo → Similarity).
  Empty-path or non-GoalReached statuses set plan_status accordingly.
  estimated_distance_to_goal = remaining hops.
- Single-frame for v1; multi-path streaming is Phase 9 server work.

Tests: 5 brain-planner BFS tests (direct edge, two-hop, no path,
edge-kind filter, self-loop guard); 5 brain-ops handler tests
(full pipeline, no-path status, transition mapping, validation
error, ByMemoryId endpoints).

No new external deps.
```

## 7. Risks

- **No LINK handler yet (7.8)**. Tests insert edges via the
  brain-metadata helper directly. Documented; production path uses
  the future LINK handler.
- **`max_branches_explored` budget interpretation**. Spec §07/06 §7
  is the canonical cost model; we approximate "branches" as
  "nodes added to either visited map". If a future spec re-read
  forces a stricter definition, we can tighten then. Out-of-scope
  to argue further now.
- **Score blend choice**. Spec §09/04 §10 specifies the formula but
  not the exact aggregation (geo-mean vs arith-mean). We use
  geometric mean — it's the right behaviour when factors are in
  [0, 1] (penalises any near-zero factor). Document.
- **Top-1 on the wire**. Spec §09/04 §3 returns `Vec<Path>` (up to
  `max_results`). The wire frame currently carries one linear path;
  Phase 9 server lifts this into a stream.

## 8. Out-of-scope flags

- No explain mode.
- No implicit-start fallback.
- No multi-shard (cross-shard would require fan-out).
- No path-pruning by `best_n_per_endpoint` (we keep top_n globally,
  not per endpoint pair).

---

PLAN READY.
