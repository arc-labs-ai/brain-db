# 08.05 PLAN and REASON Planning

How the planner builds plans for the higher-level cognitive operations.

## 1. Higher-level vs lower-level

ENCODE, RECALL, FORGET, LINK are direct operations on the substrate's primitives. PLAN and REASON are higher-level — they compose multiple primitive operations.

- **PLAN**: given a goal, find a sequence of memories that connect to it via the graph.
- **REASON**: given a query, find supporting and contradicting memories.

These don't introduce new primitive types of work; they orchestrate RECALL and graph traversal.

## 2. The PLAN request shape

```rust
struct PlanRequest {
    goal_text: String,           // What to plan toward
    agent_id: AgentId,
    starting_state: Option<String>,  // Current state
    max_depth: usize,            // How many graph hops
    max_results: usize,          // Total plan elements
    edge_kinds: Vec<EdgeKind>,   // Which edges to follow (default: CAUSED, FOLLOWED_BY)
    request_id: Option<RequestId>,
}
```

The semantics: "starting from memories similar to the current state, traverse the graph along edges of the specified kinds, returning paths to memories similar to the goal".

## 3. The PLAN execution

The planner builds a multi-step plan:

```rust
struct PlanPlan {
    embedding: EmbeddingStep,             // Embed both starting_state and goal
    starting_recall: RecallStep,          // Find memories near starting_state
    goal_recall: RecallStep,              // Find memories near goal
    traversal: TraversalStep,             // BFS/DFS along edges
    scoring: ScoringStep,                 // Rank found paths
    response: ResponseStep,
}
```

Each step is a sub-task; they run in sequence with some parallelism (the two RECALLs can be parallel).

## 4. PLAN traversal

The traversal step:

```rust
fn traverse(
    starts: Vec<MemoryId>,
    goals: Vec<MemoryId>,
    edge_kinds: Vec<EdgeKind>,
    max_depth: usize,
) -> Vec<Path> {
    // Bidirectional BFS:
    //   forward from starts (following edges in their direction)
    //   backward from goals (following edges against their direction)
    //   intersect to find paths
    
    let mut forward_frontier = starts.clone();
    let mut backward_frontier = goals.clone();
    let mut found_paths = Vec::new();

    for depth in 0..max_depth {
        let next_forward = expand_frontier(forward_frontier, edge_kinds, Direction::Forward);
        let next_backward = expand_frontier(backward_frontier, edge_kinds, Direction::Backward);

        // Check intersection
        for memory in next_forward.intersect(&next_backward) {
            found_paths.push(reconstruct_path(starts, memory, goals));
        }
        if !found_paths.is_empty() && depth >= 1 {
            break;
        }

        forward_frontier = next_forward;
        backward_frontier = next_backward;
    }
    found_paths
}
```

The traversal uses the metadata store's `edges_out` and `edges_in` tables for graph queries.

## 5. Bidirectional BFS

For typical agent graphs (many memories, sparse connectivity), bidirectional BFS is much more efficient than unidirectional. Each direction explores `b^(d/2)` nodes (branching factor `b`, total depth `d`), versus `b^d` for unidirectional.

For typical graphs with `b ≈ 8` and `d ≈ 4`, that's 64+64 = 128 nodes vs 4096. ~30× savings.

## 6. Path scoring

Multiple paths may exist between starts and goals. The substrate scores them:

```rust
fn score_path(path: &Path) -> f32 {
    let length_score = 1.0 / (path.length as f32);
    let edge_score = path.edges.iter().map(|e| e.weight).product();
    let salience_score = path.memories.iter().map(|m| m.salience).product().powf(1.0 / path.memories.len() as f32);
    
    length_score * edge_score * salience_score
}
```

Higher score = more useful path. The substrate returns the top N paths.

## 7. Search vs traversal

The starting and goal RECALLs use HNSW for vector similarity. The traversal uses the metadata's edge tables for graph hops.

These are different storage layers:
- HNSW: in-memory vector index.
- Edge tables: redb B-trees.

The planner orchestrates the alternation: vector similarity at the boundaries, graph hops in the middle.

## 8. The REASON request shape

```rust
struct ReasonRequest {
    query_text: String,
    agent_id: AgentId,
    max_supporting: usize,       // Default 5
    max_contradicting: usize,    // Default 5
    request_id: Option<RequestId>,
}
```

REASON returns:
- Memories supporting the query (similar in vector space, with positive supports/derived_from edges).
- Memories contradicting the query (with CONTRADICTS edges, or significantly different in vector space with high salience).

## 9. The REASON execution

```rust
struct ReasonPlan {
    embedding: EmbeddingStep,
    base_recall: RecallStep,                  // Top similar memories
    supports_traversal: TraversalStep,        // Follow SUPPORTS, DERIVED_FROM edges
    contradicts_traversal: TraversalStep,     // Follow CONTRADICTS edges
    aggregation: AggregationStep,             // Score and rank
    response: ResponseStep,
}
```

The two traversals run in parallel after the base RECALL.

## 10. The "explainable" response

REASON's response includes evidence:

```rust
struct ReasonResponse {
    supporting: Vec<EvidenceItem>,
    contradicting: Vec<EvidenceItem>,
    confidence: f32,    // Aggregate; based on relative weights
}

struct EvidenceItem {
    memory_id: MemoryId,
    text: Option<String>,
    score: f32,
    edge_path: Vec<EdgeKind>,  // How this connects to the query
}
```

This shape makes the reasoning interpretable — agents can show their work.

## 11. Cost considerations

PLAN and REASON do more work than RECALL:

| Operation | Typical latency |
|---|---|
| RECALL (K=10) | 10-15 ms |
| PLAN (max_depth=4) | 30-100 ms |
| REASON (default) | 30-50 ms |

The latency is mostly graph traversal. For deep traversals (max_depth > 5), latency grows quickly. The planner caps depth conservatively.

## 12. Caching of intermediate results

For PLAN and REASON, intermediate results (e.g., the starting RECALL's outputs) might be useful in subsequent calls. The substrate doesn't cache them in v1 — each call is independent.

If a workload makes many PLAN/REASON calls with the same starting state, an external caching layer can amortize.

## 13. The "max_results" cap

PLAN and REASON limit the response size:

- PLAN: `max_results` total path nodes returned.
- REASON: `max_supporting + max_contradicting` evidence items.

These caps protect against pathological queries (a goal connected to thousands of paths). The defaults are conservative: 10-20 results.

## 14. Plan validity

The planner checks:

- max_depth ≤ 10 (hard limit; deeper isn't useful and is expensive).
- edge_kinds are valid.
- max_results ≤ 100.

Out-of-bounds → error response.

## 15. The "explain" facility

Both PLAN and REASON benefit from explainability. The response can include:

- The intermediate RECALL results.
- The traversal path.
- The scoring breakdown.

This is opt-in via `explain=true` in the request. It increases response size but helps debugging.

## 16. The "no result" path

If the traversal finds no paths (PLAN) or no evidence (REASON), the response is empty (or a partial/uncertain answer). This isn't an error — it just means the substrate has nothing to offer.

For workloads that need confident answers, the response includes a `confidence` score the agent can threshold on.

---

*Continue to [`06_forget_planning.md`](06_forget_planning.md) for FORGET planning.*
