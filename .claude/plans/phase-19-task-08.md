# 19.8 — SDK schema builders

Adds the `client.schema()` entry point with builders that round-trip
the §28/05 schema wire ops landed in 19.6.

## Files written

| Path | Purpose |
|---|---|
| `crates/brain-sdk-rust/src/knowledge/schema.rs` | `SchemaClient` + `SchemaBuilder` + `client.schema()` entry. |
| `crates/brain-sdk-rust/src/knowledge/mod.rs` | Module wiring + re-exports. |
| `crates/brain-sdk-rust/Cargo.toml` | brain-protocol::schema re-used (no new deps; serde_json already pulled in by the AST). |

## Public surface

```rust
impl Client {
    pub fn schema(&self) -> SchemaClient<'_>;
}

pub struct SchemaClient<'c> { client: &'c Client }

impl<'c> SchemaClient<'c> {
    /// Upload a programmatically-built schema. The `Schema` AST is
    /// serialised back to DSL text via a canonical printer; the
    /// server parses + validates + persists.
    pub async fn upload(&self, schema: &Schema) -> Result<SchemaUploadOutcome, ClientError>;

    /// Upload raw DSL text. Matches the §29/00 example
    /// `client.schema().upload_text(&schema_text).await?`.
    pub async fn upload_text(&self, source: impl Into<String>) -> Result<SchemaUploadOutcome, ClientError>;

    /// Dry-run validate without persisting.
    pub async fn validate(&self, source: impl Into<String>) -> Result<SchemaValidateOutcome, ClientError>;

    /// Fetch a specific version (or active if `version == 0`).
    pub async fn get(&self, namespace: impl Into<String>, version: u32) -> Result<SchemaView, ClientError>;

    /// List all versions for a namespace, newest first.
    pub async fn list(&self, namespace: impl Into<String>) -> Result<SchemaListView, ClientError>;
}

pub fn builder(namespace: impl Into<String>) -> SchemaBuilder;
```

`SchemaBuilder` is a typed fluent assembler over `Schema` (the
19.2 AST) — it produces a `Schema` value that `upload()` then sends
as DSL text. Derive macros (19.9 or post-v1) layer on top of this
to contribute typed entity / relation declarations automatically.

```rust
pub struct SchemaBuilder { schema: Schema }

impl SchemaBuilder {
    pub fn new(namespace: impl Into<String>) -> Self;
    pub fn entity_type(self, def: EntityTypeDef) -> Self;
    pub fn predicate(self, def: PredicateDef) -> Self;
    pub fn relation_type(self, def: RelationTypeDef) -> Self;
    pub fn extractor(self, def: ExtractorDef) -> Self;
    pub fn item(self, item: SchemaItem) -> Self;       // escape hatch
    pub fn build(self) -> Schema;
}
```

19.9 (derive macros) will add `entity_type::<Person>()` etc. that
plug into this same SchemaBuilder. 19.8 ships the explicit form
only.

## Outcome types

```rust
pub struct SchemaUploadOutcome {
    pub namespace: String,
    /// `Some(v)` on success; `None` if the server rejected with a
    /// validation error list (also returned in `errors`).
    pub schema_version: Option<u32>,
    pub errors: Vec<SchemaValidationIssue>,
}

pub struct SchemaValidateOutcome {
    pub namespace: String,
    pub would_be_version: u32,
    pub errors: Vec<SchemaValidationIssue>,
}

pub struct SchemaValidationIssue {
    pub code: String,
    pub message: String,
    pub line: u32,
    pub column: u32,
    pub length: u32,
}

pub struct SchemaView {
    pub namespace: String,
    pub schema_version: u32,
    pub schema_document: String,
    pub source_blob: Vec<u8>,         // serde_json of AST
    pub uploaded_at_unix_nanos: u64,
    pub validator_version: u32,
}

pub struct SchemaListView {
    pub namespace: String,
    pub items: Vec<SchemaListEntry>,
    pub total: u32,
}

pub struct SchemaListEntry {
    pub schema_version: u32,
    pub uploaded_at_unix_nanos: u64,
    pub validator_version: u32,
    pub has_source_text: bool,
}
```

Notes:
- `SchemaUploadOutcome.schema_version = None` when `errors` is
  non-empty, matching the wire's `schema_version == 0` convention.
- `SchemaValidationIssue` is a thin wrapper over
  `SchemaValidationErrorWire` (drops the `severity` field — always
  `2` in v1; v2 surfaces it).

## DSL text generation

`upload(&Schema)` serialises the AST to DSL text via a small
canonical printer in `schema.rs`. For 19.8 the printer is a
straightforward emitter (no fancy formatting):

```text
namespace {namespace}

define entity_type {name} { attributes { ... } }
define predicate {name} { kind: ... object: ... description: "..." }
define relation_type {name} { from: ... to: ... cardinality: ... }
define extractor {name} { kind: ... target: ... ... }
```

Round-trip property: `parse_schema(print(s))` produces an
AST that re-validates to the same `ValidatedSchema` definitions
(modulo `Schema.source`). This is asserted in tests.

If the printer can't emit a particular AST node (e.g., new field
the printer doesn't know), it falls through to a friendly
`ClientError::Internal("SchemaBuilder upload: unsupported AST
node — please use upload_text")`. v1 covers everything
`SchemaBuilder` can construct.

## Tests

Unit tests in `schema.rs` (no real server) cover:

1. `SchemaBuilder::new + .entity_type + .predicate + .build` →
   produces a `Schema` whose `parse_schema(print(s))` re-validates.
2. Printer + parser round-trip for each item kind.
3. `SchemaUploadOutcome` mapping: empty `validation_errors` →
   `schema_version = Some(_)`; non-empty → `None`.
4. `SchemaValidationIssue` decoding from
   `SchemaValidationErrorWire`.

Integration tests against a live server land in 19.10a.

## Out of scope

- `#[derive(BrainEntity)]` / `BrainFact` / `BrainRelation` proc
  macros — phase 19.9 (may slip to 19b).
- `subscribe().events([SchemaUpdated])` — already works via the
  substrate subscribe API; no new code needed.
- Streaming `SCHEMA_LIST` — wire is single-frame in v1.

## Single commit

`feat(sdk): 19.8 — schema builders + client.schema() entry`

## Verification

```
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo test -p brain-sdk-rust --lib knowledge::schema
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```
