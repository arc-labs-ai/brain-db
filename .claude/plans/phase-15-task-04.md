# Sub-task 15.4 — LLM cache redb file

> Per-sub-task plan. Plan-first convention.

## Goal

Create a second per-shard redb file (`llm_cache.redb`) alongside `metadata.redb`. After this sub-task, opening a shard initializes both DB files; the cache file is **persistent** but starts empty. Phase 21 (LLM extractor) populates it; sub-task 24.x adds a sweeper.

Two tables inside `llm_cache.redb`:

| Table | Key | Value | Purpose |
|---|---|---|---|
| `llm_responses` | `(input_hash, extractor_id, extractor_version, model_id)` | `LlmResponse` (rkyv) | Idempotency cache: identical inputs → same response |
| `llm_response_ttl` | `(expiry_unix_secs, input_hash)` | `()` | Sweeper-traversal index |

The cache is **separate from `metadata.redb`** per spec §26 — the LLM blob payload can grow to GBs and shouldn't bloat the hot metadata file.

15.4 is scoped to: file creation, tables initialization, basic round-trip. Population logic, cost budgeting, TTL eviction, LRU-when-over-capacity all land in phase 21 / phase 24.

## Reading list

1. `spec/26_knowledge_storage/00_purpose.md` — "LLM extractor cache" section (key shape, TTL semantics, sizing).
2. `crates/brain-metadata/src/db.rs` — `MetadataDb` is the redb-wrapper precedent we mirror.
3. `crates/brain-storage/src/layout.rs` — `ShardPaths::llm_cache_db()` getter added in 15.3.
4. `crates/brain-server/src/shard/mod.rs` — `spawn_shard` is where we wire the open call.

## Pre-flight findings

### F-1 — `MetadataDb::open` is the right precedent

13-line constructor: opens (or creates) the redb file, runs `open_or_init_schema` to verify/bootstrap the schema-version table, seeds an in-memory `durable_lsn` from checkpoints. We mirror minus the schema-version + checkpoint pieces (the cache has no checkpointing — it's wipeable).

### F-2 — Cache-key composition

The spec lists `(input_hash, extractor_id, extractor_version, model_id)`. Concrete encodings:

- `input_hash`: `[u8; 32]` — blake3-256 of the input text + relevant context.
- `extractor_id`: `u32` — the interned ExtractorId from 15.1.
- `extractor_version`: `u32` — bumped whenever the extractor's prompt / model changes (AUTONOMY §23 binding).
- `model_id`: `u64` — a hash of the model identifier string (e.g. blake3 of `"anthropic/claude-haiku-4-5"`). u64 gives ~uniform distribution across models without storing strings as part of every cache key.

Redb supports tuples of primitives directly, so the key type is `([u8; 32], u32, u32, u64)` — no custom `Key` impl needed (we verified this pattern works in 15.1's `edge.rs` + new tables).

### F-3 — Value shape

```rust
pub struct LlmResponse {
    pub response_blob: Vec<u8>,        // opaque rkyv-encoded body — phase 21 defines the typed shape
    pub created_at_unix_nanos: u64,
    pub expires_at_unix_nanos: u64,    // mirrored in the TTL index
    pub token_count: u32,              // for cost budgeting in phase 21
    pub model_id: u64,                 // denormalized for fast scans (key carries this too)
}
```

`response_blob` is `Vec<u8>` placeholder. The typed shape (parsed LLM JSON, schema-validated payload) lands in phase 21 alongside the cache writer.

### F-4 — TTL index value is `()`

The TTL table is purely an order-by-expiry sorted set for the sweeper. Iterating with `range(..=now)` yields keys ready to evict. Value is `()` (same pattern as `entity_aliases` in 15.1).

### F-5 — Where to hold the handle

`spawn_shard` opens `LlmCacheDb`; for 15.4 we drop the handle at the end of the function (no caller yet). This proves the file lifecycle: open creates the file, tables initialize, file persists. Phase 21 picks up the handle by adding a field to the shard state struct and re-opening (or threading it through the closure).

Reason for "drop" vs "store as Option<LlmCacheDb> field": adding a never-used field invites the wrong call site in some future phase. Re-opening costs ~1 ms on the second spawn, which is amortized across the shard's lifetime. The cleaner story is: 15.4 verifies the file works; phase 21 wires it into hot paths.

(If you'd rather pre-wire the handle, say so in the open questions and I'll add an `Option<LlmCacheDb>` field on the shard state.)

### F-6 — Cache file size cap

Spec says "Cache size cap: configurable, default 10 GB per shard." That's a *sweeper concern*, not an open concern. 15.4 creates the file regardless of cap. Phase 21 / 24 add the LRU eviction.

## Design decisions

### D1 — New module `brain-metadata/src/llm_cache.rs`

```rust
pub struct LlmCacheDb {
    db: redb::Database,
    path: PathBuf,
}

impl LlmCacheDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LlmCacheError>;
    pub fn read_txn(&self) -> Result<redb::ReadTransaction, redb::TransactionError>;
    pub fn write_txn(&mut self) -> Result<redb::WriteTransaction, redb::TransactionError>;
    pub fn path(&self) -> &Path;
    pub fn db(&self) -> &redb::Database;
}
```

Same `&mut self` single-writer discipline as MetadataDb.

### D2 — TableDefinition constants

```rust
pub type LlmCacheKey  = ([u8; 32], u32, u32, u64);
pub type LlmTtlKey    = (u64, [u8; 32]);

pub const LLM_RESPONSES_TABLE: TableDefinition<'static, LlmCacheKey, LlmResponse> =
    TableDefinition::new("llm_responses");

pub const LLM_RESPONSE_TTL_TABLE: TableDefinition<'static, LlmTtlKey, ()> =
    TableDefinition::new("llm_response_ttl");
```

### D3 — `LlmResponse` rkyv value

Mirrors the substrate value-struct pattern (rkyv `Archive`/`Serialize`/`Deserialize` with `check_bytes`, manual `redb::Value` via the `impl_redb_rkyv_value!` macro from 15.1).

Fields per F-3.

### D4 — `LlmCacheError` enum

New error type in the new module:

```rust
#[derive(thiserror::Error, Debug)]
pub enum LlmCacheError {
    #[error("opening LLM cache redb at {path}: {source}")]
    Open { path: PathBuf, source: redb::DatabaseError },
    #[error("initializing LLM cache tables: {0}")]
    Init(#[from] redb::TableError),
    #[error("transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("commit error: {0}")]
    Commit(#[from] redb::CommitError),
}
```

Smaller surface than `MetadataDbError`; the cache has no schema-version table or checkpoint integration.

### D5 — `open` semantics

```rust
pub fn open(path: impl AsRef<Path>) -> Result<Self, LlmCacheError> {
    let path = path.as_ref().to_path_buf();
    let db = Database::create(&path).map_err(|source| LlmCacheError::Open { path: path.clone(), source })?;
    // Table-creation transaction: ensures both tables exist with the
    // declared key/value sigs. Idempotent — redb skips if present.
    let wtxn = db.begin_write()?;
    { let _ = wtxn.open_table(LLM_RESPONSES_TABLE)?; }
    { let _ = wtxn.open_table(LLM_RESPONSE_TTL_TABLE)?; }
    wtxn.commit()?;
    Ok(Self { db, path })
}
```

Idempotent: opening a pre-existing cache file with both tables present completes in microseconds.

### D6 — spawn_shard wiring

After `MetadataDb::open(paths.metadata_db())`:

```rust
let _llm_cache = LlmCacheDb::open(paths.llm_cache_db())
    .map_err(ShardError::LlmCache)?;
// Handle dropped at function exit; phase 21 will store it on the
// shard state and re-open / thread it through the Glommio closure.
```

Add `ShardError::LlmCache(#[from] LlmCacheError)` to brain-server's error enum.

### D7 — Tests

In `llm_cache.rs`:

- `open_creates_file_and_tables` — fresh tempdir → call `open` → assert file exists and both tables are present (probe `read_txn().open_table(...)`).
- `open_is_idempotent` — call `open` twice on the same path → both succeed; second call doesn't lose existing rows.
- `llm_response_round_trip` — insert one row into `llm_responses`, read back, assert equal.
- `ttl_index_round_trip` — insert a TTL key, range-scan up to `now`, assert presence.

In `shard/mod.rs`:

- `spawn_shard_creates_llm_cache_file` — spawn a shard; assert `llm_cache.redb` exists on disk after shutdown. (Already-existing `spawn_creates_knowledge_directories` test from 15.3 explicitly asserted `llm_cache.redb` does NOT exist before this sub-task — that assertion needs to be inverted/removed.)

## File plan

- `crates/brain-metadata/src/llm_cache.rs` — **new**. ~150 lines + tests.
- `crates/brain-metadata/src/lib.rs` — `pub mod llm_cache;` + re-export.
- `crates/brain-server/src/shard/mod.rs`:
  - Open `LlmCacheDb` in `spawn_shard` after `MetadataDb::open`.
  - Drop the handle at function exit (with comment for phase 21).
  - Update `spawn_creates_knowledge_directories` test: flip the `llm_cache.redb` assertion from `!exists` → `exists`.
  - Add `spawn_shard_creates_llm_cache_file` test (or fold into the renamed test).
- `crates/brain-server/src/shard/error.rs` (or wherever `ShardError` lives) — new variant `LlmCache(#[from] LlmCacheError)`.

No new dependencies.

## Done-when

- `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests` clean.
- Unit tests in `llm_cache.rs` pass.
- spawn_shard integration test confirms `llm_cache.redb` lands on disk.
- `metadata.redb` size is **unchanged** vs pre-15.4 (we did not add any new tables to the substrate file — that was 15.1).
- One commit: `feat(metadata): 15.4 — llm_cache.redb file + two cache tables`.

## Risk register

| Risk | Mitigation |
|---|---|
| Composite-key tuple `([u8; 32], u32, u32, u64)` doesn't impl `redb::Key` | Verify by writing a tiny round-trip test first; if it fails, drop to `[u8; 48]` packed key. Spec doesn't mandate composite-tuple form. |
| `LlmCacheDb::open` is slow at spawn (file create + two open_table calls) | Measure once; if >100 ms, defer to first-use lazy open. Default expectation: <1 ms (redb file create + two zero-entry table-create txns). |
| Phase 21 wants a different `LlmResponse` shape | The fields here are the spec-mandated ones (response_blob, timestamps, token_count, model_id). Phase 21 can extend (rkyv field additions are backward-compatible per redb type_name versioning). |
| The TTL key with `(u64, [u8; 32])` has a 40-byte key — large for a cache that may grow to millions of entries | The TTL table is only the secondary index; size is bounded by `llm_responses` row count. At 10 M cached responses, TTL index ~400 MB — acceptable per spec §26's 10 GB cap. |
| Dropping the handle at end of spawn re-opens unnecessarily later | One-time cost <1 ms. Phase 21 re-opens once and keeps the handle. Not a real perf concern. |
| The 15.3 test asserts `llm_cache.redb` does NOT exist; 15.4 flips that assertion | Coordinated edit: same commit updates the spawn test in shard/mod.rs. |

## Open questions for your approval

1. **Handle lifecycle (D6 / F-5)** — drop at spawn exit (recommended; cleaner; phase 21 re-opens), or pre-wire into shard state as `Option<LlmCacheDb>` field now? **Recommended: drop.**
2. **Key encoding (F-2 / D2)** — `([u8; 32], u32, u32, u64)` tuple, or packed `[u8; 48]`? **Recommended: tuple.** Compiler-checked field separation > raw bytes. If redb rejects, we'll fall back.
3. **`model_id` width** — `u64` (blake3-low-64 of the model string), or store the model string itself? **Recommended: u64.** Keys are smaller; rebuilding the hash on every cache lookup is microseconds; model-name → id mapping is cheap and stable.
4. **Error type** — new `LlmCacheError`, or extend `MetadataDbError`? **Recommended: new type.** Cleaner separation; `MetadataDbError` already carries substrate-specific variants that don't apply.

## Workflow

On your nod: implement, run `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests`, commit as `feat(metadata): 15.4 — llm_cache.redb file + two cache tables`, then stop and write the 15.5 plan (substrate-only regression test → tag `phase-15-complete`).
