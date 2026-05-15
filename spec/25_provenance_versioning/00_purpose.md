# Provenance and Versioning

## The provenance invariant

Every Statement, Relation, and Entity here has a traceable chain back to its source. The chain answers: "Where did this come from? Who derived it? When? Why?"

```
Memory(text) ──extracted_by──> Statement ──supersedes──> Statement (current)
                                  │
                                  └──evidence──> [Memory, Memory, Memory]
```

## What is tracked

For every derived record:

| Field | Meaning |
|---|---|
| `evidence: Vec<MemoryId>` | The source memories. |
| `extractor_id: ExtractorId` | Which extractor produced this. |
| `extractor_version: u32` | Pinned at extraction time. |
| `schema_version: u32` | Pinned at extraction time. |
| `extracted_at: u64` | When the extraction ran. |
| `model_metadata: Option<ModelMetadata>` | For LLM extractors: model name/version, token counts, cache hit/miss. |

For supersession chains:
- `chain_root: StatementId` — the first statement in the chain.
- `version: u32` — chain position (1 for root, 2 for first supersession, ...).
- `supersedes: Option<StatementId>` — back-pointer.
- `superseded_by: Option<StatementId>` — forward-pointer.

For tombstones:
- `tombstoned: bool`
- `tombstoned_at: Option<u64>`
- `tombstone_reason: TombstoneReason`
- `tombstoned_by: Option<Actor>`

## The audit log

A separate, append-only log records every derivation, supersession, tombstone, and merge:

```rust
struct AuditEntry {
    id: AuditId,                       // UUIDv7, ordered
    timestamp: u64,
    actor: Actor,                      // System(extractor) | User(agent_id) | Admin
    operation: AuditOp,
}

enum AuditOp {
    Extracted { memory_id, extractor_id, output_ids },
    Superseded { old, new, reason },
    Tombstoned { target, reason },
    Retracted { target, reason },
    Merged { survivor, merged, confidence },
    Unmerged { entity, restored_to },
    Renamed { entity, old_name, new_name },
    SchemaUpgraded { from_version, to_version },
}
```

Audit entries are durable (written through WAL) and queryable. Default retention: 90 days. Configurable.

## Cascading effects of FORGET

When a Memory is forgotten:

```
FORGET memory_x
  │
  ├─ Soft tombstone in the substrate substrate
  │
  ├─ Lookup statements WHERE memory_x ∈ evidence
  │     For each affected statement:
  │       - Remove memory_x from evidence list
  │       - Recompute confidence (down-weighted)
  │       - If evidence list now empty:
  │           * If confidence_after >= threshold: keep, mark "stale_evidence"
  │           * Else: tombstone with reason=SourceMemoryForgotten
  │
  ├─ Lookup relations WHERE memory_x ∈ evidence (same logic)
  │
  ├─ Lookup entity_mentions WHERE memory_id=memory_x: remove
  │
  └─ Write audit entries for each cascade
```

This cascade is performed by a worker (the FORGET cascade worker), not synchronously. The triggering FORGET returns immediately; the cascade processes in background.

If the original FORGET was soft (tombstone with grace period), the cascade is also soft: derived records are marked pending-tombstone with the same grace period. If the FORGET is reverted before grace expires, the cascade is rolled back.

## Confidence aggregation across evidence

When a Statement has multiple supporting Memories, its confidence is aggregated:

```
confidence = 1 - Π_i (1 - c_i * decay(age_i))
```

Where:
- `c_i` is the per-evidence confidence (extractor's per-memory confidence).
- `decay(age_i)` reduces older evidence: `decay(t) = exp(-t / half_life)`.
- `half_life` is 90 days default for Facts, 30 days for Preferences, no decay for Events (Events are point-in-time and don't decay).

When evidence is added or removed, confidence is recomputed.

This formula:
- Bounded in [0, 1].
- Monotonic: adding consistent evidence raises confidence.
- Diminishing returns: 10 pieces of weak evidence don't equal 1 piece of strong.

## Stale extraction detection

Each Statement carries the `schema_version` and `extractor_version` it was produced under.

When the current `schema.version` or `extractor.version` advances, statements with older versions are flagged `stale`:

```rust
fn is_stale(statement: &Statement, current: &SchemaSnapshot) -> bool {
    statement.schema_version < current.schema_version
    || statement.extractor_version < current.extractor.version_for(statement.extractor_id)
}
```

Stale statements remain queryable. The query result surfaces staleness in metadata, so clients can decide whether to trust them or trigger re-extraction.

## Re-extraction workflow

Triggered manually or by the schema migration worker:

```
RE_EXTRACT memory_x with extractor_y schema v=5
  │
  ├─ Look up existing statements for (memory_x, extractor_y)
  │
  ├─ Run extractor_y v5 on memory_x
  │     Output: new statements
  │
  ├─ Diff: for each new statement:
  │     If matching old statement (same kind, subject, predicate, object):
  │       confidence_delta = new.confidence - old.confidence
  │       If similar (delta < threshold): mark old as "refreshed", update version
  │       If different (delta >= threshold): supersede old with new
  │     If new but no matching old: create
  │
  │     For each old not matched by new:
  │       Mark as "potentially retracted"; user review or auto-tombstone
  │
  └─ Audit entry written
```

For Events: re-extraction is non-destructive. New Events are added; old Events stay.

For Preferences: supersession applies straightforwardly.

For Facts: tricky. New contradicting Facts trigger a contradiction; same-direction Facts confirm the old.

## Version visibility in queries

By default, queries return:
- Current Statements (not superseded).
- Non-tombstoned.
- Not stale (unless `include_stale: true`).

Optional query parameters:
- `as_of: Timestamp` — return Statements as they would have appeared at this time (looks at chain by `version <= ?`).
- `include_superseded: true` — return all versions.
- `include_tombstoned: true` — return tombstoned with their reasons.

Note: `as_of` operates on valid_time (`valid_from`, `valid_to`, `extracted_at`). True bitemporal "as of transaction time" is deferred to future versions.

## Retention

| Record | Retention default |
|---|---|
| Active Statements/Relations/Entities | Forever |
| Tombstoned Statements/Relations | 30 days (then hard-deleted by sweeper) |
| Superseded Statements/Relations | Forever (kept for chain history) |
| Extraction audit logs | 90 days |
| Resolution audit logs | 90 days |
| Merge logs | Forever (small, valuable) |
| LLM extractor cache | 90 days |

All configurable per deployment.
