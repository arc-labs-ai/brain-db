# 03.05 Schema Versioning and Merge Semantics

How `SCHEMA_UPLOAD` merges declarations into a namespace's active
state, increments the namespace's version when something changes,
persists the parsed document, and exposes the active version for
downstream validation. Also covers the destructive `SCHEMA_REPLACE`
opcode (§9 below). **Migration plan computation is explicitly out of
scope for v1** — see [§07](../00_overview/04_open_questions_archive.md) Q3.

Cross-references:
- [`./04_namespaces.md`](./04_namespaces.md) §4 — per-namespace
  version counter storage.
- [`../04_wire_protocol/09_typed_graph_admin.md`](../04_wire_protocol/09_typed_graph_admin.md)
  §2 — `SCHEMA_UPLOAD` wire shape.
- [`../02_data_model/00_purpose.md`](../02_data_model/00_purpose.md)
  — `schema_version: u32` on every write.

## 1. The lifecycle

```text
SCHEMA_UPLOAD(text or programmatic) →
    parse → AST
    validate → ValidatedSchema
    classify_schema_merge(rtxn, &validated):
        for each declared item:
            existing = lookup in current active namespace
            match (existing, declared) on byte-equal constraints:
                None             → Insert
                Some & match     → Idempotent
                Some & mismatch  → Conflict → abort upload
        if all Idempotent and a current version exists → return it
                                                          (no bump)
    schema_upload(wtxn, &validated_schema, now):
        lookup current active version for namespace
        new_version = current + 1
        write SCHEMA_VERSIONS_TABLE row (namespace, new_version)
        write SCHEMA_ACTIVE_VERSIONS_TABLE (namespace -> new_version)
        write / adopt entity_type / predicate / relation_type rows
          for new + changed definitions (delegates to the existing
          17.3 / 18.3 intern paths)
        commit
    emit SchemaUpdated event
    return new_version
```

`schema_upload` is the one transactional path. On failure (parser
error, validator error, storage error) **nothing changes**:

- Version counter doesn't bump.
- Definitions table doesn't gain a row.
- No event emitted.

This is the atomicity contract — partial state from a failed
upload is impossible.

## 1a. Merge semantics

`SCHEMA_UPLOAD` is **additive-merge**, not replace. Each upload
classifies its declarations against the current persisted state
for the same namespace and falls into one of three outcomes per
item:

| Outcome | Predicate | Effect |
|---|---|---|
| **Match** | byte-equal constraints with the active row | idempotent no-op; the row keeps its existing version stamp |
| **Insert** | no prior declaration in the namespace | adds the row at `new_version = current + 1` |
| **Conflict** | prior declaration exists with **incompatible constraints** | upload aborts with `SchemaConflict { kind, name, namespace, conflict }`; nothing commits |

The classification is **all-or-nothing per upload**. A single
conflict aborts the entire transaction — the prior active version
stays untouched and no items from the upload land. This keeps the
operator's view simple: an upload either fully applies or fully
rejects; never half-merges.

### What "byte-equal" means per declaration kind

| Kind | Compared fields |
|---|---|
| Predicate | `kind_constraint`, `object_type_constraint_byte`, `description`, `is_stateful` |
| Relation type | `cardinality`, `is_symmetric`, `description` (`from` / `to` entity-type ids re-resolve at apply time; divergent type names trip there) |
| Extractor | `kind` byte (pattern / classifier / llm); the encoded `ExtractorDef` blob is compared at apply time |
| Entity type | the stored `schema_blob` (empty in v1, so the check is "no prior non-empty blob"). Entity types are global — see §1c |

### Idempotent re-upload

If every item classifies as **Match** and the namespace already
has an active version, `SCHEMA_UPLOAD` returns the current version
without bumping. Operators can safely re-apply the same DSL after
a deployment without producing a new version row.

### Adoption of implicit definitions

When an upload declares a predicate or relation_type qname that
already exists in the registry with `SchemaOrigin::ImplicitFromWrite`
(interned by a prior `STATEMENT_CREATE` / `RELATION_CREATE`), the
existing id is **preserved** and its origin flips to
`SchemaDeclared { version: new_version }`. This is the merge path
for the open-vocabulary writes that landed before the schema. See
§3.5 below for the post-commit flagging sweep that follows.

## 1b. `SchemaConflict` error shape

The wire-level conflict error carries enough detail for an
operator to fix the offending declaration without inspecting
storage:

```rust
SchemaConflict {
    kind:       &'static str,   // "predicate" | "relation_type" |
                                // "extractor" | "entity_type"
    name:       String,         // local item name (unqualified)
    namespace:  String,         // namespace the upload targeted
    conflict:   String,         // comma-separated field diffs
                                // e.g. "kind: stored=Fact new=Event,
                                //       object_type: stored=1 new=2"
}
```

A conflict aborts the upload before any writer txn opens; nothing
commits.

## 1c. Entity-type scope in v1

Entity types are **global** in the v1 storage model — they live
in a single shard-wide table without a namespace key. Two
consequences flow from this:

- A user upload to namespace `acme` that declares an entity type
  `Person` shares the same row as a later upload to namespace
  `crm` declaring `Person`. The merge predicate is "is there an
  existing row with this name?", not "is there an existing row
  with this name in this namespace?".
- `SCHEMA_REPLACE` (§9) does **not** drop entity types. Dropping
  them would race with rows in other namespaces that reference
  the same shared type.

This is a documented v1 limitation; namespacing entity types is
tracked in [`./04_namespaces.md`](./04_namespaces.md) §8.

## 2. The redb rows

### 2.1 `SCHEMA_VERSIONS_TABLE`

```rust
pub const SCHEMA_VERSIONS_TABLE:
    TableDefinition<'static, (&str, u32), SchemaVersionRow>;

pub struct SchemaVersionRow {
    pub namespace: String,
    pub version: u32,
    pub uploaded_at_unix_nanos: u64,
    pub source: Vec<u8>,                  // rkyv-archived AST blob
    pub source_text: Option<String>,      // original DSL text if uploaded as such
    pub validator_version: u32,           // bump when validator changes shape
}
```

The `source` field carries the **parsed** AST — readers
deserialise to `Schema` via the same rkyv path used by other rows.
`source_text` is verbatim DSL source for round-trips back through
the validator (e.g., re-validate under a newer validator version).

### 2.2 `SCHEMA_ACTIVE_VERSIONS_TABLE`

```rust
pub const SCHEMA_ACTIVE_VERSIONS_TABLE:
    TableDefinition<'static, &str, u32>;
```

Single-row-per-namespace lookup. `schema_active(rtxn, ns)` reads
this directly; no scan.

### 2.3 Existing rows carry `schema_version`

`EntityMetadata`, `PredicateDefinition`, `RelationTypeDefinition`,
`StatementMetadata`, `RelationMetadata` already carry a
`schema_version: u32` field from prior phases. The
`schema_upload` path stamps newly-interned definitions with the
**new** version; pre-existing definitions retain their original
version (the field reflects "the version that first registered
this definition", not "the current namespace version").

## 3. Validator-level diff (no migration plan)

The v1 validator runs **structural checks only** — it doesn't
diff against the prior version of the same namespace.

Operations that would require migration semantics:

- Removing an entity type that has live entities.
- Changing an attribute's type.
- Removing or renaming a predicate.
- Tightening cardinality on an existing relation type.

These are all **silently allowed** (since Brain has no prior
deployments to break). Live rows pointing to removed types remain
queryable via their typed id; the type registry simply no longer
has an active definition row.

Diff-time enforcement is **explicitly deferred** under Brain's
no-migration directive, rather than partially implemented.

#### 3.0a Action vocabulary (deferred to v1.1+)

When migration-plan computation lands, each per-type difference between the old and new namespace versions is reconciled via one of three caller-selected actions:

| Action | Effect on existing rows |
|---|---|
| **re-extract** | A worker re-runs the matching extractor over the source memory; the new statement supersedes the old (`STATEMENT_SUPERSEDE`). |
| **keep** | The old row stays as-is. Reads still return it. The new active schema does not apply retroactively. |
| **tombstone** | The old row is marked tombstoned with reason `SchemaInvalidation`. |

Default is `re-extract`. This vocabulary is documented here as the target shape; the v1 implementation only ships the flagging sweep (§3.5) — picking and executing an action against flagged rows is a v1.1+ admin tool.

Embedding-model evolution (when the operator swaps in new BGE weights) is a separate concern — see [`../07_embedding/06_migration.md`](../07_embedding/06_migration.md) for the `ADMIN_MIGRATE_EMBEDDINGS` flow.

### 3.5 Adoption of implicit definitions + flagging sweep

When a `SCHEMA_UPLOAD` declares a predicate or relation-type
qname that already exists in the registry with
`SchemaOrigin::ImplicitFromWrite` /
`RelationTypeOrigin::ImplicitFromWrite` (interned by a prior
`STATEMENT_CREATE` / `RELATION_CREATE` in open-vocabulary mode),
`schema_upload` adopts the existing id:

- The `PredicateId` / `RelationTypeId` is **preserved** — no new
  id is allocated.
- The origin tag flips to `SchemaDeclared { version: new_version }`.
- Schema-declared constraints (`kind_constraint`,
  `object_type_constraint`, `from_type` / `to_type`,
  `cardinality`) take effect for **subsequent** writes against
  that id. Previously written rows are not retroactively
  validated.

After the upload commit, a one-pass **flagging sweep** runs over
the namespace's `statements` and `relations` tables:

- Rows whose `predicate` (statements) or `relation_type`
  (relations) is **not present** in the new active schema gain the
  `OUTSIDE_ACTIVE_SCHEMA` flag bit. Reads still return the rows
  normally; admin tools surface the flag for cleanup decisions.
- Rows whose definition is present clear the flag if previously
  set (e.g. a later upload re-introduced a previously-removed
  type).

The sweep is **single-pass per namespace** and runs inside the
post-commit worker, not the upload transaction itself — keeping
the upload commit latency bounded.

### 3.5a Post-commit flag sweep

The schema-flag transition is committed atomically with the
`SCHEMA_UPLOAD` write transaction — predicate / relation-type origin tags
flip from `ImplicitFromWrite` to `SchemaDeclared` inside the same wtxn
that bumps the namespace version. The **flag sweep** that re-evaluates
pending statements / relations against the new active schema runs as a
post-commit worker (see [`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md)).

The split matters for two reasons:

1. The upload commit completes the moment the schema is "real" — concurrent reads see the new active version immediately.
2. The flag re-evaluation can take time proportional to namespace row count; running it inside the upload txn would block the wire-level ack on a long scan.

The worker is idempotent on re-runs (the flag set is derived purely from the row's `predicate` / `relation_type` and the active schema), so a crash + restart between commit and sweep completion just re-sweeps next tick.

## 4. Built-in versions

The system schema (§06) ships at version `1` for the `brain:`
namespace on a fresh deployment. Subsequent user schemas in user
namespaces start at version `1` for that namespace. The system
schema cannot be re-uploaded; the validator rejects user uploads
of `namespace brain`.

## 5. `SCHEMA_VALIDATE` semantics

`SCHEMA_VALIDATE` (`0x0123`) runs **parse + validate** without
persisting. Useful for client-side checks before commit. Doesn't
bump the version counter; doesn't write any rows.

If validation succeeds, returns the would-be next version number
(`current + 1`) so the caller can reason about what `SCHEMA_UPLOAD`
would assign. If validation fails, returns the error list.

## 6. Canonical form

Two schemas that produce the same parsed AST + same `namespace`
+ same item order may have different source text (whitespace,
comments). The persisted `SchemaVersionRow.source_text` carries
the original; `SchemaVersionRow.source` carries the canonical
parsed form.

A round-trip `validate(parse(source_text)) == validate(parse(source_text'))`
holds when the two texts only differ in whitespace / comments /
trailing-comma punctuation.

The `schema_upload` path **doesn't** dedupe — two consecutive
uploads of structurally-identical schemas bump the version
counter twice. A "no-op upload" suppression flag may be added
if this becomes load-bearing.

## 7. Wire surface

Per [`../04_wire_protocol/09_typed_graph_admin.md`](../04_wire_protocol/09_typed_graph_admin.md):

- `SCHEMA_UPLOAD` (`0x0120`): additive-merge upload; returns new
  version or `SchemaConflict` / validation errors.
- `SCHEMA_GET` (`0x0121`): `(namespace, version)` → full
  `SchemaView` (parsed + canonical text).
- `SCHEMA_LIST` (`0x0122`): `namespace` → version history (newest
  first).
- `SCHEMA_VALIDATE` (`0x0123`): dry-run; returns errors or
  would-be-version.
- `SCHEMA_REPLACE` (`0x0127`): destructive namespace reset;
  admin-only; requires `force_drop_existing: true`. See §9.

## 8. Tests

This section verifies:

- First upload to a fresh namespace → version 1.
- Second upload → version 2.
- Upload that fails validation → no version bump.
- Active version persists across reopen.
- `schema_get(ns, v)` returns the exact uploaded AST.
- `schema_list(ns)` returns versions newest-first.
- `SCHEMA_VALIDATE` returns errors without persisting.
- Two namespaces; versions independent.
- `brain:` upload rejected.

## 9. `SCHEMA_REPLACE` — destructive counterpart

`SCHEMA_REPLACE` (`0x0127` request, `0x01A7` response) is the rare
destructive opcode for "wipe this namespace's declarations and
re-apply against a clean slate". The merge-only `SCHEMA_UPLOAD`
path cannot express this — by design — so removing a declaration,
narrowing a constraint, or changing an extractor kind requires the
explicit replace path.

### Semantics

Inside a single redb wtxn:

1. Drop every schema-declared predicate, relation_type, and
   extractor row whose namespace matches the request.
2. Run the supplied DSL through the same parse / validate / apply
   pipeline `SCHEMA_UPLOAD` uses. With the prior declared rows
   gone, the apply runs against a clean slate.
3. Bump the namespace version, write the new
   `SCHEMA_VERSIONS_TABLE` row, update
   `SCHEMA_ACTIVE_VERSIONS_TABLE`.
4. Commit. If the new schema's apply step fails (e.g. an
   `Any`-target relation_type pointing at a missing entity type),
   the whole wtxn drops and the previous schema state survives.

What is **not** dropped:

- **Implicit-from-write rows.** Predicates / relation_types
  interned by prior `STATEMENT_CREATE` / `RELATION_CREATE` with
  origin `ImplicitFromWrite` stay put — they are not part of the
  declared vocabulary.
- **Entity types.** They are global in v1 (see §1c). Dropping them
  would race with rows in other namespaces.
- **Existing statements / relations rows.** Rows that referenced a
  now-dropped predicate / relation_type remain in storage. Reads
  still return them; the post-upload flag sweep (§3.5) marks them
  `OUTSIDE_ACTIVE_SCHEMA`.

### Confirmation flag

The request body carries `force_drop_existing: bool`. The handler
rejects with `InvalidRequest` if the flag is not exactly `true`.
The flag is the wire contract's explicit-confirmation step for an
irreversible operation — a typo in a client cannot accidentally
wipe a deployment's schema.

### Permission

`SCHEMA_REPLACE` is admin-only at the dispatch layer.
`SCHEMA_UPLOAD` (additive-merge) is the operator-routine path;
`SCHEMA_REPLACE` is the explicit destructive escape hatch.

### Response shape

```rust
SchemaReplaceResponse {
    namespace:        String,
    schema_version:   u32,    // 0 on validation error
    dropped_count:    u32,    // # of declared rows removed
    validation_errors: Vec<SchemaValidationErrorWire>,
}
```

Parse / validate errors ride in `validation_errors` (mirroring the
`SCHEMA_UPLOAD` response shape), not as `OpError`.

## 10. Open questions

See [`.../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md). Notably:

- Q3 — Migration plan computation (deferred).
- Q9 — Schema deletion / rollback.
- Q10 — Validator-version evolution.
