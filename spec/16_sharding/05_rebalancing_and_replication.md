# 16.05 Rebalancing and Replication

> **TL;DR.** Future-territory: how shards move between nodes (rebalancing) and how shards replicate for HA (replication). Both are future-major-version features sketched here so v1's design accommodates them. v1 ships single-node, manual rebalance, no replication — a node loss in v1 means its agents are unavailable until snapshot-restore.

## Rebalancing

How shards move between nodes (in clustered mode) or are split/merged. Mostly future-major-version territory; v1's rebalancing is manual offline procedures.

## 1. The need to rebalance

Reasons:

- A shard has grown too large for its node (more memory than available).
- A node has too many shards (overloaded).
- A node is being decommissioned.
- New nodes are added and need work.
- Hot spots (one shard handles disproportionate load).

In v1, these are addressed by operator action (manual splitting, manual node migration). In a future major version, Brain may automate.

## 2. Shard split (offline procedure for v1)

Splitting a shard means dividing its data into two new shards.

The procedure:

```
1. Quiesce the shard (no new writes).
2. Pick a split point (e.g., a `space_id` range).
3. Create two new shard directories.
4. For each memory:
   a. Determine which new shard it belongs to.
   b. Copy its data (arena entry, metadata row, edges).
5. Verify counts match.
6. Update the routing config to point to the new shards.
7. Resume operations.
8. Delete the old shard.
```

This is a long offline procedure (minutes to hours depending on data size). v1 only.

## 3. The split's challenges

- Edges may span the split (memory A on shard X has an edge to memory B that ends up on shard Y). After the split, the edge is cross-shard, which Brain doesn't natively support.
- Need to handle in-flight requests.
- Keeping the WAL accurate during split.

For v1, splits are rare and operator-supervised. A future major version may automate with care.

## 4. Shard merge

The reverse: combining two shards into one. Less common; usually done after split-by-mistake.

The procedure mirrors split.

## 5. Shard migration (clustered, future)

Moving a shard from one node to another:

```
1. Mark the shard for migration in the control plane.
2. Snapshot the shard's data.
3. Stream the snapshot to the destination node.
4. Apply the snapshot at the destination.
5. Replay any WAL records that occurred during transfer.
6. Atomic switch: update membership table; clients now route to the new node.
7. Old node removes the shard's files.
```

During migration, writes might need to be:
- Held until the migration completes (downtime).
- Or applied to both source and destination (synced).

A future major version will likely use the synced approach for zero-downtime migration.

## 6. The downtime tradeoff

- **Hold writes**: simpler but causes user-visible downtime (10s of seconds to minutes).
- **Sync writes**: complex but no downtime; needs cross-node coordination.

For the initial clustered release, holding writes is acceptable for shard migrations (rare events). Sync writes are a later enhancement.

## 7. The auto-rebalance algorithm (sketch for a future version)

```
periodically:
  read per-node load metrics
  if std-dev of node loads > threshold:
    pick the most loaded node N_high and least loaded node N_low
    pick a shard on N_high (e.g., one that's not too big)
    schedule migration: shard → N_low
```

Many design decisions deferred (which shard to pick, how to avoid thrashing, etc.). Future-major-version territory.

## 8. The capacity-based placement

When a new shard is created:

- Check the load of all nodes.
- Place on the least-loaded node.
- Update membership.

For new clusters: distribute evenly. For growing clusters: bias toward less-loaded nodes.

## 9. The "node added" workflow (future)

When an operator adds a new node:

```
1. The node joins the cluster (registers with control plane).
2. The control plane assigns existing shards to balance load.
3. Shards migrate over time (one at a time, to avoid mass disruption).
4. Eventually, the new node has its share of work.
```

The migration could take hours to complete for large clusters. Throttling avoids overwhelming network or disk.

## 10. The "node removed" workflow (future)

When an operator decommissions a node:

```
1. Mark the node for removal in the control plane.
2. Migrate the node's shards to other nodes.
3. Once all shards are off, the node can be safely shut down.
4. Remove from membership.
```

If the node is dead (not graceful removal), its shards are unrecoverable without replicas. Replication is what makes node failure recoverable.

## 11. The "shard at capacity" alert

Brain monitors per-shard size:

- Memory count.
- Arena bytes used.
- HNSW node count.

If a shard approaches limits, an alert fires. The operator can:

- Move some agents to other shards (override map updates).
- Split the shard (offline).
- Add more capacity (more nodes / shards).

## 12. The "hot agent" problem

Sometimes a single agent generates disproportionate load on one shard:

- Many memories (storage pressure).
- Many requests (CPU pressure).

Solutions:

- Multi-shard for that agent (operator config).
- Move the agent to a less-loaded shard.
- Add more shards (and re-shard).

Brain exposes per-space load metrics for operators to identify hot agents.

## 13. The migration ordering

For multi-step rebalances (e.g., several shards moving):

- One at a time (safest, slowest).
- Pipelined (parallel; risk of overwhelming network).

V1 doesn't have automated rebalances; a future major version's policy is to be conservative (one at a time) initially.

## 14. The rebalance lock

To prevent concurrent rebalances:

- Only one rebalance operation at a time per cluster.
- Tracked in the control plane.
- Operator can manually unlock if needed (in case of stuck state).

## 15. The rebalance audit

Each rebalance is logged:

```
{
  event: "shard_migration",
  shard_id: ...,
  from_node: ...,
  to_node: ...,
  duration_sec: ...,
  bytes_transferred: ...,
}
```

Operators review these to understand what's happening.

## 16. The "no rebalance needed" common case

For most deployments, after initial setup, rebalancing is rarely needed:

- Workload distribution is stable.
- Agents grow at similar rates.
- Hardware doesn't change frequently.

So even in a future major version, rebalancing is an occasional operation, not a constant background activity.

## 17. The "manual override" preference

Brain's design preference: operators have manual override capability for everything automated.

Auto-rebalance is opt-in. Manual rebalance is always available. This protects against bugs in the auto-rebalance logic.

---

## Replication

How shards are replicated across nodes for high availability. Replication is **deferred from v1**: each shard has a single replica (the primary itself), so the loss of a node means its agents are unavailable until snapshot restore. The design below sketches the intended approach for a future major version.

## 18. The motivation

Without replication:

- Node failure = shard outage until manual recovery.
- Disk failure = data loss (modulo backups).
- Maintenance windows require accepting downtime or external HA mechanisms.

Replication addresses these by maintaining multiple copies of each shard.

## 19. The model

Each shard has:

- One **primary** — handles writes.
- One or more **replicas** — track the primary's state.

```
shard_id_0:
  primary: node_a
  replicas: [node_b, node_c]
```

Writes go to the primary. The primary replicates to replicas. Reads can go to primary (strong consistency) or replicas (eventual consistency, lower latency).

## 20. The replication protocols (options)

Several models:

### 20.1 Synchronous replication

The primary acks the write only after all replicas have it.

- Strong consistency.
- High write latency (waits for slowest replica).
- Replica failure during write blocks the write.

### 20.2 Asynchronous replication

The primary acks after writing locally; replicates to replicas in the background.

- Low write latency.
- Risk of data loss on primary failure (replicas may not have the latest writes).
- Eventual consistency on replicas.

### 20.3 Quorum

The primary acks after a majority (e.g., 2 of 3) have the write.

- Balance: tolerates one replica failure without blocking; survives node failure without data loss.
- Used by Raft, Cassandra (with tunable consistency), etc.

## 21. The recommended choice for a future major version

**Quorum with 3-replica configuration**:

- Per-shard primary plus 2 replicas.
- Writes ack after 2 of 3 confirm.
- Tolerates one node failure with no data loss and continued availability.

This matches industry standards (Cassandra, Spanner, etc.) and provides a reasonable cost/benefit.

## 22. The wire-protocol level

Replication uses Brain's existing wire protocol:

```
WAL_REPLICATE: a frame from primary to replica with a WAL record
WAL_REPLICATE_ACK: replica acks
```

The replica applies the WAL record locally, just as recovery would.

This is a "log shipping" model. Replicas are essentially always-on recovery: they replay WAL records as they arrive.

## 23. The replica's role

A replica:

- Accepts incoming WAL records from the primary.
- Applies them in order.
- Tracks its applied LSN (so primary can monitor lag).
- Serves read queries (with eventual-consistency semantics).

Replicas don't accept writes from clients directly. All writes go through the primary.

## 24. Failover

When the primary fails:

- The cluster's membership service detects (heartbeat timeout).
- Promotion: pick a replica with the most recent LSN; make it the new primary.
- Clients are redirected.

The promotion process takes ~5-30 seconds (depends on detection time and propagation).

## 25. The "split-brain" prevention

Without care, a network partition could create two primaries (split brain). To prevent:

- Use a consensus protocol (Raft, Paxos) for promotion decisions.
- Only one primary can exist for a shard at a time.

This requires the control plane to be itself replicated (Raft cluster of 3-5 nodes).

## 26. Replica reads

Reads from replicas are eventual-consistency:

- Replica may be lag behind the primary.
- Lag is typically milliseconds (for healthy replication).
- Can be minutes during heavy load or network issues.

For read-after-write semantics, reads must go to the primary (or a replica known to be caught up).

## 27. The "read-from-replica" option

Clients can opt to read from replicas (or "any node") for lower latency:

```
recall.read_consistency = ReadConsistency::Local    // From any node
recall.read_consistency = ReadConsistency::Strong   // Primary only
```

Default: depends on the operation. RECALL might default to local; ENCODE always primary.

## 28. The replication cost

Per write:

- Primary's local write: ~0.5 ms.
- Network to replica: ~0.1-1 ms.
- Replica's local apply: ~0.5 ms.
- Total replicated latency: ~1-2 ms.

For sync replication: total latency is max(primary local, network+replica). For quorum: similar.

## 29. The replication lag

A replica's lag is `primary_lsn - replica_lsn`. Healthy replicas have lag < 100 ms.

Lag rises when:

- Replica is slower than primary (under-provisioned).
- Network is slow or congested.
- Replica is doing maintenance (rebuilding HNSW, etc.).

The cluster monitors lag; high lag triggers alerts.

## 30. The "lagging replica" handling

If a replica falls too far behind (e.g., > 30 sec), the primary may:

- Stop sending it WAL records (to avoid further fall-behind).
- Mark it as "out of sync".
- Trigger a re-sync (full snapshot transfer + WAL catch-up).

## 31. The "new replica" bootstrap

When adding a replica:

1. Take a snapshot of the primary.
2. Transfer the snapshot to the new replica.
3. Apply the snapshot.
4. Begin streaming WAL records from the post-snapshot LSN.
5. Once caught up, the replica joins as fully active.

Bootstrap takes minutes to hours, depending on data size.

## 32. The "geographic replication"

Replicas may be in different data centers / regions. This adds latency:

- Cross-DC: ~10-50 ms.
- Cross-region: ~50-200 ms.

For sync replication across regions, write latency is dominated by the slowest region. For async, primary writes are fast but replicas can lag.

## 33. The "config" surface

```toml
[replication]
enabled = false              # v1 default; v1 only supports a single replica per shard (the primary).
mode = "quorum"              # quorum, sync, async
replicas_per_shard = 2       # plus the primary = 3 total

[replication.placement]
strategy = "spread"          # Spread replicas across nodes / racks / DCs
```

V1 has `enabled = false` only. A future major version enables actual replication.

## 34. The simple alternative (for v1)

If full Brain-managed replication is too complex, an alternative is available in v1:

- Use external block-level replication (DRBD, cloud-vendor replicated volumes).
- Brain sees a single durable disk.
- HA via the storage layer, not Brain.

This keeps Brain simpler at the cost of some flexibility (the granularity is whole-disk, not per-shard).

V1 deployments wanting HA can use this approach today.

---

*Continue to [`06_failure_modes.md`](06_failure_modes.md) for failure modes.*
