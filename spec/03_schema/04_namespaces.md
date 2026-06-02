# 03.04 Namespaces

How multiple schemas coexist under different namespaces in one
deployment. [`./00_purpose.md`](./00_purpose.md) §"Multiple schemas (namespaces)" sketches the
intent; this file is the operational contract.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"Multiple schemas
  (namespaces)".
- [`./03_validator.md`](./03_validator.md) §2.1 — namespace
  identifier grammar.
- [`./06_system_schema.md`](./06_system_schema.md) — `brain:`
  reserved namespace.

## 1. The contract

A deployment may host any number of namespaces. Each `SCHEMA_UPLOAD`
declares one namespace; subsequent uploads under the same
namespace bump that namespace's version. Different namespaces are
independent — uploading `acme` version 2 doesn't affect `crm` version 3.

Storage tables are **shared**:

- `entities` — one table, rows carry `entity_type_id` which carries
  its namespace transitively (via `entity_types` row).
- `statements` — one table, rows carry `predicate_id` (carries
  namespace via `predicates` row).
- `relations` — one table, rows carry `relation_type_id` (carries
  namespace via `relation_types` row).

Cross-namespace queries are queryable in v1 but **discouraged** —
clients need not surface convenience APIs for them; clients that
need cross-namespace must use lower-level lookups.

## 2. Identifier resolution rules

Within a schema document, type references resolve **only in the
declaring namespace**:

```
namespace acme

define entity_type Person { ... }
define relation_type reports_to {
    from: Person     # resolves to acme:Person
    to: Person
}
```

A reference like `from: crm:Person` is **not supported in v1**:
the validator rejects qualified references. → `UnresolvedTypeRef`.

This trades flexibility (cross-namespace edges) for clarity (each
schema is self-contained) and is one of the §07 open questions.

## 3. Wire-level qnames

Wire requests + responses use canonical qnames `namespace:name`
everywhere a type is referenced:

- `STATEMENT_CREATE.predicate: "acme:role"`.
- `RELATION_CREATE.relation_type: "acme:reports_to"`.
- `ENTITY_CREATE.entity_type: "acme:Person"` — string qname. The
  the v1 wire path uses the qname form exclusively.

## 4. Storage layout

A single deployment-wide schema-version counter doesn't work
because uploads to different namespaces are independent. Instead:

```
schema_versions
  key:   (namespace: &str, version: u32)
  value: SchemaVersionRow (rkyv-archived parsed schema)
```

Plus a single-row "active version" lookup:

```
schema_active_versions
  key:   namespace: &str
  value: u32  (the currently-active version)
```

`schema_upload` writes the row and bumps the active version
atomically inside one redb txn.

The `schema_version` field on `EntityMetadata` /
`StatementMetadata` / `RelationMetadata` already carries the
namespace's active version at the time of write. The version
number isn't unique across namespaces — but rows also carry the
namespaced type id, so the (namespace, version) pair is implicit.

## 5. Cross-namespace queries

Queries that need to span namespaces:

- `STATEMENT_LIST` filtered by `subject = entity_X` returns
  statements regardless of predicate namespace — the by-subject
  index doesn't filter on namespace.
- `RELATION_TRAVERSE` from an entity walks any relation type
  unless `relation_types: Vec<String>` filters explicitly. A
  caller that wants only `acme:*` relations must enumerate them.

The planner / query router will add
namespace-aware fan-out / filter semantics; v1 leaves this to the
caller.

## 6. Reserved namespace `brain:`

The `brain:` namespace is reserved for the system schema (§06).
User `SCHEMA_UPLOAD` requests declaring `namespace brain` are
rejected at validation time.

Built-in types like `brain:related_to`, `brain:Person`, `brain:is_a`
are seeded at `MetadataDb::open` time from the system schema
document.

## 7. Listing

`SCHEMA_LIST` (`0x0122`) returns the version history of a single
namespace. An admin form that lists *all* namespaces is deferred to
the admin opcode surface; the caller must know the namespace they
are inspecting.

## 8. Open questions

See [`.../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md). Notably:

- Q6 — Cross-namespace references in schema documents.
- Q7 — Cross-namespace traversal filter syntax.
- Q8 — Namespace renaming.

## 9. Tests

This section verifies:

- Two namespaces uploaded; versions independent.
- Active version per namespace persists across reopen.
- `brain:` upload rejected.
- Qualified reference in schema source → `UnresolvedTypeRef`.
- Cross-namespace lookup via lower-level path returns the
  type-id row regardless of caller's namespace context.
