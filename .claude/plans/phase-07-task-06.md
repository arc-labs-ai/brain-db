# Sub-task 7.6 — REASON handler (evidence traversal)

REASON is the cousin of PLAN: same graph-walking machinery, different
edge set and aggregation. Phase 6.5 shipped the planner-side
`ReasonPlan`; the executor was deferred. This sub-task ships the
executor + the brain-ops handler.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §09/05 §1 | REASON: embed query → RECALL base → traverse Supports/Contradicts edges → aggregate |
| §09/05 §4 | "Supporting" = directly similar OR reached via SUPPORTS / DERIVED_FROM |
| §09/05 §5 | "Contradicting" = reached via CONTRADICTS (no vector-distance contradiction in v1) |
| §09/05 §6 | `confidence = (s - c) / (s + c)`; range [-1, 1] |
| §09/05 §11 | Zero base memories → empty supporting/contradicting + confidence=0 |
| §09/05 §17 | `evidence_strength = base_similarity × ∏ edge.weight` |
| §03/08/5 | Wire `ReasonResponseFrame { inferences, is_final, reason_status }` |
| §03/08/5 | Wire `InferenceStep { step_index, claim, supporting_memories, contradicting_memories, confidence, inference_kind }` |

## 1. Scope

**In scope for 7.6:**

- Add `brain-planner::executor::reason::execute_reason(plan: ReasonPlan,
  ctx: &ExecutorContext) -> Result<ReasonResult, ExecError>`.
- Add `ReasonResult` + `EvidenceItem` to `executor::result`.
- Extend `ExecutionResult` with a `Reason(ReasonResult)` variant; wire
  the `execute()` dispatch arm.
- Replace `crates/brain-ops/src/reason.rs::handle_reason` stub: call
  `plan_reason_inner` → `execute_reason` → map to wire
  `ReasonResponseFrame`.
- Tests: 5 brain-planner executor tests + 5 brain-ops handler tests.

**NOT in scope:**

- Vector-distance contradiction (spec §09/05 §12). Out of scope for v1.
- Explain mode (§13).
- Edge-weight–aware scoring beyond v1's `base_similarity × geomean(edge.weight)`
  pinned at 1.0 (LINK default). Same v1 limitation as PLAN's edge-weight
  factor — `Path` carries no per-edge weight yet.
- Multi-frame streaming. Single `ReasonResponseFrame` with `is_final=true`;
  one `InferenceStep` per response (the aggregated evidence picture).
- REASON-by-id (spec §15). Wire `ObservationInput::ByMemoryId` is
  supported; "future" REASON-by-id mode is a different shape.

## 2. Implementation decisions

### 2.1 Executor shape

```rust
pub async fn execute_reason(
    plan: ReasonPlan,
    ctx: &ExecutorContext,
) -> Result<ReasonResult, ExecError> {
    // 1. Resolve base memories.
    //    - ByMemoryId(id) → base = {id}, base_similarity = 1.0
    //    - ByText(t)      → embed + ANN search (k = aggregation.max_supporting + max_contradicting,
    //                        ef from the plan's base_recall when set)
    // 2. Traverse supports edges (Supports + DerivedFrom by default) up to depth.
    //    Use list_edges_from on each frontier node; cap at max_inferences
    //    summed across both traversals. Track distance from base.
    // 3. Traverse contradicts edges (Contradicts by default) the same way.
    // 4. Score each evidence node: score = base_similarity × decay(distance)
    //    where decay(d) = 1 / (1 + d). Edge-weight pinned at 1.0 (v1 gap).
    // 5. Apply confidence_threshold (per-item floor).
    // 6. Trim to max_supporting / max_contradicting.
    // 7. Aggregate: confidence = (sum_s - sum_c) / (sum_s + sum_c); 0 if denom = 0.
}
```

Bidirectional BFS isn't right here — we expand **outward** from the
base set along one set of edge kinds at a time. Single-side BFS with
parent pointers gives `edge_path` reconstruction.

### 2.2 `ReasonResult` shape

```rust
#[derive(Debug, Clone)]
pub struct ReasonResult {
    pub base_memories: Vec<MemoryId>,
    pub supporting: Vec<EvidenceItem>,
    pub contradicting: Vec<EvidenceItem>,
    pub confidence: f32,
    pub status: ReasonStatus,
}

#[derive(Debug, Clone)]
pub struct EvidenceItem {
    pub memory_id: MemoryId,
    pub score: f32,
    pub edge_path: Vec<EdgeKind>,
    pub distance: usize,
}

/// Mirrors the wire enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonStatus { Complete, BudgetExhausted, DepthLimitReached, Cancelled }
```

### 2.3 Direct-similarity supporting items (§09/05 §4)

The spec calls these "directly similar (no edge traversal)". In v1
the base set is composed of these. We surface them as supporting
items with `edge_path: vec![]` and `distance: 0`, in addition to the
edge-traversed items. They are subject to `confidence_threshold` so a
strict caller can drop them.

### 2.4 Aggregation

```rust
let sum_s: f32 = supporting.iter().map(|e| e.score).sum();
let sum_c: f32 = contradicting.iter().map(|e| e.score).sum();
let confidence = if sum_s + sum_c == 0.0 {
    0.0
} else {
    (sum_s - sum_c) / (sum_s + sum_c)
};
```

### 2.5 Budget + status mapping

- `max_inferences` caps total items (supporting + contradicting added
  to the visited maps). On trip → `ReasonStatus::BudgetExhausted`.
- `budget_wall_time_ms` checked once per BFS level.
  Wire-side: spec wire enum has no `Timeout` — map to
  `ReasonStatus::BudgetExhausted`.
- Hitting `depth` everywhere → `ReasonStatus::DepthLimitReached` (only
  if the BFS exhausted both queues at depth boundary without budget
  trip). Default success path → `ReasonStatus::Complete`.

### 2.6 Wire mapping (handler)

```rust
fn to_wire(result: ReasonResult, claim: String) -> ReasonResponseFrame {
    let supporting_memories: Vec<u128> =
        result.supporting.iter().map(|e| e.memory_id.into()).collect();
    let contradicting_memories: Vec<u128> =
        result.contradicting.iter().map(|e| e.memory_id.into()).collect();
    let inference = InferenceStep {
        step_index: 0,
        claim,
        supporting_memories,
        contradicting_memories,
        confidence: result.confidence,
        inference_kind: InferenceKind::EvidenceAccumulation,
    };
    ReasonResponseFrame {
        inferences: vec![inference],
        is_final: true,
        reason_status: Some(to_wire_status(result.status)),
    }
}
```

The wire `claim` field carries the original query text for
`ByText`; for `ByMemoryId` it carries an empty string (the memory
itself is identified by the `base_memories` would-be field — but
the wire frame doesn't include base memories. v1 gap: documented).
`inference_kind = EvidenceAccumulation` is the closest variant; the
spec enum's other variants (CausalExplanation, AnalogicalInference)
fit different sub-modes that v1 doesn't distinguish.

### 2.7 Per-item details that don't fit the wire

Spec §09/05 §3's `EvidenceItem` (Rust-side) carries `text`,
`edge_path`, and `distance`. The wire `InferenceStep` only carries
`supporting_memories` / `contradicting_memories` lists. v1 drops
`edge_path` and `distance` on the wire; the Rust-side `ReasonResult`
keeps them for tests + future server use.

## 3. Files written / changed

```
crates/brain-planner/src/executor/reason.rs      [new — execute_reason]
crates/brain-planner/src/executor/result.rs      [edit: + ReasonResult, EvidenceItem, ReasonStatus]
crates/brain-planner/src/executor/dispatch.rs    [edit: ExecutionPlan::Reason arm]
crates/brain-planner/src/executor/mod.rs         [edit: pub use reason::execute_reason]
crates/brain-planner/src/lib.rs                  [edit: re-export ReasonResult, EvidenceItem, ReasonStatus, execute_reason]
crates/brain-planner/tests/reason_executor.rs    [new — 5 tests]
crates/brain-planner/tests/dispatch.rs           [edit: drop "Reason variant Unsupported" test, replace with one that runs]
crates/brain-ops/src/reason.rs                   [edit: real handler]
crates/brain-ops/tests/reason.rs                 [new — 5 tests]
```

No new external deps.

## 4. Test plan

### 4.1 brain-planner executor tests (5)

1. `reason_supports_one_hop` — A. Encode A and B; link `A --Supports--> B`;
   REASON with `observation = ByMemoryId(A)`; assert B in supporting.
2. `reason_contradicts_one_hop` — link `A --Contradicts--> C`; assert C
   in contradicting, B from test 1 not contradicting.
3. `reason_confidence_balance` — three supports + two contradicts of equal
   weight → confidence > 0.
4. `reason_empty_base_returns_zero_confidence` — observation by memory
   id that exists but has no outgoing supports/contradicts → empty
   evidence + confidence=0 + status Complete.
5. `reason_max_inferences_caps` — many supporting hops, set
   `max_inferences=2` → supporting capped + status BudgetExhausted.

### 4.2 brain-ops handler tests (5)

1. `reason_full_pipeline_emits_one_inference` — supports + contradicts
   present; assert single InferenceStep with both lists, claim text
   set, confidence reflects balance.
2. `reason_no_evidence_returns_zero_confidence` — no edges → empty
   lists, confidence=0, status Complete on the wire.
3. `reason_invalid_depth_returns_plan_error` — `depth=0` → planner
   rejects (depending on planner validation) → wire InvalidRequest.
4. `reason_kind_categorisation_uses_evidence_accumulation` — verify the
   wire `inference_kind = EvidenceAccumulation` for v1.
5. `reason_by_memory_id_skips_recall` — observation ByMemoryId → base
   set is exactly that memory; supports edges traversed without an
   embed call.

LINK still isn't wired; tests insert edges via the brain-metadata
helper, same pattern as 7.5.

## 5. Verify checklist

- `cargo build -p brain-planner -p brain-ops` clean.
- `cargo test -p brain-planner -p brain-ops` — old totals + 10 new.
- `cargo clippy -p brain-planner -p brain-ops --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-planner -p brain-ops -- --check` no diff.

## 6. Commit message (draft)

```
feat(brain-planner,brain-ops): REASON executor + handler (sub-task 7.6)

Ships the evidence-traversal executor (Phase 6.5 deferred) and the
brain-ops handler.

brain-planner:
- executor::reason::execute_reason: outward BFS from the base set
  along the plan's supports_traversal.edge_kinds (Supports +
  DerivedFrom by default) and contradicts_traversal.edge_kinds
  (Contradicts), separately. Each evidence item carries the
  reconstructed edge_path + distance from the base.
- Base resolution: ByMemoryId → {id} with base_similarity=1.0;
  ByText → embed + ANN search (k = max_supporting + max_contradicting).
- Score: base_similarity × decay(distance), decay=1/(1+d). Edge-weight
  is pinned at 1.0 (v1 gap; same as PLAN — weights aren't plumbed
  through yet).
- Aggregate confidence = (sum_s - sum_c) / (sum_s + sum_c), 0 if
  the denominator is zero (spec §09/05 §6).
- ExecutionResult gains Reason(ReasonResult); the Reason dispatch
  arm replaces ExecError::Unsupported. The brain-planner dispatch
  test that asserted "Unsupported" is rewritten.
- Result types: ReasonResult { base_memories, supporting,
  contradicting, confidence, status }; EvidenceItem { memory_id,
  score, edge_path, distance }; ReasonStatus mirrors the wire enum.

brain-ops:
- handle_reason: plan_reason_inner → execute_reason → map to one
  InferenceStep with supporting/contradicting memory id lists and
  the original claim text (empty for ByMemoryId observations;
  v1 gap). inference_kind = EvidenceAccumulation. Status maps
  through to the wire's ReasonStatus.
- Single-frame for v1.

Tests: 5 brain-planner executor tests (supports one-hop, contradicts
one-hop, confidence balance, empty base, max_inferences cap); 5
brain-ops handler tests (full pipeline, no-evidence, invalid depth,
kind categorisation, ByMemoryId observation). Edges inserted via
brain-metadata::tables::edge::link until 7.8 wires LINK.

No new external deps.
```

## 7. Risks

- **Empty `claim` for ByMemoryId.** Documented. Wire frame's `claim`
  field is `String` (not `Option`). A future revision can resolve
  by fetching the memory's text into the claim slot.
- **Single InferenceStep per response.** Spec §09/05 §3 returns
  evidence items, not "inferences". The wire shape uses InferenceStep
  as a generic envelope; v1 packs one envelope with both lists. The
  wire-level edge_path / distance per evidence item is lost on the
  wire (preserved on the Rust side).
- **Score decay choice.** `1/(1+d)` is a reasonable default; the spec
  doesn't pin a specific decay curve. Document.
- **Reason status `Cancelled` is unused.** Wire enum has it for
  client-initiated cancellation, which v1 doesn't support yet.

## 8. Out-of-scope flags

- No vector-distance contradiction.
- No edge-weight–aware scoring (pinned 1.0).
- No multi-frame streaming.
- No explain mode.
- No `EvidenceItem` exposure on the wire (just memory-id lists).

---

PLAN READY.
