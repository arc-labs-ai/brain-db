# 07.02 Table Layout

The tables in the metadata store. Each table is a typed B-tree maintained by redb.

## 1. Catalog of tables

| Table name | Key | Value | Purpose |
|---|---|---|---|
| `memories` | `MemoryId` | `MemoryMetadata` | Per-memory metadata |
| `texts` | `MemoryId` | `Vec<u8>` | Memory text content (UTF-8) |
| `edges_out` | `(MemoryId, EdgeKind, MemoryId)` | `EdgeData` | Outgoing edges, indexed by (source, kind, target) |
| `edges_in` | `(MemoryId, EdgeKind, MemoryId)` | `EdgeData` | Incoming edges, indexed by (target, kind, source) |
| `contexts` | `ContextId` | `ContextMetadata` | Context records |
| `context_names` | `(AgentId, &str)` | `ContextId` | Context name → ID lookup per agent |
| `agent_contexts` | `(AgentId, ContextId)` | `()` | Membership of contexts in agents |
| `idempotency` | `RequestId` | `IdempotencyEntry` | Replay protection for ENCODE/FORGET |
| `agents` | `AgentId` | `AgentMetadata` | Per-agent metadata |
| `model_fingerprints` | `ModelFingerprint` | `ModelInfo` | Registry of seen model fingerprints |
| `checkpoints` | `u64` | `CheckpointInfo` | Checkpoint records |
| `next_lsn` | `()` | `u64` | The next WAL LSN (singleton) |
| `slot_versions` | `u64` (slot_id) | `u32` (version) | Per-slot versions, for lazy reclaim |

The table count is intentional: each table has a single, focused purpose. We don't pack multiple types into one table.

## 2. Memory ID as primary key

Most tables key by `MemoryId`. The 16-byte identifier is:

- Time-ordered (UUIDv7 prefix), so iterating gives chronological order.
- Cluster-friendly (same agent's memories share a high-bit prefix).

Range queries by MemoryId are common: "all memories created in the last hour", "all memories in this agent".

## 3. Edge tables: two for two directions

Edges are stored twice: once keyed by source (in `edges_out`) and once by target (in `edges_in`). This duplication enables:

- Forward queries ("what does memory X causally lead to?") via `edges_out`.
- Reverse queries ("what supports memory X?") via `edges_in`.

The duplication doubles edge storage but is essential for query performance. Without it, a reverse query would require scanning all edges.

For 1M memories with avg 8 edges each, 8M edges, doubled to 16M index entries. At ~30 bytes per entry, ~500 MB. Significant but bounded.

## 4. Composite keys

Several tables use composite keys for efficient range queries:

- `edges_out: (source, kind, target)` — listing all edges of a kind from a source is a tight range scan.
- `context_names: (agent_id, name)` — listing context names per agent is a range scan.

The composite key encoding is little-endian concatenation; redb sorts keys lexicographically, and our encoding makes that order match logical order (e.g., all edges from source X come before edges from source Y).

## 5. Value encoding

Values are encoded with **rkyv** (the same library as the wire protocol uses for structured payloads). rkyv:

- Zero-copy deserialization: read a value, get a typed reference into the redb-mmap'd page.
- Compact: no per-field tags or alignment overhead.
- Schema-aware: each value type has a defined layout.

For variable-length values (text, edge lists), rkyv handles the indirection via offsets within the value blob.

## 6. Schema evolution

Each table has a format version embedded in its metadata. When the substrate opens the metadata store:

- Read the format version of each table.
- If older than current, run any registered migrations.
- If newer than current, refuse to open (the substrate is too old).

Migrations are detailed in [02.09 Schema Evolution](../02_data_model/09_schema_evolution.md).

## 7. The singleton tables

Some tables have at most one row:

- `next_lsn` — the next LSN counter.
- Any "global config" tables (rare).

We use redb's `()` key type for singletons. Reading is `table.get(&())`, writing is `table.insert(&(), &value)`.

## 8. Index-only tables

`agent_contexts` is an index — its key is `(AgentId, ContextId)` and its value is `()`. This is a simple "is this context in this agent?" check.

Could we use a HashSet in memory? Yes, but persistence matters; the substrate may need to re-look-up after restart.

## 9. The texts table

The `texts` table holds the original memory text:

- Key: MemoryId.
- Value: UTF-8 bytes (variable length).

Text is read on demand:
- For RECALL responses (when the client requests the text).
- For consolidation (the worker reads source texts).
- For migration (re-embedding from the original text).

Detailed in [`07_text_storage.md`](07_text_storage.md).

## 10. Context lookup

Two tables together make context lookup efficient:

- `contexts`: `ContextId → ContextMetadata`. Lookup by ID.
- `context_names`: `(AgentId, &str) → ContextId`. Lookup by name within an agent.

A context's name is typed scoped to its agent. Different agents can have contexts with the same name, but they're distinct contexts (different ContextIds).

The context's full record (with stats, timestamps, etc.) lives in `contexts`. The name table is the index.

## 11. Idempotency table TTL

The `idempotency` table grows with every ENCODE/FORGET. It's pruned on a TTL — entries older than the configured idempotency window (default 24 hours) are deleted by the maintenance worker.

Pruning is a periodic batch operation, not per-row. The worker scans for expired entries and deletes them in a single transaction. See [11. Background Workers](../11_background_workers/) §Idempotency Cleanup.

## 12. The agents table

`agents` carries per-agent metadata:

- AgentId.
- Display name (optional).
- Created at.
- Stats (memory count, contexts count, etc. — updated periodically).
- Configuration overrides (per-agent quotas, etc.).

Looking up an agent by ID is O(log N) where N is the number of agents in the shard. Typical: a few thousand to a few million agents per shard.

## 13. The slot_versions table

When a slot is reclaimed (after FORGET + grace period), its version is incremented. The new version is recorded so that future MemoryIds with the new version know which slot they refer to.

The table maps `slot_id → current_version`. Looked up:

- During ENCODE to allocate a fresh MemoryId for a reclaimed slot.
- During recovery to verify HNSW node IDs match the slot's current state.

## 14. Total table count

In the v1 spec:

- 13 tables.

Adding a table is a schema change — it requires a format version bump in the metadata store. Tables are not added lightly.

---

*Continue to [`03_memory_table.md`](03_memory_table.md) for memory metadata details.*
