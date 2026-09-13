# 09.06 Index Persistence & Recovery

The three per-shard HNSW indexes (memory, statement, entity) are **in-RAM
only**. They are *derived* state: every vector they hold is reconstructable
from durable source-of-truth (the arena, the redb metadata store). Brain does
**not** persist the HNSW graphs independently in v1; it rebuilds them at shard
startup. This file specifies how each index is rebuilt and the durability
contract that makes the rebuild lossless.

## 1. Why rebuild rather than persist

`hnsw_rs` builds an in-memory graph with no cheap incremental on-disk insert —
durability would require rewriting the whole graph dump on a cadence. Brain
keeps the source vectors durable instead (arena for memories; redb text for
statements/entities) and rebuilds the graphs on startup. This keeps the write
path cheap (one WAL append + one arena/redb write) and makes recovery a pure
function of durable state. See [`08.04 §8`](../08_storage/04_recovery.md).

## 2. The three indexes

| Index | Source of truth | Restore path | Trigger |
|---|---|---|---|
| Memory HNSW | snapshot (preferred) + arena (fallback) | `SharedHnsw::load_snapshot` from latest checkpoint; replay arena entries past `taken_at_lsn` into pending. On any snapshot failure, full rebuild from the arena. | startup, synchronous, before serving |
| Entity HNSW | `ENTITY_VECTORS_TABLE` (preferred) + `ENTITIES_TABLE.canonical_name` (fallback) | read stored vector for each live entity; re-embed canonical name when the row is absent | startup, synchronous, before serving |
| Statement HNSW | `STATEMENTS_TABLE` (text) | seed the embed queue; `StatementEmbedWorker` repopulates the index | startup seeds the queue; worker repopulates in background |

Entity vectors are persisted at write time by the resolver (see §6) so the
common path is a pure redb read with zero embedder calls. The fallback only
fires for pre-feature rows. The statement embedder builds `subject + predicate
+ object` text; seeding the embed queue with every live statement lets the
existing worker reproduce those vectors without duplicating its text-assembly
logic.

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

Memory HNSW: O(N·log N) graph build (see [`08.04 §8`](../08_storage/04_recovery.md)
figures). Entity HNSW: N embedding inferences + graph build. Statement HNSW:
background, amortized over worker ticks. For deployments where the
entity-rebuild embedding cost dominates startup, see §6.

## 6. (Future) write-time vector persistence

To make restart O(load) rather than O(re-embed), statement and entity embedding
vectors may be persisted at write time (redb tables `STATEMENT_VECTORS`,
`ENTITY_VECTORS`, keyed `StatementId | EntityId → [f32; D]` via bytemuck).
Startup then rebuilds the graph from stored vectors with **zero embedder
calls**. This is a write-path + schema change (a redb version bump); the
rebuild-from-text path in §2 is the baseline and remains the fallback when a
vector is absent (a row written before the feature, or a partial write).

## 7. Memory-HNSW graph snapshot persistence

`SharedHnsw::save_snapshot` / `load_snapshot` persist the PQ-HNSW graph itself
so restart is O(load) for the memory index when a snapshot is available.
A snapshot is a directory containing four files at one shared basename:

- `<basename>.hnsw.graph` — `hnsw_rs` graph dump.
- `<basename>.hnsw.data` — `hnsw_rs` data dump.
- `<basename>.codebook` — the PQ codebook bytes (forward-compatible with
  per-shard retraining; today's `bootstrap_codebook` is deterministic, but a
  future where shards retrain still works because the snapshot binds its own
  codebook).
- `<basename>.brain` — the wrapper, written **last**. Header CRC + footer
  BLAKE3 over wrapper-self; the wrapper body carries BLAKE3 hashes of the
  three sibling files for cross-file integrity. Any verification failure on
  load returns a clear error and the caller (recovery) falls back to the
  full arena rebuild from §2.

The snapshot binds a `taken_at_lsn` so recovery knows the WAL position past
which arena entries must be replayed into the pending buffer to bring the
loaded main forward to the latest durable LSN. The snapshot worker writes
snapshots at every checkpoint (see [08.05](../08_storage/05_checkpointing.md));
recovery picks the most-recent valid snapshot under `<snapshots_root>/`.

Entity-vector persistence (§6) is the analogous mechanism for the entity HNSW,
without the graph dump — the entity index rebuilds the graph from stored
vectors. A statement-vector / graph snapshot is a possible future
optimization; today's statement HNSW restores via the embed queue.
