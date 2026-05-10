# 09.04 PLAN

The PLAN primitive: find paths through the memory graph from a starting state to a goal.

## 1. Semantic contract

```
PLAN(goal_text, starting_state, agent_id, max_depth, edge_kinds, ...) → Vec<Path>
```

The substrate:

1. Embeds the starting state and goal.
2. Finds memories near the starting state and memories near the goal.
3. Traverses the edge graph from start side and goal side (bidirectional BFS).
4. Returns paths where the two sides intersect.

A "path" is a sequence of memories connected by edges, leading from a start memory to a goal memory.

## 2. The arguments

### goal_text

What the agent is planning toward. A description of the desired end state.

### starting_state

What the agent is currently doing or thinking. A description of the present state.

If unspecified, the substrate uses recent high-salience memories as starting points (defaulting to the agent's "implicit current state").

### agent_id

The owning agent. Plans are scoped to this agent's memories and edges.

### max_depth

How many graph hops to traverse. Default 4; max 10.

Greater depth = more thorough search but more cost. The substrate caps at 10 to avoid pathological queries.

### max_results

How many paths to return. Default 5; max 100.

### edge_kinds

Which edge types to traverse. Default: `CAUSED, FOLLOWED_BY, DERIVED_FROM, PART_OF`. These are the "actionable" edges that suggest forward movement.

The agent can specify a different list — e.g., `[REFERENCES]` for citation chains.

### scoring

Optional scoring weights:

```rust
struct PlanScoring {
    length_weight: f32,        // Default 1.0; longer paths penalized
    edge_weight_weight: f32,   // Default 1.0; edge weights matter
    salience_weight: f32,      // Default 0.5; salient memories preferred
}
```

## 3. The response

```rust
struct PlanResponse {
    paths: Vec<Path>,
    starting_memories: Vec<MemoryId>,    // What was used as start
    goal_memories: Vec<MemoryId>,        // What was used as goal
    confidence: f32,                     // Aggregate confidence
}

struct Path {
    nodes: Vec<MemoryId>,                // In order from start to goal
    edges: Vec<EdgeKind>,                // Edge types between nodes
    score: f32,                          // Higher = better path
    length: usize,                       // Number of hops
}
```

Paths are sorted by score, descending.

## 4. Path semantics

A path of length 3:

```
start_memory --CAUSED--> A --FOLLOWED_BY--> B --PART_OF--> goal_memory
```

The path connects (start_memory ≈ starting_state) to (goal_memory ≈ goal). Intermediate nodes are stepping stones.

The score reflects:
- Path length (shorter is generally better).
- Edge weights along the path.
- Salience of intermediate nodes.

## 5. Bidirectional BFS

The traversal:
- Forward: from each starting memory, follow edges in their forward direction.
- Backward: from each goal memory, follow edges in their reverse direction.
- Intersect: when forward and backward frontiers meet, a path is found.

Bidirectional cuts the cost from O(b^d) to O(b^(d/2)), where b is branching factor and d is depth.

For typical agent graphs (b≈8, d=4): ~64 nodes explored each way vs ~4000 unidirectional.

## 6. The "no paths found" case

If no path exists within max_depth, the response has empty `paths`:

- `paths: []`
- `starting_memories` and `goal_memories` are populated (so the agent can see what was attempted).
- `confidence: 0.0`.

This tells the agent: "I see your start and goal, but I can't connect them in my memory."

## 7. The "starting_state is empty" case

When starting_state is unspecified, the substrate uses:

```
top-K most salient recent memories (default K=5, recency window 24h)
```

This is "what's on the agent's mind right now" — a soft proxy for the agent's current context.

For agents that want explicit control, always pass `starting_state`.

## 8. The "goal not encoded yet" case

The goal is a text description; it doesn't need to be a stored memory. The substrate embeds the goal text and finds nearby memories as anchors for the goal side of the BFS.

If no memory is similar to the goal (low scores), the BFS has weak goal anchors. PLAN may return no paths.

## 9. Edge direction semantics

Edges have a defined direction (see [02.06 Edges](../02_data_model/06_edges.md)):

| Edge kind | Forward semantic |
|---|---|
| CAUSED | source led to target |
| FOLLOWED_BY | source then target |
| DERIVED_FROM | target derived from source |
| PART_OF | source is part of target |
| REFERENCES | source mentions target |
| ... | ... |

PLAN's forward traversal follows edges in their forward direction; backward traversal goes against. So a path:

```
A --CAUSED--> B
B --FOLLOWED_BY--> C
```

is "A caused B, then B was followed by C". Logical forward sequence.

## 10. Path scoring

```
score = length_score × edge_score × salience_score

length_score = 1 / path_length         (shorter is better)
edge_score = product(edge.weight)      (high-confidence edges matter)
salience_score = geomean(node.salience) (salient intermediate nodes preferred)
```

The score is in (0, 1]. The substrate returns paths sorted by score.

The agent can re-rank in its own layer with custom weights if the default doesn't fit.

## 11. The "best_n_per_endpoint" rule

When multiple paths exist between the same start and goal, the substrate returns up to N best (default 3 per start-goal pair). This avoids returning many similar paths.

For diverse-paths use cases (the agent wants alternatives, not just the best), the agent can request more (`max_results`) and the substrate picks across endpoints.

## 12. Latency

For typical PLAN with max_depth=4:

- p50: ~30-50 ms.
- p99: ~80-100 ms.

The latency is dominated by:
- Two embeddings (start + goal, parallel): ~10 ms.
- Two RECALLs (parallel): ~10 ms.
- Graph traversal (~10-20 ms for typical graphs).

For deeper PLAN (max_depth=8): can reach 200+ ms. The substrate's cost-budget check (in [08.07 Cost Estimation](../08_query_planner/07_cost_estimation.md)) may reject overly-expensive plans.

## 13. The "explain" option

With `explain=true`, the response includes:

- The intermediate frontier expansions.
- Paths that were considered but didn't make the top results.
- The scoring breakdown for each returned path.

Useful for debugging or showing reasoning to a human.

## 14. The "actionable edges" default

The default `edge_kinds: [CAUSED, FOLLOWED_BY, DERIVED_FROM, PART_OF]` are the "actionable" or "forward" edges. They suggest progression.

Other edges (REFERENCES, SIMILAR_TO, SUPPORTS, CONTRADICTS) are more associative; they're not great for planning.

For exploratory queries (e.g., "what's related to my goal?"), use REASON instead of PLAN.

## 15. The "stale plan" caveat

A PLAN's results reflect the current state of the graph. If the agent encodes new memories or links between calls, subsequent PLANs may give different results.

The substrate doesn't cache PLAN results. Each call sees the current graph (eventual consistency, ~10 ms publication lag).

## 16. The "self-loop" guard

The traversal avoids self-loops:

- A path doesn't visit the same memory twice.
- The forward and backward expansions skip already-visited nodes.

This prevents infinite loops in cyclic graphs.

## 17. Failure modes

### NoPathsFound

Not technically a failure — the response just has empty `paths`. The agent should handle this gracefully.

### QueryTooExpensive

If the planner estimates the PLAN exceeds the cost budget (typically due to high max_depth + dense graph), it returns this error.

The agent should reduce max_depth or narrow the start/goal.

### Timeout

If the traversal takes too long, the substrate aborts and returns whatever paths it found so far. The response is marked `partial: true`.

## 18. The "PLAN as discovery" use case

PLAN is most useful when:

- The agent has built up a graph of CAUSED, FOLLOWED_BY, etc. relationships.
- The agent has a clear goal and wants to find a path.
- The graph is dense enough that paths exist.

For sparse graphs (few edges), PLAN often returns no paths. The agent should use RECALL or REASON instead.

For text-only memories without edges, PLAN is mostly useless. The graph is the planning substrate.

---

*Continue to [`05_reason.md`](05_reason.md) for REASON.*
