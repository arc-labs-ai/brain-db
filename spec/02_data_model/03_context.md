# 02.03 Contexts

A **context** is an agent-scoped logical grouping of memories. This file specifies what contexts are, what they're for, how they're created, and the constraints on them.

## 1. Definition

A context is a (name, id) pair within an agent's namespace. Every memory belongs to exactly one context.

```rust
struct Context {
    id: ContextId,           // 8 bytes; agent-scoped
    name: String,            // human-readable; max 255 bytes
    agent_id: AgentId,       // owner
    created_at: u64,         // unix_nanoseconds
    memory_count: u64,       // approximate; for stats
    description: String,     // optional, agent-supplied
}
```

## 2. Why contexts exist

The operations are most useful when scoped. An agent's personal memories (about its operator's preferences) shouldn't pollute its work memories (about the project at hand). Contexts provide the coarse partition.

Without contexts, every `RECALL` would query across all the agent's memories, which:

- Dilutes ranking — high-salience memories from one project crowd out lower-salience but more relevant memories from another.
- Wastes work — Brain processes irrelevant candidates.
- Conflates audiences — debug-mode memories surface in production-mode queries.

With contexts, the agent narrows the search space. `RECALL` filters to the context (or a small set of contexts), and Brain's job becomes easier and the ranking sharper.

## 3. Creation

Contexts are created **lazily**. When an agent first references a context name, the server assigns a `ContextId` and persists the mapping.

The flow:

1. Agent sends an `ENCODE` (or any operation) referencing context name `"work-alpha"`.
2. Server looks up the name in the agent's context table.
3. If absent, server allocates a new `ContextId` and persists `(name, id, created_at, agent_id)`.
4. Server proceeds with the operation, using the resolved `ContextId`.

Alternative: agents can pre-create a context via the `ADMIN_CREATE_CONTEXT` operation. This is useful for setting a description or initializing metadata before any memory is encoded.

## 4. The default context

Every agent automatically has the **default context** with `ContextId = 0` and name `"default"`. This is created at agent first-touch and is special:

- It cannot be deleted.
- It cannot be renamed.
- Operations that don't specify a context use the default.

Memories encoded without an explicit context land in the default. This is the right behavior for casual usage.

## 5. Naming rules

Context names must:

- Be valid UTF-8.
- Not be empty.
- Not exceed 255 bytes.
- Not contain newline characters (rejected for log/display safety).
- Be unique within the agent.

The server normalizes names by trimming surrounding whitespace. After trimming, an empty name is rejected.

Names are case-sensitive. `"Work"` and `"work"` are different contexts.

## 6. ID allocation

`ContextId`s are assigned sequentially within an agent. The first non-default context gets id 1, the second gets id 2, and so on.

`ContextId` 0 is reserved for the default.

The agent's `ContextId` allocator is a per-agent monotonic counter, persisted in the metadata store. It is not gap-free — if a context is deleted (deferred to a future version), its id is not reused.

## 7. Capacity

A single agent can have up to 2^64 contexts (effectively unlimited). In practice, agents have a small number — single-digit to a few dozen — and contexts proliferating beyond that suggests the agent is using contexts where it should be using a different mechanism (tags, metadata fields, separate agents).

## 8. Memory ↔ context relationship

A memory's `context_id` is set at encode time. It is **mutable** by an admin operation but not by hot-path agent operations.

Why mutable at all: occasionally a memory was encoded into the wrong context and needs to move. The admin operation supports this:

- `ADMIN_MOVE_MEMORY(memory_id, new_context_id) → ack`

Why not hot-path: most apps don't need to move memories at runtime, and exposing it on the hot path would invite mistakes (mass-moving memories due to a logic bug, etc.).

## 9. Querying by context

`RECALL` accepts an optional `context_filter`:

- `None` — query across all of the agent's contexts (rarely useful).
- `Some(context_id)` — query within one context.
- `Some(set_of_context_ids)` — query within a small set of contexts.

The set form is bounded (max 16 contexts per query in v1) to keep filter evaluation cheap.

A common client default for `context_filter` is the agent's "current" context — typically the one the application is currently operating in. The protocol-level default is `None` (no filter), which a client may override per its convention.

See [05. Operations](../05_operations/00_purpose.md) §RECALL for the exact filter semantics.

## 10. Context-aware salience

Salience is per-memory, not per-context. A high-salience memory in context A doesn't automatically have high salience when querying context B; it just doesn't match the context filter at all.

This is intentional: context separation is structural, not score-mixing. Cross-context relevance scoring would require a join across contexts that Brain does not justify in v1.

## 11. Edges across contexts

Edges may cross context boundaries. A memory in context "work" may have a `DERIVED_FROM` edge pointing to a memory in context "personal".

Cross-context edges are visible during graph traversals, but the caller's context filter (if any) limits which side of the edge is returned in the result. If a `RECALL` query with `context_filter = "work"` returns a memory whose edges point into `"personal"`, the edges themselves are returned (with target memory ids) but the cross-context targets are not auto-loaded into the result.

## 12. Renaming contexts

A context's name is mutable by admin operation:

- `ADMIN_RENAME_CONTEXT(context_id, new_name) → ack`

The `ContextId` is unchanged. Other agents (none, since contexts are agent-scoped) and existing memories are unaffected.

The use case: an agent realizes a context's name is wrong, or the application's nomenclature evolved.

## 13. Deletion

Context deletion is **deferred to a future version**.

The straightforward implementation (delete the context, leaving its memories orphaned) is bad — it loses data. The careful implementation (forget all memories in the context, then remove the context) is heavy and rarely needed.

For v1, the workaround is to leave the context in place and just stop querying it. Storage cost is bounded by the context's memory count; query cost is zero (no filter targets it).

A future version will add `ADMIN_DELETE_CONTEXT(context_id, mode)` with `mode` selecting between "fail if non-empty" and "forget all members first".

## 14. Statistics

For each context, Brain maintains:

- `memory_count` — approximate count of memories in the context.
- `last_encoded_at` — most recent encode into this context.
- `last_recalled_at` — most recent recall touching this context.

These are used for stats display (`ADMIN_STATS`) and for some heuristics in the planner (e.g., "this context hasn't been touched in months — deprioritize for cross-context queries"). They're not exposed as queryable fields on memories.

## 15. The default context: when to use it vs explicit contexts

Recommendations for application developers:

- **Use the default context** for short-lived agent sessions, simple agents with one purpose, or prototyping.
- **Use explicit contexts** when the agent's memories naturally separate (different projects, different relationships, different time periods) — anything where you'd prefer cross-talk to be off by default.
- **One context per "domain"** is the typical pattern. Avoid hundreds of contexts; if you find yourself creating many, you probably want a different abstraction (tags, metadata fields, or a separate agent).

## 16. Persistence

Contexts are persisted in the metadata store (redb), in a per-agent table:

- Key: `(agent_id, context_id)`.
- Value: `(name, created_at, description)`.

A second index lets the server look up `ContextId` from `(agent_id, name)` for the lazy-creation path. See [10. Metadata + Graph Store](../10_metadata/00_purpose.md) §3.

