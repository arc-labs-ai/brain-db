# 16.02 Routing and Shard Assignment

> **TL;DR.** How Brain routes a request to the right shard (MemoryId-based bit extraction or AgentId hash) and how an agent is durably assigned to a shard at first contact (`hash(agent_id) % shard_count`, override table for VIPs, persistence so the assignment survives restart). The router is stateless and consults no peer node on the request path.

## Routing

## 1. The routing problem

For each request, Brain must determine which shard handles it.

For agent-scoped requests (most): route by agent_id.
For memory-scoped requests: route by memory_id (which encodes the shard).
For cross-shard requests: fan out.
For admin requests: targeted at a specific shard.

## 2. The routing table

Brain maintains a routing table:

```rust
struct RoutingTable {
    shard_count: usize,                              // Total shards
    shard_for_agent: HashMap<AgentId, ShardId>,      // Overrides
    shard_for_memory: fn(MemoryId) -> ShardId,       // Direct extraction
    default_hash_function: HashFunction,             // For agents not in overrides
}
```

The table is loaded at startup; updates require explicit triggers (configuration reload or cluster events).

## 3. Memory-based routing

A MemoryId encodes the runtime `shard_id` in specific bits:

```
MemoryId = (shard_id, slot_id, slot_version)
            ^^^^ 16 bits  ^^^^^ 64 bits  ^^^^^ 32 bits
```

To route by MemoryId:

```rust
fn shard_for_memory(memory_id: &MemoryId) -> ShardId {
    memory_id.shard_bits()    // Bit extraction
}
```

O(1), no lookup. Brain uses this for FORGET, LINK (to a known target), etc.

## 4. Agent-based routing

For an agent_id, the routing computes:

```rust
fn shard_for_agent(agent_id: &AgentId) -> ShardId {
    if let Some(&override_) = self.shard_for_agent.get(agent_id) {
        return override_;
    }
    let hash = blake3::hash(agent_id.bytes());
    let shard_idx = (hash.as_u64() % (self.shard_count as u64)) as ShardId;
    shard_idx
}
```

- Check the overrides map first (for VIP agents).
- Otherwise, `hash(agent_id) % shard_count`.

## 5. The hash choice

BLAKE3 hash:

- Cryptographically strong (good distribution).
- Fast (~2 GB/s on modern hardware).
- Deterministic across runs.

The hash is not security-critical here (it is not protecting against adversarial inputs); Brain uses BLAKE3 because it is already used for content-addressing elsewhere. Consistency.

## 6. The "shard count change" problem

When the shard count changes (operator adds shards), the modulo formula gives different shard assignments for many agents. Their data would need to migrate to new shards.

Brain doesn't support transparent shard count changes in v1. The shard count is fixed at deployment.

In a future major version, **consistent hashing** would minimize migration:
- Each shard owns a range of hash values.
- Adding a shard just moves a portion of one shard's range.
- Most agents' assignments are unchanged.

In v1, the simple modulo is sufficient because Brain does not change shard count.

## 7. The override map

The override map is for agents needing specific assignments:

```toml
[shards.routing.overrides]
"agent-uuid-VIP" = 0       # The VIP agent is on shard 0
"agent-uuid-XL" = 5        # An extra-large agent has its own shard
```

Overrides are checked before the hash. They give operators fine control without disrupting general routing.

## 8. Multi-shard agents

For agents whose data exceeds one shard, the routing splits:

```toml
[shards.routing.multi_shard]
"agent-uuid-XL" = [3, 4, 5]    # Spans shards 3, 4, 5
```

For these agents:
- ENCODE picks one of the assigned shards (round-robin or sticky-by-context).
- RECALL fans out to all assigned shards.

This is operator-configured; not auto-detected. A future version may add auto-spreading.

## 9. The "shard for memory, but the agent is multi-shard" case

When an existing memory is referenced (e.g., by FORGET memory_id), the MemoryId already encodes its shard. Multi-shard configuration doesn't change that.

For a multi-shard agent, the agent's recent encodes are on one shard, older ones on other shards (depending on when each was assigned). Memory operations always go to the encoded shard.

## 10. Routing for new agents

When an ENCODE arrives for an agent with no prior memories:

```
1. Check overrides; if found, use that shard.
2. Check multi-shard config; if found, pick one (round-robin).
3. Hash the agent_id.
4. Use the resulting shard.
```

The first ENCODE establishes which shard the agent uses (until reconfigured).

## 11. The router in single-node mode

In single-node, the router is a small in-memory data structure consulted by request handlers. ~100 ns per lookup. Very fast.

The router is stateless and doesn't need RPC; everything is in-process.

## 12. The router in clustered mode (future)

In clustered mode, the router maps the runtime `shard_id` to network addresses:

```rust
fn endpoint_for_shard(shard_id: ShardId) -> SocketAddr {
    self.endpoints.get(shard_id).unwrap()
}
```

Each node has an identical local copy of this table. Updates propagate via cluster gossip in a future version. The request path itself remains node-to-node-stateless.

## 13. The router cache

The router doesn't cache lookups — they're already O(1). No cache needed.

## 14. The "wrong shard" handling

If a request lands on the wrong shard (a stale routing table, or a misconfigured client):

- The shard recognizes the request isn't for it.
- Returns an error: `WrongShard { correct_shard: ShardId }`.
- Optionally, Brain proxies the request to the right shard.

Proxying isn't implemented in v1. Clients are expected to use the correct routing.

## 15. The client's role

Clients handle routing transparently:

- The client has the routing table.
- Each request is sent directly to the correct shard.
- If the routing table is stale, the client handles `WrongShard` errors and refreshes.

For most users, routing is invisible. The client abstracts it away.

## 16. The "all shards" fan-out

Some operations target all shards:

- `ADMIN_STATS` (gathers stats from all).
- Cross-agent queries (rare).

For these, Brain iterates the routing table and calls each shard:

```rust
async fn fan_out_admin_stats() -> Vec<ShardStats> {
    let futures = self.shard_count_iter().map(|shard| stats_for_shard(shard));
    futures::future::join_all(futures).await
}
```

In single-node, all shards are in-process; the calls are fast. In clustered mode, network latency adds up.

## 17. The routing's stability

Once a deployment is configured, routing is stable:

- Same agent → same shard (assuming overrides don't change).
- Same memory → same shard (encoded in the ID).

This stability matters for clients (caching) and for application correctness (no surprise migrations).

---

## Shard Assignment

How agents are durably assigned to shards — the mapping that determines which shard hosts which agent's data.

## 18. The assignment problem

Given a fixed set of shards and an arriving agent, which shard hosts the agent?

The assignment must be:

- **Deterministic** — given the same agent_id and same shard count, return the same shard.
- **Stable** — once assigned, the agent stays assigned until explicit reconfiguration.
- **Well-distributed** — agents spread roughly evenly across shards.

## 19. The default: hash-based

The default assignment uses BLAKE3:

```rust
fn shard_for_agent(agent_id: AgentId, shard_count: usize) -> ShardId {
    let hash = blake3::hash(agent_id.as_bytes());
    let value = u64::from_le_bytes(hash.as_bytes()[0..8].try_into().unwrap());
    (value % shard_count as u64) as ShardId
}
```

For UUIDv7 agent IDs, this gives roughly uniform distribution. With 16 shards and 1000 agents, each shard gets ~62-63 agents.

## 20. The override map

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

## 21. The multi-shard case

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

## 22. The persistent assignment record

For agents whose assignment shouldn't change (even if shard count changes), the assignment is persisted:

```
agents table:
  agent_id → ShardId
```

This is checked at agent creation. Once recorded, it's the source of truth.

For deployments that don't need this stability, the hash-based dynamic assignment is sufficient. Persistent assignment is opt-in.

## 23. The "first-encode wins" semantics

When an agent's first ENCODE arrives:

```
1. Compute the candidate shard (hash or override).
2. Record the assignment in the agents table on that shard.
3. Process the encode.
```

Subsequent encodes for the same agent route to the recorded shard, regardless of hash changes.

## 24. The shard-count-change scenario

If the operator changes shard count (very rare in v1):

- New agents (no recorded assignment) use the new hash → new shard.
- Existing agents (with records) stay where they are.
- Distribution may become uneven (since old agents are on old shards).

A future enhancement: rebalance to redistribute. v1 doesn't auto-rebalance.

## 25. The agent's "primary" shard

For multi-shard agents, one shard is "primary":

- Holds the agent's metadata (config, quotas).
- Initially gets new encodes (until full).
- Coordinates fan-out queries.

Other shards are "extras" — they hold overflow data.

The primary is set at first-encode; it doesn't change unless the operator forcibly migrates.

## 26. The assignment metadata

Per-agent metadata in the `agents` table (see [10. Metadata + Graph Store](../10_metadata/03_substrate_tables.md)):

```rust
struct AgentMetadata {
    agent_id: AgentId,
    primary_shard: ShardId,
    extra_shards: Vec<ShardId>,
    created_at: Timestamp,
    quota_memories: Option<u64>,
    quota_contexts: Option<u32>,
    config_overrides: Option<AgentConfig>,
}
```

This is the durable record of the assignment.

## 27. The "wrong shard" detection

If a request arrives at the wrong shard for an agent:

- The shard checks the agents table; finds no record (the agent isn't here).
- Returns a `WrongShard` error with the correct shard's ID.
- The client retries on the correct shard.

This handles routing-table staleness gracefully. The client refreshes its routing and retries.

## 28. The "agent doesn't exist" case

For ENCODE on an unknown agent:

- The shard's agents table doesn't have the agent.
- Brain creates an agent record (using the hash to determine the shard).
- The encode proceeds.

For RECALL/FORGET/etc. on an unknown agent:

- Returns `AgentNotFound`.
- No data exists for the agent.

## 29. The "delete agent" operation

`ADMIN_AGENT_DELETE <agent_id>` removes an agent and all its data:

- Tombstones all the agent's memories.
- Removes the agent's metadata.
- Removes context records.
- Schedules edge cleanup.

This is irreversible. Brain logs the operation for audit.

## 30. The "transfer agent" operation (future)

In a future major version, an admin could transfer an agent between shards:

- Mark the agent for transfer.
- Copy its data to the new shard.
- Update the assignment record.
- Tombstone the old data.

Not implemented in v1. Operators can simulate via export-import.

## 31. The reseeding scenario

If the agents table is lost or corrupted, Brain can reseed:

- Iterate all memories on the shard.
- Reconstruct the agent metadata from memory rows (memories carry agent_id).
- Rebuild the agents table.

This is a recovery path, not a routine operation.

## 32. The cross-shard agent uniqueness

An agent's ID must be unique across Brain. Brain doesn't enforce this rigorously — there's no global registry of agent IDs.

If two clients use the same agent_id, they'll write to the same shard (same hash). Their writes mix.

For operators wanting strict uniqueness, the application layer must enforce it. Brain trusts the agent_id.

## 33. The "tenancy" pattern

A common pattern: each tenant is an "agent" in Brain's terminology.

- Tenant A's data is one agent's data.
- Tenant B's data is another's.
- Agents' data are isolated (separate shards or separate ranges of memory).

Brain's agent isolation enforces tenant separation. With proper authentication (each client can only access its own agent's data), tenants don't see each other.

## 34. The auto-spread (future)

When an agent grows to dominate its shard, Brain could auto-spread:

- Detect the agent is using > 50% of its shard's resources.
- Move some of its data to a less-busy shard.
- Update the multi-shard config.

Not in v1. Operators do this manually if needed.

---

*Continue to [`03_single_node.md`](03_single_node.md) for single-node deployment.*
