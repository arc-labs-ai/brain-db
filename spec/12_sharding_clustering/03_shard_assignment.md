# 12.03 Shard Assignment

How agents are assigned to shards — the durable mapping that determines which shard hosts which agent's data.

## 1. The assignment problem

Given a fixed set of shards and an arriving agent, which shard hosts the agent?

The assignment must be:

- **Deterministic** — given the same agent_id and same shard count, return the same shard.
- **Stable** — once assigned, the agent stays assigned until explicit reconfiguration.
- **Well-distributed** — agents spread roughly evenly across shards.

## 2. The default: hash-based

The default assignment uses BLAKE3:

```rust
fn shard_for_agent(agent_id: AgentId, shard_count: usize) -> ShardLogicalId {
    let hash = blake3::hash(agent_id.as_bytes());
    let value = u64::from_le_bytes(hash.as_bytes()[0..8].try_into().unwrap());
    (value % shard_count as u64) as ShardLogicalId
}
```

For UUID v4 agent IDs (random), this gives roughly uniform distribution. With 16 shards and 1000 agents, each shard gets ~62-63 agents.

## 3. The override map

For specific agents, operators can override the default:

```toml
[shards.routing.overrides]
"agent-vip-001" = 0      # VIP agent on dedicated shard 0
"agent-large-002" = 5    # Large agent on shard with most capacity
```

Overrides are checked first; the hash is the fallback.

Override use cases:
- VIP/dedicated capacity for premium tenants.
- Co-locating related agents on the same shard.
- Routing test agents to a specific shard.

## 4. The multi-shard case

For agents needing more capacity than one shard:

```toml
[shards.routing.multi_shard]
"agent-huge-003" = { shards = [3, 4, 5], strategy = "round_robin" }
```

For multi-shard agents:

- ENCODE picks one shard per the strategy.
- RECALL fans out to all assigned shards.
- The agent's data is split.

Strategies:
- `round_robin`: cycle through shards.
- `sticky_by_context`: each context lives on one shard (chosen via context_id hash).
- `weighted`: based on shard load (advanced; not in v1).

## 5. The persistent assignment record

For agents whose assignment shouldn't change (even if shard count changes), the assignment is persisted:

```
agents table:
  agent_id → ShardLogicalId
```

This is checked at agent creation. Once recorded, it's the source of truth.

For deployments that don't need this stability, the hash-based dynamic assignment is sufficient. Persistent assignment is opt-in.

## 6. The "first-encode wins" semantics

When an agent's first ENCODE arrives:

```
1. Compute the candidate shard (hash or override).
2. Record the assignment in the agents table on that shard.
3. Process the encode.
```

Subsequent encodes for the same agent route to the recorded shard, regardless of hash changes.

## 7. The shard-count-change scenario

If the operator changes shard count (very rare in v1):

- New agents (no recorded assignment) use the new hash → new shard.
- Existing agents (with records) stay where they are.
- Distribution may become uneven (since old agents are on old shards).

A future enhancement: rebalance to redistribute. v1 doesn't auto-rebalance.

## 8. The agent's "primary" shard

For multi-shard agents, one shard is "primary":

- Holds the agent's metadata (config, quotas).
- Initially gets new encodes (until full).
- Coordinates fan-out queries.

Other shards are "extras" — they hold overflow data.

The primary is set at first-encode; it doesn't change unless the operator forcibly migrates.

## 9. The assignment metadata

Per-agent metadata in the `agents` table ([07.03 Memory Table](../07_metadata_graph/03_memory_table.md) §1.7):

```rust
struct AgentMetadata {
    agent_id: AgentId,
    primary_shard: ShardLogicalId,
    extra_shards: Vec<ShardLogicalId>,
    created_at: Timestamp,
    quota_memories: Option<u64>,
    quota_contexts: Option<u32>,
    config_overrides: Option<AgentConfig>,
}
```

This is the durable record of the assignment.

## 10. The "wrong shard" detection

If a request arrives at the wrong shard for an agent:

- The shard checks the agents table; finds no record (the agent isn't here).
- Returns a `WrongShard` error with the correct shard's ID.
- The client (or SDK) retries on the correct shard.

This handles routing-table staleness gracefully. The client refreshes its routing and retries.

## 11. The "agent doesn't exist" case

For ENCODE on an unknown agent:

- The shard's agents table doesn't have the agent.
- The substrate creates an agent record (using the hash to determine the shard).
- The encode proceeds.

For RECALL/FORGET/etc. on an unknown agent:

- Returns `AgentNotFound`.
- No data exists for the agent.

## 12. The "delete agent" operation

`ADMIN_AGENT_DELETE <agent_id>` removes an agent and all its data:

- Tombstones all the agent's memories.
- Removes the agent's metadata.
- Removes context records.
- Schedules edge cleanup.

This is irreversible. The substrate logs the operation for audit.

## 13. The "transfer agent" operation (future)

In v2, an admin could transfer an agent between shards:

- Mark the agent for transfer.
- Copy its data to the new shard.
- Update the assignment record.
- Tombstone the old data.

Not implemented in v1. Operators can simulate via export-import.

## 14. The reseeding scenario

If the agents table is lost or corrupted, the substrate can reseed:

- Iterate all memories on the shard.
- Reconstruct the agent metadata from memory rows (memories carry agent_id).
- Rebuild the agents table.

This is a recovery path, not a routine operation.

## 15. The cross-shard agent uniqueness

An agent's ID must be unique across the substrate. The substrate doesn't enforce this rigorously — there's no global registry of agent IDs.

If two clients use the same agent_id, they'll write to the same shard (same hash). Their writes mix.

For operators wanting strict uniqueness, the application layer must enforce it. The substrate trusts the agent_id.

## 16. The "tenancy" pattern

A common pattern: each tenant is an "agent" in Brain's terminology.

- Tenant A's data is one agent's data.
- Tenant B's data is another's.
- Agents' data are isolated (separate shards or separate ranges of memory).

Brain's agent isolation enforces tenant separation. With proper authentication (each client can only access its own agent's data), tenants don't see each other.

## 17. The auto-spread (future)

When an agent grows to dominate its shard, the substrate could auto-spread:

- Detect the agent is using > 50% of its shard's resources.
- Move some of its data to a less-busy shard.
- Update the multi-shard config.

Not in v1. Operators do this manually if needed.

---

*Continue to [`04_single_node.md`](04_single_node.md) for single-node deployment.*
