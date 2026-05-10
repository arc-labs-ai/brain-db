# 06.06 HNSW Persistence

The HNSW index is **not persisted** as a primary on-disk structure. It's rebuilt at startup from the arena and metadata. This file specifies the rebuild path and an optional persistence mechanism for fast restart.

## 1. Why no primary persistence

The HNSW graph is derived state — given the vectors (in the arena) and which memories are active (in the metadata), the graph can be rebuilt deterministically.

The substrate could persist the HNSW for fast startup. We don't, by default, because:

- **The graph is large** — ~150 MB per million memories.
- **The graph is fragile** — a partial write or corruption corrupts the entire structure; partial recovery is hard.
- **The rebuild is fast enough** — 5–30 seconds for 1M memories with parallel insertion.
- **The arena and metadata are the source of truth** — persisting derived state risks divergence.

Restart cost is the main downside. For deployments where minute-scale restart is acceptable, no-persistence is simpler. For deployments where every second matters, the optional persistence (§ 5) helps.

## 2. The rebuild procedure

At startup, after WAL replay completes ([05.08](../05_storage_arena_wal/08_recovery.md)):

```
1. Initialize an empty HNSW with configured parameters.
2. Iterate over all active memories in the metadata store.
3. For each memory:
   a. Read the vector from the arena.
   b. Insert into HNSW.
   c. Update id maps.
4. The HNSW is now consistent with the arena and metadata.
5. The shard is marked ready.
```

The iteration is parallelized:
- The metadata store is read sequentially (it's a B-tree; sequential read is fast).
- Vectors are batched and inserted in parallel batches.

For 1M memories on commodity hardware:
- Single-threaded: ~30 seconds.
- 4-thread: ~10 seconds.
- 16-thread: ~5 seconds.

The substrate uses up to `nproc` parallelism by default; configurable via `[ann] rebuild_threads`.

## 3. Active memories only

Only active (non-tombstoned) memories are inserted into the rebuilt HNSW. Tombstoned memories are skipped — the rebuild is also a "compaction" in the sense that it strips out tombstones.

## 4. Memory order

The order of insertion during rebuild affects HNSW quality slightly. The substrate uses metadata-store order, which is roughly insertion order (B-tree is keyed by MemoryId, and MemoryIds are roughly time-ordered via UUIDv7).

For pathological inputs (e.g., memories that lie exactly on a 1D manifold), insertion order matters more. For typical workloads, the order is effectively random.

## 5. Optional fast-restart persistence

For deployments wanting faster restart, the substrate optionally writes a serialized HNSW to disk during checkpointing. This is the **HNSW snapshot**.

### 5.1 Procedure

During checkpointing ([05.09](../05_storage_arena_wal/09_checkpointing.md)):

1. Pause writes briefly (the checkpoint drain).
2. Serialize the HNSW state to a file: `data/<shard>/hnsw_snapshot.bin`.
3. Include the durable LSN at which the snapshot was taken.
4. Resume writes.

The HNSW snapshot file format:

```
[header: 64 bytes]
  magic: "BHN0"  (Brain HNSW v0)
  format_version: u32
  shard_uuid: [u8; 16]
  taken_at_lsn: u64
  graph_size: u64
  parameters: { M, ef_construction }
  header_crc32c: u32
[graph data: serialized via hnsw_rs's built-in serialization]
[id_map data: serialized HashMaps]
[footer: 8 bytes — full-file BLAKE3 hash truncated to u64]
```

### 5.2 Restore

At startup, if a snapshot exists and is valid:

1. Read the header; verify magic, version, shard_uuid, CRC.
2. Deserialize the graph and id maps.
3. Replay WAL records since `taken_at_lsn` (these add memories that came after the snapshot).
4. The HNSW is now current.

For a snapshot covering 1M memories plus 1000 post-snapshot WAL records:
- Snapshot deserialize: ~1-2 seconds.
- WAL replay for HNSW: ~1 second.
- Total: ~3 seconds.

Compared to ~5 seconds rebuild from scratch, the gain is modest at this size. For larger shards (10M+) or slower hardware, the gain is more meaningful.

### 5.3 Failure modes

If the snapshot is corrupted (CRC fails, deserialize errors), the substrate falls back to full rebuild. No data loss; just slower startup.

If the snapshot is older than the metadata (some checkpointing failure), the substrate detects this via LSN comparison and rebuilds rather than using a stale snapshot.

## 6. The choice between persistence options

| Option | Restart time | Disk overhead | Complexity |
|---|---|---|---|
| No persistence (default) | 5-30 s for 1M | 0 | Lowest |
| Periodic snapshot | 3-5 s for 1M | ~150 MB per snapshot | Medium |

For most deployments, no persistence is fine. For large shards or restart-sensitive deployments, snapshots help.

The configuration knob:

```
[ann.persistence]
mode = "rebuild"     # or "snapshot"
snapshot_interval = "10m"
```

## 7. The metadata-only rebuild

A subtle case: what if the arena is fine but the metadata store is restored from an older backup?

- The metadata says memory M exists.
- The arena slot for M is the right vector (assuming arena wasn't restored).
- HNSW rebuild finds M in metadata, reads its vector from the arena, inserts it.

Result: the rebuilt HNSW is consistent with both the arena and metadata. No issue.

The opposite case (arena restored but metadata current) is more problematic — the metadata might reference vectors that aren't in the older arena. The substrate detects this during rebuild and logs warnings. Affected memories are skipped.

## 8. Recovery integration

HNSW rebuild happens after WAL replay during startup recovery:

```
1. Open metadata store.
2. Open arena.
3. Replay WAL.
4. (Optional: deserialize HNSW snapshot if present and valid.)
5. Rebuild HNSW from active memories (or replay-from-snapshot path).
6. Mark shard ready.
```

Steps 1-5 are sequential within a shard. Across shards, they happen in parallel.

## 9. Snapshot vs full backup

The HNSW snapshot is a fast-restart artifact, not a backup. A backup of the shard ([05.10 Snapshots](../05_storage_arena_wal/10_snapshots.md)) doesn't need to include the HNSW snapshot; the arena and metadata are sufficient to reconstruct everything.

If a backup includes the HNSW snapshot, restore can use it for faster shard ready time. If not, rebuild from arena + metadata.

## 10. Cross-version persistence

The HNSW snapshot's format version protects against incompatible loads. If the substrate is upgraded and the snapshot's format is older, the substrate falls back to rebuild.

This means a substrate upgrade can mean a slower first restart (for the rebuild) but no data loss.

## 11. The "warm" rebuild

For large shards where rebuild takes a meaningful fraction of a minute, the substrate could expose a "warm" path: respond to read queries against the partially-built index (with degraded recall) while rebuild completes.

This is not implemented in v1. The shard isn't marked ready until rebuild completes; queries return `ShardNotReady` until then.

A future enhancement (open question, [`11_open_questions.md`](11_open_questions.md)): partial-readiness, where the shard accepts queries during rebuild with a "best effort" recall caveat.

---

*Continue to [`07_maintenance.md`](07_maintenance.md) for the maintenance worker.*
