# 07.01 redb: The Choice

We chose [redb](https://github.com/cberner/redb) for the embedded ACID key-value store. This file documents the choice and the alternatives considered.

## 1. The choice

redb is:

- A pure-Rust embedded ACID key-value store.
- A B-tree-based, MVCC-enabled, transaction-safe engine.
- Linux/macOS/Windows compatible (we use Linux).
- MIT/Apache 2.0 licensed.
- Actively maintained.

GitHub: [cberner/redb](https://github.com/cberner/redb).

## 2. The selection criteria

- **Pure Rust.** No FFI, no native dependencies. Brain ships as a single Rust binary; cgo or libc++ dependencies would complicate builds.
- **ACID transactions.** Multi-key atomicity for inserting a memory with its edges.
- **MVCC for concurrency.** Reads don't block writes; writes don't block reads.
- **Embedded.** No external server process to manage.
- **Performance.** Fast enough for our latency targets.
- **License.** Permissive (MIT/Apache 2.0).
- **Maintenance.** Actively developed; bugs get fixed.

## 3. Alternatives considered

### 3.1 RocksDB

The dominant embedded LSM-tree store, used by countless projects. C++ with bindings.

**Rejected because:**

- C++ dependency. Brain wants pure Rust.
- LSM-tree is overkill for our access patterns. Our metadata is mostly read-modify-write of small records; LSM's write-amplification is a tax.
- Operationally complex. Tuning compaction, write buffers, etc. requires expertise.
- Binary size and build time are heavy.

### 3.2 sled

A pure-Rust embedded database. Was popular but is no longer actively maintained.

**Rejected because:**

- Maintenance status: the primary maintainer has effectively paused work.
- Bug-finding tools have flagged issues that haven't been resolved.
- We don't want to inherit the bug surface.

### 3.3 SQLite

The world's most-used embedded database. C with bindings.

**Rejected because:**

- C dependency.
- SQL surface area is more than we need (we don't want SQL parsing, query optimization on top of redb's primitives).
- Schema migrations are heavy for our model.
- Performance for our access patterns is comparable or worse than redb.

For deployments wanting SQL-style introspection, an external mirror (export to SQLite or PostgreSQL) is feasible at the SUBSCRIBE layer.

### 3.4 LMDB

An mmap-based embedded key-value store. Mature.

**Rejected because:**

- C dependency (via lmdb-rs binding).
- mmap-based; subject to the criticisms in [Pavlo et al. 2022](https://db.cs.cmu.edu/mmap-cidr2022/) for database use.
- Single-writer; we'd want multi-writer-with-MVCC.
- API is C-style; less idiomatic from Rust.

### 3.5 Custom

Build our own metadata store.

**Rejected because:**

- Significant time investment.
- Test surface for ACID correctness is large.
- redb is good enough.

### 3.6 fjall

A more recent pure-Rust LSM-tree library. Still maturing.

**Rejected because:**

- LSM-tree access patterns don't fit our workload.
- redb's B-tree better matches our random-access read patterns.

## 4. The redb data model

redb organizes data into **tables**, each table being a typed B-tree:

```rust
const MEMORIES_TABLE: TableDefinition<&MemoryId, &MemoryMetadata> 
    = TableDefinition::new("memories");
```

Within a table:
- Keys are sorted (B-tree property).
- Range queries are efficient.
- Point lookups are O(log N).

Multiple tables coexist in a single database file.

## 5. Transactions in redb

redb's transactions:

- **Begin a write transaction** with `db.begin_write()`. There's at most one active write transaction at a time.
- **Begin a read transaction** with `db.begin_read()`. Multiple read transactions can be concurrent with each other and with the write transaction.
- **Commit** the write transaction with `txn.commit()`. The commit is persistent.
- **Abort** by dropping the transaction without commit.

Read transactions see a consistent snapshot at the moment they began. They don't see modifications from in-progress write transactions.

Write transactions see their own modifications in addition to the snapshot.

## 6. Performance characteristics

For our access patterns:

| Operation | Latency |
|---|---|
| Get by key (cached) | < 1 µs |
| Get by key (cold) | ~5-10 µs |
| Insert/update single key | < 5 µs (within open txn) |
| Range scan (10 results) | ~10-20 µs |
| Commit (sync to disk) | 0.1-1 ms |

Throughput:
- ~10K-50K commits/sec for our typical transaction sizes.
- Higher for batched writes within a single transaction (~100K rows/sec).

These are comfortable for Brain's targets.

## 7. The disk format

redb uses a single file per database. The format:

- A header (with magic, version, table catalog).
- Pages of B-tree nodes and leaves.
- A free-list for reusing pages.

The format is documented in the [redb internals docs](https://github.com/cberner/redb/blob/master/docs/design.md).

## 8. The internal sync model

redb syncs to disk:

- On every commit (configurable; defaults to sync-on-commit).
- Optionally periodically (asynchronous mode for higher throughput at lower durability).

For Brain, we use sync-on-commit. The substrate's WAL is the durability barrier; redb's sync is a defense-in-depth that ensures redb's own state is durable too.

## 9. Compaction and reclamation

redb manages page reclamation internally. When a row is updated or deleted, the old pages become free; redb reuses them.

Periodic compaction can reduce file fragmentation. redb does this automatically; Brain doesn't drive it.

The metadata file's size grows roughly linearly with the number of live records. After many deletes, the file may have unused capacity; redb may reclaim it lazily or expose it for explicit reclamation.

## 10. Compatibility commitments

redb's on-disk format has version compatibility:

- Within a major version of redb, files are forward and backward compatible.
- Across major versions, redb provides migration tooling.

Brain pins to a specific minor version of redb during a release cycle and bumps deliberately during upgrades.

## 11. The risks of redb

redb is younger than RocksDB, SQLite, etc. The risks:

- **Bugs in less-traveled code paths.** Mitigated by our use of stable redb releases plus a strong testing regime in our integration tests.
- **Major version bumps.** Bring migration costs.
- **Maintenance dependency.** If redb stops being maintained, we'd need to migrate. Mitigated by the file format being relatively simple — we could write a one-time exporter.

## 12. The stability assumption

Brain treats redb as a stable, ACID-compliant key-value store. We don't assume crash-recovery quirks; we don't probe edge cases of redb's internals. If redb has a bug that affects us, we report and patch it; we don't engineer workarounds in Brain's logic.

## 13. The version we ship with

At the time of writing, Brain is targeting redb v2.x. The exact version is in the project's `Cargo.toml`.

---

*Continue to [`02_table_layout.md`](02_table_layout.md) for the table layout.*
