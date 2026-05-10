# 12.01 The Shard Model

The internal model of a shard — what makes a shard a shard, and how shards relate to the rest of the substrate.

## 1. The shard's contents

A shard owns:

- **Storage files**:
  - `data/<shard>/arena.bin` — vector arena.
  - `data/<shard>/wal/segment-N.bin` — WAL segments.
  - `data/<shard>/metadata.redb` — metadata store.
  - Optional: `data/<shard>/hnsw_snapshot.bin` — HNSW snapshot.

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
data/
└── <shard_uuid>/
    ├── arena.bin
    ├── wal/
    │   ├── segment-00001.bin
    │   ├── segment-00002.bin
    │   └── ...
    ├── metadata.redb
    └── hnsw_snapshot.bin    (optional)
```

The directory name is the shard's UUID. Listing the data directory lists all shards.

## 3. The shard's identity

A shard has:

- **UUID** — a 16-byte random identifier, set at creation.
- **Logical ID** — a small integer for use in MemoryIds and routing tables.
- **Name** (optional) — a human-readable label.

The UUID is permanent. The logical ID is mostly permanent (changes during cluster reconfiguration).

## 4. The shard config

```toml
[shards.<uuid>]
logical_id = 0
data_dir = "/var/lib/brain/data/<uuid>"
arena_initial_size = "1GiB"
arena_growth = "exponential"
hnsw.M = 16
hnsw.ef_construction = 200

# ... per-shard tunables
```

Most settings can use global defaults. Per-shard overrides are for special cases.

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
- Up to 100K contexts per agent.

Beyond these, the shard should be split.

## 8. The shard split decision

Splitting a shard is rare and expensive. The substrate doesn't auto-split in v1.

When does an operator split?
- The shard's HNSW rebuild takes > 5 minutes.
- The arena's read latency is rising due to size.
- A single agent has grown too large for one shard.

Splitting in v1 is a manual, offline procedure (described later in [`06_rebalancing.md`](06_rebalancing.md)).

## 9. The shard's network endpoint

In single-node mode, shards are accessed via in-process method calls. No network.

In clustered mode, each shard has a network endpoint (host:port). Cross-shard calls go over the network using the wire protocol.

## 10. The "shards share a process" model

In single-node v1, all shards run in one substrate process:

- Process startup creates all configured shards.
- Each shard runs on its own thread (Glommio executor).
- Shards share the process's memory space (but not the actual data — each has its own).

If the process crashes, all shards crash. Recovery brings them all back up.

For HA, run multiple processes (each with a subset of shards) — but v1 doesn't have replication, so this is just for sharding, not redundancy.

## 11. The "shard owns its files" rule

A shard's files belong only to it. Other shards don't read or write them.

This invariant is enforced by the substrate's code: each shard's storage handles only access its own files.

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

Clean shutdown takes a few seconds. If the substrate is killed (SIGKILL), recovery on next startup uses the WAL.

## 14. The shard's metrics namespace

All metrics are tagged with the shard ID:

```
brain_memory_count{shard="<uuid>"} 12345
brain_request_latency_p99{shard="<uuid>", op="recall"} 0.012
```

Aggregation across shards is done at the metrics tooling layer (Prometheus, etc.).

## 15. The shard's logging

Each shard's log entries include the shard ID:

```
{
  "ts": "2026-05-07T12:00:00Z",
  "level": "info",
  "shard": "<uuid>",
  "operation": "encode",
  "memory_id": "...",
  "latency_ms": 8
}
```

For multi-shard deployments, logs from all shards interleave; the shard tag separates them.

## 16. The "shard is the state" framing

In Brain's architecture, "the substrate" is largely a thin layer that orchestrates shards. The actual durable state, the actual workers, the actual tasks — they're all per-shard.

The substrate process is a runtime that hosts shards. The shards are the actual cognitive substrate.

This framing helps with reasoning: most code paths are shard-scoped; cross-shard code is the exception.

---

*Continue to [`02_routing.md`](02_routing.md) for routing.*
