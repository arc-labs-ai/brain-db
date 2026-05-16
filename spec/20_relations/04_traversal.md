# 20.04 Graph Traversal

How `RELATION_TRAVERSE` (opcode `0x0156`) explores the relation
graph from a starting entity, bounded by depth + branching factor +
cycle detection.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"Graph queries" — query
  patterns the traversal supports.
- [`./03_storage.md`](./03_storage.md) §7 — read paths the traversal
  consumes (BY_FROM / BY_TO prefix scans).
- [`./02_symmetric.md`](./02_symmetric.md) §4 — dual-index reads for
  symmetric relations.
- [`../28_knowledge_wire_protocol/07_relation_frames.md`](../28_knowledge_wire_protocol/07_relation_frames.md)
  §9 — wire shape.

## 1. The contract

```rust
pub fn traverse(
    rtxn: &ReadTransaction,
    start: EntityId,
    type_filter: &[RelationTypeId],   // empty = any type
    direction: TraversalDirection,
    max_depth: u8,                    // capped at MAX_DEPTH
    max_branching_factor: u32,         // per-level cap
    current_only: bool,
) -> Result<Vec<TraversalPath>, RelationOpError>;

pub enum TraversalDirection {
    Outgoing,
    Incoming,
    Both,
}

pub struct TraversalPath {
    pub steps: Vec<TraversalStep>,    // length = path depth
}

pub struct TraversalStep {
    pub relation_id: RelationId,
    pub from: EntityId,
    pub to: EntityId,
    pub relation_type_id: RelationTypeId,
    pub depth: u8,                    // 1-indexed from start
}
```

## 2. Algorithm

Iterative BFS — depth-first would risk stack blowup on degenerate
graphs.

```text
visited: HashSet<EntityId>      // cycle detection
frontier: Vec<(EntityId, path)> // current level
paths: Vec<TraversalPath>       // accumulator

visited.insert(start)
frontier.push((start, []))

for current_depth in 1..=max_depth:
    next_frontier = []
    for (node, path_so_far) in frontier:
        neighbours = expand(node, type_filter, direction, current_only)
        if neighbours.len() > max_branching_factor:
            neighbours.truncate(max_branching_factor)
            // Spec §6: log a tracing::warn for visibility.

        for (relation_id, other, rel_type) in neighbours:
            if visited.contains(other):
                continue  // cycle / re-entry, skip
            visited.insert(other)
            new_path = path_so_far.clone()
            new_path.push(TraversalStep {
                relation_id,
                from: if direction == Incoming { other } else { node },
                to:   if direction == Incoming { node }  else { other },
                relation_type_id: rel_type,
                depth: current_depth,
            })
            paths.push(TraversalPath { steps: new_path.clone() })
            next_frontier.push((other, new_path))

        if next_frontier.len() > max_branching_factor * (1 << current_depth):
            // Defensive cap on total state.
            return paths

    frontier = next_frontier
    if frontier.is_empty():
        break  // exhausted before max_depth

return paths
```

## 3. `expand`

Returns the neighbours of `node` reachable in one step via a
relation of any filtered type, in the requested direction.

```text
fn expand(node, type_filter, direction, current_only) -> Vec<(RelationId, EntityId, RelationTypeId)>:
    out = []
    if direction in {Outgoing, Both}:
        out += relation_list_from(rtxn, node, type_filter, current_only)
                .map(|r| (r.id, r.to_entity, r.relation_type))
    if direction in {Incoming, Both}:
        // For symmetric relations, list_from already returned the
        // edge (the relation is dual-indexed). list_to here would
        // duplicate — so for symmetric types, skip list_to.
        out += relation_list_to(rtxn, node, type_filter, current_only)
                .filter(|r| !r.is_symmetric_type)
                .map(|r| (r.id, r.from_entity, r.relation_type))

    // Dedup by (relation_id, other_entity).
    out.sort();
    out.dedup_by(|a, b| (a.0, a.1) == (b.0, b.1));
    out
```

## 4. Bounds

| Bound | Default | Cap | Rationale |
|---|---|---|---|
| `max_depth` | 3 | 5 | Spec §00 §"Graph queries". Past 3 hops, denormalise. |
| `max_branching_factor` | 1000 | 10_000 | Truncates pathological super-nodes. |
| Total visited | 100_000 | — | Soft cap; if exceeded, the traversal stops early and returns what it has. |
| Wall-clock | 500 ms | — | Soft cap enforced by the handler via the planner's query budget. |

Caller-supplied bounds are clamped to the caps server-side.

## 5. Cycle detection

The `visited` set covers entity revisits. Self-loops (edge from
`A → A`) are visited once at depth 1 then never again — the second
visit short-circuits at the `visited.contains` check.

Symmetric back-edges are handled implicitly: once `B` is added to
`visited` by visiting it from `A`, traversal won't re-add an edge
`B → A` of the same symmetric relation.

## 6. Branching-factor diagnostics

When `neighbours.len() > max_branching_factor`, the implementation
emits a `tracing::warn!` with the node id, depth, type filter, and
the actual neighbour count. Operators can spot super-nodes via the
warn log and decide whether to denormalise.

## 7. Path enumeration

Each unique node found at each depth contributes one
`TraversalPath`. A node reachable via two distinct paths is reported
once (the first time it's visited); the second path is dropped.

For "all paths between A and B" semantics, callers iterate the
returned set and post-process. Phase 23's query router will add
explicit "all paths up to depth N" if demand surfaces.

## 8. Wire response

`RELATION_TRAVERSE_RESP` (`0x01D6`) ships a single-frame snapshot in
v1: `Vec<TraversalPathWire>` + `total_paths` + `truncated_by_*`
flags. Streaming + cursor resumption land in phase 23 alongside
`STATEMENT_LIST` / `ENTITY_LIST` streaming.

## 9. Performance

For 1–2 hop queries: each `expand` runs a single `RELATIONS_BY_FROM`
(or `_TO`) prefix scan — O(log N + k) where k is the per-node
out-degree. Bounded by `max_branching_factor`.

For 3-hop queries with default branching (1000), the worst-case
visit count is 1 + 1000 + 1_000_000 = ~10^6 entities. Capped by
the `total_visited` soft cap (100k). Typical workloads have
out-degrees in the tens — visit counts stay under 10^3.

Spec §16/02 §2.4 targets: depth 1 p50 5ms, p99 25ms; depth 2 p50
15ms, p99 50ms; depth 3 p50 30ms, p99 100ms.

## 10. Open questions

See [`./06_open_questions.md`](./06_open_questions.md):

- Q4 — Should TRAVERSE return path-edge metadata (the relation_id +
  type at each hop), or just terminal-entity sets? Currently:
  path-edge metadata. Counter: simpler returns are cheaper.
- Q6 — Cross-shard traversal coordination.
- Q7 — Weight-aware shortest-path (uses `confidence` as edge weight).

## 11. Tests

Phase 18.5 unit tests cover:

- One-hop outgoing.
- One-hop incoming.
- Two-hop with type filter.
- Three-hop with mixed types.
- Cycle: `A → B → A` returns one path at depth 1, no re-entry.
- Self-loop: `A → A` visited once.
- Symmetric edge: `A ↔ B` reachable from either side.
- Branching cap: 1001-out-degree node truncates at 1000 + emits warn.
- Direction filter: Outgoing-only excludes incoming edges.
- Empty type filter = any type.
- `current_only = true` excludes superseded / tombstoned.
- Disconnected graph: traversal from isolated node returns empty.
