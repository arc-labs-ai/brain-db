# PLAN

Find paths through the memory graph from a starting state to a
goal. Bidirectional BFS over edges, returning stepping stones.

**Opcode:** `PlanReq = 0x0022` / `PlanResp = 0x00A2` (streaming).
**Spec:** §09/04. **Source:** `crates/brain-ops/src/ops/plan.rs`.

## Request fields

| Field | Type | Required | Notes |
|---|---|---|---|
| `goal_text` | `String` | yes | Goal description. Embedded; anchor memories found via RECALL-style search. |
| `starting_state` | `Option<String>` | no | Present-state description. If omitted: top-5 salient memories from the last 24 h serve as implicit start anchors. |
| `agent_id` | `AgentId` | yes | Scopes to the agent's memories + edges. |
| `max_depth` | `u32` | no | Hop budget. Default 4, max 10. |
| `max_results` | `u32` | no | Paths to return. Default 5, max 100. |
| `edge_kinds` | `Vec<EdgeKind>` | no | Edges to traverse. Default: `[CAUSED, FOLLOWED_BY, DERIVED_FROM, PART_OF]` (actionable). |
| `scoring` | `Option<PlanScoring>` | no | Weights for path length, edge weight, salience. |
| `budget_wall_time_ms` | `u32` | no | Soft wall-time cap. Server may return `partial = true` if hit. |

## Response fields

Streaming.

| Field | Type | Notes |
|---|---|---|
| `paths` | `Vec<Path>` | Up to `max_results`, sorted by `score` descending. |
| `starting_memories` | `Vec<MemoryId>` | Anchors discovered for the start. |
| `goal_memories` | `Vec<MemoryId>` | Anchors discovered for the goal. |
| `confidence` | `f32` | Aggregate confidence across returned paths. |
| `partial` | `bool` | `true` if budget or wall-time cap was hit. |

### `Path`

| Field | Type | Notes |
|---|---|---|
| `nodes` | `Vec<MemoryId>` | In order, start → goal. |
| `edges` | `Vec<EdgeKind>` | One per hop. |
| `score` | `f32` | Higher = better. |
| `length` | `usize` | Hop count. |

### Path scoring

```
score = (1 / path_length) × product(edge.weight) × geomean(node.salience)
```

`PlanScoring` lets callers override the three weights.

## Side effects

None. PLAN is read-only.

## Errors

| Code | When |
|---|---|
| `BudgetTooLarge` | `max_depth > 10` or `max_results > 100`. |
| `InvalidArgument` | Invalid `edge_kinds` value. |
| `IndexError` | Anchor RECALL failed. |
| `MetadataError` | Edge traversal hit a redb error. |

Empty `paths` is not an error — it just means no path was found.

## Idempotency

N/A — read-only. Results reflect current graph state with the
usual ~10 ms eventual-consistency lag for recent ENCODEs.

## Performance target

Spec §16/02 §5:

| Workload | p50 | p99 |
|---|---|---|
| `max_depth = 4`, typical graph | 30–50 ms | 80–100 ms |
| `max_depth = 8` | up to 200 ms | up to 500 ms |

Dominated by two embeddings (start + goal, in parallel: ~10 ms) +
two RECALLs (~10 ms each) + graph traversal (~10–20 ms).

## Substrate vs knowledge

PLAN operates purely on the substrate edge graph. The knowledge
layer is not consulted. The same opcode works in both modes.

## See also

- [`reason.md`](reason.md) — same graph machinery, different question.
- [`recall.md`](recall.md) — the anchor-finding mechanism.
- [`../../architecture/05-redb-metadata.md`](../../architecture/05-redb-metadata.md) — edge storage layout.

**Spec:** §09/04.
