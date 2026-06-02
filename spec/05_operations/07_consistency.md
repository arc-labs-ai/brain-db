# 05.07 Consistency Model

The consistency guarantees Brain provides for its operations.

## 1. The default: per-shard linearizable writes, eventual reads

Brain's default consistency model:

- **Writes within a shard** are linearizable. There's a clear order; later writes see earlier writes.
- **Reads** are eventually consistent. They see writes that have been "published" — typically within ~10 ms of the write's WAL fsync.
- **Read-after-write** is opt-in via a request flag.

This balances performance (eventual reads are fast) with usability (writes are clearly ordered).

## 2. Why eventual by default

The publication step (advancing the epoch) is asynchronous from the WAL fsync. If reads always waited for the latest publication:

- Read latency would include the publication interval (~10 ms).
- Reads would be coupled to writes (a busy writer slows down reads).

By making reads see "as-of last publication" by default, reads run lock-free against a stable snapshot. Latency is just the search/lookup cost.

For workloads that need fresher reads, the opt-in flag pays the cost only when needed.

## 3. The "read-after-write" guarantee

When a request specifies `consistency: ReadAfterWrite`:

- Brain ensures the read sees all writes durably committed before the request was issued.
- Specifically: the read waits until the per-shard publication LSN ≥ the latest committed LSN.

The wait is typically a few milliseconds. Bounded by the publication interval.

For reads with `consistency: ReadAfterWrite` after a specific write:

- If the request includes the write's `last_lsn` (returned in the encode response), Brain waits exactly until that LSN is published.
- This is more precise than the "wait for latest" mode.

## 4. The "per-shard" qualifier

Linearizability is per-shard, not global.

For a single agent on a single shard: clear ordering of all operations.

For an agent spanning multiple shards (rare): each shard has its own ordering; cross-shard ordering isn't enforced.

For cross-agent operations: similar — each agent's shard is its own world.

## 5. Why not global linearizability

Global linearizability across shards would require:

- Distributed consensus on operation order.
- Coordinated commits.
- Or a global clock.

Each adds latency and complexity. Brain's per-shard linearizability + agent-typically-on-one-shard architecture sidesteps these.

For the rare cross-shard agent, ordering is best-effort. The agent should design for this.

## 6. The transactional consistency

Within a transaction:

- Reads see the transaction's own pending writes (read-your-writes).
- Reads see committed state from before the transaction started.
- Reads don't see other transactions' uncommitted writes.

This is **read committed** with **read-your-writes** within the transaction.

Brain does not provide stronger isolation (snapshot isolation, serializable). Brain's writer-per-shard discipline obviates many of the problems stronger isolation would solve.

## 7. The SUBSCRIBE consistency

SUBSCRIBE delivers events in WAL order per shard. The client sees:

- All events from the requested start_lsn onward.
- In WAL order (per shard).
- At-least-once.

For a client that wants exactly-once: dedupe by LSN.

For cross-shard SUBSCRIBE: events from each shard arrive in their respective WAL orders; cross-shard order isn't strict.

## 8. The "stale read" examples

Examples of staleness in the default eventual model:

### Example 1: encode then recall

```
brain.encode("hello world")    # returns successfully
brain.recall("hello world")    # may not include the just-encoded memory
```

Without `ReadAfterWrite`, the recall might miss it. Within ~10 ms, future recalls will include it.

### Example 2: link then plan

```
brain.link(A, B, CAUSED)
brain.plan(start, goal)        # may not include the just-added edge
```

Similar — the edge exists durably, but PLAN's traversal may use the pre-edge graph state if the publication hasn't happened.

### Example 3: forget then recall

```
brain.forget(memory_id)
brain.recall("...", ...)       # may include the forgotten memory briefly
```

The tombstone is set durably, but RECALL may have a stale view.

## 9. The fix for staleness

In each example, adding `consistency: ReadAfterWrite` to the read fixes it:

```
brain.recall("hello world", consistency=ReadAfterWrite)
```

The cost: ~10 ms of waiting (until publication catches up).

## 10. The "read your most recent write" pattern

A common need: the client wants to confirm its own write is visible. Pattern:

```
result = brain.encode("...")
# result.lsn is the LSN of the encode

confirmation = brain.recall(
    "...",
    consistency=ReadAfterWrite(after_lsn=result.lsn)
)
```

The recall waits exactly until the encode's LSN is published. Precise.

A client can implement a "wait for write" helper over this.

## 11. The "consistency" parameter precedence

The consistency parameter is per-request. Different requests on the same connection can have different consistency:

```
brain.encode(text)                                      # write
brain.recall(cue, consistency=Eventual)                 # fast eventual read
brain.recall(cue2, consistency=ReadAfterWrite)          # waits for catchup
```

This lets the client choose per-call based on the workflow.

## 12. The "no global timestamps" rule

Brain doesn't have a global timestamp service. Per-shard timestamps come from the local clock; clocks across shards may differ by milliseconds.

For cross-shard ordering questions ("did A on shard X happen before B on shard Y?"), Brain doesn't have a definitive answer. Within a shard, LSN order is the authority.

## 13. The "monotonic" property

For an agent on a single shard:

- The agent's writes are seen in order.
- The agent's reads (from this same agent) see a monotonically-advancing view.

In particular: a recall that sees memory X means future recalls will also see X (until X is forgotten).

This is **monotonic reads** for a single client on a single shard. For multi-client or multi-shard, weaker.

## 14. The "session consistency" question

Some systems offer "session consistency" — within a session (typically a connection), reads always see the session's writes.

Brain provides this via the `ReadAfterWrite` opt-in. The session itself doesn't carry consistency state; each request explicitly opts in if needed.

Brain could make ReadAfterWrite the connection default — but this would slow down most reads (which don't need it). The per-request flag is more flexible.

## 15. The "async" propagation

Some operations propagate asynchronously:

- HNSW publication: ~10 ms after WAL fsync.
- Salience updates: batched; visible after batch flush.
- Statistics: updated periodically (every 5 sec).

For reads of statistics, expect them to lag by up to a few seconds.

For reads of memories, the publication interval applies.

## 16. The "eventual" bound

The "eventual" in eventual consistency has a bound:

- Per-shard publication: ~10 ms (configurable; default 10 ms).
- HNSW catchup after large rebuilds: minutes (during a rebuild, the old HNSW serves; the new one replaces atomically when ready).

So eventual doesn't mean "could be hours". It's bounded.

For recovery scenarios (after restart, partial failure), eventual may briefly stretch — until WAL replay completes. Once the shard is "ready", eventual is back to ~10 ms.

## 17. The "consistency cost" guidance

For workloads that don't need read-your-writes:

- Use Eventual (default).
- Save the ~10 ms wait per read.

For workloads where the agent immediately checks its writes:

- Use ReadAfterWrite for the post-write reads.
- Other reads can be Eventual.

For workloads that are mostly reads with rare writes:

- Eventual is fine. The lag is invisible.

## 18. The "SUBSCRIBE" alternative

For applications that need to react to writes immediately (rather than poll), SUBSCRIBE is the right tool. The latency from write to delivered event is similar (~10 ms), but it's push-based.

A common architecture:

- Writes happen.
- A SUBSCRIBE consumer mirrors the state to a downstream system.
- The downstream system has the up-to-date view.

This avoids the read-after-write coordination — the consumer is always near-real-time.

## 19. The cross-agent consistency

Operations across agents (rare) have weak consistency. Brain doesn't coordinate.

For applications that need cross-agent coordination, layer logic on top: an external coordination mechanism (a queue, a lock service) ensures order.

## 20. The "future stronger consistency" question

A future enhancement could add stronger consistency modes:

- Snapshot isolation.
- Linearizable reads (waiting for global publication).
- Causal consistency (track happens-before).

Currently, the eventual + opt-in ReadAfterWrite is the simplest model that meets typical agent needs. Stronger modes are open questions.

---

*Continue to [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md) for unresolved questions.*
