# 02.03 Sessions

A **session** is a grouping of memories within a space — a conversation, a run, a task. This file specifies what sessions are, what they are *not*, how they come into existence, and the constraints on them.

The surrounding tier is the **space**, which is the isolation unit. The normative tenancy contract is [`../20_tenancy/00_purpose.md`](../20_tenancy/00_purpose.md); this file specifies only the session tier.

## 1. Definition

A session is identified by a client-chosen opaque `u64` within one `(namespace, space)` scope. Every memory belongs to exactly one session.

```rust
struct SessionMetadata {
    created_at_unix_nanos: u64,
    last_active_unix_nanos: u64,
    title: Option<String>,        // optional caller-supplied display title
    memory_count: u32,            // denormalized; display-only
}
```

The scope — `(namespace_id, space_id, session_id)` — lives in the registry table's **key**, not in the row. A session row therefore cannot be read, listed or ranged into from another scope: the isolation is structural, not a filter applied after the fact.

## 2. A session is a grouping, not a boundary

This is the one load-bearing fact about sessions, and the thing most easily got wrong.

- **MUST NOT** be treated as a security or isolation boundary.
- **MUST NOT** appear in any secondary-index scope *prefix*.

Isolation is `space`. Session is ergonomics. Two sessions in the same space are the same tenant's data, co-resident and mutually visible; a `RECALL` with no session filter spans all of them by design. If an application needs two bodies of memory that must never see each other, the answer is two **spaces**, not two sessions.

The practical consequence: never derive a session id from something you are relying on to keep users apart. A per-end-user partition is a space (`"support-bot:user123"`), and the per-conversation grouping *within* that user is a session.

## 3. The id is the client's, and it is opaque

`session_id` is a `u64` the client chooses. Brain does not allocate it, does not intern it, does not maintain a name→id map, and never parses it for structure.

This is a deliberate reversal of a server-allocated-handle design. A server-allocated id forces every client write to either carry a name for lazy lookup (a string compare and an index probe on the write path) or to pre-register and remember a handle (a round trip before the first write). A client-chosen `u64` costs neither: the application already has a conversation id, and it hashes or truncates it once.

Two consequences follow:

- There is **no session-name uniqueness rule and no normalization**, because there are no server-side names. `title` is display metadata attached to the registry row; it is never an identifier and nothing resolves through it.
- There is **no capacity limit and no allocator**. The id space is the full `u64`, there is no monotonic counter to exhaust, and a deleted session's id may be reused by the client if it wants.

## 4. The default session

`session_id = 0` is the **default session**. It is present implicitly for every space — no row needs to exist for it to be writable — and memories encoded without an explicit session land there.

It is never deletable. `SESSION_DELETE` on id `0` is refused: the default is where un-sessioned writes go, so removing it would leave the next write with nowhere to land.

## 5. Creation

Sessions come into existence **two ways**, and both are first-class:

1. **Implicitly, on first write.** The apply path upserts the session's registry row in the *same* redb write transaction as the memory itself, stamping `created_at` on first sight and bumping `last_active` and `memory_count` thereafter. Zero ceremony: a client that only ever sends `ENCODE` never needs to know the registry exists.
2. **Explicitly, via `SESSION_CREATE` (`0x0073`).** Idempotent — a create for an existing session returns the existing row with `created = false`. This is for provisioning a `title` ahead of the first write, not a precondition for writing.

Because the implicit path runs inside the memory's own transaction, the registry cannot drift from the data on a crash: either both landed or neither did. WAL recovery replays the same helper, so a recovered shard rebuilds the registry rather than inheriting a stale one.

The registry is **derived, recomputable view state**. Every row summarizes memory and graph data that remains authoritative, which is what lets the counter-reconciliation worker correct the denormalized counts without a migration.

## 6. Memory ↔ session relationship

A memory's `session_id` is set at encode time. It is **mutable** by an admin operation but not by hot-path operations:

- `ADMIN_MOVE_MEMORY(memory_id, new_session_id) → ack` (`0x0067`)

Why mutable at all: occasionally a memory was encoded into the wrong session and needs to move. Why not on the hot path: most applications never need it at runtime, and exposing it there invites mass-moves from a logic bug.

## 7. Querying by session

`RECALL`, `PLAN`, `REASON`, `SUBSCRIBE`, `MATERIALIZE_PROCEDURAL` and the query-introspection ops all accept an optional `session_filter: Option<Vec<SessionId>>`:

- `None` (the wire default) — every session in the caller's space.
- `Some([id])` — one session.
- `Some([id, ...])` — a set of sessions.

The filter is bounded at 1024 entries, shared with the other recall filters. `MEMORY_LIST` scopes by session through the same mechanism.

Note the default: **no filter**. Spanning a space's sessions is the normal read, because the sessions are the same user's history and the point of a memory system is that yesterday's conversation informs today's. Narrowing to one session is the special case, and the client asks for it explicitly.

## 8. Salience and ranking are per-memory, not per-session

Salience is a property of a memory, not of a session. A high-salience memory in session A does not score differently when a query filters to session B — it simply does not match the filter.

Session separation is structural, not score-mixing. There is no cross-session relevance adjustment.

## 9. Edges across sessions

Edges may cross session boundaries. A memory in one session may have a `DerivedFrom` edge pointing at a memory in another.

One derivation stage deliberately does not cross: the **temporal-edge** worker writes `FollowedBy` edges from the space's previous memory *in the same session*, and `cross_session` is `false` by default. "What happened next" is a within-conversation question; chaining the last message of one conversation to the first of the next manufactures a sequence that did not exist.

Cross-session edges are visible during graph traversal. A `RECALL` filtered to one session returns the edges themselves (with their target ids), but does not auto-load targets that fall outside the filter.

## 10. Deletion

`SESSION_DELETE` (`0x0075`) is implemented, and it is a **cascade**, not an unlink:

- It enumerates the `(namespace, space)` memory timeline narrowed to that session and tombstones each memory through the single `submit(Write)` path.
- Each `Tombstone` fans out post-commit to the same FORGET cascade that tears down the memory's backing entity / statement / relation rows. A session delete therefore reuses the forget machinery rather than re-implementing graph teardown — there is one code path for "this memory is gone", and it is already correct.
- `hard = false` (the default) is a soft tombstone with the standard grace window; `hard = true` zeroes immediately.
- The response reports `existed` and `memories_forgotten`.

Deleting a session with no registry row is a lenient success with `existed = false`, matching FORGET's idempotency.

The v1 cascade is synchronous: it tombstones every matching memory in one pass. A very large session delete should eventually chunk behind a resumable cursor.

`SPACE_DELETE` (`0x0072`) is the same machinery without the session narrowing — GDPR erasure of every row under `(namespace, space)`, hard and immediate.

## 11. Renaming

There is nothing to rename. A session has no server-side name; `title` is a display field set at `SESSION_CREATE`. Changing how a session is labelled in a UI is a client concern.

The admin ops `ADMIN_CREATE_SESSION` (`0x0065`) / `ADMIN_RENAME_SESSION` (`0x0066`) are a separate operator surface that does carry a `name`. They are admin-gated and not `act_as`-aware, and they are for operator tooling — not the path an application uses.

## 12. Statistics and listing

Each session's registry row carries `created_at`, `last_active` and a denormalized `memory_count`. The counts are **display-only** and reconciled by the counter-reconciliation maintenance worker; nothing correctness-bearing reads them.

`SESSION_LIST` (`0x0074`) returns one space's sessions **newest-first by `last_active`**. That ordering is not sorted at read time: a second table, `session_by_scope`, is keyed `(namespace_id, space_id, last_active, session_id)`, so a listing is a reverse range scan over the 20-byte `(namespace, space)` prefix. The ordering index exists because "my recent conversations" is the only listing anyone asks for, and paying for it at write time is cheaper than sorting a scope's sessions on every read.

## 13. Persistence

Two redb tables, co-located because a session create touches both:

| Table | Key | Value |
|---|---|---|
| `sessions` | 28 bytes: `[namespace_id (4, BE) \| space_id (16) \| session_id (8, BE)]` | `SessionMetadata` |
| `session_by_scope` | 36 bytes: `[namespace_id (4) \| space_id (16) \| last_active (8, BE) \| session_id (8, BE)]` | `()` |

The leading `(namespace, space)` bytes are what make a scope's sessions a contiguous, range-scannable keyspace — and what make it impossible for one scope's scan to walk into another's rows. See [10. Metadata + Graph Store](../10_metadata/00_purpose.md).

The parallel `spaces` table is keyed `[namespace_id (4, BE) | space_id (16)]` and carries the space's original structured string, its timestamps, and denormalized memory/session counts. The 16-byte `space_id` is a non-invertible `UUIDv5` of that string, so the registry row is the only place the original survives — which is why `SPACE_LIST` echoes the string rather than the derived id.

## 14. When to use the default session vs. explicit ones

- **The default session** fits a space that holds one continuous body of memory — a single-purpose agent, or a prototype.
- **Explicit sessions** fit anything with conversations or runs: one session per conversation is the typical pattern, and it is what makes `FollowedBy` edges and "this conversation only" reads meaningful.
- **If you are reaching for sessions to keep two users' data apart, use spaces.** Sessions are not a wall.
