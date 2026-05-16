# Plan: Phase 22 — Task 01, tantivy dependency + shard init

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Pin the tantivy crate, scaffold the per-shard index handle, and
wire `Index::open` + schema-version check into the shard spawn
path so subsequent sub-tasks (tokenizer 22.2, workers 22.3/22.4,
retriever 22.5) have somewhere to plug in.

Concrete deliverables:

1. Add `tantivy = "0.26"` to the workspace dep table and to
   `brain-index/Cargo.toml`.
2. New module `brain-index::tantivy_shard` with:
   - `BRAIN_SCHEMA_VERSION: u32 = 1` constant.
   - `pub fn memory_text_schema() -> Schema` and
     `pub fn statements_schema() -> Schema` per §26/01 §2.
   - `pub struct TantivyShard { memory_text: IndexHandle, statements: IndexHandle }`.
   - `TantivyShard::open(shard_dir: &Path) -> TantivyShardStartup`
     where `TantivyShardStartup` reports per-index status
     (`Ready` / `NeedsRebuild { reason }`) without doing the
     rebuild itself.
3. `IndexHandle { index: tantivy::Index, scope: LexicalScope }`
   — owns the open `Index`. Phase 22.3/22.4 build `IndexWriter`
   on top.
4. Wire `TantivyShard::open` into `brain-server::shard::spawn`
   alongside the phase-21 `build_llm_deps` call. Output: shard
   gets a `tantivy: Arc<TantivyShard>` field on `OpsContext` (or
   a placeholder field — final type lives in 22.3+).
5. Unit tests: schema round-trips, open creates dirs, open
   detects schema-version mismatch and returns `NeedsRebuild`.

NOT in scope (deferred to per-sub-task plans):
- Tokenizer registration on the `Index` — that's 22.2.
- `IndexWriter` allocation, commit cadence wiring — 22.3 / 22.4.
- The rebuild worker itself — 22.6.
- `LexicalRetriever` trait + impl — 22.5.
- Hot-reload of `BRAIN_TANTIVY_*` env vars — explicit post-v1 cut.

## 2. Spec references

- **`spec/26_knowledge_storage/01_tantivy_layout.md`** (just
  written in 22.0):
  - §1 directory layout (`shards/000/memory_text.tantivy/` etc.).
  - §2 schemas — pinned for this sub-task to materialise.
  - §6 recovery on startup — phase 22.1 implements the
    open / version-check / fallback-to-rebuild **scheduling**;
    actual rebuild lives in 22.6.
- **`spec/23_retrievers/02_lexical_retriever.md`** §4 — scope
  enum reused (`MemoryText` / `StatementText`).
- **CLAUDE.md §6 "Tech stack"** — new dep must justify itself.
  Tantivy is THE lexical-retrieval library for Rust; explicitly
  named in §23/00 line 17. Justification trivial.

## 3. External validation

| Item | Source | Confirmed |
|---|---|---|
| Latest tantivy version | `cargo info tantivy` | 0.26.1 (MIT, MSRV 1.86, repo `quickwit-oss/tantivy`) |
| MSRV compatibility | workspace MSRV `1.95` ≥ tantivy MSRV `1.86` | ✓ |
| Features needed | `default = [mmap, stopwords, lz4-compression, columnar-zstd-compression, stemmer]` | We want `mmap` + `stemmer`; opting out of `stopwords` and `columnar-zstd-compression` because we don't use them. `lz4-compression` stays — segment compression is desirable. |
| `tantivy::schema::Schema` API | https://docs.rs/tantivy/0.26.1/tantivy/schema/struct.SchemaBuilder.html | `SchemaBuilder::new()` + `add_text_field` / `add_u64_field` / `add_bytes_field` matches §26/01 §2 field types. |
| Schema-version mechanism | tantivy `IndexMeta::payload: Option<String>` | tantivy stores a free-form payload in `meta.json`; we serialise our version into it. Confirmed via docs. |
| `Index::open_in_dir` behavior on corrupt / missing | docs.rs | Returns `Err(OpenDirectoryError)` — we map to `NeedsRebuild` |

Crate version is **pinned at 0.26** (not `^0.26.1`). v1 caps the
floor; minor upgrades are deliberate (binary-format compatibility
matters for the live indexes on disk).

## 4. Architecture sketch

```rust
// crates/brain-index/src/tantivy_shard/mod.rs

use std::path::Path;
use std::sync::Arc;

use tantivy::schema::{Schema, SchemaBuilder, FAST, INDEXED, STORED, STRING, TEXT};
use tantivy::{Index, IndexMeta};

pub const BRAIN_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy)]
pub enum LexicalScope { MemoryText, StatementText }

pub struct TantivyShard {
    pub memory_text: IndexHandle,
    pub statements: IndexHandle,
}

pub struct IndexHandle {
    pub index: Index,
    pub scope: LexicalScope,
}

#[derive(Debug)]
pub struct TantivyShardStartup {
    pub shard: Arc<TantivyShard>,
    pub memory_status: IndexStatus,
    pub statements_status: IndexStatus,
}

#[derive(Debug)]
pub enum IndexStatus {
    Ready,
    NeedsRebuild { reason: RebuildReason },
}

#[derive(Debug)]
pub enum RebuildReason {
    Missing,                       // dir doesn't exist
    OpenFailed(String),            // tantivy::OpenDirectoryError
    SchemaVersionMismatch { found: u32, expected: u32 },
}

impl TantivyShard {
    /// Opens both indexes under `shard_dir`. Creates the
    /// directories with an empty index if absent. Detects
    /// version mismatch by reading the `meta.json` payload
    /// field. Returns startup status — the caller (22.6) acts
    /// on `NeedsRebuild`.
    pub fn open(shard_dir: &Path) -> Result<TantivyShardStartup, TantivyShardError>;
}

pub fn memory_text_schema() -> Schema { /* per §26/01 §2 */ }
pub fn statements_schema() -> Schema { /* per §26/01 §2 */ }
```

Schema field bindings (memory_text):

```rust
let mut sb = SchemaBuilder::new();
sb.add_u64_field("memory_id", STORED);
sb.add_text_field("text", TEXT);   // tokenizer wired in 22.2
sb.add_bytes_field("agent_id", STRING | STORED);  // 16 bytes, exact match
sb.add_u64_field("kind", INDEXED);
sb.add_u64_field("created_at", INDEXED | FAST);  // FAST for range filter
sb.build()
```

Schema field bindings (statements):

```rust
let mut sb = SchemaBuilder::new();
sb.add_bytes_field("statement_id", STORED);  // 16 bytes for u128
sb.add_text_field("subject_name", TEXT);
sb.add_bytes_field("predicate_name", STRING);
sb.add_u64_field("predicate_id", INDEXED);
sb.add_text_field("object_text", TEXT);
sb.add_u64_field("kind", INDEXED);
sb.add_u64_field("confidence_bucket", INDEXED | FAST);
sb.add_u64_field("extracted_at", INDEXED | FAST);
sb.build()
```

Schema-version stamping — write/check via the tantivy
`IndexMeta` payload field on the first commit of a new index.
On open, if `payload` is `Some(JSON { brain_schema_version: N })`
and `N != BRAIN_SCHEMA_VERSION`, return `NeedsRebuild`.

Brand-new (just-created) indexes are stamped lazily on the
first writer commit — phase 22.1 only **reads** the payload; the
worker in 22.3 owns the write.

Phase 22.1 open behaviour on a fresh directory:
- Directory absent → create empty index with the schema, payload
  stays `None` until first commit. Status: `Ready`.

Server-side wiring (just the integration shape; field name
final-decided in 22.3 when we know the `OpsContext` surface):

```rust
// crates/brain-server/src/shard/mod.rs (spawn fn)
let tantivy_startup = brain_index::tantivy_shard::TantivyShard::open(&shard_dir)?;
if matches!(tantivy_startup.memory_status, IndexStatus::NeedsRebuild { .. }) {
    tracing::warn!(?tantivy_startup.memory_status, "memory_text rebuild scheduled");
    // actual rebuild scheduling lands in 22.6; for 22.1 we log.
}
// pass `tantivy_startup.shard` into OpsContext (or a placeholder
// field that 22.3 widens).
```

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| `tantivy = "0.26"` (this plan) | Latest stable, MSRV-compatible, ongoing maintenance | Newer than the phase doc's "0.21+" suggestion | ✓ |
| `tantivy = "0.21"` (phase doc) | Matches phase doc | 5 majors behind; missing perf + API improvements; weaker external support | rejected |
| Pin minor (`= "0.26.1"`) | Reproducible builds | Workspace policy is `"0.26"` (caret); reproducibility comes from `Cargo.lock` | rejected |
| Schema-version in `meta.json` payload (this plan) | Native tantivy mechanism; survives across opens | Payload is `Option<String>`, we own the JSON shape | ✓ |
| Schema-version in sidecar file (e.g. `version.json`) | Independent of tantivy internals | Two sources of truth; sidecar can desync; extra fsync discipline | rejected |
| Schema-version in redb | Centralised | Reaches across crate boundaries from `brain-index` into `brain-metadata` | rejected |
| Return concrete `IndexWriter` from `open` | One-shot setup | Writer-per-index allocation is 22.3's concern; coupling | rejected |
| `TantivyShard` lives in `brain-storage` | Co-located with mmap discipline | brain-index already owns ANN; tantivy is "another index" — same crate | ✓ |

## 6. Risks / open questions

- **Risk:** tantivy's `mmap` feature pulls in `memmap2`. We
  already use `memmap2` in `brain-storage` (substrate arena);
  version conflict possible. **Mitigation:** check
  `cargo tree -e features | grep memmap2` after adding the dep;
  if there's a conflict, we add `memmap2` to the workspace dep
  table at a compatible version.
- **Risk:** tantivy's stop-word default is on. Even though we
  pin the tokenizer in 22.2 (no stop-word removal), tantivy's
  default tokenizer registry includes one. **Mitigation:**
  we register our own tokenizer in 22.2 and never use
  `default`; documented in the §23/02 §3 binding.
- **Risk:** opening a *fresh* `Index` in an empty directory may
  fail on the first call until a commit has happened. **Mitigation:**
  `Index::create_in_dir` for fresh dirs, `Index::open_in_dir`
  for existing; phase 22.1 picks one based on dir existence.
- **Open question:** does `OpsContext` get the `Arc<TantivyShard>`
  directly, or do we keep tantivy entirely behind a higher-level
  abstraction (e.g. `LexicalRouter`)? **Resolution:** the 22.5
  plan picks the abstraction; 22.1 lands the handle and a
  placeholder field so we don't pre-judge.

## 7. Test plan

Unit tests in `crates/brain-index/src/tantivy_shard/tests.rs`:

- `memory_text_schema_has_expected_fields` — assert each
  `(name, type, options)` triple matches §26/01 §2.
- `statements_schema_has_expected_fields` — same for the
  statements schema.
- `open_creates_indexes_on_fresh_dir` — given a tempdir, open
  succeeds, both indexes are `Ready`, dirs exist.
- `open_returns_ready_on_existing_indexes` — open, drop, re-open
  in the same tempdir.
- `open_returns_needs_rebuild_on_version_mismatch` — write a
  payload with `brain_schema_version: 99`, re-open, status is
  `NeedsRebuild { reason: SchemaVersionMismatch { found: 99, expected: 1 } }`.
- `open_returns_needs_rebuild_on_corrupt_meta` — write garbage
  into `meta.json`, re-open, status is `NeedsRebuild { reason: OpenFailed(_) }`.

Server-side smoke test in `brain-server::shard::mod`:

- `spawn_creates_tantivy_dirs` — after spawn returns, both
  `memory_text.tantivy/` and `statements.tantivy/` exist
  under `<shard_dir>/`.

## 8. Commit shape

Single commit:

```
feat(index,server): 22.1 — tantivy 0.26 + per-shard TantivyShard init

Wires tantivy 0.26 into brain-index. Adds the per-shard index
handle the rest of phase 22 plugs into.

- crates/brain-index/Cargo.toml: tantivy = "0.26", features =
  ["mmap", "lz4-compression", "stemmer"], default-features = false.
- crates/brain-index/src/tantivy_shard/mod.rs (new): schemas
  (§26/01 §2), BRAIN_SCHEMA_VERSION = 1, TantivyShard::open
  with IndexStatus { Ready | NeedsRebuild { reason } }.
- crates/brain-server/src/shard/mod.rs: spawn calls
  TantivyShard::open; rebuild scheduling stub (warn-and-continue
  in 22.1; real worker in 22.6); passes Arc<TantivyShard> into
  OpsContext under a placeholder field.
- 7 unit tests (schema round-trip, fresh-dir, re-open, version
  mismatch, corrupt meta, server-spawn smoke).

Phase doc 22.1 done-when: ✓ tantivy dep added; ✓ per-shard
directories created with correct schema; status returned to
caller for 22.6 to act on.
```

## 9. Confirmation

Please confirm:

1. **tantivy 0.26** (latest, not the phase-doc's 0.21+) is OK.
2. **Schema-version stamping in `meta.json` payload** is the
   storage strategy we want (alternative: sidecar file or redb).
3. **22.1 deliberately does NOT touch tokenizer registration or
   IndexWriter allocation** — those are 22.2 and 22.3 respectively.
   That keeps the commit small and the schema-vs-tokenizer
   responsibility split clean.
4. **`OpsContext` gets a placeholder `tantivy` field in 22.1**;
   the final abstraction (raw handle vs. `LexicalRouter`) is
   decided in 22.5.

After approval: I write the module + tests + server wiring, run
`cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests`
+ `just docker cargo test -p brain-index --lib`, and commit.
