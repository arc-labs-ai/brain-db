# 09.06 FORGET

The FORGET primitive: delete a memory.

## 1. Semantic contract

```
FORGET(target, agent_id, mode, request_id) → ForgetResponse
```

The substrate:

1. Marks the memory(ies) as tombstoned.
2. Optionally zeroes the vector and text (hard mode).
3. After a grace period, reclaims the slot.

After FORGET succeeds, the memory is invisible to future RECALL, PLAN, and REASON.

## 2. The arguments

### target

What to forget. Either:

- `Memory(MemoryId)` — single memory.
- `Memories(Vec<MemoryId>)` — list.
- `Filter(ForgetFilter)` — declarative criteria.

The filter form is for bulk operations (e.g., "forget all memories in this context with salience < 0.1").

### mode

Either `Soft` or `Hard`:

- **Soft** (default): tombstone the memory; data remains for the grace period (default 7 days), then is reclaimed.
- **Hard**: tombstone AND immediately zero the vector and text.

Soft is the default — it allows undo within the grace period.

Hard is for compliance use cases (right-to-be-forgotten, sensitive data deletion).

### request_id

Required. Provides idempotency.

## 3. The response

```rust
struct ForgetResponse {
    forgotten: Vec<MemoryId>,        // Successfully forgotten
    not_found: Vec<MemoryId>,        // Already-gone IDs (silent no-op)
    failed: Vec<(MemoryId, Error)>,  // Per-memory errors
    grace_until: Option<u64>,        // For Soft, when reclaim happens
}
```

Per-memory errors mean some IDs failed but others succeeded. The agent can retry the failures.

## 4. Idempotency

If the same RequestId is sent twice, the substrate replays the original response. No double-forget.

Forgetting an already-forgotten memory is a no-op (returned in `not_found`); not an error.

## 5. The "soft forget" lifecycle

```
soft FORGET → tombstoned (active = false)
              ↓ (grace period, default 7 days)
              reclaimed (slot available for reuse)
              ↓
              MemoryId no longer valid (slot version incremented)
```

During the grace period:
- The memory doesn't appear in search results.
- The memory's MemoryId returns "not found" if queried directly (or returns the tombstoned record with `active=false`, depending on the operation).
- Operators can recover the memory via `ADMIN_RESTORE_FORGOTTEN` (if configured to allow).

After grace:
- The vector slot is zeroed and made available.
- The metadata row is deleted.
- The MemoryId is permanently invalid.

## 6. The "hard forget" lifecycle

```
hard FORGET → tombstoned + vector zeroed + text zeroed
              ↓ (grace period, default 7 days)
              reclaimed
```

The data is gone immediately. The grace period is just for slot management; there's nothing to recover.

Hard forget is irreversible. Use carefully.

## 7. The cascading question

When a memory is forgotten, what happens to:

### Edges

Edges referencing the forgotten memory become "stale":

- Outgoing edges: tombstoned (the source is gone).
- Incoming edges: tombstoned.

Tombstoned edges are eventually cleaned up by maintenance workers.

Queries during the cleanup window may see edges that lead nowhere — they skip them.

### Memories that DERIVED_FROM the forgotten

These keep existing. The DERIVED_FROM edge becomes a dangling reference (the source is gone). The derived memory stands on its own; only its provenance is lost.

We considered cascading FORGET (also forget memories DERIVED_FROM the target). Rejected — it's surprising semantics. The agent must request explicit cascade if desired.

### Consolidations

Consolidated memories aren't auto-forgotten when their sources are. The Consolidated stands as its own memory.

## 8. The "filter forget" path

```rust
brain.forget(
    target = Filter(ForgetFilter {
        context: Some(ctx_id),
        max_salience: Some(0.1),
    })
)
```

The substrate:

1. Discovers matching memories (a query-like step).
2. Forgets them in batch.

Limited to 100,000 memories per call. For larger bulk operations, the agent splits into multiple calls.

## 9. The "context delete" pattern

To delete an entire context:

```
brain.forget(target = Filter(ForgetFilter { context: Some(ctx_id) }))
brain.admin_context_delete(ctx_id)
```

Or use `ADMIN_CONTEXT_DELETE` directly (which combines the steps and handles paging for very large contexts).

## 10. The "agent delete" pattern

To delete all memories for an agent:

```
brain.admin_agent_delete(agent_id)
```

This is heavy — potentially millions of forgets. The substrate processes in batches, may take minutes for large agents.

## 11. Latency

For a single FORGET:

- p50: ~1 ms.
- p99: ~5 ms.

For batch (100 IDs in one FORGET): ~5-10 ms total.

For filter-based with large match set (10K memories): ~100-500 ms.

## 12. The "dangling reference" semantic

After FORGET, an agent that holds the MemoryId externally:

- Can't RECALL it (it's tombstoned).
- Can't UPDATE_KIND it.
- Can't LINK to it (it's not a valid target).

The agent should treat held MemoryIds as potentially-stale; check via RECALL or a direct lookup before using.

## 13. The "undo" facility

For Soft FORGET, an admin operation `ADMIN_RESTORE_FORGOTTEN` can undo the forget within the grace period:

```
brain.admin_restore_forgotten(memory_id)
```

This is admin-only (not exposed to typical agents) and works only during the grace period.

After grace, restoration is impossible — the slot is gone.

## 14. Failure modes

### MemoryNotFound

The MemoryId doesn't exist (never did, or was already reclaimed past grace). Returned in `not_found`; not an error.

### NotOwned

The memory belongs to a different agent. Error.

### TooManyMemories

Filter-based forget exceeds the per-call cap. Error.

### Conflict

The memory is in a transaction held by another client. The forget waits briefly; if the transaction doesn't commit, returns `Conflict`.

## 15. The privacy guarantee

Hard FORGET zeros the vector (in the arena file) and text (in the metadata file). After the OS flushes, the bytes are no longer recoverable from the file.

Filesystem-level recovery (e.g., undelete tools) might find fragments in unallocated blocks. For paranoid deployments, the substrate can also call `FALLOC_FL_PUNCH_HOLE` to encourage block release.

For full privacy guarantees, encrypt the underlying disk and rotate keys after FORGET. Brain doesn't manage encryption keys — that's deployment-level.

## 16. The reclamation timing

The grace period is configurable (default 7 days). After grace, a maintenance worker reclaims:

- Wakes periodically (every 5 min default).
- Identifies memories with `forgot_at + grace < now`.
- Reclaims in batches.

So the actual reclamation may be a few minutes after the grace expires. This isn't a problem — the memory is already invisible during grace.

## 17. The "FORGET while encoding" race

A subtle race: an ENCODE in flight while FORGET targets the same MemoryId. Possible only if:

- The agent has a stale MemoryId from a previous query.
- A new ENCODE happens to reuse the slot (after reclamation).

The slot version field prevents confusion: the old MemoryId has version N; the new memory has version N+1. The FORGET targeting version N hits the `not_found` path.

Practically, this race is rare (requires the agent to hold a stale ID across reclamation; the grace period makes this very unlikely).

## 18. The audit trail

FORGET operations are logged in the WAL (and visible in SUBSCRIBE). Operators can audit who forgot what when.

This is essential for compliance — "show me all forgets in the last 30 days".

## 19. The "forget for compliance" workflow

A typical right-to-be-forgotten flow:

```
1. Identify memories: brain.recall("user X's data", filter=...)
2. Hard forget: brain.forget(memory_ids, mode=Hard)
3. Verify: brain.recall("user X's data") returns empty
4. Audit log entry confirms.
```

The substrate's hard forget zeros the bytes; the audit log preserves the forget event itself (the memory's text is gone, but the fact that something was forgotten remains).

## 20. Limits and caps

- Single FORGET: up to 1000 memories per request.
- Filter FORGET: up to 100,000 memories per request.
- Per-agent rate limit: 100 FORGETs per second.

These prevent runaway deletion. For bulk operations beyond the caps, the agent paginates.

---

*Continue to [`07_link_unlink.md`](07_link_unlink.md) for LINK and UNLINK.*
