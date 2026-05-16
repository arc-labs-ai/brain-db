# Phase 19 — Schema DSL

Implements the user-facing schema declaration language: a single
declarative document declaring entity types, predicates, relation
types, and (for spec-completeness) extractors. The substrate parses
+ validates the document at upload time, persists versioned schema
documents in redb, and uses the active schema to drive validation
on entity / statement / relation writes.

## Prerequisites

- Phase 18 complete (`phase-18-complete` at `356f797`).
- Branch off `dev`: `feature/phase-19-schema-dsl`.

## Branch

`feature/phase-19-schema-dsl` (created off `dev`).

## Migration scope — explicitly NONE

Per user direction: **no migration plan computation**. We have no
existing deployments; there's nothing to migrate. Phase 19 keeps:

- **Schema versioning** — each accepted schema increments a version
  counter. Entities / statements / relations carry the
  `schema_version` they were written under (already in place from
  16/17/18).
- **Static validation** — the validator rejects schemas that
  reference unknown types, mismatched kinds, etc.

Phase 19 explicitly **omits**:

- Migration plan computation (which extractors changed, what needs
  re-extraction).
- The migration worker.
- The `keep / re-extract / tombstone` per-statement migration
  semantics.
- "Refuse to remove type with live entities" compatibility checks.

These all land in a future phase once the v1 deployment actually
needs migrations. Tracked as deferral in §21 open questions.

## Scope already-prepared

- `EntityType` / `PredicateDefinition` / `RelationTypeDefinition`
  redb rows from phases 15.1 / 16.1 / 17.3 / 18.3.
- `schema_versions` redb table from 15.1 (rkyv row exists).
- Spec §28/05 schema wire frames at §03-depth (sitting B,
  phase 16).
- §29/00 phase-scope already lists "phase 19 — derive macros +
  SchemaBuilder".

## Spec-first discipline — §21 backfill required first

Current §21 has 2 files (`00_purpose.md`, `01_grammar.md`). §03
depth is 16 files; pragmatically we'll match §19 / §20's 8-file
depth.

**§21 backfill files (sub-task 19.1):**

```
spec/21_schema_dsl/
├── 00_purpose.md             (live — overview + grammar overview)
├── 01_grammar.md             (live — formal EBNF)
├── 02_ast.md                 (new — typed AST shape consumed by validator + storage)
├── 03_validator.md           (new — static validation rules + error model)
├── 04_namespaces.md          (new — namespace isolation + cross-namespace reads)
├── 05_versioning.md          (new — version counter, schema_versions row,
│                              schema_version field on writes)
├── 06_system_schema.md       (new — built-in `brain:` types parsed from a
│                              static schema string at MetadataDb::open)
├── 07_open_questions.md      (new — derive macros, migration plan (deferred),
│                              cross-namespace traversal, schema diff)
└── 08_references.md          (new — cross-links to §17, §18, §19, §20, §28/05)
```

Bundled spec edits:

- §16/02 §2.4 → renumber + add new §2.5 (or extend §2.4) for
  schema-layer perf targets (UPLOAD / VALIDATE / GET / LIST).
- §29/00 phase-scope — flip 19 to "this phase".

## Sub-tasks

### 19.1 — §21 backfill + bundled spec edits

**Reads:** §21/00 + §21/01 + §17/00 + §18/00 + §20/00.
**Writes:** 7 new §21 files + §16/02 + §29/00.
**Done when:** §21 mirrors §19's 8-file depth.

### 19.2 — Schema AST in brain-core (or brain-protocol)

**Writes:** `crates/brain-protocol/src/schema/ast.rs` (pure value
types).
**Done when:** `Schema` / `SchemaItem` / `EntityTypeDef` /
`PredicateDef` / `RelationTypeDef` / `ExtractorDef` / `AttributeDecl`
/ `AttrType` / `Modifier` / `ObjectType` / `TraversalCardinality`
types compile. Serde + rkyv derived where useful (wire transport).
**Pitfalls:** AST is the public contract between the parser, the
validator, the storage layer, and the SDK SchemaBuilder. Keep it
small + composable.

### 19.3 — DSL parser

**Reads:** §21/01.
**Writes:** `crates/brain-protocol/src/schema/parser.rs` (pest 2.x).
**Done when:** All examples in §21/00 + §21/01 parse without error
into the AST. Malformed inputs surface clear `ParseError` with line
+ column.
**Pitfalls:** Pest grammar mirrors the EBNF; comment / whitespace
handling; heredoc strings.

### 19.4 — Schema validator

**Reads:** §21/03 (new from 19.1).
**Writes:** `crates/brain-protocol/src/schema/validator.rs`.
**Done when:** Static validation surfaces:
- Unresolved type references (`from: Person` where `Person` not declared).
- Predicate `kind` / `object` mismatch (`Preference` predicate with `Statement` object → invalid).
- Duplicate type definitions.
- Extractor target references resolve.
- Symmetric + cardinality combinations validated.

### 19.5 — Schema persistence

**Reads:** §21/05 (new), spec §28/05 §2-§5 wire shapes.
**Writes:** `crates/brain-metadata/src/schema_store.rs`.
**Done when:**
- `schema_upload(wtxn, &Schema, now) -> SchemaVersion` — validates
  schema vs validator + previous version; writes new
  `SchemaVersionRow` to `SCHEMA_VERSIONS_TABLE`; bumps the version
  counter.
- `schema_get(rtxn, version)` / `schema_active(rtxn)` /
  `schema_list(rtxn)`.
- New schema applies to subsequent entity / predicate / relation
  validation immediately.

### 19.6 — Wire opcodes 0x0120-0x0123

**Reads:** §28/05.
**Writes:**
- `crates/brain-protocol/src/knowledge/schema_req.rs` + `_resp.rs`.
- `crates/brain-ops/src/ops/knowledge_schema.rs` (handlers).
- Extend `Opcode` enum + `RequestBody` / `ResponseBody`.

Opcodes:
- `SCHEMA_UPLOAD` (0x0120) — parse + validate + persist; emit
  `SchemaUpdated` event.
- `SCHEMA_GET` (0x0121) — by version.
- `SCHEMA_LIST` (0x0122) — version history.
- `SCHEMA_VALIDATE` (0x0123) — parse + validate without persisting
  (dry run).

`EXTRACTOR_LIST` / `EXTRACTOR_DISABLE` (0x0124 / 0x0125) defer to
phase 20.

### 19.7 — System schema (replaces hand-seeded built-ins)

**Reads:** §21/06 (new).
**Writes:**
- `crates/brain-metadata/src/system_schema.rs` — static schema
  string with the built-in `brain:*` types + the seed function that
  parses + applies it at `MetadataDb::open` time.
- Delete the hand-seeded `BUILTIN_PREDICATES` /
  `BUILTIN_RELATION_TYPES` / `seed_builtin_entity_types` (collapse
  into one path that goes through the parser).
**Done when:** built-in types load from the static schema string;
all integration tests still pass.
**Risk:** This is the load-bearing change of phase 19 — it
exercises the parser + validator + persistence end-to-end on the
seed path. If it works, the user-facing `SCHEMA_UPLOAD` path comes
for free.

### 19.8 — SDK schema builders

**Reads:** §29/00 §"Schema management".
**Writes:**
- `crates/brain-sdk-rust/src/knowledge/schema.rs` — programmatic
  `SchemaBuilder` API + `client.schema().upload() / .validate() /
  .get() / .list()` entry points.
**Done when:**
- `SchemaBuilder::new("acme").entity_type::<Person>().predicate(...).relation_type(...).build()` returns a `Schema` value.
- `client.schema().upload(&schema).await` round-trips.
- `client.schema().upload_text("schema.brain text").await` for
  source-text uploads.

Defers derive macros (`#[derive(BrainEntity)]` etc.) to a follow-up
sub-task (19.9) — they're proc macros in a new crate.

### 19.9 — Derive macros (optional / stretch)

**Writes:** new crate `brain-sdk-macros` with `BrainEntity`,
`BrainFact`, `BrainPreference`, `BrainEvent`, `BrainRelation`
proc macros that produce trait impls + schema-builder contributions.

**Risk:** Proc macros are large surface; may split into its own
phase 19b if scope creeps. Initial implementation: `BrainEntity`
only — generates `impl BrainEntityType for T` + a static schema
fragment.

If scope-cuts: defer entire macro work to phase 21 / 22 and ship
phase 19 with programmatic SchemaBuilder only.

### 19.10a — Integration tests

**Writes:**
- `crates/brain-server/tests/knowledge_schema_wire.rs` — wire
  smoke for all 4 schema opcodes + error paths.
- `crates/brain-server/tests/knowledge_schema_phase_exit.rs` —
  upload schema → create entity of declared type → upload v2 with
  new predicate → validate that v1 entities remain queryable.
- `crates/brain-sdk-rust/tests/knowledge_schema.rs` — SDK
  builder + upload round-trip via mock server.

### 19.10b — Bench + ROADMAP + phase exit + tag

**Writes:**
- `crates/brain-metadata/benches/schema_ops.rs` — parse + validate
  + upload at 100-definition fixture.
- ROADMAP update marking phase 19 ✓.
- User-authorised tag `phase-19-complete`.

## Suggested commit cadence (~11 commits)

1. `19.1` — §21 backfill (single commit; doc-only).
2. `19.2` — Schema AST.
3. `19.3` — DSL parser.
4. `19.4` — Validator.
5. `19.5` — Persistence + schema_store.
6. `19.6` — Wire opcodes + handlers.
7. `19.7` — System schema replacing hand-seeded built-ins.
8. `19.8` — SDK SchemaBuilder + .schema() entry.
9. `19.9` — Derive macros (or split / defer).
10. `19.10a` — Integration tests.
11. `19.10b` — Bench + ROADMAP + exit.

## Risks

- **System schema replacing built-ins (19.7) is load-bearing.**
  Touching every test that relied on `Person` / `brain:related_to`
  / etc. Tests are extensive across entity / statement / relation
  layers. Plan: keep the system schema *parsed* at startup and
  emit the same `EntityTypeDefinition` / `PredicateDefinition` /
  `RelationTypeDefinition` rows as before — no behavioural change.
- **Pest parser surface.** Adding `pest` is a new workspace dep
  (large). Alternative: hand-rolled recursive-descent (smaller,
  more control). Pin pest 2.7 if we go that route.
- **Derive macros (19.9) may not fit phase 19.** If proc macros
  spiral, scope-cut to "programmatic SchemaBuilder only" and defer
  macros to phase 19b / phase 21.
- **Migration explicitly out of scope** per user direction. v1
  doesn't compute or run migration plans. The validator still
  enforces forward-compatibility on the same schema (no duplicate
  types, etc.) but doesn't compute deltas between versions.

## Out of scope (this phase)

- **Migration plan computation** — explicitly omitted per user
  direction.
- **Extractor execution** — phase 20-21.
- **EXTRACTOR_LIST / _DISABLE wire opcodes (0x0124 / 0x0125)** —
  phase 20.
- **Schema upload via the `schema.brain` file format from disk**
  through a CLI — phase 22 admin.
- **Cross-namespace traversal / shared-type imports** — post-v1.
- **Schema diff / what-changed UI** — post-v1.

## Verification gate (per sub-task)

```
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo test -p brain-core -p brain-protocol -p brain-sdk-rust
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```

## After phase 19

Phase 20 — Extractors (pattern / classifier / LLM). The schema DSL
gives extractors a stable declarative target; the extractor
implementations consume the AST.
