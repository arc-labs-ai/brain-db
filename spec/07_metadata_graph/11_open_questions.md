# 07.11 Open Questions

Metadata-store-level questions unresolved as of this spec version.

---

## OQ-MD-1: Per-agent index for memory listing

**Issue.** Listing all memories for an agent currently requires scanning all memories in the shard and filtering by agent_id. For shards with many agents, this is wasteful.

**Options.**

a) **Scan and filter (status quo).** Simple; works fine for shards with one or few agents.

b) **Add a `(AgentId, MemoryId) → ()` index table.** Range scan returns the agent's memories. Costs ~30 bytes per memory in extra storage.

c) **Group shards by agent.** Each shard hosts a single agent (or a small group). Avoids the indexing problem at the routing layer.

**Recommendation.** Add the index table when shards routinely host more than ~10 agents. For v1's typical deployment patterns, status quo is fine.

---

## OQ-MD-2: Per-context index for memory listing

**Issue.** Same as agent index, but for contexts.

**Options.**

a) **Scan and filter (status quo).**

b) **Add a `(ContextId, MemoryId) → ()` index.** ~25 bytes per memory.

c) **Combined `(AgentId, ContextId, MemoryId) → ()` index.** Slightly more compact; serves both agent-listing and context-listing.

**Recommendation.** Add index (c) in v1.1 if usage patterns show frequent context-scoped enumeration.

---

## OQ-MD-3: Soft-delete vs immediate-delete

**Issue.** The metadata table keeps tombstoned rows during the grace period. Live rows and tombstoned rows are mixed; queries always include a "is active" filter.

**Options.**

a) **Mixed table (status quo).** Filter on read.

b) **Two tables.** Active rows in `memories`; tombstoned rows in `memories_tombstoned`. Move on FORGET.

c) **Bloom-filter for active.** Quick check before reading metadata; if not in filter, skip.

**Recommendation.** Status quo. The cost of the active filter is low; the simplicity matters more.

---

## OQ-MD-4: Multi-version edges

**Issue.** A given (source, kind, target) triple can have only one edge. Some applications might want multiple edges with different annotations (a "history" of relationships).

**Options.**

a) **Single edge per triple (status quo).**

b) **Compound key with timestamp:** `(source, kind, target, version)`. Stores multiple instances.

c) **External versioning** at the application level — encode versioned edges as (source_v1, kind, target).

**Recommendation.** Status quo. Use case is rare; complexity added by (b) isn't justified.

---

## OQ-MD-5: Compressed text storage

**Issue.** Text dominates the metadata store's size. Compression (zstd, LZ4) would reduce footprint.

**Options.**

a) **Uncompressed (status quo).** Simple; fast read.

b) **Per-row compression.** Each text is independently compressed. Read decompresses on the fly.

c) **Dictionary compression.** Train a zstd dictionary on a sample of texts; compress with shared dictionary for better ratio.

**Recommendation.** Defer. Disk is cheap; the operational complexity of compression isn't justified unless storage is a bottleneck.

---

## OQ-MD-6: Indexes on flexible attributes

**Issue.** Brain currently indexes only what's hardcoded (memory ID, edges, contexts). Custom indexes (e.g., by tag, by metadata field) aren't supported.

**Options.**

a) **No custom indexes (status quo).** All filtering is post-search.

b) **User-defined indexes.** Operators or agents can request indexes on specific fields.

c) **Schema-driven indexes.** Detect commonly-queried fields and auto-index them.

**Recommendation.** Defer. Brain isn't a SQL database; flexible indexing isn't core to the value proposition.

---

## OQ-MD-7: Partitioned tables for very large shards

**Issue.** redb performance is good for moderately-sized tables but may degrade at very large sizes (hundreds of GB).

**Options.**

a) **Single redb file per shard (status quo).**

b) **Horizontal partition within shard.** E.g., split memories by time range; each range in its own redb file.

c) **Operate at smaller shard sizes.** Encourage sharding before tables grow too large.

**Recommendation.** (c). Brain's sharding is the partition mechanism; if a shard grows too large, split it.

---

## OQ-MD-8: redb backup tooling

**Issue.** Backup/restore is currently file-level (snapshot the entire metadata.redb). For large databases with small ongoing changes, this is wasteful.

**Options.**

a) **File-level backup (status quo).**

b) **Logical backup.** Export rows; import into fresh database.

c) **Incremental backup.** Diff between snapshots; only ship changes.

**Recommendation.** File-level is fine for v1. Incremental backup is a possible v2 enhancement.

---

## OQ-MD-9: redb's WAL mode

**Issue.** redb has its own write-ahead-log (separate from Brain's WAL). Brain currently uses redb's default sync-on-commit. There's a higher-throughput async mode.

**Options.**

a) **Sync-on-commit (status quo).** Each commit fsyncs.

b) **Async commits.** redb buffers commits; periodic group sync. Higher throughput; small durability window for redb's own state (Brain's WAL still ensures actual durability).

**Recommendation.** Stay with sync-on-commit. The overhead is acceptable; durability simplicity matters.

---

## OQ-MD-10: Cross-shard transactions

**Issue.** Currently, the substrate doesn't support transactions across shards. An operation that needs cross-shard atomicity isn't expressible.

**Options.**

a) **No cross-shard transactions (status quo).** Caller responsible for handling failures.

b) **Two-phase commit across shards.** Heavy; we don't want to be a distributed database.

c) **Saga pattern.** Application-level compensating actions on failure.

**Recommendation.** Stay with (a). For applications needing cross-shard atomicity, the SDK provides saga helpers.

---

*Continue to [`12_references.md`](12_references.md) for references.*
