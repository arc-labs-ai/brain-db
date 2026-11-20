# 09.06 Index Persistence & Recovery

The three per-shard HNSW indexes (memory, statement, entity) hold **derived**
state: every vector in them is reconstructable from durable source-of-truth —
redb for the live set, with the arena as the boot-time image of it (§2a) — and
the shard is always able to rebuild one from scratch. What differs is whether
it has to.

The memory index has a graph snapshot, so its restart is a load plus a tail
(§7). The entity index loads stored vectors and rebuilds its graph. The
statement index rebuilds from text, in the background. This file specifies each
path and the durability contract that makes it lossless.

## 1. Why the graph is never maintained on disk

`hnsw_rs` builds an in-memory graph with no cheap incremental on-disk insert, so
there is no way to keep a dump current as writes land — only to rewrite the
whole thing on a cadence. Brain does exactly that and no more: the memory
index's graph is dumped whole when a snapshot is taken (§7), and nothing on the
write path touches a graph file.

What the write path does instead is keep the *vectors* durable: one WAL append
plus one redb write transaction, which is where the write-time vector lands.
Recovery is then a pure function of durable state — load the most recent dump
if there is a valid one, and derive the rest. See
[`08.04 §8`](../08_storage/04_recovery.md).

## 2. The three indexes

| Index | Source of truth | Restore path | Trigger |
|---|---|---|---|
| Memory HNSW | snapshot (preferred) + arena (boot fallback) | `SharedHnsw::load_snapshot` from the latest snapshot bundle; replay arena entries past `taken_at_lsn` into pending; re-tombstone ids redb now marks inactive. On any snapshot failure, full rebuild from the arena. | startup, synchronous, before serving |
| Entity HNSW | `ENTITY_VECTORS_TABLE` (preferred) + `ENTITIES_TABLE.canonical_name` (fallback) | read stored vector for each live entity; re-embed canonical name when the row is absent | startup, synchronous, before serving |
| Statement HNSW | `STATEMENTS_TABLE` (text) | seed the embed queue; `StatementEmbedWorker` repopulates the index | startup seeds the queue; worker repopulates in background |

Entity vectors are persisted at write time by the resolver (see §6) so the
common path is a pure redb read with zero embedder calls. The fallback fires
only for an entity whose vector row is absent. The statement embedder builds `subject + predicate
+ object` text; seeding the embed queue with every live statement lets the
existing worker reproduce those vectors without duplicating its text-assembly
logic.

### 2a. A runtime rebuild must read redb, not the arena

The two memory-HNSW rebuild sources are **not** interchangeable, and picking the
wrong one is silent data loss rather than an error.

The arena is a **boot-time image**: the live write path writes no vector into it
at all, and its sole vector writer is WAL replay during recovery. So at the
moment recovery runs, the arena is the freshly-replayed image of every durable
memory and is authoritative — which is why the boot fallback reads it.

From the first served encode onward it is stale by exactly the writes taken
since. A rebuild that read it at runtime would therefore publish a graph missing
**every memory written during this run**, with nothing failing to say so. Every
runtime rebuild — the maintenance worker's and the admin rebuild route's — reads
redb instead, which holds the durable write-time vector for the complete live
set.

## 3. Liveness & idempotency

- Tombstoned entities (`flags::TOMBSTONED`) and tombstoned statements are
  skipped — the rebuild reflects only live rows.
- Re-seeding the statement embed queue is idempotent: the worker skips any
  statement already present in the HNSW, and re-enqueuing upserts the same row.
- The memory HNSW rebuild folds the (empty-at-startup) pending buffer, so a
  stray insert during boot can't be lost.

## 4. Startup ordering

WAL replay ([`08.04 §2`](../08_storage/04_recovery.md) steps 1–5) restores the
arena + metadata first. The HNSW rebuilds (this file) are step 6, run before
the shard is marked ready ([`08.04 §8`](../08_storage/04_recovery.md)), so the
shard never serves with a half-populated semantic index. The statement HNSW is
the one exception: it repopulates asynchronously via the embed-queue worker, so
statement-scoped semantic search fills in shortly after the shard is ready.
Memory recall — the primary path — is fully available at ready.

## 5. Recovery cost

Memory HNSW: O(load) from a snapshot, or O(N·log N) graph build on the fallback
(see [`08.04 §8`](../08_storage/04_recovery.md) figures). Entity HNSW: N redb
reads + graph build, with an embedding inference only per vector-less row.
Statement HNSW: background, amortized over worker ticks, and off the critical
path to ready.

## 6. Write-time vector persistence

Entity vectors are written at resolve time into `ENTITY_VECTORS_TABLE`
(`EntityId → [f32; 384]`, 1536 bytes of raw little-endian floats), so the
entity index's startup is a redb read and a graph build with **zero embedder
calls**. Re-embedding the canonical name is the fallback for a row whose vector
is absent, which keeps a partial write from stalling the rebuild.

Statement vectors are not stored. The statement index restores by seeding the
embed queue and letting `StatementEmbedWorker` reproduce them from
`subject + predicate + object` text — the same path that produced them
originally, so there is no second text-assembly implementation to keep in
agreement. That is affordable precisely because the statement index is the one
index allowed to fill in after ready (§4).

## 7. Memory-HNSW graph snapshot persistence

`SharedHnsw::save_snapshot` / `load_snapshot` persist the full-precision HNSW
graph itself, so restart is O(load) for the memory index when a snapshot is
available. A snapshot is a directory containing three files at one shared
basename:

- `<basename>.hnsw.graph` — `hnsw_rs` graph dump.
- `<basename>.hnsw.data` — `hnsw_rs` data dump.
- `<basename>.brain` — the wrapper, written **last**, so its presence is the
  marker for "snapshot complete". Header CRC + footer BLAKE3 over
  wrapper-self; the wrapper body carries BLAKE3 hashes of both sibling files
  for cross-file integrity, and the id map and tombstone bitmap that
  interpret the graph. Any verification failure on load returns a clear error
  and the caller (recovery) falls back to the full arena rebuild from §2.

The snapshot binds a `taken_at_lsn` — the checkpoint's durable LSN — so recovery
knows the WAL position past which arena entries must be replayed into the
pending buffer to bring the loaded main forward.

**When the dump happens.** The snapshot worker takes one snapshot per cycle,
on `workers.snapshot_interval_sec` — 3600 s in code, 21600 s in the shipped
configs — and skips its first tick so a freshly spawned shard does not snapshot
an empty index. Taking it brackets a checkpoint and then lays out a bundle
directory, and the graph dump is part of the **bundle**: the checkpoint sequence
itself — CHECKPOINT_BEGIN, msync the arena, CHECKPOINT_END — contains no index
step. The triple is written after CHECKPOINT_END, beside the reflinked
`arena.bin`, `metadata.redb` and WAL segments.

**Writes are never paused for it.** There is no drain and no write stall
anywhere in the checkpoint or snapshot path: the sequence runs as a task on the
shard's own executor, and a write that interleaves with it lands above
`taken_at_lsn` and is replayed on restore. See
[08.05](../08_storage/05_checkpointing.md) §3a for why the durable watermark
makes a drain unnecessary, and §13 there for what the dump costs on a large
shard (~30-60 s for 10M memories) — the one part of a snapshot cycle that does
scale with memory count.

Recovery picks the most-recent valid snapshot under `<snapshots_root>/`. The
bundle's manifest deliberately omits the triple from the files it hashes: a
restored bundle rebuilds the index rather than trusting a graph out of it, so
the dump accelerates *this* shard's own restart and nothing else
([03_hnsw_lifecycle.md](03_hnsw_lifecycle.md) §9).

Entity-vector persistence (§6) is the analogous mechanism for the entity HNSW,
without the graph dump — the entity index rebuilds the graph from stored
vectors. A statement-vector / graph snapshot is a possible future
optimization; today's statement HNSW restores via the embed queue.
