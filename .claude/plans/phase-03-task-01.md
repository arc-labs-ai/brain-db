# Phase 3 — Task 3.1: Schema versioning header

**Classification:** light. The schema-meta table is one row of one column with three branches of logic. The work is mostly establishing crate scaffolding (deps, lib.rs, error type, redb conventions) that the remaining 11 sub-tasks will reuse.

**Spec:** `spec/07_metadata_graph/02_table_layout.md` §6 (schema evolution at the table level) + `spec/02_data_model/09_schema_evolution.md` (general policy). The 02/09 doc is broad — the operative directives for 3.1 are §1 ("add fields, don't remove") and §9 ("storage formats have versions").

## 1. Scope

Deliver:

- `crates/brain-metadata/Cargo.toml` — replace the Phase-0 placeholder with the real deps (`redb`, `rkyv`, `brain-storage`, `tracing`, `bytemuck`).
- `crates/brain-metadata/src/lib.rs` — replace the placeholder with the real module structure (declares `pub mod schema;`; later sub-tasks add `pub mod tables;` etc.).
- `crates/brain-metadata/src/schema.rs` — `CURRENT_SCHEMA_VERSION`, `SCHEMA_META_TABLE`, `SchemaError`, `open_or_init_schema(&Database) -> Result<u32>`. Plus tests.

Out:

- Any other tables (they're 3.2–3.9).
- The `MetadataDb` wrapper (3.10).
- Migration registry (not needed at v1 — there's no v0 to migrate from; placeholder doc comment only).
- A `cargo doc` warnings sweep (covered in phase exit).

## 2. Spec quotes

> **§07/02 §6 (schema evolution at the table level):**
> > Each table has a format version embedded in its metadata. When the substrate opens the metadata store:
> > - Read the format version of each table.
> > - If older than current, run any registered migrations.
> > - If newer than current, refuse to open (the substrate is too old).
>
> **§02/09 §9 (storage-format evolution):** "Storage formats (arena, WAL, redb tables) have their own versions."

Reading the spec literally, §07/02 §6 says "*each table* has a format version." We deviate by using a *single* `__schema_meta` table that tracks one global version. Justification:

- The 13 tables co-evolve. They share the same code base; bumping one means bumping the whole crate's format-version.
- Per-table versions multiply the surface area (13× the open-time check, 13× the migration registry) for no concrete benefit at v1.
- The spec's framing is consistent with "one version for the whole metadata file" — §06 talks about "the metadata store" being upgraded as a unit; §02/09 §9 says "storage formats have versions" (plural across stores, singular within).

Flag as a deviation candidate but not in the deviation log: it's a *coverage scope* call, parallel to the miri scope decision. Document in the schema.rs module doc.

## 3. Architecture

### 3.1 Constants

```rust
// crates/brain-metadata/src/schema.rs

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

/// The schema version this crate writes. Bumped on backward-incompatible
/// changes to the redb table layout or value encoding.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Singleton table tracking the on-disk schema version. The "__" prefix
/// signals "internal — not a domain table"; the 13 spec'd tables (§07/02
/// §1) avoid this prefix.
pub const SCHEMA_META_TABLE: TableDefinition<'static, &'static str, u32> =
    TableDefinition::new("__schema_meta");
```

`TableDefinition<&'static str, u32>` — key is a static string (we use a single key `"schema_version"` for the singleton), value is `u32`. Why not `TableDefinition<(), u32>` for a pure singleton? redb's `()` key type works, but using a named string key is friendlier for ad-hoc inspection (`redb-cli dump`-style tools surface readable keys). Trade-off is one extra byte on disk; negligible.

### 3.2 Errors

```rust
#[derive(thiserror::Error, Debug)]
pub enum SchemaError {
    #[error("redb error: {0}")]
    Redb(#[from] redb::Error),

    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),

    #[error("redb commit error: {0}")]
    Commit(#[from] redb::CommitError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error(
        "schema version {found} is newer than this binary supports ({supported}); \
         upgrade the substrate or restore from a compatible backup"
    )]
    SchemaVersionTooNew { found: u32, supported: u32 },
}
```

redb's error hierarchy is split across several types; absorb the common ones with `#[from]`.

### 3.3 The open-or-init function

```rust
/// Read the schema version from `db`, or initialize it on a fresh DB.
///
/// Behavior:
/// - **Fresh DB** (`__schema_meta` table absent): write
///   `CURRENT_SCHEMA_VERSION`, return it.
/// - **Same version**: return the stored version.
/// - **Older version**: return the stored version (the caller can dispatch
///   to a migration registry — v1 has no migrations).
/// - **Newer version**: return `SchemaVersionTooNew`.
pub fn open_or_init_schema(db: &Database) -> Result<u32, SchemaError>;
```

Implementation outline:

```rust
pub fn open_or_init_schema(db: &Database) -> Result<u32, SchemaError> {
    // 1. Peek with a read transaction.
    {
        let rtxn = db.begin_read()?;
        match rtxn.open_table(SCHEMA_META_TABLE) {
            Ok(table) => {
                if let Some(stored) = table.get("schema_version")? {
                    let v = stored.value();
                    if v > CURRENT_SCHEMA_VERSION {
                        return Err(SchemaError::SchemaVersionTooNew {
                            found: v,
                            supported: CURRENT_SCHEMA_VERSION,
                        });
                    }
                    return Ok(v);
                }
                // Table exists but row missing — treat as fresh.
            }
            Err(redb::TableError::TableDoesNotExist(_)) => { /* fresh */ }
            Err(e) => return Err(e.into()),
        }
    }

    // 2. Initialize.
    let wtxn = db.begin_write()?;
    {
        let mut table = wtxn.open_table(SCHEMA_META_TABLE)?;
        table.insert("schema_version", &CURRENT_SCHEMA_VERSION)?;
    }
    wtxn.commit()?;
    Ok(CURRENT_SCHEMA_VERSION)
}
```

Two redb txns (read + maybe-write) is slightly wasteful but readable. Alternative: one write txn that does both check and init. Stick with two; startup cost is negligible.

### 3.4 Crate skeleton

`Cargo.toml`:

```toml
[package]
name = "brain-metadata"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
description = "Metadata store (redb) and graph storage for Brain."

[dependencies]
brain-core = { path = "../brain-core" }
brain-storage = { path = "../brain-storage" }
redb.workspace = true
rkyv.workspace = true
thiserror.workspace = true
bytemuck.workspace = true
tracing.workspace = true

[dev-dependencies]
proptest.workspace = true
tempfile.workspace = true
```

`brain-storage` is added now (3.1) even though we don't use it until 3.11. Keeps the Cargo.toml stable across sub-tasks.

`lib.rs`:

```rust
//! # brain-metadata
//!
//! redb-backed metadata store. See `spec/07_metadata_graph/` for the
//! authoritative design.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod schema;
```

The `#![forbid(unsafe_code)]` is inherited from the Phase-0 placeholder. brain-metadata has no syscalls; redb owns those internally.

### 3.5 Module placement

```
crates/brain-metadata/src/
├── lib.rs       (modified: declare pub mod schema;)
└── schema.rs    (new)
```

Later sub-tasks will add `tables/`, `db.rs`, `sink.rs`.

## 4. Trade-offs

| Option | Verdict | Why |
|---|---|---|
| Single global `__schema_meta` vs per-table format versions | Single global | Spec §07/02 §6 implies per-table but says "the substrate" upgrades as a unit. 13× the bookkeeping for no benefit at v1. Documented in module docs. |
| `TableDefinition<&str, u32>` vs `TableDefinition<(), u32>` | `&str` | One-byte overhead for ad-hoc-tool readability. |
| Two-txn open (read first, then init) vs one write txn always | Two-txn | Cleaner code; negligible startup-cost difference. |
| Migration registry | Defer | No migrations exist at v1; placeholder doc comment only. |
| `tracing` events at open time | Yes (info-level) | Useful operational signal: "opened brain-metadata at schema_version=1, file=..." |

## 5. Risks

- **redb's API surface for `TableDefinition` lifetimes.** redb 2.x uses `TableDefinition<'static, ...>`. Confirming the `const`-able form during implementation. If lifetime ergonomics bite, fallback is `pub fn schema_meta_table() -> TableDefinition<&str, u32>` (fn instead of const).
- **`TableError::TableDoesNotExist` variant name.** redb 2.x changed it across point releases. If the literal `TableDoesNotExist(_)` doesn't match, swap for the current variant. (Compile-time error if wrong — easy to fix.)
- **`redb::Error` is a sealed enum** with `#[non_exhaustive]`. Our `From` impls work; user code can't pattern-match exhaustively. Document.
- **No `cfg(target_os = "linux")` gate.** brain-metadata is portable in principle (redb works on macOS/Windows); the gate stays at brain-storage. Confirm `cargo check -p brain-metadata` works on macOS host before merging.

## 6. Test plan

All tests in `schema.rs`'s `#[cfg(test)] mod tests`. Use `tempfile::TempDir` for ephemeral DB files.

1. **Fresh DB initializes at v1.** `Database::create(tmp/test.redb)` → `open_or_init_schema(&db) == 1`.
2. **Reopen reads v1.** Init, drop, reopen the same file, `open_or_init_schema == 1`.
3. **Future version refuses.** Init at v1, then manually `wtxn.insert("schema_version", &2)?`, drop, reopen, expect `SchemaVersionTooNew { found: 2, supported: 1 }`.
4. **Empty DB with no tables initializes correctly.** Edge case: redb may create the file lazily.
5. **Idempotent re-init.** Call `open_or_init_schema` twice on the same DB handle; both return 1, no error.

**Total: 5 tests.**

Native pass = miri pass: `schema.rs` is pure-data + redb. redb internally uses `mmap` (which miri doesn't shim), so the test module is gated `#[cfg(all(test, not(miri)))]` for consistency with Phase 2's pattern. Document.

## 7. Estimated commit shape

One commit on a new branch `feature/brain-metadata`:

> `feat(brain-metadata): schema versioning header (sub-task 3.1)`

Body covers:
- Crate scaffolding (real deps, real lib.rs).
- Schema-version singleton table + open-or-init logic.
- The "global version instead of per-table" deviation from a literal reading of §07/02 §6.
- Test count + miri gating.

Files touched:
- `crates/brain-metadata/Cargo.toml`
- `crates/brain-metadata/src/lib.rs`
- `crates/brain-metadata/src/schema.rs`

Verify gate: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p brain-metadata`, `./scripts/check-skills.sh`. Also confirm `cargo check -p brain-metadata` works on the macOS host (no Linux gate).

---

PLAN READY: see `.claude/plans/phase-03-task-01.md` — confirm to proceed.
