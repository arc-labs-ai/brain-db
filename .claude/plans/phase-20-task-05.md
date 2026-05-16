# 20.5 — Extractor registry persistence (schema fan-out)

Fleshes out 19.7's deferred `SchemaItem::Extractor` arm in
`apply_schema_definitions`. Widens `ExtractorDefinition` to match
the predicate / relation-type pattern (namespace + name + qname
index) and lands `extractor_ops` with the intern + lookup +
enable/disable API the wire opcodes 0x0124-0x0126 will consume.

## Files written / modified

| Path | Change |
|---|---|
| `crates/brain-metadata/src/tables/knowledge/extractor.rs` | Widen `ExtractorDefinition` row (add `namespace`, `schema_version`); bump type tag to `::v2`; add `EXTRACTORS_BY_QNAME_TABLE` secondary index. |
| `crates/brain-metadata/src/extractor_ops.rs` | New: `extractor_intern`, `extractor_get`, `extractor_lookup_by_qname`, `extractor_list`, `extractor_set_enabled`, `ExtractorOpError`. |
| `crates/brain-metadata/src/schema_apply.rs` | Add `SchemaItem::Extractor` arm calling `extractor_intern` with JSON-encoded AST. |
| `crates/brain-metadata/src/lib.rs` | Module + re-exports. |
| `crates/brain-metadata/Cargo.toml` | (no change — serde_json already a dep from 19.5). |

## Storage shape

```rust
pub const EXTRACTORS_TABLE:
    TableDefinition<'static, u32, ExtractorDefinition> =
    TableDefinition::new("extractors");

pub const EXTRACTORS_BY_QNAME_TABLE:
    TableDefinition<'static, &str, u32> =
    TableDefinition::new("extractors_by_qname");

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct ExtractorDefinition {
    pub extractor_id: u32,
    pub namespace: String,            // NEW
    pub name: String,                  // bare name (no qname)
    pub kind: u8,                      // ExtractorKind discriminant
    pub enabled: u8,
    pub schema_version: u32,           // RENAMED from `version`
    /// `serde_json::to_vec(&ExtractorDef)` — the AST blob.
    pub definition_blob: Vec<u8>,
    pub created_at_unix_nanos: u64,
}
```

Type tag bumped `::v1` → `::v2`. Pre-19 hand-seeded constants did
not populate this table; v1 hasn't shipped; clean cutover.

## extractor_ops API

```rust
pub fn extractor_intern(
    wtxn: &WriteTransaction,
    namespace: &str,
    name: &str,
    kind: ExtractorKind,
    schema_version: u32,
    definition_blob: Vec<u8>,
    now_unix_nanos: u64,
) -> Result<ExtractorId, ExtractorOpError>;

pub fn extractor_get(
    rtxn: &ReadTransaction,
    id: ExtractorId,
) -> Result<Option<ExtractorDefinition>, ExtractorOpError>;

pub fn extractor_lookup_by_qname(
    rtxn: &ReadTransaction,
    namespace: &str,
    name: &str,
) -> Result<Option<ExtractorDefinition>, ExtractorOpError>;

pub fn extractor_list(
    rtxn: &ReadTransaction,
) -> Result<Vec<ExtractorDefinition>, ExtractorOpError>;

pub fn extractor_set_enabled(
    wtxn: &WriteTransaction,
    id: ExtractorId,
    enabled: bool,
) -> Result<bool, ExtractorOpError>;   // returns previous enabled state
```

```rust
#[derive(thiserror::Error, Debug)]
pub enum ExtractorOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("invalid extractor identifier: {reason}")]
    InvalidIdentifier { reason: &'static str },
    #[error("extractor {qname:?} already exists with id {existing_id:?} but kind / definition differ")]
    AlreadyExists { qname: String, existing_id: ExtractorId },
    #[error("extractor not found: id {id:?}")]
    NotFound { id: ExtractorId },
}
```

### Idempotency semantics (mirrors `predicate_intern`)

- Lookup by qname.
- Existing row with **identical** kind + schema_version +
  definition_blob → return existing id (no-op).
- Existing row with **diverging** kind / blob → `AlreadyExists`.
- No existing row → allocate next id (`max(existing) + 1`), write
  primary + qname index, set `enabled = 1`.

### `extractor_set_enabled`

Wire ops 0x0125 (DISABLE) / 0x0126 (ENABLE) in phase 20.8 call
this. Returns the previous enabled state so the wire handler can
populate `previously_enabled` / `previously_disabled` per §28/05 §7.2.

`NotFound` on unknown id. Idempotent on already-in-state calls.

## schema_apply update

Replace the no-op `SchemaItem::Extractor(_)` arm with:

```rust
SchemaItem::Extractor(e) => {
    let kind = map_extractor_kind(e.kind);
    let definition_blob = serde_json::to_vec(e)
        .map_err(|err| SchemaApplyError::ExtractorEncode(err.to_string()))?;
    extractor_intern(
        wtxn,
        namespace,
        &e.name,
        kind,
        schema_version,
        definition_blob,
        now_unix_nanos,
    )?;
}
```

`SchemaApplyError` gains `Extractor(#[from] ExtractorOpError)` +
`ExtractorEncode(String)` variants.

`map_extractor_kind` translates `ExtractorKindAst` → `ExtractorKind`:
- `Pattern` → `ExtractorKind::Pattern`
- `Classifier` → `ExtractorKind::Classifier`
- `Llm` → `ExtractorKind::Llm`

## Tests

`extractor_ops.rs` tests (Linux-docker, mirrors predicate_ops):

1. `intern_fresh_assigns_id_1` — first intern in empty table.
2. `intern_idempotent_on_identical_definition` — second call returns
   same id, no second row.
3. `intern_rejects_diverging_definition` → `AlreadyExists`.
4. `intern_allocates_max_plus_one` — third intern with a different
   qname gets id 3, etc.
5. `intern_populates_qname_index` — `extractor_lookup_by_qname`
   returns the row after intern.
6. `lookup_unknown_returns_none`.
7. `list_returns_all_rows` — order-agnostic.
8. `set_enabled_toggles_and_returns_previous`.
9. `set_enabled_unknown_id_returns_not_found`.
10. `set_enabled_idempotent_on_same_state` — disabling an already-
    disabled extractor returns `false` (previous state) and writes
    nothing new.

`tables/knowledge/extractor.rs` round-trip test gets updated for
the widened row.

`schema_apply` test:
- `extractor_item_is_persisted` — apply a 1-extractor schema, then
  `extractor_lookup_by_qname` returns the row; `definition_blob`
  round-trips back to `ExtractorDef` via serde_json.

## Out of scope

- Wire opcodes (EXTRACTOR_LIST / _DISABLE / _ENABLE) — phase 20.8.
- Loading registered extractors into the in-memory `ExtractorRegistry`
  at MetadataDb::open — phase 20.6 / 20.7.
- Versioned re-intern across `extractor_version` bumps — only
  `schema_version` is tracked in v1; `extractor_version` semantics
  land when the dedicated bump path arrives (§22/07 follow-up).

## Single commit

`feat(metadata): 20.5 — extractor registry + schema fan-out`

## Verification

```
just docker cargo test -p brain-metadata --lib extractor schema_apply
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```
