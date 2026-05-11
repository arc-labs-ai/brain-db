# Phase 3 — Task 3.10: `MetadataDb` public type

**Classification:** simple-to-moderate. Wraps `redb::Database`, runs the schema check on open (composing 3.1), and exposes a minimal transaction surface that enforces single-writer-per-shard at compile time via `&mut self`. No new table content; this is the first composition layer over the 13 tables landed in 3.1–3.9.

**Spec:** `spec/07_metadata_graph/08_transactions.md` (full — two transaction kinds, MVCC, write-transaction granularity, multi-table consistency, single-writer-per-shard, best practices). Cross-checked `crates/brain-metadata/src/schema.rs` (3.1's `open_or_init_schema` is the open-time entry point).

## 1. Scope

In:

- `crates/brain-metadata/src/db.rs` (new):
  - `pub struct MetadataDb` — owns the `redb::Database` plus the schema version read at open.
  - `pub fn MetadataDb::open(path: impl AsRef<Path>) -> Result<Self, MetadataDbError>` — `Database::create(path)?` → `open_or_init_schema(&db)?` → construct.
  - `pub fn read_txn(&self) -> Result<redb::ReadTransaction, redb::TransactionError>` — `&self` so any number can coexist (MVCC).
  - `pub fn write_txn(&mut self) -> Result<redb::WriteTransaction, redb::TransactionError>` — `&mut self` to enforce single-writer-per-shard at compile time (CLAUDE.md §5 invariant 2).
  - `pub fn schema_version(&self) -> u32` — accessor for the version cached at open.
  - `pub fn path(&self) -> &Path` — for diagnostics / observability. Cheap, doesn't touch the DB.
  - `pub enum MetadataDbError` — unifies `redb::DatabaseError`, `redb::TransactionError`, and `SchemaError` under one `thiserror` umbrella for the open path. Read/write txn errors propagate as their native `redb::TransactionError`.
- `crates/brain-metadata/src/lib.rs` — add `pub mod db;` (and re-export `pub use db::{MetadataDb, MetadataDbError};` for ergonomics).

Out (deferred):

- **`impl MetadataSink for MetadataDb`** — 3.11. This is where `apply(lsn, payload)` translates WAL records into table writes, and where `durable_lsn()` reads from the `checkpoints` table.
- **Cross-crate integration test** — 3.12.
- **Typed convenience methods** (e.g. `db.get_memory(&id)`, `db.insert_memory(&meta)`) — deliberately not. Spec §07/08 §5 demonstrates the multi-table-per-txn pattern, and forcing callers through one-method-per-row-type would (a) duplicate redb's API verbosely, (b) break batching, (c) hide the transaction granularity from the caller. Callers `use brain_metadata::tables::memory::MEMORIES_TABLE;` and do their own `wtxn.open_table(...)`.
- **Cached table handles** (spec §07/08 §14 mentions a v1 cache on hot paths). Premature: hot paths aren't built yet; profile-driven.
- **Write-transaction timeout** (spec §07/08 §16, default 30 sec). Caller-level concern — the writer task owns this. `MetadataDb` doesn't auto-abort.
- **Snapshot integration** (spec §07/02 §10) — Phase 11+.

## 2. Spec quotes that bind the design

> **§07/08 §1:** "Read transaction — sees a consistent snapshot. Many can be concurrent. Write transaction — at most one active at a time."
>
> **§07/08 §3:** "The single-writer-per-shard discipline means there's only one writer per shard, naturally serializing redb's write transactions." → `&mut self` on `write_txn` is the type-system enforcement of this. Two writer tasks couldn't both hold `&mut MetadataDb`.
>
> **§07/08 §5:** the example txn opens six tables inside one write transaction. → caller-controlled multi-table txns are the norm; don't wrap.
>
> **§07/08 §10:** read-after-write within a transaction sees the writer's own changes (redb-native behaviour). → no test work for us; pin one anyway since it's a load-bearing assumption.
>
> **§07/08 §11:** "A single write transaction can update multiple tables atomically." → again, caller-controlled.
>
> **§07/08 §17 best practices:** "Open write transactions briefly. Don't do I/O or compute within them. Don't share transactions across async tasks." → `write_txn` returning a non-`Send` `WriteTransaction` already enforces "don't share across tasks" via Rust's type system. The "brief" + "no I/O" advice is caller discipline; we can't enforce it.

## 3. Design decisions

### 3.1 Open returns one `MetadataDbError`; transactions return native redb errors

The open path has three distinct error sources (file I/O via `redb::DatabaseError`, transaction begin via `redb::TransactionError`, schema version via `SchemaError` from 3.1). A unified `MetadataDbError` keeps the call site clean. After open, callers handle transaction-level errors with redb's native types — wrapping every `redb::TransactionError` in our own enum would force a `?` cascade and an unnecessary indirection.

```rust
#[derive(thiserror::Error, Debug)]
pub enum MetadataDbError {
    #[error("redb database error: {0}")]
    Database(#[from] redb::DatabaseError),
    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("schema: {0}")]
    Schema(#[from] crate::schema::SchemaError),
}
```

### 3.2 `write_txn` takes `&mut self`, not `&self`

The redb library itself allows `&self` on `begin_write` (it has internal serialization). We deliberately tighten to `&mut self` because:

- It encodes the single-writer-per-shard invariant in the type system. If a shard accidentally tries to host two writer tasks, the borrow checker stops it at compile time rather than waiting for runtime to serialize via redb's internal lock.
- CLAUDE.md §5 invariant 2 is "Single writer per shard. No locks needed; the discipline enforces it." `&mut self` makes "the discipline" load-bearing in the API rather than a convention.

This is the same call 2.9 made on `Wal::append(&mut self, ...)` — recorded as SD-2.9-1 there because the spec literally prescribed `&self`. For `MetadataDb`, the spec doesn't prescribe either way — §07/08 §3 just says "single-writer-per-shard"; tightening to `&mut self` is consistent with how we encoded it in the WAL layer.

### 3.3 Caller-controlled tables, no `db.get_memory(&id)` typed methods

§07/08 §5's example is *exactly* the pattern: open the DB, then within one txn, open several tables, do the inserts, commit. Typed convenience methods would (a) be N × #tables × #operations methods, (b) prevent the caller from batching into one txn, (c) hide the txn boundary. The current `pub mod tables` already gives callers the constants; that's enough.

### 3.4 `schema_version` is cached at open, not re-read on every call

Spec §07/02 §6 ("each table has a format version") implies the version is checked at open and held — re-checking on every call would be wasteful. `open_or_init_schema` returns the version once at open; `MetadataDb` stores it. If a future caller needs the *current* on-disk version (after an external migration), they can re-call `open_or_init_schema` against `self.db()` (we expose `db()` for this kind of escape hatch — see §3.6).

### 3.5 Expose `path: PathBuf` for diagnostics

Cheap to store, useful for `tracing::error!(path = %db.path().display(), "...")` patterns and `ADMIN_STATS`-style introspection. Not load-bearing for correctness.

### 3.6 Expose `pub fn db(&self) -> &redb::Database`

Escape hatch for operations the wrapper doesn't surface (e.g. backup/restore, compact, statistics). Read-only borrow. The borrow checker prevents callers from accidentally circumventing `&mut self` on writes by going through `db().begin_write()` — `db()` returns `&Database`, and `Database::begin_write(&self)` is allowed by redb but the resulting `WriteTransaction` can be obtained without `&mut MetadataDb`. **Possible workaround leak.**

Decision: ship `db()` anyway. Anyone calling `db().begin_write()` is opting out of the discipline knowingly; the type system can't perfectly seal it without sacrificing the escape hatch. Add a `#[doc(hidden)]` or doc warning that explicitly says "do not use this to start a write transaction." Cleaner than no escape hatch.

### 3.7 No `Drop` impl; redb handles cleanup

`redb::Database`'s own `Drop` closes the file cleanly. We don't add a manual close path.

## 4. Files touched

- `crates/brain-metadata/src/db.rs` (new) — ~150 LOC including tests.
- `crates/brain-metadata/src/lib.rs` — `pub mod db; pub use db::{MetadataDb, MetadataDbError};`.
- `docs/phases/phase-03-metadata.md` — flip 3.10 to ✅ post-implementation.

No new SD entry (we're consistent with 2.9's `&mut self` pattern, which already has SD-2.9-1).

## 5. Tests (gated `#[cfg(all(test, not(miri)))]`)

1. **`open_fresh_creates_schema_v1`** — open a fresh path; `db.schema_version() == 1`; the `__schema_meta` row exists.
2. **`open_existing_reads_schema`** — open, drop, reopen the same path; version is still 1; no error.
3. **`open_too_new_schema_refuses`** — pre-seed a file with `schema_version = 99` (using raw redb); `MetadataDb::open` returns `MetadataDbError::Schema(SchemaVersionTooNew { .. })`.
4. **`write_then_read_round_trip`** — write a `MemoryMetadata` via `write_txn`/`commit`, read it back via `read_txn`. Sanity that the full open + txn + table path works.
5. **`read_txn_doesnt_see_uncommitted_write`** — start a write txn, insert, then start a read txn from the same `MetadataDb` (read uses `&self`, write `&mut self` — need to drop the wtxn binding before borrowing immutably; the test demonstrates the borrow-checker-enforced flow). Confirm read doesn't see the insert until commit.
6. **`commit_makes_write_visible_to_new_read`** — write, commit, then read sees the change. Pairs with #5.
7. **`concurrent_read_txns_coexist`** — `db.read_txn()` twice from the same `&self`; both see the same snapshot. MVCC pin.
8. **`schema_version_accessor_returns_v1`** — direct read of `db.schema_version()`. Cheap, but pins that the cached value matches the on-disk value.
9. **`path_accessor_returns_open_path`** — `db.path()` equals what was passed to `open()`.

## 6. Verification

Linux dev-container harness, same as 3.5–3.9:

```
docker run --rm -v "$(pwd)":/workspaces/brain ... brain-dev:latest \
  bash -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-metadata"
```

Expected: 91 brain-metadata tests (82 prior + 9 new).

## 7. Commit

Branch: `feature/brain-metadata`. AUTONOMY §5 format:

```
feat(brain-metadata): MetadataDb public type (sub-task 3.10)
```

Body summarises: wrapper around `redb::Database`, schema check on open, `&mut self` enforcement of single-writer-per-shard, deliberate non-implementations (no typed convenience methods, no cached handles, no auto-abort timeout), 9 new tests. First composition piece — Phase 3 progress 10 of 12.

## 8. Done when

- [ ] `MetadataDb::open(path)` opens or creates the file, runs `open_or_init_schema`, and rejects too-new schemas.
- [ ] `read_txn(&self)` and `write_txn(&mut self)` both work; the borrow checker enforces single-writer.
- [ ] Round-trip through a `MemoryMetadata` write/read works end-to-end via the wrapper.
- [ ] MVCC properties pinned: read txn doesn't see uncommitted, post-commit read sees the change.
- [ ] 9 tests green; brain-metadata total: 91. `just verify` green in container.
- [ ] `docs/phases/phase-03-metadata.md` 3.10 flipped to ✅.

PLAN READY.
