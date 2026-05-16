# 19.7 — System schema replaces hand-seeded built-ins

Load-bearing sub-task: replaces the `BUILTIN_PREDICATES` /
`BUILTIN_RELATION_TYPES` / Person bootstrap from 16.1 / 17.3 / 18.3
with a parsed-and-applied static `schema.brain` document. This
makes the parser + validator + persistence + intern fan-out the
**single** path every typed registration goes through, including
the substrate's own built-ins.

## Surface changes

### brain-protocol::schema

- Add `validate_system_schema(&Schema) -> Result<ValidatedSchema, ValidationErrors>`
  with identical rules to `validate` **except** it allows
  `namespace = "brain"`. Same `ValidatedSchema` newtype. Used only
  by the system-schema seed path.

### brain-metadata

- Add `entity_type_intern(wtxn, name, schema_blob, schema_version, now)`
  mirroring `predicate_intern` / `relation_type_intern`: idempotent
  on name (linear scan; few rows), allocates next id on first
  registration. Returns `EntityTypeId`. Person → id `1` because
  it's the first to be interned.
- Add `apply_schema_definitions(wtxn, &ValidatedSchema, schema_version, now)`
  walking `validated.items` in source order:
  - `EntityType` → `entity_type_intern`.
  - `Predicate` → translate object kind + ObjectTypeDecl into
    `(StatementKind option, object_type_constraint_byte)` and call
    `predicate_intern`.
  - `RelationType` → translate `from_type` / `to_type` (`"Any"` →
    None; else lookup) + cardinality + symmetric → `relation_type_intern`.
  - `Extractor` → ignored in 19.7 (phase 20).
- `schema_upload`: after writing the version row + active pointer,
  call `apply_schema_definitions`. This is the §21/05 §1 lifecycle
  fully realised.
- New module `system_schema`:
  - `crates/brain-metadata/src/system_schema/schema.brain` — the
    static DSL document (lifted from §21/06).
  - `crates/brain-metadata/src/system_schema/mod.rs` — exposes
    `pub const SYSTEM_SCHEMA_SOURCE: &str = include_str!("schema.brain");`
    and `seed_system_schema(db: &mut Database) -> Result<(), …>`.
  - Seed path:
    1. Open rtxn; `schema_active(rtxn, "brain")` → if `Some(_)`, return Ok.
    2. parse → validate_system_schema → `schema_upload` → commit.
    3. Panic if parse / validate fails: it's `include_str!` content;
       a failure is a build bug.
- `MetadataDb::open` calls `seed_system_schema(&mut db)` in place
  of the three hand-seeded fns.
- **Delete:** `BUILTIN_PREDICATES`, `BUILTIN_RELATION_TYPES`,
  `seed_builtin_entity_types`, `seed_builtin_predicates`,
  `seed_builtin_relation_types`, the
  `MetadataDbError::BuiltinPredicateSeed` /
  `BuiltinRelationTypeSeed` variants (replaced by `SystemSchemaSeed`).

### Other crates

No external API changes. The wire `SCHEMA_UPLOAD` handler from
19.6 already calls `schema_upload`; the new fan-out kicks in
automatically. User uploads of `namespace brain` still rejected by
`validate` (not `validate_system_schema`).

## Files written / modified

| Path | Change |
|---|---|
| `crates/brain-protocol/src/schema/validator.rs` | Add `validate_system_schema`; refactor `validate` to a parameterised inner. |
| `crates/brain-protocol/src/schema/mod.rs` | Re-export `validate_system_schema`. |
| `crates/brain-metadata/src/entity_type_ops.rs` | New: `entity_type_intern` + qname-by-name lookup helper. |
| `crates/brain-metadata/src/schema_apply.rs` | New: `apply_schema_definitions` fan-out. |
| `crates/brain-metadata/src/schema_store.rs` | Call `apply_schema_definitions` from `schema_upload`. |
| `crates/brain-metadata/src/system_schema/mod.rs` | New: `seed_system_schema` + `SYSTEM_SCHEMA_SOURCE`. |
| `crates/brain-metadata/src/system_schema/schema.brain` | New: the DSL. |
| `crates/brain-metadata/src/db.rs` | Replace three seed callsites with one; remove BUILTIN_* + variants. |
| `crates/brain-metadata/src/lib.rs` | Module wiring + re-exports. |

## Predicate / relation mapping rules (in `schema_apply.rs`)

```rust
fn statement_kind_constraint(k: StatementKindAst) -> Option<StatementKind> {
    match k {
        Fact       => Some(Fact),
        Preference => Some(Preference),
        Event      => Some(Event),
        Any        => None,                 // no kind constraint
    }
}

fn object_type_constraint_byte(o: &ObjectTypeDecl) -> u8 {
    match o {
        Value { .. }   => 2,
        Entity { .. }  => 1,
        Memory         => 3,
        Statement      => 4,
        Any            => 0,
    }
}

fn cardinality(c: CardinalityAst) -> Cardinality {
    OneToOne | OneToMany | ManyToOne | ManyToMany   // 1:1 map
}

fn resolve_entity_type(
    wtxn: &WriteTransaction,
    name: &str,
) -> Result<Option<EntityTypeId>, …> {
    if name == "Any" { return Ok(None); }
    entity_type_lookup_by_name(wtxn, name)
}
```

`Entity<Person>` predicate objects: the byte=1 doesn't carry the
specific type id; the existing registry semantics are unchanged.
Storing the named-type ref inside the predicate row is a future
phase (§22+ typed predicate constraints).

## Order stability

Hand-seeded IDs were:
- `EntityTypeId(1)` → Person.
- Predicate ids 1..6 → `is_a, has_name, mentions, related_to, prefers, scheduled`.
- Relation-type ids 1..3 → `related_to, reports_to, co_authored`.

The new `schema.brain` interleaves these in the **same order** so
the intern allocator produces identical ids. An invariant
integration test asserts the post-19.7 ids match the pre-19.7
ids.

## Tests

### `crates/brain-metadata/src/system_schema/mod.rs`

1. `system_schema_parses_and_validates` — static text parses + passes
   `validate_system_schema` (no runtime panic).
2. `seed_first_open_creates_brain_v1` — fresh tempdir, open, expect
   `schema_active(rtxn, "brain") == Some(1)`.
3. `seed_reopen_is_idempotent` — open, drop, open again, expect
   still `Some(1)`, only one row in `schema_list`.

### `crates/brain-metadata/src/schema_apply.rs`

4. `entity_type_intern_assigns_id_1_to_person` — fresh db,
   apply schema with Person → Person resolves to `PERSON_ID`.
5. `apply_idempotent_on_existing_definitions` — apply same schema
   twice, second call is a no-op (no new ids).

### Existing tests

The four pre-existing failing tests (`builtin_relation_types_seed_idempotent`,
`history_walks_chain`, `many_to_one_auto_supersedes_on_from_side`,
`one_to_many_auto_supersedes_on_to_side`) should turn green after
19.7 because the system schema delivers the same built-ins via the
new path. Re-run after the implementation.

If they don't go green, scope-cut: revert the new path's behaviour
for the affected tests and surface the regression to the user.

## Out of scope

- Migration-on-binary-upgrade when `schema.brain` content changes
  (§21/07 Q11). Phase 22+ adds binary-bootstrap migration.
- Extractor application from system schema (none declared there;
  fan-out skips `SchemaItem::Extractor`).
- Per-namespace ID space (`brain:Person` vs `acme:Person` collide
  in the linear-scan entity_type lookup). v1 deployments don't mix
  brain and user entity_type names because users can't use the
  `brain:` namespace. Real per-namespace separation lands when
  user entity types arrive (later sub-tasks of phase 19).

## Single commit

`feat(metadata,protocol): 19.7 — system schema bootstrap`

## Verification

```
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
just docker cargo test -p brain-metadata --lib
just docker cargo test -p brain-server --test knowledge_compat
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```
