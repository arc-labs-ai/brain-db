# 16. Sharding & Clustering

> **TL;DR.** The shard is Brain's unit of partitioning and parallelism. Each shard owns an arena, WAL, metadata store, HNSW, writer task, executor, and workers, and pins to one CPU core. Agents map to shards by `hash(agent_id) % shard_count`; MemoryIds carry a 16-bit runtime `shard_id` in the high bits so `shard_for_memory` is bit extraction; storage records the durable `shard_uuid` (UUIDv7). v1 is single-node, multi-shard; the wire protocol and identifiers leave room for clustered deployment with networked cross-shard calls in a future major version.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Operators; cluster-mode implementers |
| Voice | Hybrid (rationale + normative) |
| Depends on | [01. System Architecture](../01_architecture/00_purpose.md), [08. Storage](../08_storage/00_purpose.md), [14. Concurrency](../14_concurrency/00_purpose.md) |
| Referenced by | [06. Client Interface](../06_sdk/00_purpose.md), [17. Observability](../17_observability/00_purpose.md) |

## What this spec defines

How Brain partitions data across shards and (in future versions) across nodes in a cluster. The single-node sharding model is fully specified for v1; clustered deployments are sketched and tracked as future work.

## What this document covers

- The shard as Brain's unit of partitioning.
- How agents are assigned to shards.
- How requests are routed.
- Single-node deployment models.
- Clustered deployment models (future).
- Rebalancing and replication concerns.

## What this document does not cover

- **The intra-shard storage details.** Defined in [08. Storage](../08_storage/00_purpose.md), [09. Indexing](../09_indexing/00_purpose.md), and [10. Metadata + Graph Store](../10_metadata/00_purpose.md).
- **The wire protocol for cross-shard calls.** Defined in [04. Wire Protocol](../04_wire_protocol/00_purpose.md).
- **Operational concerns (deployment, monitoring).** Defined in [17. Observability](../17_observability/00_purpose.md).

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

## 4. The clustered case (future)

In a future major version, Brain may run as a cluster:

- Multiple processes on multiple machines.
- Shards distributed across machines.
- Cross-shard calls over the network.
- Replication for high availability.

V1's design accommodates this future direction (the wire protocol is network-ready, etc.) but doesn't implement it.

## 5. The agent-shard mapping

In the simplest case, all of an agent's data is on one shard. The shard hosts the agent.

For very large agents (millions of memories), data may span multiple shards. The agent's data is split.

The mapping (agent → shard) is deterministic — given the agent's ID, Brain computes which shard owns it.

## 6. Why shard-per-core

With Glommio's thread-per-core model, the natural unit of capacity is one CPU core. One shard per core gives:

- One executor per core.
- One writer task per core.
- One set of workers per core.

For an N-core machine, N shards. For 16 cores: 16 shards.

This matches Brain's concurrency model and is the recommended configuration.

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

A shard has a `shard_uuid` — a UUIDv7 set at shard creation, durably stored alongside the shard's data.

Shards also have a 16-bit runtime `shard_id` — a small integer (0, 1, ..., up to `shard_count - 1`) used for routing tables and encoded in the high bits of `MemoryId`. The runtime `shard_id` maps to the `shard_uuid` via configuration.

## 11. The agent ID

An agent has an AgentId — also a UUID. Routing maps AgentId to the runtime `shard_id`.

## 12. The "router" entity

Brain has a stateless router component:

```rust
trait Router {
    fn shard_for_agent(&self, agent_id: AgentId) -> ShardId;
    fn shard_for_memory(&self, memory_id: MemoryId) -> ShardId;
}
```

The router is consulted on every request. It returns the shard responsible for the data. The router holds no state beyond the shard count and override map; nodes do not communicate on the request path to make routing decisions.

For single-node deployments, the router is in-process. For clustered deployments (future), each node carries an identical local copy.

## 13. The MemoryId carries shard info

A MemoryId encodes the 16-bit runtime `shard_id` in its high bits (see [02. Data Model: Memory](../02_data_model/02_memory.md)). So `shard_for_memory` is just bit extraction:

```rust
fn shard_for_memory(memory_id: &MemoryId) -> ShardId {
    extract_shard_bits(memory_id)
}
```

This is O(1), no router lookup needed.

## 14. The agent-to-shard hash

For agents, the mapping uses a hash:

```rust
fn shard_for_agent(agent_id: AgentId) -> ShardId {
    let hash = blake3::hash(&agent_id.bytes());
    let shard_idx = (hash.as_u64() % shard_count) as ShardId;
    shard_idx
}
```

Simple, deterministic, well-distributed.

For deployments wanting specific agents on specific shards (e.g., a VIP agent on a dedicated shard), Brain supports overrides via configuration.

## 15. The "shard is a logical concept" framing

A shard is:
- A set of files (arena, WAL, metadata).
- A set of in-memory state (HNSW, caches).
- A set of running tasks (executor, writer, workers).

In single-node deployment, multiple shards live in one process. They're separated by code-level isolation, not OS-level.

In clustered deployment, shards may live in different processes on different machines. The isolation is stronger.

## 16. The deployment lifecycle

A shard's lifecycle:

- **Created** when Brain is initialized (or via `ADMIN_SHARD_CREATE`).
- **Active** during normal operation.
- **Maybe migrated** between nodes (in clustered mode).
- **Maybe split** if it grows too large.
- **Deleted** when no longer needed.

Most shards live for the lifetime of Brain.

---

*Continue to [`01_shard_model.md`](01_shard_model.md) for the shard model.*
