# 20.06 Open Questions

Relation-specific open questions. Wire-shape open questions live in
[`../28_knowledge_wire_protocol/09_open_questions.md`](../28_knowledge_wire_protocol/09_open_questions.md).

## Active

### Q1 — Discrete `RELATION_CARDINALITY_CONFLICT` event

[`./01_cardinality.md`](./01_cardinality.md) §3.4: `OneToOne`
two-sided conflict errors at `relation_create`. The error surfaces
to the caller but is otherwise invisible to monitoring.

Should the substrate emit a discrete `RelationCardinalityConflict`
event distinct from the standard error path? Phase 18 doesn't —
errors are sufficient. Monitoring use cases that need event-stream
visibility into cardinality violations would benefit.

**Target:** phase 23 (when query routing makes cardinality
violations actionable). **Status:** open. **Likely outcome:** add
the event.

---

### Q2 — Bulk-mode cardinality skip

[`./01_cardinality.md`](./01_cardinality.md) §2: cardinality is
checked at every `relation_create`. For bulk extractor backfills
(phase 22) running millions of creates, the per-create lookup is
the dominant cost.

Should the wire request carry a `skip_cardinality_check: bool`
flag for bulk imports? Caller takes responsibility for cardinality
correctness; a post-import sweep verifies.

**Target:** phase 22. **Status:** open. **Likely outcome:** add as
`RelationCreateRequest.skip_cardinality_check`, requires admin
permission.

---

### Q3 — Symmetric deduplication on create

[`./02_symmetric.md`](./02_symmetric.md) §9: two
`discussed_with(A, B)` rows with different `topic` properties
currently coexist. Some operators expect symmetric ManyToMany to
dedupe by `(canonical_from, canonical_to)`.

**Target:** phase 22 / 23. **Status:** open. **Likely outcome:**
predicate-level config: `dedup_by_endpoints: bool`. Default false
(preserve property diversity).

---

### Q4 — Path-edge vs terminal-set TRAVERSE response

[`./04_traversal.md`](./04_traversal.md) §10: v1 returns full path
metadata (relation_id, from, to, type at each step). Some queries
("set of entities reachable within 3 hops") only need the terminal
set; returning paths is wasteful.

**Target:** phase 23 query router. **Status:** open. **Likely
outcome:** add `RelationTraverseRequest.return_paths: bool`. Default
true.

---

### Q5 — `RELATION_RETRACT` opcode

[`./03_storage.md`](./03_storage.md) §5: v1 doesn't ship a hard-
delete path for relations. Operators wanting to permanently remove
a relation (privacy compliance, mis-extraction cleanup) must
tombstone + wait for the (non-existent) GC sweeper.

**Target:** phase 22. **Status:** open. **Likely outcome:** add
`RELATION_RETRACT` (0x0157) mirroring `STATEMENT_RETRACT`. Phase
21+ GC worker handles physical reclamation.

---

### Q6 — Cross-shard TRAVERSE coordination

[`./04_traversal.md`](./04_traversal.md) §9: deep traversal across
shards needs an inter-shard coordination mechanism. v1 ships
same-shard only; queries that need to cross shard boundaries either
fan-out via the planner (phase 23) or return early.

**Target:** phase 23. **Status:** open. **Likely outcome:** planner
spawns per-shard sub-traversals and unions results client-side or
in the router.

---

### Q7 — Weight-aware shortest-path traversal

Relations carry `confidence`. A natural extension is "shortest path
weighted by `1 / confidence`" — find the most-supported route
between two entities. v1 BFS treats every edge as weight 1.

**Target:** post-v1.0. **Status:** deferred. **Likely outcome:**
optional `RelationTraverseRequest.weight_by: ConfidenceMetric` enum
in a future hardening pass.

---

### Q8 — FORGET-cascade auto-tombstone configurability

[`./05_evidence.md`](./05_evidence.md) §6: when FORGET removes a
relation's last evidence, the v1 default tombstones the relation.
Operators wanting "preserve as low-confidence" need a config knob.

**Target:** phase 22 (alongside extractor configs). **Status:**
open. **Likely outcome:** deployment-level config
`brain.relation.cascade.tombstone_on_zero_evidence: bool`. Default
true.

---

### Q9 — Entity-merge relation re-routing

Phase 16 deferred relation re-routing on entity merge. When entity
A is merged into B (`ENTITY_MERGE`), relations citing A as `from`
or `to` should logically re-route to B.

Phase 18 ships without the re-route path — relations citing the
merged-away entity become orphaned (the entity is tombstoned, the
relation still references its tombstone). Tracked in §18 open
questions as well.

**Target:** phase 18.9 if scope allows, else phase 23. **Status:**
open. **Likely outcome:** add `relation_reroute_on_merge` in a
worker that subscribes to `EntityMerged` events.

---

### Q10 — `relations_by_type` index

[`./03_storage.md`](./03_storage.md) §1.5: the per-type index is
deferred. Admin queries "list all current `manages` relations"
currently scan `RELATIONS_TABLE` filtered by type, O(N).

**Target:** phase 22 if admin demands it; else stays deferred.
**Status:** open. **Likely outcome:** add when an admin opcode
needs the scan to be O(log N).

## Resolved

(none yet — §20 backfill is recent)
