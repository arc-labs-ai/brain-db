# 18.03 Entity Merge

Mechanics for `merge_entity(survivor, merged, confidence, actor)` — the operation that collapses two entities into one. Wire opcode: `ENTITY_MERGE` (`0x0134`); see [`../28_knowledge_wire_protocol/01_entity_frames.md`](../28_knowledge_wire_protocol/01_entity_frames.md) §7 for the wire shape.

This file specifies the **value-type / storage-level** mechanics. The wire shape and per-field validation are over there.

## 0. Phase 16.7 scope note

Phase 16.7 lands the merge / unmerge code at a phase-scoped subset of the full spec below:

- **§5 steps 1-7, 10-15 are implemented in 16.7.** Aliases / attributes / mention_count fold; survivor / merged row updates; secondary-index teardown for merged; audit row write; event emission.
- **§5 steps 8-9 (statements / relations re-routing) are deferred.** The statement and relation tables don't exist until phases 17 / 18. Phase 16.7 writes `statements_rerouted = 0` and `relations_rerouted = 0` into the audit. Phase 17 (when statements land) re-issues a sweep over `entity_merge_log` to re-route any statements whose subject / object referenced now-merged entities, and writes overflow lists. Phase 18 does the same for relations.

A merge performed in 16.7 is **complete and unmerge-able** for its phase-scoped diff. When phases 17 / 18 land, they fix up the audit retroactively — the unmerge code path reads the post-fix-up audit (so an unmerge issued after phase 17 lands will correctly re-route any statements created against the survivor in the intervening window, including ones that referenced the merged entity before the merge).

## 1. Purpose

A merge is the resolver's (or operator's) declaration that two entities refer to the same real-world thing. After merge:

- The **survivor** is the canonical entity. Queries by id or name return it.
- The **merged** entity becomes a redirect — `merged.merged_into = Some(survivor.id)`. Queries through the merged id transparently follow the redirect (per [§01](./01_resolution.md) §"Resolution ambiguity" + tier 5).
- All statements / relations that referenced the merged entity get re-routed to the survivor.

## 2. Inputs

```rust
pub struct MergeInputs {
    pub survivor: EntityId,
    pub merged: EntityId,
    pub confidence: f32,              // [0.0, 1.0]
    pub actor: Actor,                 // Agent or System
    pub reason: String,               // for audit
    pub now_unix_nanos: u64,          // server clock at op start
}
```

`Actor::Agent(AgentId)` for human-initiated merges (via `ENTITY_MERGE` opcode). `Actor::System` for resolver-initiated merges (when the auto-merge threshold passes — see §4.1).

## 3. Pre-conditions

The merge is rejected (returning a structured error) if any pre-condition fails:

| Check | Failure mode | Wire error |
|---|---|---|
| `survivor != merged` | self-merge | `ENTITY_MERGE_CONFLICT` |
| both entities exist (active rows) | one missing | `ENTITY_NOT_FOUND` |
| both `entity_type` are equal | cross-type merge | `ENTITY_TYPE_MISMATCH` |
| neither is already `merged_into = Some(_)` | double-merge | `ENTITY_MERGE_CONFLICT` |
| neither is tombstoned | merge of tombstoned | `ENTITY_MERGE_CONFLICT` |
| `confidence` ∈ `[0.7, 1.0]` | low-confidence | `INVALID_ARGUMENT` |

Cross-type merges are forbidden in v1.0 — see [`../28_knowledge_wire_protocol/09_open_questions.md`](../28_knowledge_wire_protocol/09_open_questions.md) Q3.

## 4. Confidence-banded behavior

The bands distinguish **system-initiated** merges (from the resolver's LLM tier in phase 21+) from **operator-initiated** merges (via the `ENTITY_MERGE` wire opcode):

### 4.1 Operator-initiated (the only path in phase 16.7)

When `actor = Actor::Agent(_)` — the merge came from `ENTITY_MERGE` over the wire — the merge **applies immediately** at any confidence `>= 0.7`. The operator is making an explicit decision; there is no review queue (the operator IS the reviewer). Audit row written, `MERGED` event emitted.

`confidence < 0.7` is rejected as `INVALID_ARGUMENT` regardless of actor — extremely low confidence merges are caller bugs, not policy decisions.

### 4.2 System-initiated (phase 21+, when resolver's LLM tier suggests merges)

When `actor = Actor::System` — the merge came from an internal worker (the LLM-tier resolver, phase 21):

- `confidence >= 0.95`: **autonomous merge.** Audit row written; `MERGED` event emitted. Operators see the merge after the fact via `ADMIN_LIST_PENDING_RESOLUTIONS` audit history.
- `0.7 <= confidence < 0.95`: **review required.** The merge is **not** applied. Instead, an audit row is written with `outcome = Pending` and the merge appears in `ADMIN_LIST_PENDING_RESOLUTIONS`. An operator confirms via `ADMIN_RESOLVE_AMBIGUITY` (audit kind discriminates merge-pending vs resolution-pending).
- `confidence < 0.7`: not a merge candidate; the resolver shouldn't even surface it.

### 4.3 Phase 16.7 implementation

Only §4.1 is wired in 16.7. The handler always sets `actor_kind = Agent` (from the connection's authenticated agent) and always applies the merge if §3 pre-conditions pass. The review-queue path lands when the LLM tier lands (phase 21+).

## 5. Mechanics (autonomous path)

Single redb write transaction. All steps below are atomic — either every table sees the change or none do.

```text
1. Load survivor and merged rows.
2. Re-check §3 pre-conditions inside the txn (TOCTOU defense).
3. Allocate AuditId (UUIDv7) for the merge audit.
4. Write merge audit row to `entity_merge_log`:
       MergeRecord {
           merge_id,
           survivor_id,
           merged_id,
           confidence,
           reason,
           actor,
           created_at: now,
           grace_period_expires_at: now + DEFAULT_MERGE_GRACE_PERIOD,
           statements_rerouted: <count after step 8>,
           relations_rerouted: <count after step 9>,
           attribute_conflicts: Vec<AttributeConflictRecord>,
           unmerged_at: None,
       }
5. Merge aliases: survivor.aliases |= merged.aliases (deduplicated on normalize_name).
   Add merged.canonical_name to survivor.aliases (deduplicated).
6. Merge attributes (see §6 for conflict resolution).
7. Merge mention_count: survivor.mention_count += merged.mention_count.
8. Re-route statements:
       SELECT * FROM statements WHERE subject = merged.id;
       UPDATE: subject = survivor.id; bump version; chain_root unchanged.
       SELECT * FROM statements WHERE object = StatementObject::Entity(merged.id);
       UPDATE: object = StatementObject::Entity(survivor.id).
9. Re-route relations:
       SELECT * FROM relations WHERE from_entity = merged.id;
       UPDATE: from_entity = survivor.id.
       SELECT * FROM relations WHERE to_entity = merged.id;
       UPDATE: to_entity = survivor.id.
10. Update survivor row in `entities`.
11. Mark merged: merged.merged_into = Some(survivor.id); merged.updated_at = now.
    Write back to `entities`.
12. Tear down merged's secondary indexes:
        - Remove from entity_by_canonical_name.
        - Remove from entity_aliases.
        - Remove from entity_trigrams.
    (Merged entity is no longer resolution-reachable except via id-based lookups.)
13. Update survivor's secondary indexes for the newly-added aliases / trigrams.
14. Commit redb txn.
15. Post-commit: emit ENTITY_MERGED event on the SUBSCRIBE channel.
```

### 5.1 Why secondary indexes torn down for merged

The merged entity is no longer a discoverable target. Its row stays for unmerge (§04) and audit purposes, but resolver queries (`entity_by_canonical_name`, `entity_aliases`, `entity_trigrams`) MUST NOT return it. Steps 12+13 maintain this invariant.

### 5.2 Embedding handling

The survivor's embedding is **not** automatically recomputed. Phase 21's embedding worker re-checks `embedding_version` and re-embeds asynchronously if needed.

The merged entity's HNSW entry is tombstoned during step 12 (knowledge HNSW tombstone is implemented via the standard HNSW deletion path; rebuild reclaims).

## 6. Attribute conflict resolution

When both entities have a value for the same attribute key, conflict policy decides:

| Policy | Behavior | Configurable by |
|---|---|---|
| `survivor_wins` (default) | Keep survivor's value. Log conflict in audit. | per entity type, schema DSL |
| `merged_wins` | Replace with merged's value. Log conflict. | per entity type, schema DSL |
| `newest_wins` | Whichever entity has a more recent `updated_at` for the attribute (when tracked) wins. | per entity type |
| `concat_text` | For text-typed attributes, concatenate with `"; "` separator. | per attribute |
| `reject_merge` | Pre-condition fail — return `ENTITY_MERGE_CONFLICT`. | per attribute |

`AttributeConflictRecord` rows record the conflicts in the merge audit:

```rust
pub struct AttributeConflictRecord {
    pub attribute_key: String,
    pub survivor_value: SerializedValue,
    pub merged_value: SerializedValue,
    pub policy: ConflictPolicy,
    pub outcome: ConflictOutcome,    // KeptSurvivor / ReplacedWithMerged / Concatenated
}
```

For v1.0 the default is `survivor_wins` for every attribute; per-attribute overrides land in phase 19's schema DSL.

## 7. Grace period

The merge is **reversible** during a grace period (default 7 days, configurable per deployment via `DEFAULT_MERGE_GRACE_PERIOD`). During the window:

- `ENTITY_UNMERGE` can reverse the merge (see [`./04_unmerge.md`](./04_unmerge.md)).
- The merged entity's row is preserved with full pre-merge attribute snapshot in the audit.

After the grace period:

- Unmerge is **permanently disallowed**. The merge is canonical.
- The merged entity's row may be hard-reclaimed by the GC sweep (see [`./05_garbage_collection.md`](./05_garbage_collection.md)) once tombstoned.

## 8. Multi-hop merge

A merge survivor that is later merged into a third entity creates a chain:

```
A merged_into B
B merged_into C
```

Queries through `A` need two redirects to reach `C`. The implementation **collapses chains** during read:

- On `entity_get(A)`: read A, follow merged_into to B, follow to C, return C's row but with audit trail showing both merges.
- On periodic GC: physically collapse the chain — set `A.merged_into = Some(C)` directly (skipping B).

The chain-collapse pass is part of the periodic consolidation worker (phase 21+); it's idempotent and safe to interleave with concurrent merges.

## 9. Concurrent merge handling

Per-shard single-writer discipline ([`../05_storage_arena_wal/`](../05_storage_arena_wal/)) makes concurrent merges on the same shard impossible at the storage layer. Cross-shard merges (entities live on different shards in a multi-shard deployment) require a 2PC dance:

1. Lock both shards (sorted by shard id to avoid deadlock).
2. Run §5 steps on each shard (statements / relations live on the subject's shard).
3. Coordinate the merge audit row (lives on the survivor's shard).

Phase 16.7 implementation will pick the coordination strategy; tracked in [`./06_open_questions.md`](./06_open_questions.md).

## 10. Audit retention

The `entity_merge_log` row is kept indefinitely (no automatic deletion). Operators may export and prune via `ADMIN_GET_AUDIT` + offline tooling, but the substrate doesn't garbage-collect merge audits as part of normal operations.

## 11. Tests

Test coverage for merge mechanics (lands phase 16.7):

- All §3 pre-conditions, both inside and outside the redb txn.
- Each confidence band (auto / review / reject).
- Attribute conflict resolution for every policy.
- Re-routing: statements with `subject = merged`, with `object = Entity(merged)`, with both; relations with `from = merged`, with `to = merged`.
- Multi-hop merge: A→B then B→C, verify chain collapse.
- Concurrent merges on different shards.
- Crash mid-transaction: verify either the full merge applied or none.
- Grace-period boundary: unmerge succeeds at `expires_at - 1ns`, fails at `expires_at + 1ns`.

Exercised in `crates/brain-server/tests/knowledge_entity_merge.rs` (lands phase 16.7).
