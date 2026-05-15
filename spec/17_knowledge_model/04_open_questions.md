# Open Questions: Knowledge Model

Decisions deferred or unresolved at the conceptual model level.

## OQ-1.1: Should statements be sharded by subject entity or by extractor?

**Status:** Tentatively decided: shard by subject.

**Detail:** the substrate shards memories by BLAKE3(agent_id). For statements, two options:

- Shard by subject EntityId: all statements about Priya are in one shard. Subject-filtered queries are local. Cross-subject queries (rare) fan out.
- Shard by extractor / source memory's shard: extractions are local to where the source memory lives. But entity-scoped queries fan out (common).

Decision: shard by subject. Aligns with the dominant query pattern. Cross-subject queries are rare and acceptable to fan out.

**To verify:** workload analysis once we have one. Easy to switch in future versions if wrong.

## OQ-1.2: What's the upper limit on evidence list length per statement?

**Status:** Tentatively decided: 64.

If a Fact has 200 supporting memories, we don't need to store all of them on the row. The query rarely needs more than ~10. Store top-K by recency or confidence; spill the rest to a side table (`statement_evidence_overflow`).

K = 64 default. Configurable per deployment.

**To verify:** real workload. If users routinely have hundreds of supporting memories, the overflow design must be fast.

## OQ-1.3: Are statements derivable from other statements?

**Status:** Yes, but limited.

A "meta-statement" can have other statements in its evidence list (`evidence_statements: Vec<StatementId>`). Use case: a rule-based extractor concludes "Priya manages Bob" from "Bob reports to Priya" + "reports_to is transitive."

**Constraint:** the evidence-statement chain has a maximum depth (default 3). Beyond that we refuse to derive, to prevent runaway transitivity.

**To verify:** is this needed in the knowledge layer, or can it wait until future versions? Lean toward future versions.

## OQ-1.4: Entity merge — what happens to existing statements?

When the user (or a resolver worker) decides that `entity_42` and `entity_77` are the same person:

- Choose a survivor (say `entity_42`).
- All statements with `subject = entity_77` are updated to `subject = entity_42`.
- All statements with `object = entity_77` are updated to `object = entity_42`.
- All relations with `from = entity_77` or `to = entity_77` are updated.
- `entity_77` is marked merged-into-`entity_42`; its row is kept as a redirect.

The mechanics are clear. Open question: do we *always* require the user to confirm merges, or can a high-confidence resolver merge autonomously?

**Tentative answer:** autonomous merging is allowed only if confidence ≥ 0.95 *and* the merge is reversible within a grace period (7 days). Lower confidence flags for human review.

## OQ-1.5: Entity split — is it supported?

If `entity_42` was incorrectly created as one entity but is actually two different people:

This is hard. Statements about `entity_42` need to be redistributed between the two new entities. The substrate cannot do this autonomously — it requires per-statement disambiguation.

**Tentative answer:** entity split is a user-driven operation. The user invokes `SPLIT_ENTITY` with rules (which statements go where, perhaps by source-memory range). The substrate executes the split as an atomic operation. Not high priority for the knowledge layer.

## OQ-1.6: How does a Memory's `agent_id` interact with Entity identity?

Memories belong to agents. Entities are referenced *by* agents in their memories. If agent A says "Priya is the manager" and agent B says "Priya is a junior engineer," are these the same Priya?

**Tentative answer:** Entities are global within a deployment (not per-agent). Two agents referring to "Priya" by name will resolve to the same Entity unless they explicitly disambiguate. This may cause cross-agent leakage and is a feature for the cognitive substrate use case (agents share an entity space). For deployments where per-agent isolation is required, use separate Brain deployments or wait for future versions multi-tenancy.

## OQ-1.7: Statement object types — what's the union?

Object types currently proposed:

```rust
enum StatementObject {
    Entity(EntityId),
    Value(serde_json::Value),  // typed literal (string, number, bool, struct)
    Memory(MemoryId),          // "evidenced by" type relations
    Statement(StatementId),    // meta-statements
}
```

Open: do we need a `List(Vec<StatementObject>)` variant for "Priya likes [tea, coffee, espresso]"? Or do users express this as three statements?

**Tentative answer:** three statements. Single-object simplicity. If users need list semantics often, we revisit in future versions.
