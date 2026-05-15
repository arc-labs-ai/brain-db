# 18.05 Entity Garbage Collection

Tombstone semantics and the optional periodic GC sweep that reclaims storage for tombstoned / fully-merged entities. Wire opcode: `ENTITY_TOMBSTONE` (`0x0138`); see [`../28_knowledge_wire_protocol/01_entity_frames.md`](../28_knowledge_wire_protocol/01_entity_frames.md) §11.

## 1. Two levels of "gone"

| Level | State | Reversible? | Recovers space? |
|---|---|---|---|
| **Tombstoned** | `flags & TOMBSTONED != 0`. Row preserved; secondary indexes torn down. | Yes, via `ENTITY_UPDATE` clearing the flag (manual op). | No. |
| **Reclaimed** | Row removed from `entities` table. Audit retained. | No. | Yes. |

Tombstoning is a wire-exposed soft delete. Reclamation is an operator-driven GC pass (off by default).

## 2. Tombstone mechanics

`entity_tombstone(id, reason, now)`:

```text
1. Load entity row.
2. If already tombstoned: return success (idempotent).
3. Tear down secondary indexes (same as merge §5 step 12):
       - Remove from entity_by_canonical_name.
       - Remove from entity_aliases (one row per alias).
       - Remove from entity_trigrams (one row per trigram in the canonical+aliases set).
       - HNSW: tombstone-mark the embedding entry (HNSW rebuild reclaims).
4. Update row:
       - flags |= TOMBSTONED
       - tombstoned_at = now
       - tombstone_reason = reason
       - aliases = vec![]   // clear so future ENTITY_UPDATE doesn't double-index
5. Write audit row to entity_resolution_audit (kind=Tombstoned).
6. Commit redb txn.
7. Post-commit: emit ENTITY_TOMBSTONED event.
```

### 2.1 What stays queryable

A tombstoned entity:

- **Is still readable via `ENTITY_GET`**. Returns the row with `flags & TOMBSTONED != 0`. Clients filter or display accordingly.
- **Is not returned by `ENTITY_LIST`** unless `include_tombstoned = true`.
- **Is not a resolver target**. Tier 1 / 2 / 3 ignore tombstoned entities (their secondary indexes are gone).
- **Is not re-routable**. `ENTITY_MERGE` rejects tombstoned entities (`ENTITY_MERGE_CONFLICT`).

### 2.2 Statements / relations referencing tombstoned entities

Statements / relations are **not** automatically tombstoned or rewritten. They retain their references to the tombstoned id. Queries that follow the references see a tombstoned row.

Operators may use `ADMIN_LIST_STALE_STATEMENTS` ([`../28_knowledge_wire_protocol/14_admin_frames.md`](../28_knowledge_wire_protocol/14_admin_frames.md) §7) to find statements whose subject / object is now tombstoned and decide whether to tombstone the statements too.

## 3. GC eligibility

An entity becomes a GC candidate when **all** of:

| Condition | Where checked |
|---|---|
| `flags & TOMBSTONED != 0` | entity row |
| `now - tombstoned_at >= GC_TOMBSTONE_GRACE` (default 90 days) | entity row + clock |
| No active (non-tombstoned, non-superseded) statement has `subject = id` or `object = Entity(id)` | `statements_by_subject` + `statements_by_object` scans |
| No active relation has `from_entity = id` or `to_entity = id` | `relations_by_from` + `relations_by_to` scans |
| `mention_count == 0` after the scans (re-counted; the stored counter may be stale) | recomputed during GC |
| If `merged_into.is_some()`: corresponding merge audit's grace period expired | `entity_merge_log` |

The cumulative test is conservative — false negatives are fine (an entity stays around longer than necessary); false positives would orphan references and are forbidden.

## 4. GC sweep

The sweep is a background worker, **off by default**. Operators enable it via deployment config (`brain.gc.entities.enabled = true`).

### 4.1 Frequency

Default: daily. Configurable per deployment. The sweep doesn't need to run frequently — entities are cheap and the cost of orphaning identity is high.

### 4.2 Per-sweep work

```text
For each shard:
    Scan entities table for tombstoned rows.
    For each candidate:
        Verify §3 conditions inside a read txn.
        If eligible:
            Add to reclamation batch.
    Sort batch by entity_id (deterministic order).
    For each batch chunk (up to N entities, default N=100):
        Open write txn.
        For each entity in chunk:
            Re-verify §3 (TOCTOU).
            DELETE row from entities.
            DELETE any straggler index rows (defensive; tombstone should have cleaned).
            DELETE entity_mentions entries.
        Commit chunk.
```

### 4.3 What doesn't get reclaimed

- The `entity_resolution_audit` rows touching this entity. Audits are kept indefinitely.
- The `entity_merge_log` rows where this entity was survivor or merged. Same — kept indefinitely.
- Statements / relations that still reference the id, **if** an operator chose not to tombstone them. The GC sweep refuses to reclaim such entities (per §3); operators must address the references first.

### 4.4 Conservative defaults

- `GC_TOMBSTONE_GRACE` is 90 days, not 7. Entities are cheap to keep; recovering identity after hard delete is expensive.
- The sweep is **off by default**. Most deployments don't enable it — entity churn is low and the savings small.
- High-churn deployments (test data, ephemeral mentions, scratch workloads) flip it on.

## 5. Hard delete (RETRACT_ENTITY)

There is **no** `RETRACT_ENTITY` wire opcode. The only path to physical removal is:

1. `ENTITY_TOMBSTONE` (sets flag).
2. Wait `GC_TOMBSTONE_GRACE`.
3. GC sweep (if enabled) reclaims.

Operators that need immediate hard delete (privacy law compliance, etc.) drop to offline tooling against the redb file directly. The wire protocol intentionally has no privacy-immediate-erasure path — privacy-driven deletes use `STATEMENT_RETRACT` for statements about the entity and leave the entity row in place.

## 6. Reclamation audit

When the GC sweep reclaims an entity, it writes a final audit:

```rust
pub struct ReclamationAudit {
    pub audit_id: AuditId,
    pub entity_id: EntityId,
    pub entity_type_id: EntityTypeId,
    pub last_canonical_name: String,
    pub last_known_state_blob: Vec<u8>,  // rkyv-encoded final Entity row
    pub tombstoned_at: u64,
    pub reclaimed_at: u64,
    pub reclaim_actor: Actor,            // System (GC worker)
}
```

Kept in `entity_resolution_audit` with discriminator `kind = Reclamation`. Lets operators trace what was removed and when.

## 7. Tests

GC sweep test coverage (lands phase 16.7 or 16.8, alongside the optional worker):

- Eligibility:
  - Tombstoned + grace expired + no references → reclaimed.
  - Tombstoned but `mention_count > 0` after recount → not reclaimed.
  - Tombstoned but active statement still references → not reclaimed.
  - Tombstoned but merge audit grace not expired → not reclaimed.
- TOCTOU: another op modifies the entity mid-sweep → sweep skips and retries next pass.
- Batch chunking: 1000 candidates, sweep processes in 10 chunks of 100.
- Disabled by default: fresh deployment never runs the sweep without explicit enable.

Exercised in `crates/brain-workers/tests/entity_gc.rs` (phase 21).
