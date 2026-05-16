# 21.07 Open Questions

Schema-DSL-specific open questions. Wire-shape questions live in
[`../28_knowledge_wire_protocol/09_open_questions.md`](../28_knowledge_wire_protocol/09_open_questions.md).

## Active

### Q1 — `Schema.parent_version` for diff computation

[`./02_ast.md`](./02_ast.md) §1: `Schema` doesn't carry a
parent-version pointer. Adding one would let the migration
planner (phase 22+) diff against the previous version
cleanly.

**Target:** phase 22+. **Status:** deferred per v1 no-migration
scope.

---

### Q2 — Multi-document schemas per namespace

A user might prefer to split a 200-definition schema across
multiple files (one per concern: people, projects, events). v1
requires single-document uploads. `use other_namespace;` imports
or multi-file `SCHEMA_UPLOAD` payloads land post-v1.

**Target:** post-v1. **Status:** deferred. **Likely outcome:**
add a `Vec<SchemaDocument>` to `SchemaUploadRequest` with merge
semantics.

---

### Q3 — Migration plan computation

[`./05_versioning.md`](./05_versioning.md) §3: v1 validator runs
**structural** checks only. It does NOT compute deltas between
schema versions, doesn't reject removals of types with live
entities, doesn't enumerate which statements need re-extraction.

**Target:** v1.1+ (post-first-deployment). **Status:** explicitly
deferred per project scope. The v1 deployment has no existing
data to migrate; introducing migration semantics now would be
speculative and over-fit to imagined needs.

When this lands:

- Validator gets a `parent: &ValidatedSchema` arg and emits
  `MigrationStep` items for each affected resource.
- `MigrationPlan` is computed at validate time and surfaced to
  the operator via `SCHEMA_UPLOAD.dry_run = true`.
- A migration worker (phase 24+) executes the plan against live
  data.

---

### Q4 — Warnings vs errors split

[`./03_validator.md`](./03_validator.md) §4: v1 treats every
validation issue as an error. Some checks (e.g., "this attribute
should probably be a relation") are advisory; gating uploads on
them is overzealous.

**Target:** phase 22. **Status:** open. **Likely outcome:** add
`ValidationWarning` distinct from `ValidationError`; uploads
succeed with warnings.

---

### Q5 — Custom validation rules / plugins

Some deployments may want domain-specific rules ("our predicates
must follow `noun_verb_object` naming"). v1 ships fixed rules.

**Target:** post-v1. **Status:** deferred. **Likely outcome:**
declarative rule extension via a `validation_rules` section in
the schema document.

---

### Q6 — Cross-namespace references in schema documents

[`./04_namespaces.md`](./04_namespaces.md) §2: v1 forbids
qualified references like `from: crm:Person` from inside a
schema document. This rules out shared-type patterns.

**Target:** phase 23. **Status:** open. **Likely outcome:** add
`use crm.Person;` imports with explicit version pinning.

---

### Q7 — Cross-namespace traversal filter syntax

[`./04_namespaces.md`](./04_namespaces.md) §5: `RELATION_TRAVERSE
.relation_types: Vec<String>` requires the caller to enumerate
relation types if they want only one namespace's. A
`namespace:*` wildcard would help.

**Target:** phase 23. **Status:** open.

---

### Q8 — Namespace renaming

Operators may want to rename a namespace post-deployment (e.g.,
`acme` → `acme_corp`). v1 doesn't support this — namespace is
part of every type id's qname and is on disk.

**Target:** post-v1. **Status:** deferred. **Likely outcome:**
admin opcode that rewrites all rows under the old namespace,
heavyweight but doable.

---

### Q9 — Schema deletion / rollback

Can a deployment "delete" a namespace, or "roll back" to an
earlier version? v1 says no — schema versions are append-only
audit history. Operators wanting cleanup must do so manually via
direct redb access.

**Target:** post-v1. **Status:** deferred. **Likely outcome:**
hard-delete admin opcode with require-confirmation flag.

---

### Q10 — Validator-version evolution

[`./05_versioning.md`](./05_versioning.md) §2.1: `SchemaVersionRow`
carries a `validator_version: u32`. When the validator's rules
change (e.g., add a new check), previously-uploaded schemas may
now fail re-validation.

**Target:** phase 22+. **Status:** open. **Likely outcome:**
running schemas under their original validator version (read-only)
+ requiring re-upload through the new validator to upgrade.

---

### Q11 — Binary-bootstrap migration for system schema

[`./06_system_schema.md`](./06_system_schema.md) §2: when a new
binary ships with a changed system schema (added type / changed
description), the deployment doesn't auto-upgrade. Existing
`brain` namespace at version 1 stays.

**Target:** v1.1+. **Status:** deferred. **Likely outcome:** add
a binary-bootstrap migration path that detects the diff at
`MetadataDb::open` and emits a "system schema mismatch — run
`brain admin migrate-system-schema`" warning.

---

### Q12 — Should system schema be queryable via SCHEMA_GET?

[`./06_system_schema.md`](./06_system_schema.md) §6: yes — the
read path doesn't distinguish `brain` from user namespaces; only
upload is gated.

**Target:** phase 19 (this phase). **Status:** resolved by §06 §6.

---

### Q13 — Derive macros + their generated schema contributions

[`../29_knowledge_sdk/00_purpose.md`](../29_knowledge_sdk/00_purpose.md)
§"Phase scope" lists `#[derive(BrainEntity)] / BrainFact /
BrainRelation` macros in phase 19. These auto-generate trait
impls + a static schema fragment per type. Phase 19's "macros"
sub-task (19.9) may slip to phase 19b / phase 21 if scope creeps.

**Target:** phase 19.9 if scope allows, else phase 19b. **Status:**
open. **Risk:** proc macros are a large new surface; defer if
the rest of phase 19 lands first.

---

### Q14 — Pest vs hand-rolled parser

[`./01_grammar.md`](./01_grammar.md) §"Parser implementation
choice" prefers `pest`. v1 will use pest 2.7. Alternative:
hand-rolled recursive-descent, smaller dep tree.

**Target:** phase 19.3 — chosen at implementation time. **Status:**
provisional pest.

---

### Q15 — §28/05 wire spec vs §21/05 truth

[`../28_knowledge_wire_protocol/05_schema_frames.md`](../28_knowledge_wire_protocol/05_schema_frames.md)
was authored before this section's no-migration directive landed.
Several §28/05 fields encode migration semantics that v1 doesn't
implement:

| §28/05 field | v1 behaviour |
|---|---|
| `SchemaUploadRequest.allow_breaking` | Accepted and ignored. |
| `SchemaUploadResponse.backward_compatible` | Always `true`. |
| `SchemaUploadResponse.migration_summary` | Replaced with `migration_summary_blob: Vec<u8>` (empty). |
| `SchemaGetRequest.version_id` (no namespace) | Phase 19.6 extends with a `namespace: String` field. |
| `SchemaUpdatedEvent` | Phase 19.6 adds `namespace: String`. |
| `SchemaListRequest.cursor` | Accepted and ignored (single-frame v1). |
| Admin authorization (§28/05 §8) | Open access in v1 — phase 21 admin adds auth. |

When §28/05 is next edited, fold these resolutions back. None of
the spec divergences alter the wire opcode table.

**Target:** §28/05 cleanup pass post-v1. **Status:** resolved in
phase 19.6 implementation.

## Resolved

(Q12 by §06 §6.)
