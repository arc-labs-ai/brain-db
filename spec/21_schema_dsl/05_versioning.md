# 21.05 Schema Versioning

How `SCHEMA_UPLOAD` increments a namespace's version, persists the
parsed document, and exposes the active version for downstream
validation. **Migration plan computation is explicitly out of
scope for v1** — see [§07](./07_open_questions.md) Q3.

Cross-references:
- [`./04_namespaces.md`](./04_namespaces.md) §4 — per-namespace
  version counter storage.
- [`../28_knowledge_wire_protocol/05_schema_frames.md`](../28_knowledge_wire_protocol/05_schema_frames.md)
  §2 — `SCHEMA_UPLOAD` wire shape.
- [`../17_knowledge_model/00_purpose.md`](../17_knowledge_model/00_purpose.md)
  — `schema_version: u32` on every write.

## 1. The lifecycle

```text
SCHEMA_UPLOAD(text or programmatic) →
    parse → AST
    validate → ValidatedSchema
    schema_upload(wtxn, &validated_schema, now):
        lookup current active version for namespace
        new_version = current + 1
        write SCHEMA_VERSIONS_TABLE row (namespace, new_version)
        write SCHEMA_ACTIVE_VERSIONS_TABLE (namespace -> new_version)
        write entity_type / predicate / relation_type rows for
          new + changed definitions (delegates to the existing
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

These are all **silently allowed** in v1 (since we have no prior
deployments to break). Live rows pointing to removed types remain
queryable via their typed id; the type registry simply no longer
has an active definition row.

Phase 22+ may introduce diff-time enforcement. Per the project's
no-migration v1 directive, this is **explicitly deferred** rather
than partially implemented.

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
counter twice. Phase 22 admin may add a "no-op upload" suppression
flag if it becomes load-bearing.

## 7. Wire surface

Per §28/05:

- `SCHEMA_UPLOAD` (`0x0120`): text or AST form; returns new version
  or validation errors.
- `SCHEMA_GET` (`0x0121`): `(namespace, version)` → full
  `SchemaView` (parsed + canonical text).
- `SCHEMA_LIST` (`0x0122`): `namespace` → version history (newest
  first).
- `SCHEMA_VALIDATE` (`0x0123`): dry-run; returns errors or
  would-be-version.

## 8. Tests (phase 19.5)

Phase 19.5 verifies:

- First upload to a fresh namespace → version 1.
- Second upload → version 2.
- Upload that fails validation → no version bump.
- Active version persists across reopen.
- `schema_get(ns, v)` returns the exact uploaded AST.
- `schema_list(ns)` returns versions newest-first.
- `SCHEMA_VALIDATE` returns errors without persisting.
- Two namespaces; versions independent.
- `brain:` upload rejected.

## 9. Open questions

See [`./07_open_questions.md`](./07_open_questions.md). Notably:

- Q3 — Migration plan computation (deferred).
- Q9 — Schema deletion / rollback.
- Q10 — Validator-version evolution.
