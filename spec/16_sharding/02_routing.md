# 16.02 Routing and Shard Assignment

> **TL;DR.** How Brain routes a request to the right shard (MemoryId-based bit extraction or SpaceId hash) and how a space is durably assigned to a shard at first contact (`hash(space_id) % shard_count`, override table for VIPs, persistence so the assignment survives restart). The router is stateless and consults no peer node on the request path.

## Routing

## 1. The routing problem

For each request, Brain must determine which shard handles it.

For space-scoped requests (most): route by space_id.
For memory-scoped requests: route by memory_id (which encodes the shard).
For cross-shard requests: fan out.
For admin requests: targeted at a specific shard.

## 2. The routing table

Brain maintains a routing table:

```rust
struct RoutingTable {
    shard_count: u16,                       // total shards; must be >= 1
    overrides: HashMap<SpaceId, ShardId>,   // startup-time pins for VIP / extra-large spaces
}
```

The table is **immutable after construction** and validated there: a `shard_count` of `0` is rejected, and so is any override whose target shard is `>= shard_count` — a pin to a shard that does not exist is a configuration error, not something to discover at the first request that hashes to it. v1 has no dynamic shard reconfiguration; updates require a restart.

The memory-based route is a free function rather than a table field, because it is pure bit extraction and depends on nothing the table holds.

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

## 4. Space-based routing

For a space_id, the routing computes:

```rust
fn shard_for_space(&self, space: SpaceId) -> ShardId {
    if let Some(&s) = self.overrides.get(&space) {
        return s;
    }
    hash_space_to_shard(space, self.shard_count)
}

fn hash_space_to_shard(space: SpaceId, shard_count: u16) -> ShardId {
    let h = blake3::hash(space.as_bytes());              // the 16 UUID bytes
    let hi = u64::from_le_bytes(h.as_bytes()[..8]);      // first 8 bytes, little-endian
    (hi % u64::from(shard_count)) as ShardId
}
```

- Check the overrides map first (for VIP spaces).
- Otherwise, `hash(space_id) % shard_count`.

## 5. The hash choice

BLAKE3 hash:

- Cryptographically strong (good distribution).
- Fast (~2 GB/s on modern hardware).
- Deterministic across runs.

The hash is not security-critical here (it is not protecting against adversarial inputs); Brain uses BLAKE3 because it is already used for content-addressing elsewhere. Consistency.

## 6. The "shard count change" problem

When the shard count changes (operator adds shards), the modulo formula gives different shard assignments for many spaces. Their data would need to migrate to new shards.

Brain doesn't support transparent shard count changes in v1. The shard count is fixed at deployment.

In a future major version, **consistent hashing** would minimize migration:
- Each shard owns a range of hash values.
- Adding a shard just moves a portion of one shard's range.
- Most spaces' assignments are unchanged.

In v1, the simple modulo is sufficient because Brain does not change shard count.

## 7. The override map

The override map is for spaces needing specific assignments:

```toml
[shards.routing.overrides]
"space-uuid-VIP" = 0       # The VIP space is on shard 0
"space-uuid-XL" = 5        # An extra-large space has its own shard
```

Overrides are checked before the hash. They give operators fine control without disrupting general routing.

## 8. Multi-shard spaces

For spaces whose data exceeds one shard, the routing splits:

```toml
[shards.routing.multi_shard]
"space-uuid-XL" = [3, 4, 5]    # Spans shards 3, 4, 5
```

For these spaces:
- ENCODE picks one of the assigned shards (round-robin or sticky-by-session).
- RECALL fans out to all assigned shards.

This is operator-configured; not auto-detected. A future version may add auto-spreading.

## 9. The "shard for memory, but the space is multi-shard" case

When an existing memory is referenced (e.g., by FORGET memory_id), the MemoryId already encodes its shard. Multi-shard configuration doesn't change that.

For a multi-shard space, the space's recent encodes are on one shard, older ones on other shards (depending on when each was assigned). Memory operations always go to the encoded shard.

## 10. Routing for new spaces

When an ENCODE arrives for a space with no prior memories:

```
1. Check overrides; if found, use that shard.
2. Check multi-shard config; if found, pick one (round-robin).
3. Hash the space_id.
4. Use the resulting shard.
```

The first ENCODE establishes which shard the space uses (until reconfigured).

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
- Cross-space queries (rare).

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

- Same space → same shard (assuming overrides don't change).
- Same memory → same shard (encoded in the ID).

This stability matters for clients (caching) and for application correctness (no surprise migrations).

---

## Shard Assignment

How spaces are durably assigned to shards — the mapping that determines which shard hosts which space's data.

## 18. The assignment problem

Given a fixed set of shards and an arriving space, which shard hosts the space?

The assignment must be:

- **Deterministic** — given the same space_id and same shard count, return the same shard.
- **Stable** — once assigned, the space stays assigned until explicit reconfiguration.
- **Well-distributed** — spaces spread roughly evenly across shards.

## 19. The default: hash-based

The default assignment uses BLAKE3:

```rust
fn shard_for_space(space_id: SpaceId, shard_count: usize) -> ShardId {
    let hash = blake3::hash(space_id.as_bytes());
    let value = u64::from_le_bytes(hash.as_bytes()[0..8].try_into().unwrap());
    (value % shard_count as u64) as ShardId
}
```

For UUIDv7 space IDs, this gives roughly uniform distribution. With 16 shards and 1000 spaces, each shard gets ~62-63 spaces.

## 20. The override map

For specific spaces, operators can override the default:

```toml
[shards.routing.overrides]
"space-vip-001" = 0      # VIP space on dedicated shard 0
"space-large-002" = 5    # Large space on shard with most capacity
```

Overrides are checked first; the hash is the fallback.

Override use cases:
- VIP/dedicated capacity for premium tenants.
- Co-locating related spaces on the same shard.
- Routing test spaces to a specific shard.

## 21. The multi-shard case

For spaces needing more capacity than one shard:

```toml
[shards.routing.multi_shard]
"space-huge-003" = { shards = [3, 4, 5], strategy = "round_robin" }
```

For multi-shard spaces:

- ENCODE picks one shard per the strategy.
- RECALL fans out to all assigned shards.
- The space's data is split.

Strategies:
- `round_robin`: cycle through shards.
- `sticky_by_context`: each session lives on one shard (chosen via session_id hash).
- `weighted`: based on shard load (advanced; not in v1).

## 22. The persistent assignment record

For spaces whose assignment shouldn't change (even if shard count changes), the assignment is persisted:

```
spaces table:
  space_id → ShardId
```

This is checked at space creation. Once recorded, it's the source of truth.

For deployments that don't need this stability, the hash-based dynamic assignment is sufficient. Persistent assignment is opt-in.

## 23. The "first-encode wins" semantics

When a space's first ENCODE arrives:

```
1. Compute the candidate shard (hash or override).
2. Record the assignment in the spaces table on that shard.
3. Process the encode.
```

Subsequent encodes for the same space route to the recorded shard, regardless of hash changes.

## 24. The shard-count-change scenario

If the operator changes shard count (very rare in v1):

- New spaces (no recorded assignment) use the new hash → new shard.
- Existing spaces (with records) stay where they are.
- Distribution may become uneven (since old spaces are on old shards).

A future enhancement: rebalance to redistribute. v1 doesn't auto-rebalance.

## 25. The space's "primary" shard

For multi-shard spaces, one shard is "primary":

- Holds the space's metadata (config, quotas).
- Initially gets new encodes (until full).
- Coordinates fan-out queries.

Other shards are "extras" — they hold overflow data.

The primary is set at first-encode; it doesn't change unless the operator forcibly migrates.

## 26. The assignment metadata

Per-space metadata in the `spaces` table (see [10. Metadata + Graph Store](../10_metadata/03_substrate_tables.md)):

```rust
struct SpaceMetadata {
    space_id: SpaceId,
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

If a request arrives at the wrong shard for a space:

- The shard checks the spaces table; finds no record (the space isn't here).
- Returns a `WrongShard` error with the correct shard's ID.
- The client retries on the correct shard.

This handles routing-table staleness gracefully. The client refreshes its routing and retries.

## 28. The "space doesn't exist" case

For ENCODE on an unknown space:

- The shard's spaces table doesn't have the space.
- Brain creates a space record (using the hash to determine the shard).
- The encode proceeds.

For RECALL/FORGET/etc. on an unknown space:

- Returns `AgentNotFound`.
- No data exists for the space.

## 29. The "delete space" operation

`ADMIN_AGENT_DELETE <space_id>` removes a space and all its data:

- Tombstones all the space's memories.
- Removes the space's metadata.
- Removes session records.
- Schedules edge cleanup.

This is irreversible. Brain logs the operation for audit.

## 30. The "transfer space" operation (future)

In a future major version, an admin could transfer a space between shards:

- Mark the space for transfer.
- Copy its data to the new shard.
- Update the assignment record.
- Tombstone the old data.

Not implemented in v1. Operators can simulate via export-import.

## 31. The reseeding scenario

If the spaces table is lost or corrupted, Brain can reseed:

- Iterate all memories on the shard.
- Reconstruct the space metadata from memory rows (memories carry space_id).
- Rebuild the spaces table.

This is a recovery path, not a routine operation.

## 32. The cross-shard space uniqueness

A space's ID must be unique across Brain. Brain doesn't enforce this rigorously — there's no global registry of space IDs.

If two clients use the same space_id, they'll write to the same shard (same hash). Their writes mix.

For operators wanting strict uniqueness, the application layer must enforce it. Brain trusts the space_id.

## 33. The tenancy model

A tenant is a **namespace** (company); an application within it is an **space**. The data-isolation scope of every record is the pair `(namespace, space)` — namespace the outer wall (companies never see each other), space the inner wall (apps within a company are separate). See [`../03_schema/04_namespaces.md`](../03_schema/04_namespaces.md) for the operational contract and [`../04_wire_protocol/04_handshake.md`](../04_wire_protocol/04_handshake.md) §10 for how the `(namespace, space, permissions)` scope is derived from the authenticated API key.

- Company A is namespace `acme`; its chatbot and research apps are spaces within `acme`.
- Company B is namespace `globex`; it cannot see `acme`'s data under any request or flag.
- The boundary is enforced logically on every read/write (owner `namespace_id` + `space_id` on every row, folded into index key prefixes), not by physical shard separation — a single deployment safely hosts many tenants.

The earlier "tenant = space by convention" framing is superseded: namespace is now the committed tenant boundary (open-question `OQ-V2-4`, resolved). Identity is server-derived from the key, so a client cannot name another tenant's namespace or space; cross-namespace access is rejected at the boundary.

## 34. The auto-spread (future)

When a space grows to dominate its shard, Brain could auto-spread:

- Detect the space is using > 50% of its shard's resources.
- Move some of its data to a less-busy shard.
- Update the multi-shard config.

Not in v1. Operators do this manually if needed.

---

*Continue to [`03_single_node.md`](03_single_node.md) for single-node deployment.*
