# 20.1 — Extractor trait + registry types

New crate `brain-extractors` with the trait + registry + output
types. No actual extractor implementations (those land in 20.2 /
20.3); just the surface every later sub-task plugs into.

Per memory `feedback_src_folder_layout`: crate-level `src/lib.rs`
is allowed; concerns split into modules under it.

## Files written

| Path | Purpose |
|---|---|
| `crates/brain-extractors/Cargo.toml` | New crate. |
| `crates/brain-extractors/src/lib.rs` | Crate root; module declarations + re-exports. |
| `crates/brain-extractors/src/extractor.rs` | `Extractor` trait + `ExtractionContext` + `ExtractionResult` + `ExtractorError`. |
| `crates/brain-extractors/src/item.rs` | `ExtractedItem` sum type + `EntityMention` / `StatementMention` / `RelationMention`. |
| `crates/brain-extractors/src/registry.rs` | `ExtractorRegistry` with `register` / `lookup` / `iter_enabled` / `set_enabled`. |
| `crates/brain-extractors/src/idempotency.rs` | `IdempotencyKey` + `BLAKE3` text-hash helper. |
| `crates/brain-extractors/src/options.rs` | `ExtractorRunOptions` (`replay`, `include_cached_outputs`). |
| `Cargo.toml` (root) | Add `brain-extractors` to `[workspace.members]`. |

## Dependencies

- `brain-core` (path) — `MemoryId`, `EntityTypeId`, `ExtractorId`, `StatementKind`, etc.
- `brain-protocol` (path) — `ExtractorTarget`, `StatementKindAst`, `ObjectTypeDecl` AST types from §19.2 (extractor configs reference these at runtime).
- `blake3.workspace` — for `IdempotencyKey::input_hash`.
- `thiserror.workspace` — for `ExtractorError`.
- `tracing.workspace` — for instrumentation hooks (downstream extractors will log via this).
- `serde` (`derive`) — `ExtractedItem` round-trips for tests + future audit storage.

## Public surface

```rust
// extractor.rs

pub trait Extractor: Send + Sync {
    fn id(&self) -> ExtractorId;
    fn kind(&self) -> ExtractorKind;
    fn name(&self) -> &str;                       // qname
    fn extractor_version(&self) -> u32;
    fn run(&self, ctx: &ExtractionContext, mem: &Memory) -> ExtractionResult;
}

pub struct ExtractionContext<'a> {
    pub schema_version: u32,
    pub now_unix_nanos: u64,
    pub registry: &'a ExtractorRegistry,
}

pub struct ExtractionResult {
    pub items: Vec<ExtractedItem>,
    pub status: ExtractionStatus,
    pub started_at_unix_nanos: u64,
    pub completed_at_unix_nanos: u64,
    pub status_reason: String,
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum ExtractorError {
    #[error("regex compilation failed at index {index}: {message}")]
    RegexCompile { index: usize, message: String },
    #[error("resource limit exceeded at index {index}: {limit}")]
    ResourceLimit { index: usize, limit: &'static str },
    #[error("empty patterns")]
    EmptyPatterns,
    #[error("classifier model not found: {id:?}")]
    ModelNotFound { id: String },
    #[error("feature extraction failed: {reason}")]
    FeatureExtractionFailed { reason: String },
    #[error("inference failed: {reason}")]
    InferenceFailed { reason: String },
    #[error("output decode failed: {reason}")]
    OutputDecodeFailed { reason: String },
    #[error("trigger eval error: {reason}")]
    TriggerEval { reason: String },
}
```

```rust
// item.rs — outputs an extractor emits before resolver/persist.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExtractedItem {
    EntityMention(EntityMention),
    StatementMention(StatementMention),
    RelationMention(RelationMention),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityMention {
    pub entity_type_qname: String,    // resolver consults registry to convert to EntityTypeId
    pub text: String,
    pub start: usize,                 // byte offset in memory.text
    pub end: usize,
    pub confidence: f32,
    pub extractor_id: u32,
    pub extractor_version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatementMention {
    pub kind: u8,                     // StatementKind discriminant
    pub subject_text: Option<String>,
    pub predicate_qname: String,
    pub object_text: Option<String>,
    pub confidence: f32,
    pub extractor_id: u32,
    pub extractor_version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelationMention {
    pub relation_type_qname: String,
    pub subject_text: String,
    pub object_text: String,
    pub confidence: f32,
    pub extractor_id: u32,
    pub extractor_version: u32,
}
```

```rust
// registry.rs

pub struct ExtractorRegistry {
    by_id: HashMap<ExtractorId, Arc<dyn Extractor>>,
    enabled: HashSet<ExtractorId>,
}

impl ExtractorRegistry {
    pub fn new() -> Self;
    pub fn register(&mut self, ext: Arc<dyn Extractor>);
    pub fn lookup(&self, id: ExtractorId) -> Option<&Arc<dyn Extractor>>;
    pub fn is_enabled(&self, id: ExtractorId) -> bool;
    pub fn set_enabled(&mut self, id: ExtractorId, enabled: bool);
    pub fn iter_enabled(&self) -> impl Iterator<Item = &Arc<dyn Extractor>>;
    pub fn len(&self) -> usize;
}
```

```rust
// idempotency.rs

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IdempotencyKey {
    pub memory_id: MemoryId,
    pub text_hash: [u8; 32],
    pub extractor_id: ExtractorId,
    pub extractor_version: u32,
    pub schema_version: u32,
}

pub fn hash_memory_text(text: &str) -> [u8; 32] {
    blake3::hash(text.as_bytes()).into()
}
```

```rust
// options.rs

#[derive(Debug, Clone, Copy, Default)]
pub struct ExtractorRunOptions {
    pub replay: bool,
    pub include_cached_outputs: bool,
}
```

```rust
// lib.rs — re-exports.

pub use extractor::{ExtractionContext, ExtractionResult, ExtractionStatus, Extractor, ExtractorError};
pub use idempotency::{hash_memory_text, IdempotencyKey};
pub use item::{EntityMention, ExtractedItem, RelationMention, StatementMention};
pub use options::ExtractorRunOptions;
pub use registry::ExtractorRegistry;
```

`ExtractionStatus` lives in `extractor.rs` (since it's tightly
coupled with `ExtractionResult`) and mirrors the spec §22/05 enum
discriminants exactly.

## Object-safety

The trait is `Send + Sync + 'static`, takes `&self` only, returns
owned `ExtractionResult` — i.e. fully `dyn`-friendly. The registry
stores `Arc<dyn Extractor>`.

## Tests

Unit tests in each module (~12 total):

- `extractor::tests::extraction_result_default`.
- `extractor::tests::status_enum_discriminants_match_spec` —
  assert byte values vs §22/05 §3.
- `item::tests::entity_mention_round_trips_serde_json`.
- `item::tests::statement_mention_includes_optional_subject_object`.
- `item::tests::relation_mention_requires_subject_and_object`.
- `registry::tests::register_then_lookup`.
- `registry::tests::enabled_defaults_true_on_register`.
- `registry::tests::set_enabled_false_excludes_from_iter`.
- `registry::tests::lookup_unknown_returns_none`.
- `idempotency::tests::hash_is_deterministic`.
- `idempotency::tests::key_round_trips_eq_hash`.
- `options::tests::default_is_no_replay_no_cached`.

## Out of scope

- Pattern / classifier `impl Extractor` — 20.2 / 20.3.
- Audit row writes — 20.4.
- Schema fan-out into `EXTRACTORS_TABLE` — 20.5.
- ENCODE integration — 20.6.

## Single commit

`feat(extractors): 20.1 — extractor trait + registry types`

## Verification

```
cargo zigbuild --target x86_64-unknown-linux-gnu -p brain-extractors --tests
cargo test -p brain-extractors
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```
