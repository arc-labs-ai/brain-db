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

`RoutingTable` carries a `HashMap<SpaceId, ShardId>` of overrides, consulted
before the hash, and validates at construction that every target is inside
`[0, shard_count)` (`crates/brain-server/src/network/routing.rs:56-91`). It has
no configuration surface: the server constructs the table with an empty map
(`crates/brain-server/src/lib.rs:460`), and the config schema has no
`[shards]` section at all — every config struct is
`#[serde(deny_unknown_fields)]`, so inventing one is a boot failure, not an
ignored key.

Pinning a named space to a chosen shard is therefore a future capability, not
an operator knob. The mechanism is in place so that adding one is a config
field and a populate call rather than a change to the read path.

## 8. One space, one shard

A space's data lives on exactly one shard. That is what makes per-shard
linearizability sufficient: every operation on one space is ordered against
every other operation on that space, because they all land on the same
single-writer executor.

Multi-shard spaces are out of scope. The routing module names them, with
"WrongShard" handling and consistent hashing, as deferred work
(`crates/brain-server/src/network/routing.rs:12-15`). A space that outgrows
one shard is the demonstrated operator need that would justify the
distributed-coordination work, not a TOML entry.

## 9. Routing by memory id

When an existing memory is referenced — `FORGET memory_id`, `MEMORY_INSPECT`, a
`LINK` endpoint — the `MemoryId` already encodes its owning shard in its high
16 bits, so routing is a bit extraction and not a hash
(`routing::shard_for_memory`, `crates/brain-server/src/network/routing.rs:119-125`).

This is the same shard the space hash would give, since the space is whole, but
the id path is used anyway: it is O(1) and it stays correct for an id whose
space the caller never names.

## 10. Routing for new spaces

When an ENCODE arrives for a space with no prior memories:

```
1. Check the override map; if the space is present, use that shard.
2. Otherwise hash the space_id: u64::from_le_bytes(BLAKE3(space_id)[..8]) % shard_count.
```

Nothing is established by the first ENCODE and nothing is recorded: the
assignment is a pure function of the space id and the shard count, so every
later request recomputes the same answer. That is why `shard_count` is pinned
by the storage-layout record rather than taken from config on every boot —
changing it would silently re-route every existing id
(`crates/brain-server/src/storage_layout.rs:1-13`).

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

- `GET /v1/shards` and `GET /v1/workers` (listings that fan out across shards).
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

See §7. The table consults an override map before hashing, and nothing
populates it; there is no `[shards]` config section to populate it from.

## 21. Why a space is never split

Splitting one space across shards would cost the property the whole design
rests on. Per-shard linearizability is enough precisely because a space is
whole: all of its operations are ordered with respect to each other on one
executor. Spread it over three shards and an ENCODE and a RECALL for the same
space can be concurrent on different cores, and every cross-shard read needs a
merge step with no single order to merge against.

So the capacity answer for a space larger than one shard is a larger shard —
`[shard] arena_capacity_bytes` and the host behind it — not a wider one. The
distributed alternatives (range-based sharding, cross-node fan-out) are
tracked as larger architectural changes in
[`ROADMAP.md`](../../ROADMAP.md), each gated on an operator need the
single-node architecture cannot meet.

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

## 25. The space's one shard

A space has one shard and no notion of a primary. Its metadata, its vectors,
its typed-graph rows and its lexical postings all live in that shard's
directory, and the shard is recomputed from the space id on every request
rather than stored anywhere.

Namespace-wide RECALL is the one read that touches every shard, and it is
fanned out per-request rather than coordinated by an elected shard: each shard
returns its own raw candidate pool (`ShardRequest::RecallGather`), the
connection layer merges the pools, and one shard then shapes the merged input
(`ShardRequest::RecallShapeMerged`) so membership and abstention are decided
exactly once — `crates/brain-server/src/shard/mod.rs:155-175`.

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

`DELETE /v1/spaces/{id}` removes a space and all its data:

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

The earlier "tenant = space by convention" framing is superseded: namespace is now the committed tenant boundary. Identity is server-derived from the key, so a client cannot name another tenant's namespace or space; cross-namespace access is rejected at the boundary.

## 34. A space that dominates its shard

A space is never spread, so a space that outgrows its shard is answered by
growing the shard: raise `[shard] arena_capacity_bytes` and give the host the
RAM. §21 is why the alternative is not a knob.

Auto-spreading, and the migration machinery it would need, belong to the
range-based-sharding direction tracked in [`ROADMAP.md`](../../ROADMAP.md).

---

*Continue to [`03_single_node.md`](03_single_node.md) for single-node deployment.*
