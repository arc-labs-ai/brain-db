# 07.05 Contexts Table

Contexts are named buckets that memories belong to (one per memory). They scope queries and shape access patterns. This file specifies their storage.

## 1. The model

From [02.04 Context](../02_data_model/04_context.md):

- A context belongs to an agent.
- A context has a unique-within-agent name (e.g., "project_alpha", "personal_journal").
- A context has a `ContextId` (8 bytes).
- A memory belongs to exactly one context.

## 2. Three tables

### 2.1 `contexts: ContextId → ContextMetadata`

The full context records.

```rust
struct ContextMetadata {
    context_id: ContextId,
    agent_id: AgentId,
    name: String,                  // Human-readable name, scoped to agent
    created_at: u64,
    last_active_at: u64,
    memory_count: u32,             // Denormalized; updated periodically
    description: Option<String>,
    tags: Vec<String>,
}
```

Lookup by ContextId.

### 2.2 `context_names: (AgentId, &str) → ContextId`

The name → ID index, scoped to agent.

Lookup: "in agent A, what's the ContextId of context named 'foo'?" → range query.

### 2.3 `agent_contexts: (AgentId, ContextId) → ()`

The membership index. Lists all contexts an agent has.

Lookup: "what contexts does agent A have?" → range query for prefix `(A, ...)`.

## 3. Why three tables

Each enables a different access pattern:

- By ID: `contexts`.
- By name: `context_names`.
- By agent: `agent_contexts`.

A single denormalized table couldn't efficiently support all three. Three small tables are cheaper than one big one with multiple indexes.

## 4. ContextId allocation

A ContextId is an 8-byte UUIDv7-derived identifier:

```
ContextId = pack(timestamp_ms_high48, random_low16)
```

This gives:
- ~2^48 contexts globally addressable.
- Time-ordered prefix for ergonomic listing.

Allocated when a context is created (first memory in a new context, or explicit `ADMIN_CONTEXT_CREATE`).

## 5. Lazy creation

When ENCODE specifies a context name that doesn't exist:

1. Try lookup `(agent_id, name)` in `context_names`.
2. If found, use the ContextId.
3. If not found, allocate a new ContextId and insert into all three tables.

This is done within the ENCODE transaction. The context creation is atomic with the memory creation.

## 6. Implicit context

If ENCODE doesn't specify a context, the substrate uses a special "default" context per agent. The default is created on first memory:

- Lookup `(agent_id, "_default")` in `context_names`.
- If not found, create.

The leading underscore distinguishes implicit contexts from user-named ones. The substrate refuses creation of contexts with names starting with `_` from clients (reserved namespace).

## 7. Per-agent context limits

The substrate enforces:

- Soft limit: 1000 contexts per agent.
- Hard limit: 65,535 contexts per agent (matches a 16-bit count field).

Beyond the soft limit, ENCODE logs a warning. Beyond the hard limit, ENCODE fails with `TooManyContexts`.

These limits are configurable. Most agents have 1-10 contexts; a few have hundreds. Operationally, agents with thousands of contexts are unusual and may indicate misuse.

## 8. Context renaming

`ADMIN_CONTEXT_RENAME` can rename a context:

1. Look up the old `(agent_id, old_name)` in `context_names`.
2. Verify the new `(agent_id, new_name)` doesn't exist.
3. Delete the old name entry.
4. Insert the new name entry.
5. Update the `name` field in `contexts`.

Rename is atomic. Memories' `context_id` references are unchanged (they don't include the name).

## 9. Context deletion

`ADMIN_CONTEXT_DELETE` is heavy:

1. Verify all memories in the context are forgotten or moved out.
2. Delete from `contexts`, `context_names`, `agent_contexts`.

If the context still has memories, the operation fails. The operator must FORGET the memories or move them first.

## 10. Memories per context

The `memory_count` field in `ContextMetadata` is denormalized. Updated by:

- Periodic recount by a maintenance worker (true count from `memories` table).
- Incremental updates on ENCODE / FORGET (best-effort; may drift).

For exact counts (rare in practice), the count is recomputed from `memories`. For UI displays, the denormalized count is fine.

## 11. Context iteration

For "list contexts for agent A":

```rust
let contexts: Vec<ContextId> = agent_contexts
    .range((A, ContextId::MIN)..(A_next, ContextId::MIN))?
    .map(|(k, _)| k.1)
    .collect();
```

Returns ContextIds in time-order (earliest first). Pagination is straightforward.

## 12. Context membership lookup

"Is context C in agent A?":

```rust
let exists = agent_contexts.get(&(A, C))?.is_some();
```

O(log N) lookup.

## 13. Cross-agent context separation

Two agents can each have a context named "personal":

- Agent A's "personal" → ContextId X.
- Agent B's "personal" → ContextId Y.

These are distinct contexts. Their memories are separate. There's no global namespace.

This makes context naming intuitive — agents don't have to coordinate names.

## 14. Total table sizes

For typical workloads:

- `contexts`: hundreds to tens of thousands per shard. < 10 MB.
- `context_names`: same. < 10 MB.
- `agent_contexts`: same. < 5 MB.

These tables are small relative to memories and edges. Their performance overhead is negligible.

---

*Continue to [`06_idempotency.md`](06_idempotency.md) for idempotency.*
