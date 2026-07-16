# 03.04 Namespaces

A namespace is the **tenant (company) data boundary** *and* the
schema/type-declaration scope. One company → one namespace. Within a
namespace, applications are distinguished by `agent`; the effective
isolation scope of every record is the pair **`(namespace, agent)`** —
namespace the outer wall (company), agent the inner wall (application).
[`./00_purpose.md`](./00_purpose.md) §"Multiple schemas (namespaces)" sketches the intent; this
file is the operational contract.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"Multiple schemas (namespaces)".
- [`./03_validator.md`](./03_validator.md) §2.1 — namespace identifier grammar.
- [`./06_system_schema.md`](./06_system_schema.md) — `brain:` reserved namespace.
- [`../04_wire_protocol/04_handshake.md`](../04_wire_protocol/04_handshake.md) §10 — the
  `(namespace, agent, permissions)` scope is derived from the
  authenticated key, never client-supplied.

## 1. The contract

A deployment hosts any number of namespaces (tenants). A namespace is
created when an operator provisions it (mints the first key bound to it);
there is **no implicit or default namespace**. Each `SCHEMA_UPLOAD`
declares one namespace and may only target the caller's own namespace;
subsequent uploads under that namespace bump its version. Different
namespaces are fully independent — uploading `acme` v2 doesn't affect
`crm` v3.

**Namespace is the ownership boundary.** Every memory, entity,
statement, and relation is *owned by exactly one namespace*, recorded as
an interned `namespace_id` on the record and folded into the prefix of
every secondary index key. Combined with the owning `agent`, the
`(namespace, agent)` pair scopes every read and write.

**Cross-namespace access is forbidden.** No read returns another
namespace's data; no request flag (`agent_filter`, `include_other_agents`)
widens past the caller's namespace — they only widen *within* it. A
request that names another namespace, or a redundant namespace at all, is
rejected (the namespace is taken from the authenticated key, see §3).

**Fail-closed.** Namespace is a required attribute of every write. A
write whose namespace cannot be resolved to a real, provisioned tenant
namespace is **rejected** — never silently bucketed. See
[`../04_wire_protocol/07_error_handling.md`](../04_wire_protocol/07_error_handling.md)
(`NamespaceRequired` / `NamespaceUnknown`). The reserved `brain`
namespace (§6) is **read-only to users**: it owns only the system
schema's own type/registry rows and is never a valid owner for user
data.

**Owner namespace vs type namespace.** These are distinct and must not
be conflated:
- The **owner namespace** is the tenant that owns a data row.
- The **type namespace** is the qname prefix on the *type* a row
  references (`acme:Role`, or the shared `brain:Person`).
A row owned by `acme` may reference a `brain:`-namespace system type; it
is still owned by, and walled to, `acme`.

## 2. Identifier resolution rules

Within a schema document, type references resolve in the declaring
namespace, with the shared `brain:` system namespace also referenceable:

```
namespace acme

define entity_type Person { ... }
define relation_type reports_to {
    from: Person     # resolves to acme:Person
    to: Person
}
```

A reference to another *user* namespace (`from: crm:Person`) is **not
supported**: the validator rejects it → `UnresolvedTypeRef`. References to
the shared `brain:` system types are permitted (every namespace may use
the system vocabulary). Each schema is otherwise self-contained.

## 3. Wire-level qnames + scope derivation

Wire requests + responses use canonical qnames `namespace:name` wherever
a **type** is referenced:

- `STATEMENT_CREATE.predicate: "acme:role"`.
- `RELATION_CREATE.relation_type: "acme:reports_to"`.
- `ENTITY_CREATE.entity_type: "acme:Person"` (or `"brain:Person"`).

The **owner** namespace of the resulting rows is NOT taken from these
qnames — it is the caller's authenticated namespace, derived once at AUTH
from the API key's `(namespace, agent, permissions)` scope
([`../04_wire_protocol/04_handshake.md`](../04_wire_protocol/04_handshake.md) §10) and
carried on the connection. Clients never send an owner namespace; a
request carrying one is rejected.

## 4. Storage layout

Per-namespace schema versioning (unchanged):

```
schema_versions        key: (namespace: &str, version: u32)  value: SchemaVersionRow
schema_active_versions key: namespace: &str                  value: u32
```

Namespace interning (the data-boundary registry):

```
namespaces          key: namespace_id: u32   value: NamespaceDefinition { name, created_at }
namespace_by_name   key: name: &str          value: namespace_id: u32
```

The reserved `brain` namespace interns to `namespace_id = 0`
(`NamespaceId::SYSTEM`); user namespaces are allocated from `1`.

**Records are namespace-partitioned.** `MemoryMetadata`,
`EntityMetadata`, `StatementMetadata`, and `RelationMetadata` each carry
an owner `namespace_id` (and the owning `agent`), immutable for the
record's life. Every secondary index folds `(namespace_id[, agent])`
into its key prefix so a range scan for one tenant physically cannot
traverse another's rows (defense in depth, not a post-fetch filter). See
[`../10_metadata/02_table_layout.md`](../10_metadata/02_table_layout.md).

`schema_upload` writes the schema row and bumps the active version
atomically inside one redb txn, only for the caller's own namespace.

## 5. Reads are namespace-scoped

Every read path filters to the caller's `(namespace, agent)` scope:

- `RECALL` returns only rows owned by the caller's namespace;
  `agent_filter` / `include_other_agents` widen only *within* it.
- `STATEMENT_LIST` / `RELATION_LIST` by subject/entity return only rows
  in the caller's scope — the by-subject/by-entity indexes are
  namespace-prefixed.
- `ENTITY_RESOLVE` and entity resolution generally are scoped to
  `(namespace, agent)`: `acme/chatbot`'s "John" never resolves to
  `acme/research`'s or `globex`'s "John".
- `GET_CAPABILITIES.schema_namespaces` returns only the caller's
  namespace (plus the always-present `brain`), not a shard-wide list.

## 6. Reserved namespace `brain`

The `brain` namespace is reserved for the system schema (§06) and is
**read-only to users**: built-in types (`brain:related_to`,
`brain:Person`, `brain:is_a`, …) are seeded at `MetadataDb::open` and may
be *referenced* by any namespace, but user `SCHEMA_UPLOAD` /
`SCHEMA_REPLACE` declaring `namespace brain` are rejected, and no user
data is ever *owned* by `brain` (`NamespaceId::SYSTEM` is reserved for
the system's own registry rows only).

## 7. Listing

`SCHEMA_LIST` (`0x0122`) returns the version history of the caller's
namespace. There is no cross-namespace listing on the user surface.

## 8. Open questions

See [`.../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md).
`OQ-V2-4` (per-tenant schemas + isolated entity spaces) is **resolved by
this contract** (tenant = namespace). Remaining:

- Q7 — Cross-namespace traversal filter syntax (still N/A; cross-namespace
  reads are forbidden, not merely discouraged).
- Q8 — Namespace renaming.

## 9. Tests

This section verifies:

- Two namespaces uploaded; versions independent.
- Active version per namespace persists across reopen.
- `brain:` upload rejected; no user data owned by `brain`.
- Qualified reference to another *user* namespace in schema source →
  `UnresolvedTypeRef`; `brain:` references accepted.
- **Cross-namespace isolation:** two namespaces on one shard; no
  RECALL / STATEMENT_LIST / RELATION_LIST / ENTITY_RESOLVE /
  capabilities result crosses the namespace boundary; the same entity
  name resolves to distinct entities per `(namespace, agent)`.
- A write whose namespace can't be resolved is rejected
  (`NamespaceRequired` / `NamespaceUnknown`), never bucketed.
