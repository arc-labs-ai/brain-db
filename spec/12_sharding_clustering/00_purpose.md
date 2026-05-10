# 12.00 Purpose

This document specifies how Brain partitions data across shards and how (in future versions) shards distribute across cluster nodes.

## What this document covers

- The shard as Brain's unit of partitioning.
- How agents are assigned to shards.
- How requests are routed.
- Single-node deployment models.
- Clustered deployment models (future).
- Rebalancing and replication concerns.

## What this document does not cover

- **The intra-shard storage details.** Defined in [05–07. Storage layers].
- **The wire protocol for cross-shard calls.** Defined in [03. Wire Protocol](../03_wire_protocol/).
- **Operational concerns (deployment, monitoring).** Defined in [14. Observability + Operations](../14_observability_ops/).

## 1. The shard as the unit

Brain's primary scaling lever is **sharding** — splitting data into independent partitions, each handled by its own resources.

A shard:

- Has its own arena, WAL, metadata store, HNSW.
- Has its own writer task, executor, and workers.
- Operates independently of other shards.

This makes shards good failure boundaries (one shard's issues don't cascade), good performance boundaries (each scales independently), and good evolution boundaries (different shards can use different versions, in principle).

## 2. The "everything is per-shard" rule

Within Brain, almost everything is per-shard:

- Identifiers (MemoryId, ContextId) are per-shard (the shard is encoded in the high bits).
- Storage files are per-shard.
- Metrics are per-shard (with aggregation at higher levels).
- Workers are per-shard.

The exceptions are global concerns:
- Cluster topology (in distributed mode).
- Authentication/authorization.
- Configuration loading.

## 3. The single-node case (v1)

In v1, Brain runs as a single process on a single machine. Multiple shards on that one machine share resources:

- Each shard pinned to a CPU core.
- Each shard's storage files on the same filesystem.
- Cross-shard "calls" are direct method calls (no network).

This is the only deployment mode supported in v1.

## 4. The clustered case (v2+)

In future, Brain may run as a cluster:

- Multiple processes on multiple machines.
- Shards distributed across machines.
- Cross-shard calls over the network.
- Replication for high availability.

V1's design accommodates this future direction (the wire protocol is network-ready, etc.) but doesn't implement it.

## 5. The agent-shard mapping

In the simplest case, all of an agent's data is on one shard. The shard hosts the agent.

For very large agents (millions of memories), data may span multiple shards. The agent's data is split.

The mapping (agent → shard) is deterministic — given the agent's ID, the substrate computes which shard owns it.

## 6. Why shard-per-core

With Glommio's thread-per-core model, the natural unit of capacity is one CPU core. One shard per core gives:

- One executor per core.
- One writer task per core.
- One set of workers per core.

For an N-core machine, N shards. For 16 cores: 16 shards.

This matches the substrate's concurrency model and is the recommended configuration.

## 7. The shard count calibration

How many shards to provision?

- Too few: less parallelism, larger per-shard data, more rebuild cost.
- Too many: more overhead per request (routing, fan-out), smaller per-shard data.

For typical deployments: one shard per core. Brain's architecture is designed around this ratio.

For very large workloads (millions of agents), more shards may be needed; for small ones, fewer.

## 8. Cross-shard operations

Some operations need data from multiple shards:

- A RECALL for an agent whose data spans shards.
- A query that mixes data from multiple agents (rare).

These fan out:
- Each shard processes its sub-query.
- Results are merged.

## 9. The "shard is independent" guarantee

Shards are independent units:
- They can fail without affecting others.
- They can be backed up independently.
- They can be migrated (with care) to other nodes.

This independence simplifies operations.

## 10. The shard ID

A shard has a UUID — a 16-byte random identifier set at shard creation.

Shards also have a "logical" ID — a small integer (0, 1, 2, ...) used for routing tables. The logical ID maps to the UUID via configuration.

## 11. The agent ID

An agent has an AgentId — also a UUID. Routing maps AgentId to shard logical ID.

## 12. The "router" entity

The substrate has a router component:

```rust
trait Router {
    fn shard_for_agent(&self, agent_id: AgentId) -> ShardLogicalId;
    fn shard_for_memory(&self, memory_id: MemoryId) -> ShardLogicalId;
}
```

The router is consulted on every request. It returns the shard responsible for the data.

For single-node deployments, the router is in-process. For clustered, it's distributed (with caching).

## 13. The MemoryId carries shard info

A MemoryId encodes the shard logical ID in some bits ([02.03 Identifiers](../02_data_model/03_identifiers.md)). So `shard_for_memory` is just bit extraction:

```rust
fn shard_for_memory(memory_id: &MemoryId) -> ShardLogicalId {
    extract_shard_bits(memory_id)
}
```

This is O(1), no router lookup needed.

## 14. The agent-to-shard hash

For agents, the mapping uses a hash:

```rust
fn shard_for_agent(agent_id: AgentId) -> ShardLogicalId {
    let hash = blake3::hash(&agent_id.bytes());
    let shard_idx = (hash.as_u64() % shard_count) as ShardLogicalId;
    shard_idx
}
```

Simple, deterministic, well-distributed.

For deployments wanting specific agents on specific shards (e.g., a VIP agent on a dedicated shard), the substrate supports overrides via configuration.

## 15. The "shard is a logical concept" framing

A shard is:
- A set of files (arena, WAL, metadata).
- A set of in-memory state (HNSW, caches).
- A set of running tasks (executor, writer, workers).

In single-node deployment, multiple shards live in one process. They're separated by code-level isolation, not OS-level.

In clustered deployment, shards may live in different processes on different machines. The isolation is stronger.

## 16. The deployment lifecycle

A shard's lifecycle:

- **Created** when the substrate is initialized (or via `ADMIN_SHARD_CREATE`).
- **Active** during normal operation.
- **Maybe migrated** between nodes (in clustered mode).
- **Maybe split** if it grows too large.
- **Deleted** when no longer needed.

Most shards live for the lifetime of the substrate.

---

*Continue to [`01_shard_model.md`](01_shard_model.md) for the shard model.*
