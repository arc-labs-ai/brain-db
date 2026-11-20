# 03.06 System Schema

The built-in `brain:` namespace types are loaded by parsing a
static schema string at `MetadataDb::open` time — replacing the
hand-seeded `BUILTIN_PREDICATES` /
`BUILTIN_RELATION_TYPES` / Person bootstrap from phases
16.1 / 17.3 / 18.3.

This makes the parser + validator + persistence path the
load-bearing route for **every** type registered with Brain
(including the built-ins), so the user-facing
`SCHEMA_UPLOAD` shares its code paths with the bootstrap.

Cross-references:
- [`./04_namespaces.md`](./04_namespaces.md) §6 — reserved
  `brain:` namespace.
- [`./05_versioning.md`](./05_versioning.md) §4 — built-in
  version stamping.
- Earlier hand-seeded paths for entity / statement / relation
  registry rows — superseded by the parsed system schema.

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
# used by Brain for bootstrapping. Users CANNOT redefine
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

# --- Procedural memory (behavior_*) ---------------------------
# Five Preference predicates an agent uses to record its own
# standing behavior. Materialised together by
# MATERIALIZE_PROCEDURAL into a single system-prompt block.

define predicate behavior_tone {
    kind: Preference
    object: Value<text>
    description: "Agent's preferred communication tone (e.g. 'async-first', 'concise')."
}

define predicate behavior_style {
    kind: Preference
    object: Value<text>
    description: "Agent's preferred output style (e.g. 'terse', 'verbose with examples')."
}

define predicate behavior_avoids {
    kind: Preference
    object: Value<text>
    description: "Behavior the agent declines to perform (e.g. 'speculation about user motives')."
}

define predicate behavior_prefers {
    kind: Preference
    object: Value<text>
    description: "Behavior the agent affirmatively prefers (e.g. 'cite sources inline')."
}

define predicate behavior_constraint {
    kind: Preference
    object: Value<text>
    description: "Hard constraint the agent must respect (e.g. 'never modify .env without confirmation')."
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
bumped explicitly via a binary-bootstrap migration.
For v1, this is fine — the system schema is stable across the
release.

## 3. Version 1 stability

The system schema ships at version 1 and SHOULD NOT change shape
during v1. Adding new types is forwards-compatible (write paths
ignore unknown ids); removing or renaming is not (existing rows
in user DBs would break references).

The five `behavior_*` predicates (added for procedural memory; see
[`../04_wire_protocol/03_opcodes.md`](../04_wire_protocol/03_opcodes.md))
occupy `PredicateId` slots 21–25 in deterministic order
(`behavior_tone = 21`, `behavior_style = 22`, `behavior_avoids = 23`,
`behavior_prefers = 24`, `behavior_constraint = 25`). The ids are
pinned because `MATERIALIZE_PROCEDURAL`'s renderer filters on the id
range — re-ordering would silently break the materialiser.

The validator-version field (§05 §2.1) gives Brain a way
to detect when the validator's understanding of the system schema
has evolved. v1 fixes validator-version = 1.

## 4. Tests

This section verifies:

- `MetadataDb::open` on a fresh dir parses + applies the system
  schema; `schema_active("brain")` returns `Some(1)`.
- Re-opening the same dir is a no-op (no second version row).
- All built-in type IDs (Person, related_to, reports_to,
  co_authored, brain:is_a, etc.) are allocated in a fixed,
  deterministic order, so a fresh seed reproduces them exactly.
- An invariant test in `system_schema/mod.rs` parses + validates
  the embedded string at compile time of the test suite (no
  runtime panic at `MetadataDb::open`).

## 5. Reading the system schema

The system schema is queryable through the same `SCHEMA_GET` opcode
the user types use. The only difference is that `namespace =
"brain"` is read-only at upload.
