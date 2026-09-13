# 02.06 Entity Lifecycle

> **TL;DR.** An entity is Brain's canonical noun: a stable identity for a real-world referent (Person, Organization, Project, Place, …). Entities have a UUIDv7 `EntityId`, a canonical name, alias list, and typed attributes. Resolution (turning a surface form like `"Priya"` into an `EntityId`) runs through the resolver gauntlet in [`../11_extractors/03_resolver.md`](../11_extractors/03_resolver.md). Merge collapses two entities into one with a grace-period unmerge path.

This file collects the entity record shape (§"Entity") and the merge / unmerge mechanics that drive the full entity lifecycle.

## Entity

### What an entity is

An entity is the **identity** of a referent: the thing a Memory mentions, a Statement is about, or a Relation connects. Two memories that mention `"Priya Patel"` and `"Priya"` should resolve to the same entity if the resolver decides they refer to the same person.

Entities are distinct from:

- **Memories** — raw experience text. Memories *mention* entities; entities are the things being mentioned.
- **Statements** — claims *about* entities (`Statement(subject=priya, predicate=role, object="manager")`). Entities are the subject; statements are the claim.
- **Relations** — edges *between* entities (`Relation(from=bob, to=priya, type=reports_to)`). Entities are the endpoints; relations are the edges.

### Entity record shape

```rust
struct Entity {
    id: EntityId,                       // UUIDv7
    namespace_id: NamespaceId,          // owning tenant; 0 = reserved `brain` system namespace
    agent_id: AgentId,                  // owning agent
    entity_type_id: EntityTypeId,       // interned u32; "Person", "Organization", ...
    canonical_name: String,             // normalized display name
    aliases: Vec<String>,               // alternative surface forms
    attributes: EntityAttributes,       // typed key-value pairs per the schema
    created_at_unix_nanos: u64,
    last_seen_at_unix_nanos: u64,       // last time a write touched this entity
    tombstoned: bool,
    merged_into: Option<EntityId>,      // None for live entities; Some(id) after merge
}
```

The `entity_type_id` is interned from the schema's entity-type declarations. Built-in types (`Person`, `Organization`, `Project`, `Place`, `Concept`, `Event`) are seeded in the system schema; user schemas add deployment-specific types.

**Owner scope (`namespace_id` + `agent_id`).** Every entity is owned by exactly one `(namespace, agent)` tenant pair, stamped from the caller's authenticated scope at create time (fail-closed by construction). This owner namespace is **distinct** from the qname namespace of `entity_type_id` — an entity owned by `acme` may have the shared `brain:Person` type. The reserved `brain` system namespace (id `0`) owns only seeded rows and is never a valid owner of user-written entities.

### Identity

`EntityId` is a UUIDv7 — time-ordered, 128 bits. The id is stable across:

- **Renames**: the `canonical_name` field changes, the id does not.
- **Alias additions**: aliases are added to the list, the id does not change.
- **Attribute updates**: attributes change in place, the id does not change.

`EntityId` changes only via merge (the merged-away entity's id becomes a redirect — see §"Entity merge" below).

**Resolution is per `(namespace, agent)`.** The resolver's exact-name, alias, and trigram indexes are all keyed under a leading `(namespace_id, agent_id)` scope prefix, so a surface form resolves only against entities the caller's own tenant owns. The same name resolves to **distinct** `EntityId`s under different scopes: `"Priya Patel"` in tenant `acme` and `"Priya Patel"` in tenant `globex` are two separate entities with two separate ids — one tenant's resolution can never reach another's rows.

## Entity merge

Mechanics for `merge_entity(survivor, merged, confidence, actor)` — the operation that collapses two entities into one. Wire opcode: `ENTITY_MERGE` (`0x0134`); see [`../04_wire_protocol/08_typed_graph_frames.md`](../04_wire_protocol/08_typed_graph_frames.md) §7 for the wire shape.

This file specifies the **value-type / storage-level** mechanics. The wire shape and per-field validation are over there.

### Mechanics scope

The merge / unmerge code covers all 15 mechanics steps in §"Mechanics" below: aliases / attributes / mention_count fold; survivor / merged row updates; statements / relations re-routing; secondary-index teardown for merged; audit row write; event emission. A merge is **complete and unmerge-able** the moment its transaction commits.

### Purpose

A merge is the resolver's (or operator's) declaration that two entities refer to the same real-world thing. After merge:

- The **survivor** is the canonical entity. Queries by id or name return it.
- The **merged** entity becomes a redirect — `merged.merged_into = Some(survivor.id)`. Queries through the merged id transparently follow the redirect (per §"Resolution ambiguity" + tier 5 in this file).
- All statements / relations that referenced the merged entity get re-routed to the survivor.

### Inputs

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

`Actor::Agent(AgentId)` for human-initiated merges (via `ENTITY_MERGE` opcode). `Actor::System` for resolver-initiated merges (when the auto-merge threshold passes — see §"Confidence-banded behavior").

### Pre-conditions

The merge is rejected (returning a structured error) if any pre-condition fails:

| Check | Failure mode | Wire error |
|---|---|---|
| `survivor != merged` | self-merge | `ENTITY_MERGE_CONFLICT` |
| both entities exist (active rows) | one missing | `ENTITY_NOT_FOUND` |
| both `entity_type` are equal | cross-type merge | `ENTITY_TYPE_MISMATCH` |
| neither is already `merged_into = Some(_)` | double-merge | `ENTITY_MERGE_CONFLICT` |
| neither is tombstoned | merge of tombstoned | `ENTITY_MERGE_CONFLICT` |
| `confidence` ∈ `[0.7, 1.0]` | low-confidence | `INVALID_ARGUMENT` |

Cross-type merges are forbidden — see [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md) Q3.

### Confidence-banded behavior

The bands distinguish **system-initiated** merges (from the resolver's LLM tier) from **operator-initiated** merges (via the `ENTITY_MERGE` wire opcode):

#### Operator-initiated

When `actor = Actor::Agent(_)` — the merge came from `ENTITY_MERGE` over the wire — the merge **applies immediately** at any confidence `>= 0.7`. The operator is making an explicit decision; there is no review queue (the operator IS the reviewer). Audit row written, `MERGED` event emitted.

`confidence < 0.7` is rejected as `INVALID_ARGUMENT` regardless of actor — extremely low confidence merges are caller bugs, not policy decisions.

#### System-initiated (resolver's LLM tier suggests merges)

When `actor = Actor::System` — the merge came from an internal worker (the LLM-tier resolver):

- `confidence >= 0.95`: **autonomous merge.** Audit row written; `MERGED` event emitted. Operators see the merge after the fact via `ADMIN_LIST_PENDING_RESOLUTIONS` audit history.
- `0.7 <= confidence < 0.95`: **review required.** The merge is **not** applied. Instead, the proposal is written to the `merge_review_queue` table, an audit row is stamped with `outcome = Pending`, and the proposal surfaces via `ADMIN_LIST_PENDING_RESOLUTIONS`. An operator confirms via `ADMIN_RESOLVE_AMBIGUITY` (audit kind discriminates merge-pending vs resolution-pending).
- `confidence < 0.7`: not a merge candidate; the resolver shouldn't even surface it.

#### The `merge_review_queue` table

```rust
pub const MERGE_REVIEW_QUEUE_TABLE:
    TableDefinition<'static, [u8; 16], MergeReviewProposal>;

pub struct MergeReviewProposal {
    pub proposal_id: AuditId,
    pub survivor_id: EntityId,
    pub merged_id: EntityId,
    pub confidence: f32,            // [0.7, 0.95)
    pub confidence_band: u8,        // 0 (0.70-0.80), 1 (0.80-0.90), 2 (0.90-0.95)
    pub created_at_unix_nanos: u64,
    pub last_revisited_at_unix_nanos: u64,
    pub reason: String,
    pub evidence_summary: Vec<u8>,  // rkyv-archived blob with surrounding context
}
```

The `confidence_band` is a coarse bucket the admin UI uses to surface the highest-confidence proposals first. Bands let operators prioritize without sorting through a long flat list.

#### Ambiguity resolver worker

A periodic `ambiguity_resolver` worker re-runs entity resolution against every active proposal as new context arrives — additional statements citing one or the other entity, additional aliases, new mentions in fresh memories. When the re-run produces a confidence outcome that crosses a band boundary:

- New confidence ≥ 0.95 → auto-merge promoted; proposal dequeued; `MERGED` event emitted.
- New confidence < 0.7 → proposal dequeued as "no longer a candidate"; both entities remain separate.
- Confidence still in `[0.7, 0.95)` → proposal updated in place; `last_revisited_at` bumped; band may shift.

The worker is the path that makes `Pending(audit_id)` statement subjects eventually resolve without forcing an operator into every borderline case — as evidence accumulates, the resolver's confidence rises, and once it crosses 0.95 the merge applies on its own.

Cadence default: every 6 hours. Bounded batches per tick.

#### Operator-initiated implementation

The operator-initiated path is wired. The handler always sets `actor_kind = Agent` (from the connection's authenticated agent) and applies the merge if pre-conditions pass. The review-queue path lands alongside the LLM-tier resolver.

### Mechanics (autonomous path)

Single redb write transaction. All steps below are atomic — either every table sees the change or none do.

```text
1. Load survivor and merged rows.
2. Re-check pre-conditions inside the txn (TOCTOU defense).
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
6. Merge attributes (see "Attribute conflict resolution" for conflict policy).
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

#### Why secondary indexes torn down for merged

The merged entity is no longer a discoverable target. Its row stays for unmerge and audit purposes, but resolver queries (`entity_by_canonical_name`, `entity_aliases`, `entity_trigrams`) MUST NOT return it. Steps 12+13 maintain this invariant.

#### Embedding handling

The survivor's embedding is **not** automatically recomputed. The embedding worker re-checks `embedding_version` and re-embeds asynchronously if needed.

The merged entity's HNSW entry is tombstoned during step 12 (knowledge HNSW tombstone is implemented via the standard HNSW deletion path; rebuild reclaims).

### Attribute conflict resolution

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

For v1.0 the default is `survivor_wins` for every attribute; per-attribute overrides land here's schema DSL.

### Grace period

The merge is **reversible** during a grace period (default 7 days, configurable per deployment via `DEFAULT_MERGE_GRACE_PERIOD`). During the window:

- `ENTITY_UNMERGE` can reverse the merge (see §"Entity unmerge" below).
- The merged entity's row is preserved with full pre-merge attribute snapshot in the audit.

After the grace period:

- Unmerge is **permanently disallowed**. The merge is canonical.
- The merged entity's row may be hard-reclaimed by the GC sweep once tombstoned.

### Multi-hop merge

A merge survivor that is later merged into a third entity creates a chain:

```
A merged_into B
B merged_into C
```

Queries through `A` need two redirects to reach `C`. The implementation **collapses chains** during read:

- On `entity_get(A)`: read A, follow merged_into to B, follow to C, return C's row but with audit trail showing both merges.
- On periodic GC: physically collapse the chain — set `A.merged_into = Some(C)` directly (skipping B).

The chain-collapse pass is part of the periodic consolidation worker; it's idempotent and safe to interleave with concurrent merges.

### Concurrent merge handling

Per-shard single-writer discipline ([`../08_storage/`](../08_storage/00_purpose.md)) makes concurrent merges on the same shard impossible at the storage layer. Cross-shard merges (entities live on different shards in a multi-shard deployment) require a 2PC dance:

1. Lock both shards (sorted by shard id to avoid deadlock).
2. Run §"Mechanics" steps on each shard (statements / relations live on the subject's shard).
3. Coordinate the merge audit row (lives on the survivor's shard).

The coordination strategy is tracked in [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md).

### Audit retention

The `entity_merge_log` row is kept indefinitely (no automatic deletion). Operators may export and prune via `ADMIN_GET_AUDIT` + offline tooling, but Brain doesn't garbage-collect merge audits as part of normal operations.

### Tests

Test coverage for merge mechanics (lands _(later)_):

- All pre-conditions, both inside and outside the redb txn.
- Each confidence band (auto / review / reject).
- Attribute conflict resolution for every policy.
- Re-routing: statements with `subject = merged`, with `object = Entity(merged)`, with both; relations with `from = merged`, with `to = merged`.
- Multi-hop merge: A→B then B→C, verify chain collapse.
- Concurrent merges on different shards.
- Crash mid-transaction: verify either the full merge applied or none.
- Grace-period boundary: unmerge succeeds at `expires_at - 1ns`, fails at `expires_at + 1ns`.

Exercised in `crates/brain-server/tests/knowledge_entity_merge.rs` (lands _(later)_).

## Entity unmerge

Mechanics for `unmerge_entity(merged_entity, actor)` — the operation that reverses a recent `merge_entity` (see §"Entity merge" above). Wire opcode: `ENTITY_UNMERGE` (`0x0135`); see [`../04_wire_protocol/08_typed_graph_frames.md`](../04_wire_protocol/08_typed_graph_frames.md) §8 for the wire shape.

### Purpose

Merges are wrong sometimes. The resolver can over-merge ("Priya Patel" and "Priya P." were different people); operators may discover the mistake within days. Unmerge undoes a recent merge by:

- Clearing `merged.merged_into = None`.
- Restoring `merged` to a discoverable entity (re-adding to secondary indexes).
- Splitting back the contributed aliases / attributes.
- Re-routing statements / relations whose audit trail attributes them to the original merged entity.

### Inputs

```rust
pub struct UnmergeInputs {
    pub merged_entity: EntityId,      // the entity that was merged (NOT the survivor)
    pub actor: Actor,
    pub now_unix_nanos: u64,
}
```

The unmerge is keyed by the **merged** entity's id because that's the one the operator wants restored. The survivor is inferred from `merged.merged_into`.

### Pre-conditions

| Check | Failure mode | Wire error |
|---|---|---|
| `merged_entity` exists | unknown id | `ENTITY_NOT_FOUND` |
| `merged_entity.merged_into.is_some()` | never merged | `ENTITY_NOT_FOUND` (interpretable as "nothing to unmerge") |
| corresponding merge audit row exists | audit gone (shouldn't happen) | `ENTITY_MERGE_CONFLICT` |
| `now <= audit.grace_period_expires_at` | grace expired | `ENTITY_MERGE_CONFLICT` |
| `audit.unmerged_at.is_none()` | already unmerged | `ENTITY_MERGE_CONFLICT` |
| `survivor` still exists and is itself active | survivor merged further | `ENTITY_MERGE_CONFLICT` (see "Multi-hop unmerge" below for multi-hop handling) |

### Mechanics

Single redb write transaction.

```text
1. Load merged entity, audit row, survivor entity.
2. Re-check pre-conditions.
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
7. Re-route relations back: symmetric to step 6.
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

#### Why the audit row drives unmerge

The audit row carries the **complete diff** between pre-merge and post-merge state — every alias added, every attribute changed, every statement / relation re-routed. Without it, an unmerge would have to infer the diff from current state, which is impossible when concurrent edits have touched the entities since the merge.

The audit row's `statements_rerouted` / `relations_rerouted` lists store id + original-pointer pairs. The merge transaction builds this list and the unmerge path reads it.

#### Audit growth

For an entity with 100 statements re-routed, the audit row is ~10 KB. Bounded by the number of references the merged entity had. For most production entities (typical mention_count < 1000), audits are small.

For entities with very high mention counts (popular topics, frequently-mentioned people), the audit row may exceed redb's per-value cap (configurable, default 1 MiB). The implementation handles this by writing audit overflow rows to a separate `entity_merge_log_overflow` table keyed by `(audit_id, chunk_index)`.

### Statements / relations added since the merge

Between the merge (`audit.created_at`) and the unmerge (`now`), new statements and relations may have been authored against the survivor. The unmerge does **not** re-route those — they stay with the survivor.

This means after unmerge:

- The merged entity has its original statements + relations (re-routed back).
- The survivor has its original + survivor's own + anything authored since the merge.
- Statements / relations authored during the merge window referencing the merged id implicitly are not affected (no such references exist — the merged id was redirected during the window).

### Multi-hop unmerge

If `merged → survivor → other`:

- Unmerging `merged` is safe **only if** `survivor` is still active (not itself merged into `other`).
- If `survivor` has been merged into `other` since, the unmerge of `merged` is rejected with `ENTITY_MERGE_CONFLICT`.

Operators in this case must unmerge `other → survivor` first (if still in grace), then `survivor → merged`. Brain enforces the order; tracked in [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md) Q-multi-hop-unmerge.

### Re-merging after unmerge

The same `(survivor, merged)` pair can be re-merged after an unmerge. Each merge writes a new audit; the previous one's `unmerged_at` field remains set as historical record.

### Tests

Test coverage for unmerge (lands _(later)_):

- All pre-conditions.
- Round-trip: merge → unmerge → verify state matches pre-merge.
- Statements / relations authored during the merge window stay with the survivor.
- Multi-hop refusal: `merged → survivor → other` unmerge fails until upstream unmerge.
- Grace-period boundary.
- Re-merge after unmerge.
- Crash mid-transaction.
- Audit-row overflow path (>1 MiB statement re-route list).

Exercised in `crates/brain-server/tests/knowledge_entity_unmerge.rs` (lands _(later)_).

## What follows in this section

- [`07_statement.md`](07_statement.md) — the Statement record type lifecycle (Fact / Preference / Event, supersession, contradiction, confidence, evidence)
- [`08_relation.md`](08_relation.md) — the Relation record type lifecycle (cardinality, symmetry, evidence)
- [`../11_extractors/03_resolver.md`](../11_extractors/03_resolver.md) — how surface forms become `EntityId`s
