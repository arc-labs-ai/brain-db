# 05 — redb metadata

**Audience:** anyone reading or writing a metadata table —
debugging a missing memory, adding a worker that needs durable
state, or extending the knowledge layer.

**Goal:** by the end you should know which table holds what,
where the on-disk file is, what compile-time guarantees prevent
two writers from clobbering each other, and what a transaction
looks like.

This chapter assumes [03 — Arena and WAL](03-arena-and-wal.md) —
the metadata store is *derived* from the WAL, so it leans on
recovery for its consistency story.

---

## What this layer is

Every shard owns one redb file: `<data_dir>/<shard_id>/metadata.redb`.
[redb](https://github.com/cberner/redb) is an embedded, ACID,
copy-on-write B-tree key/value store written in pure Rust. We
use it as the structured-data side of a shard.

The arena holds *vectors* (chapter 03). The HNSW holds *vector
neighbors* (chapter 04). Everything else — who owns this memory,
when it was created, what its salience is, what context it
belongs to, which edges connect it, what idempotency key any
recent request used, where the WAL last checkpointed — is in
redb.

Two important properties shape how the layer is used:

- **redb provides MVCC.** Many concurrent read transactions can
  coexist with one write transaction. Readers never block writers
  and vice versa.
- **redb is on the shard's executor.** Glommio-thread-local;
  `!Send`. The single-writer-per-shard discipline is encoded as
  a borrow-checker rule, described below.

The crate is `brain-metadata`
(`crates/brain-metadata/src/lib.rs`); the top of `lib.rs` re-exports
the most commonly-used ops. Like `brain-protocol`, it sets
`#![forbid(unsafe_code)]` — redb has its own unsafe internals,
but our wrapper doesn't add any.

---

## Why redb

The choice deserves one paragraph because it shows up in the
chapter's failure modes. We needed:

- Pure Rust (no C dep on the hot path).
- ACID transactions (multi-table writes are atomic).
- MVCC reads (HNSW snapshotting reads metadata without blocking
  the writer task).
- Copy-on-write so a snapshot of `metadata.redb` is consistent
  without quiescing the database.
- A B-tree with typed keys/values (range scans over edges,
  point lookups over memories).

Embedded key/value stores in the same general class — `sled`,
`heed` (LMDB wrapper), `fjall`, RocksDB — each fall short on one
of these. `sled` has a sophisticated cache and no copy-on-write
snapshot story we trusted. `heed` is solid but C-backed. RocksDB
is heavier than we need and writes are not atomic across CFs by
default. redb is younger but the trade-offs fit, and the codebase
is small enough to read end-to-end.

---

## The substrate catalog

Thirteen tables live in `crates/brain-metadata/src/tables/`. One
file per table; one `TableDefinition` constant per file.

| # | Constant | Key | Value | Purpose |
|---|---|---|---|---|
| 1 | `MEMORIES_TABLE` | `[u8; 16]` (`MemoryId`) | `MemoryMetadata` | Per-memory metadata. |
| 2 | `TEXTS_TABLE` | `[u8; 16]` | `Vec<u8>` (UTF-8) | Original memory text. |
| 3 | `EDGES_OUT_TABLE` | `(src, kind, tgt)` | `EdgeData` | Outgoing edges. |
| 4 | `EDGES_IN_TABLE` | `(tgt, kind, src)` | `EdgeData` | Incoming edges. |
| 5 | `CONTEXTS_TABLE` | `u64` (`ContextId`) | `ContextMetadata` | Context records. |
| 6 | `CONTEXT_NAMES_TABLE` | `(agent, name)` | `u64` | Name → ID lookup per agent. |
| 7 | `AGENT_CONTEXTS_TABLE` | `(agent, ctx)` | `()` | Membership index. |
| 8 | `IDEMPOTENCY_TABLE` | `[u8; 16]` (`RequestId`) | `IdempotencyEntry` | Replay protection. |
| 9 | `AGENTS_TABLE` | `[u8; 16]` (`AgentId`) | `AgentMetadata` | Per-agent metadata. |
| 10 | `MODEL_FINGERPRINTS_TABLE` | `[u8; 16]` | `ModelInfo` | Registry of seen models. |
| 11 | `CHECKPOINTS_TABLE` | `u64` | `CheckpointMeta` | Checkpoint records. |
| 12 | `NEXT_LSN_TABLE` | `()` | `u64` | Singleton next-LSN counter. |
| 13 | `SLOT_VERSIONS_TABLE` | `u64` | `u32` | Per-slot version (lazy reclaim). |

Catalog index lives in `crates/brain-metadata/src/tables/mod.rs:1`.
Each constant is a typed `redb::TableDefinition<'static, K, V>`
pinning the key and value Rust types — opening the wrong type for
a known table is a compile error.

There's also one **internal** table not in the catalog:
`__schema_meta`. The underscore prefix marks it as ours, not a
user-visible domain table
(`crates/brain-metadata/src/schema.rs:39`).

### `MemoryMetadata` — the most-touched row

Per-memory, ~140 bytes
(`crates/brain-metadata/src/tables/memory.rs:93`). Fields that
matter to readers:

- `memory_id_bytes`, `agent_id_bytes`, `context_id`,
  `slot_id`, `slot_version` — identity (the latter two pair with
  the arena slot from [chapter 03](03-arena-and-wal.md)).
- `kind` (`Episodic | Semantic | Consolidated`, mapped to `u8`).
- `text_size` — bytes of text in `TEXTS_TABLE`.
- `created_at_unix_nanos`, `last_accessed_at_unix_nanos`,
  `forgot_at_unix_nanos`, `tombstoned_at_unix_nanos`,
  `consolidated_at_unix_nanos` — lifecycle timestamps.
- `salience`, `salience_initial`, `access_count` — the decay
  worker reads and writes these (chapter 07).
- `embedding_model_fp` — the fingerprint of the model that
  produced this vector.
- `flags` — bitfield
  (`crates/brain-metadata/src/tables/memory.rs:39`):
  - `ACTIVE` (bit 0) — clear means tombstoned.
  - `HARD_FORGOTTEN` (bit 1) — vector was zeroed.
  - `PINNED` (bit 2) — won't be auto-evicted.
  - `STALE` (bit 3) — model fingerprint changed; not re-embedded.
  - Bits 4..=31 reserved.

The struct's `brain-core` types (`MemoryId`, `AgentId`,
`MemoryKind`) deliberately aren't `rkyv::Archive` — we don't want
the data-model layer to depend on a particular encoding. Instead
the row stores byte-shaped versions (`[u8; 16]`, `u64`, `u8`) and
the row type's getters convert at the API boundary
(`crates/brain-metadata/src/tables/memory.rs:6`).

### Edges — two tables, two views

Edges are stored *twice* — once keyed by `(source, kind, target)`
in `EDGES_OUT_TABLE`, once by `(target, kind, source)` in
`EDGES_IN_TABLE` (`crates/brain-metadata/src/tables/edge.rs:43`).
Same `EdgeData` body. The duplication doubles storage and turns
both forward queries ("what does X cause?") and reverse queries
("what supports X?") into tight range scans on a contiguous key
prefix.

The composite key encoding is little-endian-concatenated; redb
sorts lexicographically; so listing every edge of a given kind
from a given source is a single B-tree range scan — no in-memory
filter. `EdgeData`
(`crates/brain-metadata/src/tables/edge.rs:81`) carries the
weight, the origin (`EXPLICIT` vs `AUTO_DERIVED`), the worker
that created it (`CLIENT | CONSOLIDATION_WORKER |
SIMILARITY_WORKER`), the timestamp, and an optional annotation
string.

`link` and `unlink` helpers
(`crates/brain-metadata/src/tables/edge.rs:30`) take both tables
in one transaction so the two indexes are *guaranteed*
consistent. There is no API that writes only one direction.

### `IdempotencyEntry` — the 24-hour-replay table

`IDEMPOTENCY_TABLE` is keyed by `RequestId` (16-byte UUIDv7) and
holds an `IdempotencyEntry`
(`crates/brain-metadata/src/tables/idempotency.rs:85`):

```
response_kind:           u8       which op produced this entry
memory_id_bytes:         Option<[u8; 16]>
response_payload:        Vec<u8>  the bytes to replay verbatim
request_hash:            [u8; 32] BLAKE3 over the canonical request
created_at_unix_nanos:   u64
```

The `response_kind` byte is a wire-stable enum
(`crates/brain-metadata/src/tables/idempotency.rs:44`):
`ENCODE`, `FORGET`, `LINK`, `UNLINK`, `UPDATE_KIND`,
`UPDATE_CONTEXT`, `TXN_BEGIN`, `TXN_COMMIT`. Eight ops require
idempotency; `RECALL` and `PLAN` don't (they're read-only).

The flow on a state-mutating request is:

1. Compute the canonical hash of the incoming request.
2. Look up `RequestId` in the table.
3. If absent → run the op, store the entry, ack.
4. If present and `request_hash` matches → **replay** the stored
   response bytes verbatim. The op is *not* re-executed.
5. If present and `request_hash` doesn't match → return
   `IdempotencyConflict` — same RequestId reused for a different
   request body, which is a client bug.

Entries TTL out at 24 hours
(`crates/brain-metadata/src/tables/idempotency.rs:63`). The
idempotency cleanup worker (chapter 07) calls
`prune_expired`/`prune_expired_bounded`
(`crates/brain-metadata/src/tables/idempotency.rs:180`) once per
sweep — a single redb transaction that walks the table and
deletes anything past the TTL.

### `NEXT_LSN_TABLE` — the singleton

One row, key `()`, value `u64`. Holds the next LSN to assign. The
sink advances it on every WAL record applied
(`crates/brain-metadata/src/sink.rs:80`). On boot, the WAL knows
where to resume from by reading this row plus the latest
checkpoint
(`crates/brain-metadata/src/db.rs:106`).

### `CHECKPOINTS_TABLE` — recovery breadcrumbs

Keyed by checkpoint id (a `u64`). The latest row's `durable_lsn`
is what `recover()` from [chapter 03](03-arena-and-wal.md) uses
to skip records already reflected in the metadata. `MetadataDb::open`
reads it once at startup and caches it as `durable_lsn`
(`crates/brain-metadata/src/db.rs:55`).

---

## The knowledge-layer tables

When a schema is declared (`SCHEMA_UPLOAD`), 25 additional tables
become live inside the same `metadata.redb`. Module layout in
`crates/brain-metadata/src/tables/knowledge/mod.rs:9`:

| Family | Tables | Highlights |
|---|---|---|
| `entity` | 5 | Primary + canonical-name + aliases + trigram index + mentions |
| `statement` | 8 | Primary + 6 secondary indexes (subject, predicate, object, event-time, evidence) + evidence overflow + chain |
| `relation` | 4 | Primary + 2 direction indexes + evidence index |
| `predicate` | 1 | Interned predicate registry (`u32` ids) |
| `entity_type` | 1 | User-declared entity types |
| `relation_type` | 1 | User-declared relation types |
| `extractor` | 1 | Active extractors + per-extractor enable flag |
| `schema_version` | 1 | Schema upload history |
| `audit` | 2 | Extraction audit + resolution audit |
| `merge` | 1 | Entity merge log |

That's 25 knowledge tables on top of the 13 substrate tables. The
crate ships **both** sets unconditionally; declaring a schema
just changes whether the knowledge tables get written to. A
substrate-only deployment has empty knowledge tables. See
[09 — knowledge layer](09-knowledge-layer.md) for what activates
them.

The knowledge tables share one boilerplate concern: every value
type needs a `redb::Value` impl. They all do the same
rkyv-with-`check_bytes` dance, so the crate ships a macro,
`impl_redb_rkyv_value!`
(`crates/brain-metadata/src/tables/knowledge/mod.rs:48`), to
collapse 11 identical impls.

The LLM extractor cache is a **separate** redb file — `llm_cache.redb`
in the same shard directory — wrapped by `LlmCacheDb`
(`crates/brain-metadata/src/llm_cache.rs`). Keeping it out of the
main metadata file means cache traffic doesn't compete with hot
substrate writes and snapshots can omit it cheaply.

---

## How rows get encoded

Values are encoded with **rkyv** — same library as the wire
protocol ([chapter 02](02-wire-protocol.md)), different goal.

For a substrate row like `MemoryMetadata`, the path is:

- **Write:** `rkyv::to_bytes::<_, 256>(&row)` produces a
  `Vec<u8>`, redb stores it under the key.
- **Read (today):** `redb::Value::from_bytes` runs the full rkyv
  deserialise to return an owned `MemoryMetadata`
  (`crates/brain-metadata/src/tables/memory.rs:14`).

The "zero-copy reads" story (return a `&ArchivedMemoryMetadata`
straight into the redb-mmap'd page) is *available* in rkyv but
deferred until profiling identifies a hot read path. The current
deserialise-on-read code is shorter and easier to test; the
trade-off is documented in the file's module docs.

A few tables hold raw bytes instead of rkyv values: `texts`
stores UTF-8 directly (`Vec<u8>`), and the trigram-index tables
in the knowledge layer use bit-packed keys for compactness.

### Variable-length values

rkyv handles variable-length fields (`String`, `Vec<T>`,
`Option<T>`) via offsets within the blob. A `MemoryMetadata`
row's `text_size` is held separately from the actual text bytes
(which live in `TEXTS_TABLE`) so listings of memory rows are
fixed-cost regardless of text length. The cost of reading the
text on demand is one extra B-tree lookup.

---

## Transactions

redb gives us ACID transactions; the wrapper exposes them
unchanged. `MetadataDb`
(`crates/brain-metadata/src/db.rs:50`) wraps the open database
and the cached durable LSN:

```rust
pub struct MetadataDb {
    pub(crate) db: Database,
    schema_version: u32,
    path: PathBuf,
    pub(crate) durable_lsn: u64,
    pub(crate) pending_checkpoints: HashMap<u64, u64>,
}
```

Two methods open transactions
(`crates/brain-metadata/src/db.rs:135`):

```rust
pub fn read_txn(&self)      -> Result<ReadTransaction, …>   // &self
pub fn write_txn(&mut self) -> Result<WriteTransaction, …>  // &mut self
```

The signatures are doing real work. `read_txn` takes `&self` —
many can coexist. `write_txn` takes `&mut self` — **the borrow
checker enforces single-writer-per-shard at compile time.** A
shard cannot accidentally host two writer tasks because both
would need `&mut MetadataDb`, which Rust refuses.

This is the rare case where the type system replaces a runtime
contract. We don't need a mutex; we don't need a lock; we don't
even need a runtime check — the discipline is structural.

There is an escape hatch — `db()` returns the raw `&redb::Database`
— but its doc explicitly warns against calling `begin_write` on
it (`crates/brain-metadata/src/db.rs:174`). The borrow checker
can't seal it perfectly; from there it's caller discipline.

### Multi-table atomic writes

A handler that needs to update several tables opens them all
inside one write transaction. The `apply_encode` path in the
sink is a good example
(`crates/brain-metadata/src/sink.rs:107`):

```rust
let wtxn = self.db.begin_write()?;
{
    // 1. memories
    let mut t = wtxn.open_table(MEMORIES_TABLE)?;
    t.insert(&memory_id.to_be_bytes(), &mem)?;

    // 2. texts
    let mut t = wtxn.open_table(TEXTS_TABLE)?;
    t.insert(&memory_id.to_be_bytes(), text)?;

    // 3. idempotency
    let mut t = wtxn.open_table(IDEMPOTENCY_TABLE)?;
    t.insert(&request_id_bytes, &entry)?;

    // 4. model_fingerprints (if new)
    // 5. edges (both EDGES_OUT and EDGES_IN)
    // 6. slot_versions

    self.bump_next_lsn_in_txn(&wtxn, lsn)?;
}
wtxn.commit()?;
```

`wtxn.commit()` is the durability point for *this* transaction.
Either every table reflects the write, or none do. There is no
intermediate state visible to a reader.

But — and this matters — **the WAL is the durability boundary
for the whole shard**, not the redb commit. The WAL was already
fsynced before the redb work began. If the process crashes
between WAL ack and redb commit, the next recovery replays the
WAL record and re-runs `apply_encode`. The handler returns idempotent
results because every table key it touches is keyed by something
the WAL also carries (`memory_id`, `request_id`).

### How a sink handler stays idempotent

Each `apply_*` function does absolute writes, not deltas. The
`apply_encode` shown above inserts a fresh row regardless of
whether one was there before. A second invocation with the same
LSN produces byte-for-byte the same row. `apply_forget`
(`crates/brain-metadata/src/sink.rs:196`) sets `ACTIVE = 0`
unconditionally; running it twice has the same effect as once.

`bump_next_lsn` is the one trick: it only advances `NEXT_LSN` if
the incoming LSN is greater, never decreasing. Replay-twice
behaviour is safe.

### Read transactions and MVCC

A `ReadTransaction` sees a consistent snapshot of the database
at the moment it was opened. A writer can commit *afterwards*;
the reader continues to see the old state. This is what lets
the HNSW background workers
([chapter 07](07-background-workers.md)) take consistent reads of
the metadata table while the request handler is writing to it.

Read transactions are cheap to open (one atomic counter
increment) and cheap to drop. There's no limit on how many can
coexist; you don't need to share one across an executor.

---

## Schema versioning

There's one schema version per file, in `__schema_meta`
(`crates/brain-metadata/src/schema.rs:39`):

```rust
pub const CURRENT_SCHEMA_VERSION: u32 = 1;
pub const SCHEMA_VERSION_KEY: &str = "schema_version";
pub const SCHEMA_META_TABLE: TableDefinition<'static, &'static str, u32> =
    TableDefinition::new("__schema_meta");
```

`open_or_init_schema`
(`crates/brain-metadata/src/schema.rs:71`) runs at every
`MetadataDb::open`:

- **Fresh file (table absent):** writes `CURRENT_SCHEMA_VERSION`.
- **Same version:** returns the stored version.
- **Older version:** returns the stored version (a future
  migration registry would dispatch here; no migrations exist at
  v1).
- **Newer version:** returns `SchemaError::SchemaVersionTooNew`
  — the binary is too old for the file
  (`crates/brain-metadata/src/schema.rs:60`).

A "newer than the binary" file is a hard refusal: rolling back
to an older binary is not a supported operation; the operator
needs to either upgrade or restore from a compatible backup.

There's an explicit decision to keep this version *file-global*
rather than per-table, despite the per-table option being in the
design space (`crates/brain-metadata/src/schema.rs:14`). The 13
substrate tables and 25 knowledge tables co-evolve from the same
crate, so a per-table version registry would be bookkeeping with
no concrete benefit at v1. If a future version diverges
per-table, the module will extend.

---

## Lifecycle: how the file gets populated

`MetadataDb::open`
(`crates/brain-metadata/src/db.rs:97`) is the entry point and
runs in this order:

1. `redb::Database::create(path)` — opens or creates the file.
2. `open_or_init_schema(&db)` — writes the schema version on a
   fresh file, validates on an existing one.
3. Read the latest `CheckpointMeta` (if the table exists) to
   seed `durable_lsn` for the recovery driver
   (`crates/brain-metadata/src/db.rs:106`).
4. `seed_system_schema(&db)` — idempotently writes the built-in
   schema entries (knowledge-layer system types, default
   extractors). Re-opens are no-ops because the seed checks
   "is `brain` schema already active?" first
   (`crates/brain-metadata/src/db.rs:122`).
5. Return the `MetadataDb`.

The seed step is what makes a fresh shard *immediately* usable
without an explicit `SCHEMA_UPLOAD`. It lays down the built-in
predicates, entity types, and extractor placeholders for the
knowledge layer, but those tables stay empty in
substrate-only deployments because no schema is declared.

After `MetadataDb::open` returns, the WAL recovery driver
([chapter 03](03-arena-and-wal.md)) replays every WAL record
since `durable_lsn`. Each record passes through `MetadataSink::apply`
(`crates/brain-metadata/src/sink.rs:59`), which dispatches to
`apply_encode` / `apply_forget` / `apply_link` / etc., each of
which is one redb write transaction. Once recovery returns, the
metadata reflects every durable WAL record.

---

## Failure modes

**redb fails to open the file.** Disk full, permission denied,
or actually corrupted. `MetadataDb::open` returns
`MetadataDbError::Database` and the shard's `spawn_shard` aborts.
The server exits non-zero before opening any listener.

**Schema version too new.** The file was written by a binary
newer than the one starting up. `SchemaError::SchemaVersionTooNew`
with the stored and supported versions both in the error string.
Operator either upgrades the binary or restores from a
compatible backup.

**`__schema_meta` table exists but the key is missing.** Treated
as fresh — the init path runs and writes `CURRENT_SCHEMA_VERSION`.
This isn't expected in practice (the init is atomic), but the
code is robust to it.

**A redb commit fails mid-transaction.** The transaction aborts;
no tables are updated. The WAL record is already durable, so
recovery will re-run the handler later. The handler's response
to the client is "this looks transient; retry" via the
appropriate `MetadataError` wire code
([chapter 02](02-wire-protocol.md)).

**redb file corrupted past redb's own checks.** redb verifies
its B-tree pages on read; a checksum failure becomes a
`redb::StorageError`. The shard refuses to operate; operator
restores from snapshot.

**Out of space mid-write.** `wtxn.commit()` returns an error.
Same outcome as commit-fails above — the WAL is durable, no
tables updated, recovery will retry.

**Concurrent `&mut MetadataDb` impossible.** Compile-time
guarantee. If you see two write transactions on a shard, the
discipline broke somewhere upstream; the borrow checker prevents
it inside the crate.

---

## Configuration & tuning

There's not much to tune; redb's defaults are appropriate for
the shard's working set. The knobs that do exist:

| Knob | Where | Default | Notes |
|---|---|---|---|
| Idempotency TTL | `DEFAULT_TTL_NANOS` | 24 h | The cleanup worker calls `prune_expired` against this. |
| Schema version | `CURRENT_SCHEMA_VERSION` | 1 | Compile-time. Bump on layout/encoding changes. |
| `MetadataDb` page cache size | redb default | — | The file is mmap'd; OS page cache does the work. |

Operational rules:

- **`metadata.redb` is mmap'd.** Don't `cp` it while the server
  is running; use the snapshot worker (chapter 07) or stop the
  server first.
- **Shard ownership is exclusive.** Two `brain-server` processes
  against the same `data_dir` will both write to the same redb
  file. redb's own consistency assumes a single process; multiple
  writers will corrupt it. The single-writer-per-shard rule is
  for the shard's *threads*; you also need single-process per
  `data_dir`.
- **Idempotency table is the largest growth source** on a write-
  heavy shard. The 24 h sweep is what bounds it. If the sweep
  worker is paused, the table grows linearly with request rate.

---

## Where it lives in the code

| Topic | Path |
|---|---|
| Public exports | `crates/brain-metadata/src/lib.rs` |
| `MetadataDb` wrapper | `crates/brain-metadata/src/db.rs` |
| Schema version table | `crates/brain-metadata/src/schema.rs` |
| `MetadataSink for MetadataDb` (WAL → tables) | `crates/brain-metadata/src/sink.rs` |
| Substrate table catalog | `crates/brain-metadata/src/tables/mod.rs` |
| `MEMORIES_TABLE`, `MemoryMetadata` | `crates/brain-metadata/src/tables/memory.rs` |
| `EDGES_OUT_TABLE`, `EDGES_IN_TABLE`, `EdgeData` | `crates/brain-metadata/src/tables/edge.rs` |
| `IDEMPOTENCY_TABLE`, `IdempotencyEntry`, `prune_expired` | `crates/brain-metadata/src/tables/idempotency.rs` |
| `CONTEXTS_TABLE`, name lookup, membership | `crates/brain-metadata/src/tables/context.rs` |
| `AGENTS_TABLE` | `crates/brain-metadata/src/tables/agent.rs` |
| Model fingerprint registry | `crates/brain-metadata/src/tables/model_fingerprint.rs` |
| Checkpoints | `crates/brain-metadata/src/tables/checkpoint.rs` |
| Per-slot version (lazy reclaim) | `crates/brain-metadata/src/tables/slot_version.rs` |
| Knowledge-layer tables | `crates/brain-metadata/src/tables/knowledge/` |
| LLM extractor cache (separate redb file) | `crates/brain-metadata/src/llm_cache.rs` |
| System schema seed | `crates/brain-metadata/src/system_schema/` |

---

## Further reading

- [03 — Arena and WAL](03-arena-and-wal.md) for what the WAL
  recovery driver feeds into the sink and how the durable LSN
  watermark is maintained.
- [07 — Background workers](07-background-workers.md) for the
  idempotency sweep, checkpoint worker, edge scrubber, and
  statistics reconciler.
- [09 — Knowledge layer](09-knowledge-layer.md) for what the
  knowledge tables hold and when they activate.
- [10 — Extractors](10-extractors.md) for what populates the
  extraction audit tables and uses the LLM cache.
