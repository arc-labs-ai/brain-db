# 19.5 — Schema persistence + schema_store

Persists `ValidatedSchema` documents per-namespace and exposes the
active version to downstream validation paths. Backbone of
`SCHEMA_UPLOAD` / `SCHEMA_GET` / `SCHEMA_LIST` / `SCHEMA_VALIDATE`
opcodes (wired in 19.6).

## Crate / dep changes

- `crates/brain-metadata/Cargo.toml`: add `brain-protocol`,
  `serde_json` deps. (brain-protocol owns `ValidatedSchema` + the
  AST.)

## Files written / modified

| Path | Purpose |
|---|---|
| `crates/brain-metadata/src/tables/knowledge/schema_version.rs` | Widen the placeholder row type per §21/05 §2: per-namespace key, drop `migration_plan_blob`, add `source` / `source_text` / `validator_version`. Add `SCHEMA_ACTIVE_VERSIONS_TABLE`. |
| `crates/brain-metadata/src/schema_store.rs` | `schema_upload` / `_get` / `_active` / `_list` / `_namespaces` / `_validate`. |
| `crates/brain-metadata/src/lib.rs` | Module + re-exports. |
| `crates/brain-metadata/src/tables/knowledge/mod.rs` | Register `SCHEMA_ACTIVE_VERSIONS_TABLE` for `fresh_db` ensure (only if existing pattern wants it). |
| `crates/brain-server/tests/knowledge_compat.rs` | Update assertion of the renamed value type if needed. |

## Storage schema (§21/05 §2)

```rust
// schema_version.rs
pub const SCHEMA_VERSIONS_TABLE:
    TableDefinition<(&str, u32), SchemaVersionRow> =
    TableDefinition::new("schema_versions");

pub const SCHEMA_ACTIVE_VERSIONS_TABLE:
    TableDefinition<&str, u32> =
    TableDefinition::new("schema_active_versions");

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct SchemaVersionRow {
    pub namespace: String,
    pub version: u32,
    pub uploaded_at_unix_nanos: u64,
    pub source: Vec<u8>,           // serde_json(Schema) — see note
    pub source_text: Option<String>,
    pub validator_version: u32,
}
```

**On the AST encoding choice.** Spec §21/05 §2 says "rkyv-archived
AST blob"; spec §21/02 §6 says the AST is value-typed (serde only,
no rkyv on the protocol side). Resolving in favour of §21/02 — the
canonical AST encoding is `serde_json::to_vec(&Schema)`. Pros:
zero AST duplication, schema round-trip via the same path the SDK
uses for JSON uploads. Cons: bytes are JSON not rkyv. Document in
the module header; revisit if hot-path reads ever bottleneck on
deserialise cost (schema fetches aren't hot).

`VALIDATOR_VERSION` constant: `u32 = 1` for v1. Bump when the
validator rules change shape (open question Q10).

## schema_store API

```rust
pub fn schema_upload(
    wtxn: &WriteTransaction,
    validated: &ValidatedSchema,
    now_unix_nanos: u64,
) -> Result<u32, SchemaStoreError>;

pub fn schema_get(
    rtxn: &ReadTransaction,
    namespace: &str,
    version: u32,
) -> Result<Option<SchemaVersionRow>, SchemaStoreError>;

pub fn schema_active(
    rtxn: &ReadTransaction,
    namespace: &str,
) -> Result<Option<u32>, SchemaStoreError>;

pub fn schema_active_row(
    rtxn: &ReadTransaction,
    namespace: &str,
) -> Result<Option<SchemaVersionRow>, SchemaStoreError>;

pub fn schema_list(
    rtxn: &ReadTransaction,
    namespace: &str,
) -> Result<Vec<SchemaVersionRow>, SchemaStoreError>;

pub fn schema_namespaces(
    rtxn: &ReadTransaction,
) -> Result<Vec<String>, SchemaStoreError>;
```

```rust
#[derive(thiserror::Error, Debug)]
pub enum SchemaStoreError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("redb commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("json encode failed: {0}")]
    Encode(String),
    #[error("schema_version overflow for namespace {namespace:?}")]
    VersionOverflow { namespace: String },
}
```

Notes:
- `schema_upload` is **atomic** — bumps the active version + writes
  the version row in a single `wtxn`. Caller commits.
- `schema_list` returns versions **newest first** (descending
  version order).
- `schema_namespaces` enumerates the `SCHEMA_ACTIVE_VERSIONS_TABLE`
  keys.
- `SCHEMA_VALIDATE` is **not** in schema_store — it's a pure
  parser+validator call (no storage interaction). The wire handler
  (19.6) composes it from `brain-protocol::schema::{parse_schema,
  validate}` + `schema_active(rtxn, ns).map(|v| v + 1)` for the
  would-be-next-version hint.

## Downstream coupling (defer to 19.7)

The §21/05 §1 lifecycle also writes entity_type / predicate /
relation_type rows for new + changed definitions. 19.5 ships only
the version+row layer; the definition-write fan-out happens in
**19.7 system-schema bootstrap** (where it's load-bearing) and
later in the wire handler (19.6).

Rationale: keeping 19.5 narrow makes it testable in isolation
(persistence only). The fan-out into existing intern paths
(`predicate_intern`, `relation_type_intern`,
`entity_type_intern`) is mechanical once the API is in place — it
just iterates `validated.as_schema().items` and calls the existing
helpers. 19.7's system-schema test exercises the full path.

## Tests (per §21/05 §8)

Unit tests inside `schema_store.rs` (skipped under miri; require
tempdir + redb):

1. First upload to namespace `acme` → version 1; active = 1.
2. Second upload to `acme` → version 2; active = 2; v1 still
   readable via `schema_get`.
3. `schema_get(ns, missing_version)` → `Ok(None)`.
4. `schema_list("acme")` returns `[v2, v1]` (newest first).
5. Active persists across DB close + reopen.
6. Independent namespaces — uploads to `acme` don't affect `crm`.
7. `schema_active("never_uploaded")` → `Ok(None)`.
8. `schema_namespaces` returns all active namespaces.
9. `SchemaVersionRow` round-trip via redb (validates
   `impl_redb_rkyv_value`).
10. `schema_get` returns a row whose `source_text` matches the
    original DSL input.

Integration tests outside schema_store left to 19.6 (wire path)
and 19.7 (system schema).

## Out of scope

- Validator-version migration (§21/07 Q10).
- Schema deletion / rollback (§21/07 Q9).
- "No-op upload" suppression (§21/05 §6).
- Definition fan-out into entity_type / predicate / relation_type
  intern paths — 19.7.
- Wire framing — 19.6.

## Single commit

`feat(metadata): 19.5 — schema_store + per-namespace version layout`

## Verification

```
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo test -p brain-metadata
cargo test -p brain-server knowledge_compat
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```
