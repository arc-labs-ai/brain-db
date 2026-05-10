# 12.07 Replication (Future)

How shards are replicated across nodes for high availability. V2 territory; v1 has no replication.

## 1. The motivation

Without replication:

- Node failure = shard outage until manual recovery.
- Disk failure = data loss (modulo backups).
- Maintenance windows require accepting downtime or external HA mechanisms.

Replication addresses these by maintaining multiple copies of each shard.

## 2. The model

Each shard has:

- One **primary** — handles writes.
- One or more **replicas** — track the primary's state.

```
shard_logical_id_0:
  primary: node_a
  replicas: [node_b, node_c]
```

Writes go to the primary. The primary replicates to replicas. Reads can go to primary (strong consistency) or replicas (eventual consistency, lower latency).

## 3. The replication protocols (options)

Several models:

### 3.1 Synchronous replication

The primary acks the write only after all replicas have it.

- Strong consistency.
- High write latency (waits for slowest replica).
- Replica failure during write blocks the write.

### 3.2 Asynchronous replication

The primary acks after writing locally; replicates to replicas in the background.

- Low write latency.
- Risk of data loss on primary failure (replicas may not have the latest writes).
- Eventual consistency on replicas.

### 3.3 Quorum

The primary acks after a majority (e.g., 2 of 3) have the write.

- Balance: tolerates one replica failure without blocking; survives node failure without data loss.
- Used by Raft, Cassandra (with tunable consistency), etc.

## 4. The recommended choice for v2

**Quorum with 3-replica configuration**:

- Per-shard primary plus 2 replicas.
- Writes ack after 2 of 3 confirm.
- Tolerates one node failure with no data loss and continued availability.

This matches industry standards (Cassandra, Spanner, etc.) and provides a reasonable cost/benefit.

## 5. The wire-protocol level

Replication uses Brain's existing wire protocol:

```
WAL_REPLICATE: a frame from primary to replica with a WAL record
WAL_REPLICATE_ACK: replica acks
```

The replica applies the WAL record locally, just as recovery would.

This is a "log shipping" model. Replicas are essentially always-on recovery: they replay WAL records as they arrive.

## 6. The replica's role

A replica:

- Accepts incoming WAL records from the primary.
- Applies them in order.
- Tracks its applied LSN (so primary can monitor lag).
- Serves read queries (with eventual-consistency semantics).

Replicas don't accept writes from clients directly. All writes go through the primary.

## 7. Failover

When the primary fails:

- The cluster's membership service detects (heartbeat timeout).
- Promotion: pick a replica with the most recent LSN; make it the new primary.
- Clients are redirected.

The promotion process takes ~5-30 seconds (depends on detection time and propagation).

## 8. The "split-brain" prevention

Without care, a network partition could create two primaries (split brain). To prevent:

- Use a consensus protocol (Raft, Paxos) for promotion decisions.
- Only one primary can exist for a shard at a time.

This requires the control plane to be itself replicated (Raft cluster of 3-5 nodes).

## 9. Replica reads

Reads from replicas are eventual-consistency:

- Replica may be lag behind the primary.
- Lag is typically milliseconds (for healthy replication).
- Can be minutes during heavy load or network issues.

For read-after-write semantics, reads must go to the primary (or a replica known to be caught up).

## 10. The "read-from-replica" option

Clients can opt to read from replicas (or "any node") for lower latency:

```
recall.read_consistency = ReadConsistency::Local    // From any node
recall.read_consistency = ReadConsistency::Strong   // Primary only
```

Default: depends on the operation. RECALL might default to local; ENCODE always primary.

## 11. The replication cost

Per write:

- Primary's local write: ~0.5 ms.
- Network to replica: ~0.1-1 ms.
- Replica's local apply: ~0.5 ms.
- Total replicated latency: ~1-2 ms.

For sync replication: total latency is max(primary local, network+replica). For quorum: similar.

## 12. The replication lag

A replica's lag is `primary_lsn - replica_lsn`. Healthy replicas have lag < 100 ms.

Lag rises when:

- Replica is slower than primary (under-provisioned).
- Network is slow or congested.
- Replica is doing maintenance (rebuilding HNSW, etc.).

The cluster monitors lag; high lag triggers alerts.

## 13. The "lagging replica" handling

If a replica falls too far behind (e.g., > 30 sec), the primary may:

- Stop sending it WAL records (to avoid further fall-behind).
- Mark it as "out of sync".
- Trigger a re-sync (full snapshot transfer + WAL catch-up).

## 14. The "new replica" bootstrap

When adding a replica:

1. Take a snapshot of the primary.
2. Transfer the snapshot to the new replica.
3. Apply the snapshot.
4. Begin streaming WAL records from the post-snapshot LSN.
5. Once caught up, the replica joins as fully active.

Bootstrap takes minutes to hours, depending on data size.

## 15. The "geographic replication"

Replicas may be in different data centers / regions. This adds latency:

- Cross-DC: ~10-50 ms.
- Cross-region: ~50-200 ms.

For sync replication across regions, write latency is dominated by the slowest region. For async, primary writes are fast but replicas can lag.

## 16. The "config" surface

```toml
[replication]
enabled = false              # v1 default
mode = "quorum"              # quorum, sync, async
replicas_per_shard = 2       # plus the primary = 3 total

[replication.placement]
strategy = "spread"          # Spread replicas across nodes / racks / DCs
```

V1 has `enabled = false` only. V2 enables actual replication.

## 17. The simple alternative (for v1+)

If v2-style replication is too complex, an alternative:

- Use external block-level replication (DRBD, cloud-vendor replicated volumes).
- The substrate sees a single durable disk.
- HA via the storage layer, not the substrate.

This keeps Brain simpler at the cost of some flexibility (the granularity is whole-disk, not per-shard).

V1 deployments wanting HA can use this approach today.

---

*Continue to [`08_failure_modes.md`](08_failure_modes.md) for failure modes.*
