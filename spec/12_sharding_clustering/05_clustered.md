# 12.05 Clustered Deployment (Future)

A sketch of clustered deployment for v2+. Not implemented in v1.

## 1. The motivation

Clustering addresses:

- **Horizontal scale** — workloads that exceed a single machine's capacity.
- **High availability** — surviving machine failures.
- **Geographic distribution** — placing shards near their users.

For workloads up to single-machine capacity, single-node v1 is sufficient.

## 2. The model

```
┌──────────────┐  ┌──────────────┐  ┌──────────────┐
│ Node A       │  │ Node B       │  │ Node C       │
│  Shards 0-3  │  │  Shards 4-7  │  │  Shards 8-11 │
└──────┬───────┘  └──────┬───────┘  └──────┬───────┘
       │                 │                 │
       └─────────────────┴─────────────────┘
                         │
                  Cluster Membership
                  + Routing Table
```

Multiple nodes; each hosts some shards. The substrate's protocol coordinates them.

## 3. The shard placement

A shard's home node is recorded in the cluster's membership state:

```
shard_assignments:
  shard_logical_id_0 → node_a
  shard_logical_id_1 → node_a
  ...
  shard_logical_id_8 → node_c
```

Updates happen during membership changes (node added/removed, shard migrated).

## 4. The routing in clustered mode

A request arrives at any node:

```
1. Determine the shard (via routing rules).
2. Look up the shard's home node.
3. If local: process directly.
4. If remote: forward to the home node.
```

Each node has a local copy of the cluster's membership table. Updates propagate via gossip.

## 5. The cluster membership

Implemented via a consensus protocol (Raft or similar):

- A small set of "control plane" nodes form a Raft cluster.
- They store the cluster's configuration (membership, shard assignments, etc.).
- Other nodes (data plane) read from the control plane.

For small clusters (3-5 nodes), the control plane and data plane can be the same nodes. For larger clusters, separation makes sense.

## 6. The cross-node call

Cross-node calls use Brain's wire protocol over the network:

```rust
async fn forward_to(node: NodeId, frame: WireFrame) -> Result<WireFrame> {
    let conn = self.connection_pool.get(node).await?;
    conn.send_frame(frame).await?;
    conn.recv_frame().await
}
```

Connection pooling, retries, timeouts handled at this layer.

## 7. Latency considerations

Cross-node call latency: ~0.1-1 ms intra-datacenter, ~1-50 ms cross-region.

For agent-scoped operations (most), a single shard handles them; the cross-node hop only happens once per request.

For multi-shard operations, multiple cross-node hops; can become noticeable.

## 8. The data plane vs control plane

```
Data plane:
  - Shard processes
  - Connection layer
  - Cross-node call dispatch

Control plane:
  - Cluster membership
  - Shard placement decisions
  - Health monitoring
  - Rebalancing triggers
```

The data plane handles requests at line rate. The control plane is consulted occasionally (on membership changes).

## 9. Failure handling

Node failures:

- Other nodes detect via failed gossip / heartbeats.
- Control plane updates membership.
- Affected shards are unavailable until remediated.

Without replication (next file), a failed node's shards are offline. Recovery requires either restarting that node or restoring shards on another node.

## 10. Replication (sketch)

For HA, each shard could be replicated to multiple nodes:

```
shard_logical_id_0:
  primary: node_a
  replicas: [node_b, node_c]
```

Writes go to the primary; replicated to replicas. On primary failure, a replica is promoted.

The replication protocol (synchronous? asynchronous? quorum?) is a major design decision deferred to v2.

## 11. The "thin v2" path

A minimal v2 might:

- Add cross-node dispatch (no replication).
- Add basic membership (manually configured, no auto-discovery).
- Otherwise keep v1's mechanics.

This gives horizontal scale without HA. Some deployments may want HA via external means (filesystem replication, etc.) and this thin v2 is enough.

## 12. The "full v2" path

A more ambitious v2:

- Replication with configurable consistency (sync, async, quorum).
- Auto-failover.
- Auto-rebalancing.
- Cluster expansion without downtime.

This is significantly more engineering. Deferred until concrete demand.

## 13. The wire protocol's role

Brain's wire protocol (defined in [03. Wire Protocol](../03_wire_protocol/)) is used for both client-to-substrate and (in v2) substrate-to-substrate.

The protocol's framing, opcodes, and error handling apply uniformly. Cross-node calls are just network calls of the same protocol.

## 14. The SDK in clustered mode

Client SDKs handle the cluster:

- The SDK has a list of bootstrap nodes.
- Connects to one; learns the membership.
- For each request, sends to the appropriate node (based on routing).
- Handles node failures by reconnecting and rerouting.

This is similar to existing distributed-database clients (Cassandra, MongoDB).

## 15. The "single-node deployment as a degenerate cluster" framing

A single-node deployment is logically a cluster of one. The single-node code is a special case of clustered code where:

- All shards are on the same node.
- Cross-node dispatch is a no-op (it's local).

This framing helps with v2 design: the protocols and abstractions are shared; only the network adds complexity.

## 16. The deferred decisions

Aspects deferred to v2:

- Consistency model across nodes (strong? eventual?).
- Cross-shard transactions (likely "no", same as v1).
- Schema migration coordination across nodes.
- Backup/restore semantics across cluster.
- Multi-DC operation.

Each of these requires careful design. v2 will tackle them as concrete needs emerge.

## 17. The v1 limitation

If a v1 user's workload exceeds a single machine, options are:

- Run multiple independent v1 substrates with manual sharding at the application layer.
- Wait for v2.
- Use a different system.

Brain doesn't pretend to be a distributed database in v1. The roadmap is honest about this.

---

*Continue to [`06_rebalancing.md`](06_rebalancing.md) for rebalancing.*
