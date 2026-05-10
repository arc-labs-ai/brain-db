# 12.02 Routing

How Brain routes requests to the appropriate shard.

## 1. The routing problem

For each request, the substrate must determine which shard handles it.

For agent-scoped requests (most): route by agent_id.
For memory-scoped requests: route by memory_id (which encodes the shard).
For cross-shard requests: fan out.
For admin requests: targeted at a specific shard.

## 2. The routing table

The substrate maintains a routing table:

```rust
struct RoutingTable {
    shard_count: usize,                    // Total shards
    shard_for_agent: HashMap<AgentId, ShardLogicalId>,    // Overrides
    shard_for_memory: fn(MemoryId) -> ShardLogicalId,     // Direct extraction
    default_hash_function: HashFunction,    // For agents not in overrides
}
```

The table is loaded at startup; updates require explicit triggers (configuration reload or cluster events).

## 3. Memory-based routing

A MemoryId encodes the shard logical ID in specific bits:

```
MemoryId = (shard_logical_id, slot_id, slot_version)
              ^^^^ 16 bits     ^^^^^ 64 bits  ^^^^^ 32 bits
```

To route by MemoryId:

```rust
fn shard_for_memory(memory_id: &MemoryId) -> ShardLogicalId {
    memory_id.shard_bits()    // Bit extraction
}
```

O(1), no lookup. The substrate uses this for FORGET, LINK (to a known target), etc.

## 4. Agent-based routing

For an agent_id, the routing computes:

```rust
fn shard_for_agent(agent_id: &AgentId) -> ShardLogicalId {
    if let Some(&override_) = self.shard_for_agent.get(agent_id) {
        return override_;
    }
    let hash = blake3::hash(agent_id.bytes());
    let shard_idx = (hash.as_u64() % (self.shard_count as u64)) as ShardLogicalId;
    shard_idx
}
```

- Check the overrides map first (for VIP agents).
- Otherwise, hash the agent_id and modulo by shard count.

## 5. The hash choice

BLAKE3 hash:

- Cryptographically strong (good distribution).
- Fast (~2 GB/s on modern hardware).
- Deterministic across runs.

The hash isn't security-critical here (we're not protecting against adversarial inputs); we use BLAKE3 because it's already used for content-addressing elsewhere in Brain. Consistency.

## 6. The "shard count change" problem

When the shard count changes (operator adds shards), the modulo formula gives different shard assignments for many agents. Their data would need to migrate to new shards.

Brain doesn't support transparent shard count changes in v1. The shard count is fixed at deployment.

For v2, **consistent hashing** would minimize migration:
- Each shard owns a range of hash values.
- Adding a shard just moves a portion of one shard's range.
- Most agents' assignments are unchanged.

In v1, the simple modulo is sufficient because we don't change shard count.

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

This is operator-configured; not auto-detected. v2 may add auto-spreading.

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

The router doesn't need RPC; everything is in-process.

## 12. The router in clustered mode (future)

In clustered mode, the router maps shard logical IDs to network addresses:

```rust
fn endpoint_for_shard(shard_logical_id: ShardLogicalId) -> SocketAddr {
    self.endpoints.get(shard_logical_id).unwrap()
}
```

Each node has a local copy of this table. Updates propagate via cluster gossip (in v2).

## 13. The router cache

The router doesn't cache lookups — they're already O(1). No cache needed.

## 14. The "wrong shard" handling

If a request lands on the wrong shard (a stale routing table, or a misconfigured client):

- The shard recognizes the request isn't for it.
- Returns an error: `WrongShard { correct_shard: ShardLogicalId }`.
- Optionally, the substrate proxies the request to the right shard.

Proxying isn't implemented in v1. Clients are expected to use the correct routing.

## 15. The SDK's role

Client SDKs handle routing transparently:

- The SDK has the routing table.
- Each request is sent directly to the correct shard.
- If the routing table is stale, the SDK handles `WrongShard` errors and refreshes.

For most users, routing is invisible. The SDK abstracts it away.

## 16. The "all shards" fan-out

Some operations target all shards:

- `ADMIN_STATS` (gathers stats from all).
- Cross-agent queries (rare).

For these, the substrate iterates the routing table and calls each shard:

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

This stability matters for client SDKs (caching) and for application correctness (no surprise migrations).

---

*Continue to [`03_shard_assignment.md`](03_shard_assignment.md) for shard assignment details.*
