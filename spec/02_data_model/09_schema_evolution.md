# 02.09 Schema Evolution

How the data model changes over time without breaking existing deployments. This file is the data-model side of [00.03 Versioning](../00_master_overview/03_versioning.md); read both for the full story.

## 1. What changes, what doesn't

The data model evolves by:

- **Adding optional fields** to existing entities.
- **Adding new entity types** (rare).
- **Adding new edge kinds** (versioned; see §3).
- **Adjusting calibrated constants** (salience weights, decay half-lives).

The data model does **not** change by:

- Removing fields (always at least deprecated first).
- Renaming fields (causes wire-format incompatibility).
- Changing field types (a different field is added; the old is deprecated).

These are conservative rules. They keep the substrate forward-compatible across format-version bumps within reasonable limits.

## 2. Field addition

### 2.1 Mechanism

New fields are added with a default value for existing records. When an old format-version record is read, missing fields default. When a new-format-version reader reads a new-format record, all fields are present.

Wire format: rkyv handles optional fields via versioned schemas. Storage format: the redb table schema is versioned; new fields are added at the end of the value tuple, with default values for old records.

### 2.2 Example

Suppose v1.1 adds a field `confidence_floor: f32` to memory metadata, with default 0.0.

- A v1 reader reads a v1.1 record: it ignores the new field. (rkyv lets you skip fields the schema doesn't know.)
- A v1.1 reader reads a v1 record: the field defaults to 0.0.
- A v1.1 writer writes a v1 record: it sets the new field; reads from v1.0 readers see the field but ignore it.

This is forward-compatible across one version (v1 readers can read v1.1 data) and backward-compatible within the format (v1.1 readers can read v1 data).

### 2.3 Limits

Field addition is not free:

- Adds storage cost per memory.
- Adds wire-format size.
- Adds code complexity.

Each field addition should justify itself. Avoid speculative additions.

## 3. Edge kind evolution

The eight edge kinds are versioned with the format version. Adding a new edge kind:

- Existing readers don't understand it; they treat it as "unknown" and skip it during traversal.
- Adding the kind requires bumping the format version (or at least the on-disk format version).
- Auto-derivation rules for the new kind are defined alongside.

Removing an edge kind is harder. Existing data may have edges of that kind. The path:

1. Mark the kind as "deprecated" — new edges of that kind aren't created.
2. Continue to read existing edges of that kind.
3. After enough time, run a migration that removes existing edges (or rewrites them as a different kind).
4. Remove the kind from the codebase.

This takes multiple versions. Brain doesn't expect to remove kinds frequently.

## 4. Memory kind evolution

The three kinds (Episodic, Semantic, Consolidated) are versioned similarly. Adding a fourth kind:

- Reserved numeric value.
- Migration path: existing memories don't have the new kind; they keep their existing assignments.
- New auto-derivation rules.

Removing a kind: same care as removing an edge kind. Probably never happens in practice; we'd find a way to absorb the kind's semantics into one of the others.

## 5. Salience formula evolution

The salience formula is documented constant-by-constant in [`05_salience.md`](05_salience.md) §3. The formula itself is the substrate's decision; the **constants** are configuration.

When constants change between versions:

- Existing memories' stored salience is unchanged.
- New computations use the new constants.
- This may produce visible discontinuity at upgrade time — memories created just before the upgrade have salience computed differently from those just after. This is acceptable; salience is calibrated for the long run, and short-term inconsistency is unproblematic.

When the formula structure changes (e.g., adding a new term), it's a bigger deal:

- A migration may be needed to recompute salience for all memories.
- Or: a one-time decay event applies to all memories at upgrade time.

The structural change is rare; we expect to add new terms gradually rather than reformulate.

## 6. Identifier format evolution

`MemoryId`, `AgentId`, etc. have fixed formats. Changing them is a major break.

If we ever need to change `MemoryId`'s format (e.g., to reserve more bits for shard ID), it would:

- Bump the format version at every level.
- Require a migration of all stored data.
- Break clients that have cached `MemoryId`s.

We don't expect this. The current format has slack (32 reserved bits in `MemoryId`) that should accommodate foreseeable expansions.

## 7. The model fingerprint and embedding migration

A separate kind of evolution: when the embedding model changes.

### 7.1 The fingerprint

Every memory carries the model's fingerprint. The fingerprint is a stable identifier for the model — its weights, configuration, and version, hashed together. When the model is upgraded, the fingerprint changes.

### 7.2 Cross-model query refusal

A `RECALL` request triggers embedding the cue with the *current* model. When the substrate searches the index, it knows the index is built from vectors with various fingerprints (typically all the same, but possibly mixed during a migration).

If the cue's fingerprint differs from a candidate's fingerprint, the candidate is **excluded** from results. Cross-model similarity is meaningless — different models embed text differently, and dot products between vectors from different models are noise.

### 7.3 Migration path

When the operator changes the embedding model:

1. The new model is loaded; new memories are embedded with the new fingerprint.
2. Old memories with the old fingerprint are excluded from queries (fingerprint mismatch).
3. The `ADMIN_MIGRATE_EMBEDDINGS` operation is invoked. It:
   - Reads each old-fingerprint memory.
   - Re-embeds the text with the new model.
   - Writes the new vector and updates the fingerprint.
   - Rebuilds the affected HNSW entries.
4. After migration completes, all memories have the new fingerprint and are queryable again.

During migration, the substrate is partially queryable: only the migrated memories are visible. The migration is online (no downtime) but capacity-impacting (background work uses resources).

### 7.4 Why not just blanket-allow cross-model queries

The temptation: ignore the fingerprint mismatch and query as if all vectors were comparable. This is wrong because cross-model similarity scores are uncorrelated with semantic similarity. A user asking "What did I say about budgets?" with a new-model cue would get random results from old-model memories.

The substrate's strict policy — refuse cross-model results — preserves correctness at the cost of partial visibility during migration. This is the right trade-off; correctness matters more than convenience.

## 8. Wire-format evolution

The wire protocol has its own version negotiated at handshake. See [03. Wire Protocol](../03_wire_protocol/) §10.

Data-model changes that affect the wire format require a wire-protocol-version bump. Examples:

- Adding a new edge kind that's transmitted on the wire.
- Adding a new memory kind.
- Changing field encodings.

Some data-model changes don't affect the wire format (e.g., adding internal-only fields). These don't require a wire bump.

## 9. Storage-format evolution

Storage formats (arena, WAL, redb tables) have their own versions. See [05. Storage: Arena & WAL](../05_storage_arena_wal/) §3.

Most data-model evolutions imply storage-format changes. The exceptions are pure metadata changes that fit in existing fields.

## 10. Migration tooling

For migrations that require offline work:

- **`brainctl migrate`** reads the existing storage and writes the new format.
- Idempotent — safe to rerun.
- Documented in [14. Observability + Operations](../14_observability_ops/) §Migrations.

For migrations that can be done online (like embedding migration):

- **Online operation** invoked via `ADMIN_*` opcodes.
- Streaming, incremental, observable.
- Pausable — can be stopped and resumed.

Online migrations are preferred. Offline migrations are reserved for changes that fundamentally rewrite storage (rare).

## 11. Backward-incompatible changes are rare

In summary, the data model is designed to evolve smoothly. The common case is field addition or constant adjustment, neither of which requires user action. Edge or kind additions are versioned and gradual.

True backward-incompatible changes (removing a field, changing a type, restructuring a record) are designed to be rare. When they do occur, the migration path is documented with the change.

---

*Continue to [`10_failure_modes.md`](10_failure_modes.md) for data-model failure modes.*
