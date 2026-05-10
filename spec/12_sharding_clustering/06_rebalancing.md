# 12.06 Rebalancing

How shards move between nodes (in clustered mode) or are split/merged. Mostly v2 territory; v1's rebalancing is manual offline procedures.

## 1. The need to rebalance

Reasons:

- A shard has grown too large for its node (more memory than available).
- A node has too many shards (overloaded).
- A node is being decommissioned.
- New nodes are added and need work.
- Hot spots (one shard handles disproportionate load).

In v1, these are addressed by operator action (manual splitting, manual node migration). In v2, the substrate may automate.

## 2. Shard split (offline procedure for v1)

Splitting a shard means dividing its data into two new shards.

The procedure:

```
1. Quiesce the shard (no new writes).
2. Pick a split point (e.g., agent-id range).
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

For v1, splits are rare and operator-supervised. v2 may automate with care.

## 4. Shard merge

The reverse: combining two shards into one. Less common; usually done after split-by-mistake.

The procedure mirrors split.

## 5. Shard migration (clustered, v2)

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

V2 will likely use the synced approach for zero-downtime migration.

## 6. The downtime tradeoff

- **Hold writes**: simpler but causes user-visible downtime (10s of seconds to minutes).
- **Sync writes**: complex but no downtime; needs cross-node coordination.

For initial v2, holding writes is acceptable for shard migrations (rare events). Sync writes is a v2.x enhancement.

## 7. The auto-rebalance algorithm (sketch for v2)

```
periodically:
  read per-node load metrics
  if std-dev of node loads > threshold:
    pick the most loaded node N_high and least loaded node N_low
    pick a shard on N_high (e.g., one that's not too big)
    schedule migration: shard → N_low
```

Many design decisions deferred (which shard to pick, how to avoid thrashing, etc.). V2 territory.

## 8. The capacity-based placement

When a new shard is created:

- Check the load of all nodes.
- Place on the least-loaded node.
- Update membership.

For new clusters: distribute evenly. For growing clusters: bias toward less-loaded nodes.

## 9. The "node added" workflow (v2)

When an operator adds a new node:

```
1. The node joins the cluster (registers with control plane).
2. The control plane assigns existing shards to balance load.
3. Shards migrate over time (one at a time, to avoid mass disruption).
4. Eventually, the new node has its share of work.
```

The migration could take hours to complete for large clusters. Throttling avoids overwhelming network or disk.

## 10. The "node removed" workflow (v2)

When an operator decommissions a node:

```
1. Mark the node for removal in the control plane.
2. Migrate the node's shards to other nodes.
3. Once all shards are off, the node can be safely shut down.
4. Remove from membership.
```

If the node is dead (not graceful removal), its shards are unrecoverable without replicas. Replication is what makes node failure recoverable.

## 11. The "shard at capacity" alert

The substrate monitors per-shard size:

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

The substrate exposes per-agent load metrics for operators to identify hot agents.

## 13. The migration ordering

For multi-step rebalances (e.g., several shards moving):

- One at a time (safest, slowest).
- Pipelined (parallel; risk of overwhelming network).

V1 doesn't have automated rebalances; v2's policy is to be conservative (one at a time) initially.

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

So even in v2, rebalancing is an occasional operation, not a constant background activity.

## 17. The "manual override" preference

Brain's design preference: operators have manual override capability for everything automated.

Auto-rebalance is opt-in. Manual rebalance is always available. This protects against bugs in the auto-rebalance logic.

---

*Continue to [`07_replication.md`](07_replication.md) for replication.*
