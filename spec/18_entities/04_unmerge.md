# 18.04 Entity Unmerge

Mechanics for `unmerge_entity(merged_entity, actor)` — the operation that reverses a recent `merge_entity` (see [`./03_merge.md`](./03_merge.md)). Wire opcode: `ENTITY_UNMERGE` (`0x0135`); see [`../28_knowledge_wire_protocol/01_entity_frames.md`](../28_knowledge_wire_protocol/01_entity_frames.md) §8 for the wire shape.

## 1. Purpose

Merges are wrong sometimes. The resolver can over-merge ("Priya Patel" and "Priya P." were different people); operators may discover the mistake within days. Unmerge undoes a recent merge by:

- Clearing `merged.merged_into = None`.
- Restoring `merged` to a discoverable entity (re-adding to secondary indexes).
- Splitting back the contributed aliases / attributes.
- Re-routing statements / relations whose audit trail attributes them to the original merged entity.

## 2. Inputs

```rust
pub struct UnmergeInputs {
    pub merged_entity: EntityId,      // the entity that was merged (NOT the survivor)
    pub actor: Actor,
    pub now_unix_nanos: u64,
}
```

The unmerge is keyed by the **merged** entity's id because that's the one the operator wants restored. The survivor is inferred from `merged.merged_into`.

## 3. Pre-conditions

| Check | Failure mode | Wire error |
|---|---|---|
| `merged_entity` exists | unknown id | `ENTITY_NOT_FOUND` |
| `merged_entity.merged_into.is_some()` | never merged | `ENTITY_NOT_FOUND` (interpretable as "nothing to unmerge") |
| corresponding merge audit row exists | audit gone (shouldn't happen) | `ENTITY_MERGE_CONFLICT` |
| `now <= audit.grace_period_expires_at` | grace expired | `ENTITY_MERGE_CONFLICT` |
| `audit.unmerged_at.is_none()` | already unmerged | `ENTITY_MERGE_CONFLICT` |
| `survivor` still exists and is itself active | survivor merged further | `ENTITY_MERGE_CONFLICT` (see §6 for multi-hop handling) |

## 4. Mechanics

Single redb write transaction.

```text
1. Load merged entity, audit row, survivor entity.
2. Re-check §3 pre-conditions.
3. Identify contributed pieces (from audit):
       - aliases_added: aliases the merged entity contributed to survivor.
       - attribute_deltas: attributes the merged entity contributed (or replaced).
       - statements_rerouted: list of (statement_id, original_subject_or_object).
       - relations_rerouted: list of (relation_id, original_from_or_to).
4. Restore the merged entity:
       - merged.merged_into = None
       - merged.updated_at = now
   Note: aliases / attributes / mention_count on merged are NOT re-incremented from the audit — they remain frozen from pre-merge.
   Operators who want to "freshly resurrect" the merged with new attributes use ENTITY_UPDATE afterward.
5. Strip survivor of what merged contributed:
       - survivor.aliases -= aliases_added (preserving any aliases survivor had pre-merge)
       - survivor.attributes: revert attributes per attribute_deltas
       - survivor.mention_count -= merged.mention_count
6. Re-route statements back:
       For each (statement_id, original_subject) in audit.statements_rerouted:
           IF statement.subject == survivor.id THEN statement.subject = original_subject
       Same for object.
7. Re-route relations back: symmetric to §6.
8. Re-add merged to its secondary indexes:
       - entity_by_canonical_name.insert((entity_type, normalize(canonical_name)) -> merged.id)
       - entity_aliases.insert each alias
       - entity_trigrams.insert each trigram
       - HNSW: re-insert merged's embedding (or queue re-embed)
9. Update survivor's secondary indexes:
       - Remove aliases that were merged's contribution
       - Remove trigrams that came only from merged
10. Mark audit as unmerged:
        audit.unmerged_at = Some(now)
        audit.unmerged_by = actor
11. Write back rows + commit redb txn.
12. Post-commit: emit ENTITY_UNMERGED event.
```

### 4.1 Why the audit row drives unmerge

The audit row carries the **complete diff** between pre-merge and post-merge state — every alias added, every attribute changed, every statement / relation re-routed. Without it, an unmerge would have to infer the diff from current state, which is impossible when concurrent edits have touched the entities since the merge.

The audit row's `statements_rerouted` / `relations_rerouted` lists store id + original-pointer pairs. Phase 16.7 builds this list during the merge transaction; phase 16.7 also implements unmerge.

### 4.2 Audit growth

For an entity with 100 statements re-routed, the audit row is ~10 KB. Bounded by the number of references the merged entity had. For most production entities (typical mention_count < 1000), audits are small.

For entities with very high mention counts (popular topics, frequently-mentioned people), the audit row may exceed redb's per-value cap (configurable, default 1 MiB). The implementation handles this by writing audit overflow rows to a separate `entity_merge_log_overflow` table keyed by `(audit_id, chunk_index)`.

## 5. Statements / relations added since the merge

Between the merge (`audit.created_at`) and the unmerge (`now`), new statements and relations may have been authored against the survivor. The unmerge does **not** re-route those — they stay with the survivor.

This means after unmerge:

- The merged entity has its original statements + relations (re-routed back).
- The survivor has its original + survivor's own + anything authored since the merge.
- Statements / relations authored during the merge window referencing the merged id implicitly are not affected (no such references exist — the merged id was redirected during the window).

## 6. Multi-hop unmerge

If `merged → survivor → other`:

- Unmerging `merged` is safe **only if** `survivor` is still active (not itself merged into `other`).
- If `survivor` has been merged into `other` since, the unmerge of `merged` is rejected with `ENTITY_MERGE_CONFLICT`.

Operators in this case must unmerge `other → survivor` first (if still in grace), then `survivor → merged`. Phase 16.7 enforces the order; tracked in [`./06_open_questions.md`](./06_open_questions.md) Q-multi-hop-unmerge.

## 7. Re-merging after unmerge

The same `(survivor, merged)` pair can be re-merged after an unmerge. Each merge writes a new audit; the previous one's `unmerged_at` field remains set as historical record.

## 8. Tests

Test coverage for unmerge (lands phase 16.7):

- All §3 pre-conditions.
- Round-trip: merge → unmerge → verify state matches pre-merge.
- Statements / relations authored during the merge window stay with the survivor.
- Multi-hop refusal: `merged → survivor → other` unmerge fails until upstream unmerge.
- Grace-period boundary.
- Re-merge after unmerge.
- Crash mid-transaction.
- Audit-row overflow path (>1 MiB statement re-route list).

Exercised in `crates/brain-server/tests/knowledge_entity_unmerge.rs` (lands phase 16.7).
