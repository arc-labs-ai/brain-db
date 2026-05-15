# Sub-task 15.3 — On-disk artifact directory layout

> Per-sub-task plan. Plan-first convention.

## Goal

Centralize the per-shard file/dir name layout and ensure that on `spawn_shard`, the knowledge-layer directories exist on disk (empty). After this sub-task, opening a brand-new shard OR an existing substrate-only shard both end up with:

```
<data_dir>/<shard_id>/
  shard.uuid              (existing)
  arena.bin               (existing)
  metadata.redb           (existing)
  wal/                    (existing — directory)
  statements.tantivy/     NEW empty dir
  memory_text.tantivy/    NEW empty dir
  entity.hnsw             NOT created here — empty marker only (decided below)
  statement.hnsw          NOT created here — empty marker only
  llm_cache.redb          handled in 15.4 (separate sub-task)
```

The HNSW and tantivy backends create their own files on first index write (phases 16, 17, 22); 15.3 is purely about parent-directory presence so those phases never have to worry about `mkdir`.

15.4 owns the `llm_cache.redb` file creation; 15.3 only creates its parent (which is the shard dir, already extant).

## Reading list

1. `spec/26_knowledge_storage/00_purpose.md` — "Per-shard layout" section.
2. `crates/brain-server/src/shard/mod.rs` (around lines 700–780) — `spawn_shard` is where the substrate paths are created today; this is where the new ones will be ensured.
3. `crates/brain-storage/src/wal/wal.rs` — WAL dir creation precedent (`create_dir_all` from spawn_shard).
4. `crates/brain-storage/src/lib.rs` — top-level exports (we'll add `pub mod layout`).

## Pre-flight findings

### F-1 — No centralized layout module today

Substrate path names are inlined at the call site:

```rust
// spawn_shard, brain-server/src/shard/mod.rs
let arena_path     = dir.join("arena.bin");
let metadata_path  = dir.join("metadata.redb");
let wal_dir        = dir.join("wal");
let uuid_path      = dir.join("shard.uuid");
```

Tests also use these names directly (`crates/brain-storage/src/wal/checkpoint.rs`, `tests/random_kill.rs`, planner tests). Renaming would touch ~20 sites.

**Decision (D2 below):** add `crates/brain-storage/src/layout.rs` with **constants** for all per-shard file/dir names. Migrate `spawn_shard`'s use to the constants (cheap — same crate dep already). Do NOT mass-migrate test sites in 15.3; they hardcode names in their own scopes and migrating each is gratuitous churn. Leave a TODO comment in `layout.rs` so a future cleanup phase can pick it up.

### F-2 — `wal/` directory is created at `spawn_shard:733`

That gives a precedent for the right place to put the new mkdir calls: just after the existing `wal_dir` mkdir, before recovery. Doing it before recovery means recovery code sees a fully-formed layout even if a future phase needs that.

### F-3 — Knowledge HNSW files: no parent dir, just the file path

`entity.hnsw` and `statement.hnsw` are *files*, not dirs. They live directly under `<shard_dir>/`. We don't pre-create them in 15.3 — the HNSW backend opens them when needed (phases 16 and 17). 15.3 only exposes their canonical paths via `layout` constants.

### F-4 — Tantivy directories: must exist before tantivy opens them

`tantivy::Index::create_in_dir(path)` requires `path` to be a pre-existing directory. So `statements.tantivy/` and `memory_text.tantivy/` must be `mkdir -p`'d before tantivy is asked to open them in phase 22. Creating them in 15.3 is the right time — substrate code doesn't touch them; phase-22 code finds them ready.

### F-5 — Idempotency requirement

The phase doc says "existing substrate shards must still open." That means an already-running shard upgraded to this code must:
- Pre-existing files (`arena.bin`, `metadata.redb`, `wal/`, `shard.uuid`) untouched.
- New dirs created if missing (`create_dir_all` is idempotent — returns Ok if dir already exists).
- No write to `arena.wal` / metadata files during 15.3's path-creation step.

Tested: existing checkpoint/recovery integration tests use real shard dirs; they'd catch regressions.

## Design decisions

### D1 — Add `crates/brain-storage/src/layout.rs`

New module with:

```rust
//! Per-shard on-disk file/dir layout.

use std::path::{Path, PathBuf};

// Substrate layout (file/dir names live under <shard_dir>/).
pub const SHARD_UUID_FILE: &str  = "shard.uuid";
pub const ARENA_FILE: &str       = "arena.bin";
pub const METADATA_DB_FILE: &str = "metadata.redb";
pub const WAL_DIR: &str          = "wal";

// Knowledge-layer additions (spec §26).
pub const ENTITY_HNSW_FILE: &str           = "entity.hnsw";
pub const STATEMENT_HNSW_FILE: &str        = "statement.hnsw";
pub const STATEMENTS_TANTIVY_DIR: &str     = "statements.tantivy";
pub const MEMORY_TEXT_TANTIVY_DIR: &str    = "memory_text.tantivy";
pub const LLM_CACHE_DB_FILE: &str          = "llm_cache.redb"; // sub-task 15.4

/// Typed view of a shard's paths.
pub struct ShardPaths {
    pub root: PathBuf,
}

impl ShardPaths {
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self { Self { root: root.into() } }

    pub fn shard_uuid(&self)         -> PathBuf { self.root.join(SHARD_UUID_FILE) }
    pub fn arena(&self)              -> PathBuf { self.root.join(ARENA_FILE) }
    pub fn metadata_db(&self)        -> PathBuf { self.root.join(METADATA_DB_FILE) }
    pub fn wal_dir(&self)            -> PathBuf { self.root.join(WAL_DIR) }
    pub fn entity_hnsw(&self)        -> PathBuf { self.root.join(ENTITY_HNSW_FILE) }
    pub fn statement_hnsw(&self)     -> PathBuf { self.root.join(STATEMENT_HNSW_FILE) }
    pub fn statements_tantivy(&self) -> PathBuf { self.root.join(STATEMENTS_TANTIVY_DIR) }
    pub fn memory_text_tantivy(&self)-> PathBuf { self.root.join(MEMORY_TEXT_TANTIVY_DIR) }
    pub fn llm_cache_db(&self)       -> PathBuf { self.root.join(LLM_CACHE_DB_FILE) }
}

/// Idempotent mkdir for every directory the layout requires. Files
/// (arena.bin, metadata.redb, hnsw, llm_cache.redb) are NOT created
/// here — their owning module opens or creates them on demand.
pub fn ensure_dirs(root: &Path) -> std::io::Result<()> {
    let p = ShardPaths::at(root);
    std::fs::create_dir_all(&p.root)?;
    std::fs::create_dir_all(p.wal_dir())?;
    std::fs::create_dir_all(p.statements_tantivy())?;
    std::fs::create_dir_all(p.memory_text_tantivy())?;
    Ok(())
}
```

### D2 — Migrate `spawn_shard` opportunistically

In `crates/brain-server/src/shard/mod.rs`, replace the inline `dir.join("arena.bin")` etc. with `paths.arena()` style. Call `layout::ensure_dirs(&dir)` early to pre-create the knowledge directories (substrate dirs come along for free since `ensure_dirs` also creates `wal/`).

Don't migrate test files that use literal `"arena.bin"` / `"metadata.redb"` — those are local to their tests and migrating each is churn.

### D3 — Skip HNSW + tantivy "touch" files

Some layout schemes create zero-byte placeholder files at the canonical paths to make `ls <shard_dir>` self-document the layout. Spec §26 doesn't mandate this and it adds I/O — skipping. `ShardPaths::*_hnsw()` returns the canonical path; phase 16/17 code creates the file when it first writes to the index.

### D4 — Don't fsync the directories

`spawn_shard` doesn't currently fsync directory entries on substrate startup; same approach for the new dirs. The substrate's durability discipline (WAL-before-ack) lives one level down. Creating an empty dir doesn't compromise that.

### D5 — Tests

New tests in `crates/brain-storage/src/layout.rs`:

- `ensure_dirs_creates_all_required_paths` — fresh tempdir → call `ensure_dirs` → assert every dir exists.
- `ensure_dirs_is_idempotent` — call `ensure_dirs` twice on the same root → Ok both times.
- `ensure_dirs_preserves_existing_substrate_files` — pre-create `arena.bin`, `metadata.redb`, `wal/seg-0001.wal` → call `ensure_dirs` → assert pre-existing files untouched, new dirs added.
- `shard_paths_join_correctly` — typed-getter spot-check (8 canonical paths).

Integration confirmation in `crates/brain-server/src/shard/mod.rs`:

- `spawn_shard_creates_knowledge_dirs` — spawn a shard; assert `statements.tantivy/` and `memory_text.tantivy/` exist after spawn; shut down cleanly. Existing spawn_shard tests already validate substrate files; this complements.

### D6 — Sub-task 15.4 receives the `llm_cache.redb` path via `ShardPaths::llm_cache_db()`

`layout.rs` exposes the constant + getter now; 15.4 calls it to open the redb file. Avoids the path string being known in two places.

## File plan

- `crates/brain-storage/src/layout.rs` — **new file**. ~80 lines + tests.
- `crates/brain-storage/src/lib.rs` — add `pub mod layout;` and a re-export of `ShardPaths`.
- `crates/brain-server/src/shard/mod.rs` — call `layout::ensure_dirs` once near the existing dir-create site; replace inline path strings with `ShardPaths` getters (substrate paths migrated opportunistically, ~6 lines changed).
- `crates/brain-server/src/shard/mod.rs` (tests) — `spawn_shard_creates_knowledge_dirs`.

No new dependencies.

## Done-when

- `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests` clean.
- Layout unit tests pass.
- spawn_shard test confirms knowledge dirs land on disk.
- Existing substrate integration / recovery tests stay green (no test-file migration in 15.3).
- One commit: `feat(storage): 15.3 — knowledge-layer on-disk path layout`.

## Risk register

| Risk | Mitigation |
|---|---|
| Renaming a substrate file by accident in the migration | Use the same string literals in the new `layout.rs` constants — verify by grep that the new constants match the old hardcoded names. |
| `ensure_dirs` fails on `mkdir` for a transient reason (disk full, perms) | Returns `io::Error`; spawn_shard already propagates `ShardError::dir_create(...)`. Wire the new call to the same error path. |
| Tantivy / HNSW phases reject pre-existing empty dirs | Tantivy accepts an empty pre-existing dir as a "create_in_dir" target; HNSW uses files, not dirs. Both validated against their docs before phases 16/22 land. |
| Idempotency surprise: `create_dir_all` on a file path raises ErrorKind::AlreadyExists | Doesn't apply — we're calling it on directory names, never file paths. |
| Test pollution if two tests share a tempdir | Standard `tempfile::tempdir()` gives unique per-test dirs. |

## Open questions for your approval

1. **Migration scope (D2)** — migrate only `spawn_shard`'s inline path strings to `ShardPaths`, or also the ~20 test sites? **Recommended: only `spawn_shard`.** Tests work in local tempdirs and migrating them is churn that should land separately (or never).
2. **Typed view shape (D1)** — `ShardPaths::at(root)` typed wrapper, or just free functions like `entity_hnsw_path(root: &Path) -> PathBuf`? **Recommended: typed wrapper.** Cheaper at call sites and reads better.
3. **HNSW touch files (D3)** — skip (spec doesn't mandate; saves I/O) or create zero-byte placeholders for `ls`-discoverability? **Recommended: skip.**
4. **`llm_cache.redb` creation** — kept in 15.4 as planned, or pulled into 15.3? **Recommended: kept in 15.4.** Different concern (redb file lifecycle vs. directory layout); they decompose cleanly.

## Workflow

On your nod: implement, run `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests`, commit as `feat(storage): 15.3 — knowledge-layer on-disk path layout`, then stop and write the 15.4 plan.
