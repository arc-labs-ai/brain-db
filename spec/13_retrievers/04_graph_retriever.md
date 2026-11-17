# 13.04 GraphRetriever

Normative spec for the graph retriever. Sits
beside `00_purpose.md`, `01_rrf_fusion.md`, `02_lexical_retriever.md`,
and `03_semantic_retriever.md`.

The GraphRetriever operates on the entity-relation-statement
graph laid out in §02 (entities, statements, relations) and
stored per §10 (table layout). It walks the redb tables for
entities, statements, and relations.

## 1. Surface

Trait shape:

```rust
pub trait GraphRetriever: Send + Sync {
    fn retrieve(
        &self,
        query: &GraphQuery,
        config: &GraphRetrieverConfig,
    ) -> Result<Vec<RankedItem>, GraphError>;
}

pub enum GraphQuery {
    /// BFS from `anchor` outward up to `depth`. Three knobs:
    /// direction of relation traversal, optional whitelist of
    /// relation types, and whether to emit statements about
    /// the visited entities alongside the entities + relations.
    Star {
        anchor: EntityId,
        depth: u8,
        direction: Direction,
        relation_types: Option<Vec<RelationTypeId>>,
        include_statements: bool,
    },
    /// Find paths from `from` to `to` up to `max_depth`.
    /// Returns relations + intermediate entities on at least
    /// one shortest path.
    Path {
        from: EntityId,
        to: EntityId,
        max_depth: u8,
    },
    /// Closed neighbourhood: every entity / relation /
    /// statement reachable within `depth` hops of `anchor`.
    Subgraph {
        anchor: EntityId,
        depth: u8,
    },
}

pub enum Direction {
    Outgoing,
    Incoming,
    Both,
}

pub struct GraphRetrieverConfig {
    pub top_k: usize,         // default 64
    pub max_depth: u8,        // default 3; hard cap 5 (§4)
    pub max_branching: u32,   // default 200 (per-node child cap, §4)
    pub timeout_ms: u32,      // config default 50; the plan sets 1 s (§8)
}
```

`retrieve()` is **read-only**. No side effects.

`RankedItem.id` is the shared `RankedItemId` of §13/02 §6. The graph
retriever is the only one that emits the `Entity` and `Relation`
variants — the semantic and lexical lanes can only produce the two
corpus variants:

```rust
pub enum RankedItemId {
    Memory(MemoryId),         // semantic, lexical, graph
    Statement(StatementId),   // semantic, lexical, graph
    Entity(EntityId),         // graph only
    Relation(RelationId),     // graph only
}
```

Fusion is therefore mixed-type by construction: a single fused list can
carry a memory, a statement and an entity, and RRF compares them only
by rank within their producing lane.

## 2. Proximity scoring

Score is a function of hop distance from the anchor (`Star` /
`Subgraph`) or path length (`Path`):

```
score = 1.0 / ((hop_distance as f32) + 1.0)
```

- Anchor itself (`hop_distance == 0`) is **omitted from the
  result** in `Star` and `Subgraph` (the caller already has
  the anchor). For `Path`, the `from` and `to` endpoints are
  emitted with `score = 1.0`.
- Score ties (same hop distance) are broken by deterministic
  entity / relation / statement id ascending sort.

The score is internal to the graph retriever. RRF fusion uses
rank, not score (§13/01).

## 3. Three modes

### Star

BFS from `anchor` along relations matching the optional
`relation_types` whitelist in `direction`. At each hop:

1. Expand the current frontier of entities by following
   matching relations.
2. For each newly-visited entity, emit a `RankedItemId::Entity`
   with `hop_distance` of the current ring.
3. For each traversed relation, emit a `RankedItemId::Relation`
   with `hop_distance` of the source-side ring.
4. If `include_statements`, emit `RankedItemId::Statement` for
   every statement whose `subject` is one of the visited
   entities. The statement's `hop_distance` is the subject's.

Loops are broken via a visited-set keyed by `EntityId`.

### Path

Find paths from `from` to `to` up to `max_depth`. Algorithm:
single-source BFS from `from` with early termination when `to`
is dequeued; if multiple paths of the same length exist, all
are emitted (up to `top_k`).

Bidirectional BFS is a future optimisation; the current single-
direction BFS at `max_depth ≤ 5` is bounded enough that the
asymmetric cost is acceptable.

Emit:
- `to` and `from` with `score = 1.0`.
- Each intermediate entity on a path with `score = 1/(d+1)`
  where `d` is its position from `from`.
- Each relation on a path with the source entity's score.

If no path within `max_depth`, returns `Ok(vec![from-entity-row, to-entity-row])`
with score `0.5` for each (i.e. half-credit; both anchors
still surface for the caller). Clients distinguish this from
"path found" via `RetrieverContribution.contributing_retrievers`
in §13/05.

### Subgraph

Closed neighbourhood of `anchor` within `depth` hops. Emits
every entity, relation, and statement reachable. No
`relation_types` filter (it's the whole subgraph by
definition); to filter, use `Star`.

## 4. Depth + branching caps

`max_depth`:

- Default 3.
- Hard cap **5** (`GraphError::QueryParseFailed` if exceeded).
- Counted in hops; the anchor is hop 0.

`max_branching`:

- Per-node children cap, default 200
  (`crates/brain-index/src/graph_retriever.rs:21`).
- When a node has more than `max_branching` children, the BFS
  truncates the excess.
- Truncation is deterministic — children are sorted by
  `RelationId` ascending and the first `max_branching` are
  followed.
- **Truncation is silent.** This retriever emits no metric and no
  warning when it drops a super-node's excess children, so an
  operator cannot tell from the scrape surface that a result was
  truncated rather than complete. The wider `RELATION_TRAVERSE` path
  does warn on its own cap (see
  [`./06_post_processing.md`](./06_post_processing.md)
  §"Branching-factor diagnostics"); the two caps are separate
  mechanisms with separate defaults, and only one of them is
  observable.

Total result cap is `config.top_k`. Once `top_k` results are
collected, BFS terminates early (best-effort: BFS rings closer
to the anchor are emitted before rings further away).

## 5. Pre-filter push-down

Per §13/05 §"Filter as retriever vs filter":

| Filter | Push-down |
|---|---|
| `relation_types` | Pushed into BFS expansion — relations not matching the whitelist are not followed. |
| `direction` | Same. |
| `include_statements` | Same — skips the statement table read entirely when false. |
| Post-traversal `kind` / `predicate_id` on emitted statements | Applied to the result slice (statement set is small enough). Push-down via the statement-by-predicate redb index is a deferred polish. |

## 6. Returns + idempotency

Same `RankedItem` shape as §13/02:

- `Vec<RankedItem>` ordered by ascending `hop_distance` (i.e.
  descending score), then by id for ties.
- `rank` 1-based, dense.
- `score` computed per §2.
- `snippet` always `None`.

**Idempotency:** read-only; identical inputs return identical
outputs between commits.

## 7. Errors

`GraphError` taxonomy:

| Variant | Trigger | Visible to clients |
|---|---|---|
| `AnchorNotFound` | The anchor entity (or `from` / `to`) does not exist in `ENTITIES_TABLE`. | Yes — client bug or stale id. |
| `MaxDepthExceeded` | `query.depth` or `max_depth` > 5. | Yes — client bug. |
| `IndexUnavailable` | Entity / relation / statement table read failed (e.g. redb corruption). | Yes — operator. |
| `Timeout` | Traversal exceeded `config.timeout_ms`. | Yes — degraded. |
| `Internal(String)` | Anything else. Logged at error level. | Yes — opaque. |

As with the other two lanes, the planned budget is a soft 1 s: the
traversal is recorded as `Timeout` and warned about, not cancelled, and
a late result still reaches fusion
(`crates/brain-planner/src/retrieval/planner/logic.rs:55-65`).

## 8. Performance

Pinned in §19 (perf targets):

| Operation | p50 | p99 |
|---|---|---|
| `Star` depth=1 (≤ 200 children) | 5 ms | 20 ms |
| `Star` depth=2 | 10 ms | 40 ms |
| `Path` (small graph, depth ≤ 3) | 8 ms | 30 ms |
| `Subgraph` depth=2 | 15 ms | 60 ms |

Performance is bounded by the entity + relation table reads.
Graphs with deep clustering or high branching are slower; the
caps in §4 keep worst-case bounded.

## 9. Limitations

- **`EntityMerge.merged_into` redirects NOT followed
  automatically.** The router resolves the anchor before
  invoking. If the caller passes a merged-away entity, the
  retriever returns `Ok(vec![])` (the entity exists but has no
  outgoing relations of its own; the merge target is invisible
  to this retriever).
- **Statement-object recursion NOT expanded.** If a statement's
  object is another `Statement(StatementId)` (meta-statement,
  §02/10 §"StatementObject"), the retriever emits the outer
  statement but does NOT recurse into the inner statement's
  subject. Deferred to a future version.
- **Cross-shard traversal NOT supported.** Per-shard retrieval
  only; the router fans out and merges.

## 10. Boundaries

- GraphRetriever does NOT write to the graph — read-only.
- GraphRetriever does NOT decide direction or relation
  whitelist heuristically — the router translates the query's
  intent into the explicit `GraphQuery` shape.
- GraphRetriever does NOT fuse — RRF (§13/01) does.
- GraphRetriever does NOT consult the semantic / lexical
  indexes — pure graph traversal.
