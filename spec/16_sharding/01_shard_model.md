# 16.01 The Shard Model

The internal model of a shard — what makes a shard a shard, and how shards relate to the rest of Brain.

## 1. The shard's contents

A shard owns:

- **Storage files** (names are centralized in
  `crates/brain-storage/src/layout.rs:36-71`, which is the single source of
  truth for production paths):
  - `shard.uuid` — 16 bytes holding the shard's stable UUIDv7.
  - `arena.bin` — vector arena.
  - `wal/` — WAL segments, named `seg-XXXXXXXXXX.wal`.
  - `metadata.redb` — substrate and typed-graph tables.
  - `entity.hnsw`, `statement.hnsw` — persisted typed-graph indexes.
  - `statements.tantivy/`, `memory_text.tantivy/` — BM25 indexes.
  - `llm_cache.redb` — the extractor and HyPE response cache.

- **In-memory state**:
  - The arena's mmap.
  - The HNSW index.
  - Caches (cue cache, etc.).
  - The writer task's pending state.

- **Running tasks**:
  - The Glommio executor.
  - The writer task.
  - Background workers.
  - Connection handlers (if connections are accepted directly).

## 2. The shard's directory layout

```
<data_dir>/
├── layout.json                 # shard_count the directory was created with
├── api_keys.redb               # process-wide, not per-shard
└── <shard_id>/                 # "0", "1", ... — the runtime integer id
    ├── shard.uuid
    ├── arena.bin
    ├── wal/
    │   ├── seg-0000000001.wal
    │   └── ...
    ├── metadata.redb
    ├── entity.hnsw             # plus .hnsw.graph / .hnsw.data siblings
    ├── statement.hnsw
    ├── statements.tantivy/
    ├── memory_text.tantivy/
    └── llm_cache.redb
```

The directory name is the **runtime `shard_id`**, not the UUID: the path is
`cfg.data_dir.join(shard_id.to_string())`
(`crates/brain-server/src/shard/mod.rs:2819`). The UUID lives inside the
directory, in `shard.uuid`.

That is deliberate. A `MemoryId` encodes its `shard_id`, so an id can be
resolved to a directory by arithmetic alone, with no index from UUID to path to
consult first. It is also why `shard_count` cannot change under an existing
directory, and why the count is recorded in `layout.json` and checked on every
open (`crates/brain-server/src/storage_layout.rs:19-47`): a directory served
with a different count routes every id at a shard that does not hold it, with
the data intact and unreachable and nothing to say so.

Listing the data directory lists all shards.

Each file is created by its owning module on first use — HNSW on first insert,
tantivy on first index write, redb on open — so a young shard's directory is
legitimately sparse.

## 3. The shard's identity

A shard has:

- **`shard_uuid`** — a UUIDv7, set at creation; durably stored with the shard's data.
- **`shard_id`** — a 16-bit runtime integer (0..`shard_count-1`) encoded in MemoryIds and used as a routing-table key.
- **Name** (optional) — a human-readable label.

The `shard_uuid` is permanent. The runtime `shard_id` is mostly permanent (it changes only during cluster reconfiguration).

## 4. The shard config

There is no per-shard config section. Shard tunables are global and apply to
every shard the server runs
(`crates/brain-server/src/config/mod.rs:243-315`):

```toml
[storage]
data_dir = "/var/lib/brain/data"
# Omit to size to the machine: available_parallelism() - 1, one shard per
# core with one core held back for the network and admin runtimes.
shard_count = 4

[shard]
arena_capacity_bytes = "1GiB"
wal_segment_size_bytes = "64MiB"
wal_retention_segments = 16

[hnsw]
m = 16
ef_construction = 200
ef_search = 64
```

`data_dir` is the **root**; each shard's directory is derived from its id
beneath it, so there is nothing per-shard to point anywhere.

Per-shard divergence is not a deferred feature so much as a thing the design
rules out: shards are interchangeable by construction, which is what lets a
space hash to any of them. A shard with a different arena size or a different
`m` would make the choice of shard a performance decision, and the hash does
not make performance decisions.

Two caveats on the keys above, both enforced:

- Every config struct is `#[serde(deny_unknown_fields)]`. A misspelled or
  invented key is a boot failure with a parse error, not a warning.
- `arena_capacity_bytes` and `wal_segment_size_bytes` are human byte sizes
  (`"1GiB"`, `"64MiB"`), parsed by `deserialize_human_bytes`; `KiB`/`MiB`/`GiB`/
  `TiB` are powers of two and `KB`/`MB`/`GB` are powers of ten.

## 5. The shard's lifecycle

```
1. Provisioned: directory created, config registered.
2. Initialized: empty arena, empty WAL, empty metadata.
3. Open: tasks running, accepting requests.
4. Maybe migrated: state moved to another node (clustered).
5. Maybe split: contents divided across multiple new shards.
6. Closed: tasks stopped, files closed.
7. Deleted: files removed.
```

A typical shard goes 1→2→3→6 (no migration, no split). 

## 6. The shard's resource budget

Per shard, on commodity hardware:

- 1 CPU core.
- 4-32 GB RAM (depending on data size).
- 50-500 GB disk.

These are guidelines. For very large shards (10M+ memories), more resources are needed.

## 7. The shard's capacity limits

A v1 shard's targets:

- Up to 10M memories.
- Up to 100M edges.
- Up to 100K agents (rare; typically 1-100).
- Up to 100K sessions per space.

Beyond these, the shard should be split.

## 8. The shard split decision

Splitting a shard is rare and expensive. Brain doesn't auto-split in v1.

When does an operator split?
- The shard's HNSW rebuild takes > 5 minutes.
- The arena's read latency is rising due to size.
- A single agent has grown too large for one shard.

Splitting in v1 is a manual, offline procedure (described later in [`05_rebalancing_and_replication.md`](05_rebalancing_and_replication.md)).

## 9. The shard's network endpoint

In single-node mode, shards are accessed via in-process method calls. No network.

In clustered mode, each shard has a network endpoint (host:port). Cross-shard calls go over the network using the wire protocol.

## 10. The "shards share a process" model

In single-node v1, all shards run in one Brain process:

- Process startup creates all configured shards.
- Each shard runs on its own thread (Glommio executor).
- Shards share the process's memory space (but not the actual data — each has its own).

If the process crashes, all shards crash. Recovery brings them all back up.

For HA, run multiple processes (each with a subset of shards) — but v1 doesn't have replication, so this is just for sharding, not redundancy.

## 11. The "shard owns its files" rule

A shard's files belong only to it. Other shards don't read or write them.

This invariant is enforced in code: each shard's storage handles only access its own files.

For backup tools, this means a single shard's files can be copied independently — they're a self-contained unit.

## 12. The shard's startup sequence

```
1. Open the data directory; verify ownership.
2. Open the WAL; identify last LSN.
3. Open the metadata store.
4. Open the arena.
5. Replay WAL records from last checkpoint.
6. Build (or restore) the HNSW.
7. Start the writer task.
8. Start workers.
9. Begin accepting requests.
```

Per shard, ~10-30 seconds startup time depending on data size.

For multi-shard deployments, all shards start in parallel (each on its own thread); total startup time is dominated by the slowest shard.

## 13. The shard's shutdown sequence

```
1. Stop accepting new requests.
2. Drain in-flight requests (with timeout).
3. Stop workers (let them complete current cycles).
4. Stop the writer task (after final commit).
5. Trigger a checkpoint (so recovery is fast next time).
6. Close storage files.
```

Clean shutdown takes a few seconds. If Brain is killed (SIGKILL), recovery on next startup uses the WAL.

## 14. The shard's metrics namespace

All metrics are tagged with the shard's UUID:

```
brain_arena_slots_used{shard="<shard_uuid>"} 12345
brain_request_duration_ms_bucket{shard="<shard_uuid>",op="recall",le="25"} 9876
```

Aggregation across shards is done at the metrics tooling layer (Prometheus, etc.).

## 15. The shard's logging

Each shard's log entries include the shard's UUID:

```
{
  "ts": "2026-05-07T12:00:00Z",
  "level": "info",
  "shard": "<shard_uuid>",
  "operation": "encode",
  "memory_id": "...",
  "latency_ms": 8
}
```

For multi-shard deployments, logs from all shards interleave; the shard tag separates them.

## 16. The "shard is the state" framing

In Brain's architecture, the Brain process is largely a thin runtime that orchestrates shards. The actual durable state, the actual workers, the actual tasks — they're all per-shard.

This framing helps with reasoning: most code paths are shard-scoped; cross-shard code is the exception.

---

*Continue to [`02_routing.md`](02_routing.md) for routing.*
