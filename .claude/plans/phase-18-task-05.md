# 18.5 — Relation traversal

BFS over the relation graph backing `RELATION_TRAVERSE` (`0x0156`).
Per spec §20/04 — iterative BFS with depth + branching caps,
visited-set cycle detection, and direction filter.

## Spec refs

- `spec/20_relations/04_traversal.md` — algorithm, defaults, bounds.
- `spec/20_relations/02_symmetric.md` §4 — dual-indexed reads for
  symmetric edges.

## Files written

- `crates/brain-metadata/src/relation_traversal.rs` — new module.
- `crates/brain-metadata/src/lib.rs` — re-exports.

## Types

```rust
pub enum TraversalDirection {
    Outgoing,
    Incoming,
    Both,
}

pub struct TraversalConfig {
    pub max_depth: u8,                  // capped at MAX_DEPTH = 5
    pub max_branching_factor: u32,      // capped at MAX_BRANCHING = 10_000
    pub current_only: bool,
}

pub const DEFAULT_MAX_DEPTH: u8 = 3;
pub const DEFAULT_MAX_BRANCHING: u32 = 1_000;
pub const MAX_DEPTH: u8 = 5;
pub const MAX_BRANCHING: u32 = 10_000;

pub struct TraversalStep {
    pub relation_id: RelationId,
    pub from: EntityId,
    pub to: EntityId,
    pub relation_type: RelationTypeId,
    pub depth: u8,
}

pub struct TraversalPath {
    pub steps: Vec<TraversalStep>,
}
```

## Surface

```rust
pub fn traverse(
    rtxn: &ReadTransaction,
    start: EntityId,
    type_filter: &[RelationTypeId],
    direction: TraversalDirection,
    config: &TraversalConfig,
) -> Result<Vec<TraversalPath>, RelationOpError>;
```

Empty `type_filter` = any type. `config` field values exceeding the
caps are clamped server-side.

## Algorithm

Per spec §20/04 §2:

```text
visited: HashSet<EntityId>
frontier: Vec<(EntityId, partial_path)>
paths:   Vec<TraversalPath>

visited.insert(start)
frontier.push((start, []))

for current_depth in 1..=clamped(config.max_depth, MAX_DEPTH):
    next_frontier = []
    for (node, partial) in frontier:
        neighbours = expand(node, type_filter, direction, current_only)
        if neighbours.len() > clamped_branching:
            tracing::warn!(...super-node truncated)
            neighbours.truncate(clamped_branching)

        for (relation_id, other, rel_type) in neighbours:
            if visited.contains(&other): continue
            visited.insert(other)
            let mut new = partial.clone()
            new.push(TraversalStep { relation_id, from, to, rel_type, depth })
            paths.push(TraversalPath { steps: new.clone() })
            next_frontier.push((other, new))

    frontier = next_frontier
    if frontier.is_empty(): break

return paths
```

`expand` calls `relation_list_from / _to` with the type filter +
`current_only`. For symmetric relations the dual-index population
from 18.4 already makes both directions reachable via `list_from`
— `expand` only calls `list_to` for asymmetric direction-flipped
queries, dedupe'ing as needed.

## Tests (~10)

Build entities A, B, C, D, E (already exist via tests harness).
Register `knows` (ManyToMany asymmetric) + `co_authored`
(ManyToMany symmetric).

- `traverse_one_hop_outgoing` — A→B; depth 1 returns 1 path.
- `traverse_one_hop_incoming` — query from B, direction Incoming.
- `traverse_two_hop` — A→B→C; depth 2 returns paths of length 1
  AND 2 (both depths emitted).
- `traverse_three_hop_with_default_config` — full chain.
- `traverse_depth_cap_clamped` — request depth 99 → clamped to 5.
- `traverse_branching_truncated_and_warns` — single node with > 1000
  out-edges; truncation kicks in.
- `traverse_cycle_short_circuits` — A→B→A; visits B at depth 1,
  doesn't re-emit A.
- `traverse_self_loop_visits_once` — A→A; visited once at depth 1.
- `traverse_symmetric_reachable_from_either_side` — co_authored(A, B);
  query from A finds B and vice versa.
- `traverse_type_filter` — knows + co_authored both present; filter
  to just `knows`.
- `traverse_current_only_excludes_tombstoned`.
- `traverse_direction_outgoing_only` excludes incoming edges.

## Verify

```
cargo zigbuild --target x86_64-unknown-linux-gnu -p brain-metadata --tests
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu -p brain-metadata --all-targets -- -D warnings
```

## Out of scope

- Cross-shard traversal (phase 23).
- Weight-aware shortest-path (post-v1).
- Streaming TRAVERSE response (phase 23).
- TRAVERSE total-visited soft cap (added as a const but no error
  exit; phase 23 may turn it into a hard cap + truncation flag).

## Commit message draft

```
feat(brain-metadata): relation traversal (18.5)

Iterative BFS over the relation graph per spec §20/04. ...
```
