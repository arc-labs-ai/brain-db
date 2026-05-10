# 12.09 Open Questions

Sharding and clustering questions unresolved as of this spec version.

---

## OQ-SC-1: Consistent hashing for routing

**Issue.** The default hash-modulo routing means changing shard count requires re-routing all agents. Consistent hashing minimizes re-routing.

**Options.**

a) **Hash-modulo (status quo).** Simple; doesn't support shard count changes.

b) **Consistent hashing.** Each shard owns a hash range. Adding a shard splits one range. Most agents stay put.

c) **Rendezvous (highest-random-weight) hashing.** Each agent independently picks the shard with the highest hash. Adding a shard reassigns ~1/N of agents.

**Recommendation.** Implement (b) or (c) in v2. (b) is simpler; (c) gives more uniform spread under shard count changes.

---

## OQ-SC-2: Auto-split

**Issue.** When a shard grows too large, splitting is manual. Could the substrate auto-split?

**Options.**

a) **Manual split (status quo, v1).**

b) **Auto-detect; alert; manual confirm.**

c) **Fully automatic.**

**Recommendation.** Stay with manual through v1; add (b) in v1.x; consider (c) in v2 with safety limits.

---

## OQ-SC-3: Multi-shard agent strategies

**Issue.** Multi-shard agents have several distribution strategies (round-robin, sticky-by-context, weighted). Which is best?

**Options.** All of the above, with operator choice.

**Recommendation.** Default to sticky-by-context (so a context's data is in one place; queries within a context don't need fan-out). Round-robin as opt-in for finer-grained spreading. Weighted is v2.

---

## OQ-SC-4: Cross-shard transactions

**Issue.** v1 doesn't support transactions across shards. Some applications might need them.

**Options.**

a) **No support (status quo).** Sagas in the application.

b) **Two-phase commit.** Standard but heavy.

c) **Calvin-style determinism.** Order operations globally; execute deterministically across shards.

**Recommendation.** (a). Cross-shard transactions are complex; the substrate's other primitives are usually sufficient.

---

## OQ-SC-5: Replication consistency

**Issue.** When v2 introduces replication, what's the default consistency level?

**Options.**

a) **Async (high throughput, risk of data loss on failure).**

b) **Sync (strong consistency, high latency).**

c) **Quorum (balanced).**

**Recommendation.** (c) for the default. Configurable for tuning.

---

## OQ-SC-6: Replication topology

**Issue.** How replicas are placed (same DC? cross-DC? cross-region?).

**Options.**

a) **Operator config.** Operator specifies topology.

b) **Auto-spread.** Substrate places replicas across failure domains.

**Recommendation.** Both. Operators specify zones; the substrate spreads within them.

---

## OQ-SC-7: Cluster size limits

**Issue.** What's the upper bound on cluster size? 10 nodes? 100? 1000?

**Options.**

a) **Small (3-10).** Tractable; well-tested.

b) **Medium (10-100).** Needs more care with gossip / membership.

c) **Large (100+).** Hard; may need different architecture.

**Recommendation.** Target small in v2 initial release; medium in v2.x. Large is a different system.

---

## OQ-SC-8: Failover automation

**Issue.** When a primary fails, who decides the new primary?

**Options.**

a) **Manual (operator).** Slow but safe.

b) **Auto via consensus protocol.** Fast but adds complexity.

**Recommendation.** (b) for v2, using a Raft-based control plane.

---

## OQ-SC-9: Geo-replication

**Issue.** v2 may want to support geo-replication for data residency or latency.

**Options.**

a) **No geo features (just naive cross-DC).**

b) **Geo-aware routing.** Read from the nearest replica.

c) **Geo-pinned shards.** Specific shards always live in specific regions.

**Recommendation.** Defer. Single-region v2 is the priority.

---

## OQ-SC-10: Capacity-based placement

**Issue.** When creating new shards in a cluster, the substrate could pick less-loaded nodes automatically.

**Options.**

a) **Operator-specified.** Operator chooses.

b) **Auto-select.** Substrate picks based on metrics.

**Recommendation.** Both, with auto as default.

---

## OQ-SC-11: Online schema migration in clusters

**Issue.** Schema migrations across many shards / nodes need coordination.

**Options.**

a) **Quiesce-and-migrate.** Downtime during migration.

b) **Rolling migration.** Migrate shards one at a time; cluster mixes versions during.

c) **Forward-compatible schema.** New version reads both old and new formats; migration in background.

**Recommendation.** (c). Brain's redb-based migration is well-suited for this. Will need careful design in v2.

---

*Continue to [`10_references.md`](10_references.md) for references.*
