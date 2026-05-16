# 21.06 System Schema

The built-in `brain:` namespace types are loaded by parsing a
static schema string at `MetadataDb::open` time — replacing the
hand-seeded `BUILTIN_PREDICATES` /
`BUILTIN_RELATION_TYPES` / Person bootstrap from phases
16.1 / 17.3 / 18.3.

This makes the parser + validator + persistence path the
load-bearing route for **every** type registered with the
substrate (including the built-ins), so the user-facing
`SCHEMA_UPLOAD` shares its code paths with the bootstrap.

Cross-references:
- [`./04_namespaces.md`](./04_namespaces.md) §6 — reserved
  `brain:` namespace.
- [`./05_versioning.md`](./05_versioning.md) §4 — built-in
  version stamping.
- Phase 16.1 / 17.3 / 18.3 commits — the hand-seeded paths being
  replaced.

## 1. Source

The system schema lives at:

```text
crates/brain-metadata/src/system_schema/schema.brain
```

It's `include_str!()`-embedded into the binary, so no runtime
file I/O. Single source-of-truth text; the validator + parser
operate on it like any user schema.

Sketch:

```text
# Brain system schema. Loaded at MetadataDb::open.
# Defines built-in entity types, predicates, and relation types
# used by the substrate for bootstrapping. Users CANNOT redefine
# anything under the `brain:` namespace.

namespace brain

# --- Entity types ---------------------------------------------

define entity_type Person {
    attributes {
        email:    text optional unique
        role:     text optional
        team:     text optional
        timezone: text optional
    }
}

# --- Predicates -----------------------------------------------

define predicate is_a {
    kind: Fact
    object: Entity<Any>
    description: "Subject is an instance of the object entity type."
}

define predicate has_name {
    kind: Fact
    object: Value<text>
    description: "Subject's canonical name as a text value."
}

define predicate mentions {
    kind: Fact
    object: Any
    description: "Generic mention — subject mentions object."
}

define predicate related_to {
    kind: Fact
    object: Entity<Any>
    description: "Generic relation between subject and object entities."
}

define predicate prefers {
    kind: Preference
    object: Value<text>
    description: "Generic Preference about the subject (any text value)."
}

define predicate scheduled {
    kind: Event
    object: Any
    description: "Generic Event scheduled at event_at_unix_nanos."
}

# --- Relation types -------------------------------------------

define relation_type related_to {
    from: Any
    to: Any
    cardinality: many-to-many
    description: "Generic relation between two entities."
}

define relation_type reports_to {
    from: Any
    to: Any
    cardinality: many-to-one
    description: "Generic ManyToOne relation; second create on same `from` auto-supersedes."
}

define relation_type co_authored {
    from: Any
    to: Any
    cardinality: many-to-many
    symmetric: true
    description: "Generic symmetric ManyToMany relation."
}
```

## 2. Bootstrap path

At `MetadataDb::open`:

```text
1. Open or create the redb file.
2. Run `schema_active(rtxn, "brain")`:
   - if Some(v): system schema already seeded; nothing to do.
   - if None: seed.
3. Seed path:
   - parse(include_str!("schema.brain")) → AST.
   - validate(&ast) → ValidatedSchema (panic on error — this is
     compile-time content; a failure is a build bug).
   - schema_upload(&wtxn, &validated, now): writes the
     SCHEMA_VERSIONS row for ("brain", 1), the active-version
     row, and the entity_types / predicates / relation_types
     definitions.
   - commit.
```

The seed is idempotent: re-opening an existing DB skips the seed
because `schema_active(rtxn, "brain")` already returns `Some(1)`.

If the system schema content changes between versions of the
binary, the running deployment **doesn't** auto-upgrade. The next
deployment that opens the redb file with a newer binary picks up
the new content only if the existing `brain` namespace version is
bumped explicitly via a binary-bootstrap migration (see §07 Q11).
For v1, this is fine — the system schema is stable across the
release.

## 3. What this replaces

| Pre-19 (hand-seeded)                       | Post-19 (system schema)                |
|---|---|
| `BUILTIN_PREDICATES` const in `db.rs`      | `define predicate ...` blocks         |
| `BUILTIN_RELATION_TYPES` const in `db.rs`  | `define relation_type ...` blocks     |
| `seed_builtin_entity_types` for `Person`   | `define entity_type Person { ... }`   |
| `seed_builtin_predicates` writer fn        | `seed_system_schema` (parser path)    |
| `seed_builtin_relation_types` writer fn    | same                                  |

The downstream `EntityTypeDefinition`, `PredicateDefinition`,
`RelationTypeDefinition` rows are byte-identical to the previous
hand-seeded versions. Integration tests don't change. The diff
is internal to the bootstrap path.

## 4. Version 1 stability

The system schema ships at version 1 and SHOULD NOT change shape
during v1. Adding new types is forwards-compatible (write paths
ignore unknown ids); removing or renaming is not (existing rows
in user DBs would break references).

The validator-version field (§05 §2.1) gives the substrate a way
to detect when the validator's understanding of the system schema
has evolved. v1 fixes validator-version = 1.

## 5. Tests (phase 19.7)

Phase 19.7 verifies:

- `MetadataDb::open` on a fresh dir parses + applies the system
  schema; `schema_active("brain")` returns `Some(1)`.
- Re-opening the same dir is a no-op (no second version row).
- All built-in type IDs (Person, related_to, reports_to,
  co_authored, brain:is_a, etc.) match the pre-19 hand-seeded
  IDs — verifies the system schema is the same definitions in DSL
  form.
- An invariant test in `system_schema/mod.rs` parses + validates
  the embedded string at compile time of the test suite (no
  runtime panic at `MetadataDb::open`).

## 6. Open questions

See [`./07_open_questions.md`](./07_open_questions.md). Notably:

- Q11 — Binary-bootstrap migration when the system schema's
  content changes across binary versions.
- Q12 — Should the system schema be queryable via the same
  `SCHEMA_GET` opcode the user types use? Yes; the only
  difference is `namespace = "brain"` is read-only at upload.
