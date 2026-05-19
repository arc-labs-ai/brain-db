# 15 — Schemas

A **schema** is how you tell Brain what your domain looks
like — what kinds of entities you have, what predicates
exist, what relation types, and what extractors should mine
memories for them. Declaring a schema is what activates the
knowledge layer.

A Brain schema is *not* a SQL schema. There are no columns,
no primary keys, no foreign keys, no table definitions. It's
a small declarative language that describes *the types your
domain uses* and *the extractors that produce them from
text*.

---

## A worked example

```
# A small schema for an engineering-team agent.

entity_type Person {
    attributes {
        email: String?
        role:  String?
    }
}

entity_type Project {
    attributes {
        codename:   String
        started_at: Timestamp?
    }
}

predicate prefers   (Person, *)            # any object shape
predicate works_on  (Person, Project)
predicate role_of   (Person, String)

relation_type reports_to {
    domain = Person
    range  = Person
}

relation_type works_on {
    domain = Person
    range  = Project
}

extractor person_mentions {
    kind     = pattern
    target   = entity Person
    patterns = [ /\b[A-Z][a-z]+\b/ ]
    confidence = 0.7
}

extractor preference_extraction {
    kind          = llm
    target        = statement Preference
    model         = "claude-haiku-4-5"
    prompt        = """ ... """
    schema        = { ... }
    cost_budget   = "$0.001 per memory"
    trigger       = on encode where memory.kind = episodic
}
```

A client uploads this text:

```
session.schema_upload(
    namespace = "acme",
    source    = """ ... above DSL ... """,
)
```

Brain parses the source, validates it, runs it through the
schema-apply path, and flips the shard's schema gate to
*active*. From that point on, every `encode` runs through
the declared extractors, and the knowledge layer is alive.

---

## What a schema declares

Six categories:

| What | Example | Purpose |
|---|---|---|
| **Entity types** | `Person`, `Project` | Categories of entities. |
| **Entity-type attributes** | `email: String?` on `Person` | What typed fields entities of this type carry. |
| **Predicates** | `prefers`, `role_of` | Verbs for statements. Each has a domain + range signature. |
| **Relation types** | `reports_to`, `works_on` | Typed edges between entities. |
| **Extractors** | `person_mentions` | What turns text into entities/statements/relations. |
| **Namespace** | `acme` | The schema is scoped to a namespace; you can have multiple coexisting. |

That's the full surface. No SQL tables. No "columns." No
indexes (the substrate maintains the indexes it needs based
on the declared types).

---

## Schemas vs SQL schemas: the comparison

| Aspect | SQL schema | Brain schema |
|---|---|---|
| Defines | Tables, columns, constraints, indexes | Types, predicates, relations, extractors |
| Storage shape | Direct (each table is a thing) | Indirect (each type informs how the substrate generates rows from text) |
| Mutation | `ALTER TABLE`, migrations | `SCHEMA_UPLOAD` with version bump, optional backfill |
| Determinism on data | The schema *is* the data shape | The schema is the *type system*; data shape lives in the substrate's tables |
| Adding a type | `CREATE TABLE` | New `entity_type` block; existing data unchanged |
| Adding a relationship | `FOREIGN KEY` | New `relation_type` block; relations populate as extractors find them |

A SQL schema tells the database **what tables exist** and
constrains every row to fit. A Brain schema tells the
substrate **what types your domain uses** and lets extractors
populate them. The shapes on disk are the same regardless;
the schema just determines what extractors run and what
types Brain knows about.

This is why a Brain schema is so much smaller than a typical
SQL schema for the same domain. You're not modelling the
*tables*; you're modelling the *concepts*.

---

## The schema DSL, briefly

The DSL is intentionally small. You don't need to learn
a Turing-complete language to declare your domain. Six
constructs:

```
# An entity type with typed attributes.
entity_type <Name> {
    attributes { <field>: <Type>[?] ... }
}

# A predicate with a signature.
predicate <name> (<DomainType>, <RangeShape>)
    # RangeShape can be a type, * (any), or a literal type (String, etc.)

# A relation type.
relation_type <name> {
    domain    = <Type>
    range     = <Type>
    symmetric = true | false   # optional
}

# An extractor (three kinds: pattern | classifier | llm).
extractor <name> {
    kind   = pattern | classifier | llm
    target = entity <T> | statement <Kind> | relation <T>
    ...kind-specific fields...
}

# Optional: import a built-in schema.
use brain.entity_mentions
use brain.temporal_expressions

# Optional: namespace declaration (otherwise defaults to "default").
namespace "acme"
```

That's it. No expression language, no joins, no triggers (in
the SQL sense; extractor triggers are different).

The full grammar lives in
[`../reference/schema-dsl/`](../reference/schema-dsl/). This
chapter just sketches the shape.

---

## The system schema

Even without any user-uploaded schema, Brain ships with a
built-in **system schema** that declares the meta-types the
substrate itself uses internally: things like `MemoryKind`,
the standard `EdgeKind`s, and a small set of predicates the
knowledge layer uses for housekeeping (e.g., `derived_from`,
`mentioned_in`).

The system schema is loaded automatically at shard open.
It's idempotent — re-opens are no-ops. You don't have to
upload it; it's just *there*.

User schemas live in their own namespace alongside the
system schema. By default the substrate uses `"default"`;
you can declare a `namespace "..."` directive to pick a
custom one. Two deployments wanting to share the same Brain
server with separate knowledge graphs use different
namespaces (though more commonly you'd just use separate
data directories).

---

## The upload flow

When a client sends `SCHEMA_UPLOAD`:

1. **Parse.** The DSL is parsed into a typed AST. Syntax
   errors come back with line and column numbers; nothing
   is written.
2. **Validate.** Semantic checks: unknown types referenced,
   duplicate predicates, regex patterns too complex,
   contradictory declarations. Validation errors come back
   structurally.
3. **Dry-run option.** If the client set `dry_run = true`,
   the response is the validation result and nothing else
   happens. Useful for CI.
4. **Apply.** Otherwise, in one redb write transaction:
   - The DSL source is stored in the `schema_versions`
     table at the next version number for this namespace.
   - Every new entity type, predicate, and relation type is
     interned into its `*_types` table (it gets a `u32`
     ID).
   - Every declared extractor lands in the `extractors`
     table with its `enabled` flag.
   - The schema becomes the namespace's *active version*.
5. **WAL.** A `SCHEMA_UPDATE` record goes to the
   write-ahead log so recovery can replay the schema.
6. **Flip the gate.** The per-shard schema gate becomes
   "declared" if this is the first upload, and stays that
   way. Subsequent recalls take the hybrid path
   (chapter 17).
7. **Respond.** The client receives the new schema version
   number.

The whole thing is *atomic*: either the upload commits
fully (parse + validate + apply + WAL) or nothing happens.
A partial-apply state can't occur.

---

## Schema versioning

Every successful upload bumps the schema's version number
for that namespace:

```
schema_versions {
    namespace = "acme"
    version   = 1   → original upload
    version   = 2   → added a new extractor
    version   = 3   → changed a prompt
    ...
}
```

Statements, entities, and relations carry the version they
were produced under in their metadata. A query can filter
by version if you need to know "what does the schema look
like for this row?"

Bumping the version doesn't invalidate existing data. Old
extractions are still queryable. They may be *flagged stale*
by the `stale_extraction_detector` worker if their version
trails the active version too far behind; operators can
trigger a backfill to update them.

---

## What schema uploads can change

A schema upload is **additive by default**:

- **Add a new entity type.** Fine. Old entities of other
  types unaffected.
- **Add a new predicate.** Fine.
- **Add a new relation type.** Fine.
- **Add a new extractor.** Fine — it starts running on
  encodes after the upload. Optionally backfill it over old
  memories.
- **Modify an existing extractor** (new prompt, new model,
  new patterns). Fine — the extractor's `version` field
  bumps, extractions under the old version are flagged
  stale, and the `schema_migration` worker (chapter 07)
  can re-run them.
- **Disable / enable an extractor.** Fine. Doesn't delete
  anything; just stops it running on new encodes.

What it *can't* do without complications:

- **Remove an entity type, predicate, or relation type that
  existing data references.** Brain refuses — would orphan
  the rows. To genuinely remove, you'd first need an admin
  cascade that drops the dependent rows (not shipped in
  v1).
- **Change a predicate's signature incompatibly.**
  `predicate role_of (Person, String)` → `(Person, Person)`
  is rejected; existing String-object rows wouldn't fit.
- **Drop the entire schema.** No `SCHEMA_DROP` opcode in
  v1 (chapter 02 covers this). Start over with a fresh
  data directory.

The summary: schema evolution is *forward-compatible by
default*. Removing things is the hard case and v1 doesn't
ship the admin path for it.

---

## A schema is a contract for extractors

The schema doesn't just describe types — it tells extractors
*what to look for*. Two pieces interact:

- The **types** in the schema are what extractors target.
  An extractor with `target = entity Person` only makes
  sense if `entity_type Person { ... }` exists.
- The **extractors** in the schema are what populate the
  types. An entity type declared without any extractor
  targeting it stays empty forever (no one's mining for
  it).

So a usable schema needs both. A schema with types but no
extractors is a declaration of intent; a schema with
extractors but no matching types fails validation.

---

## Validation errors

A taste of what validation catches:

```
extractor person_mentions {
    kind     = pattern
    target   = entity Engineer        # not declared
    patterns = [ /.../ ]
}
# → validation: target type "Engineer" is not declared

predicate prefers (User, *)            # not declared
# → validation: domain type "User" is not declared

relation_type reports_to {
    domain = Person
    range  = Person
}
relation_type reports_to {             # duplicate
    domain = Person
    range  = Person
}
# → validation: duplicate relation type "reports_to"

extractor bad_pattern {
    kind     = pattern
    target   = entity Person
    patterns = [ "(a+)+b" ]            # exponential backtracking
}
# → validation: regex too complex (size limit exceeded)
```

Validation runs entirely on the parsed AST against the
existing schema state. It doesn't run any extractors or
touch the data. The dry-run mode is for CI: you upload the
schema with `dry_run = true`, see whether it passes, and
only commit when it does.

---

## Recap

- A schema declares your domain's **types and extractors**:
  entity types, predicates, relation types, and the
  extractors that populate them.
- Not a SQL schema. No columns. No foreign keys.
- The DSL is small — six constructs. Reference docs cover
  the grammar in full.
- Upload flow is parse → validate → apply (atomically) →
  WAL → flip the schema gate.
- Schemas are versioned per namespace. Each upload bumps
  the active version; older extractions stay queryable but
  may be flagged stale.
- Additive changes are the default; removing things isn't
  supported in v1.
- The system schema is always present, idempotent at boot;
  you don't have to upload it.

---

## Where to go next

- **Entities, statements, relations:** chapters 10–13.
- **Extractors in detail:** [chapter 14](14-extractors.md).
- **The DSL reference:**
  [`../reference/schema-dsl/`](../reference/schema-dsl/).
- **What the gate flip means:** [chapter 02](02-two-layer-model.md)
  — the two-layer model.
- **How retrieval changes:** [chapter 17](17-hybrid-retrieval.md)
  — hybrid retrieval.
