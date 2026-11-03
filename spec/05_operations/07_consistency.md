# 05.07 Consistency Model

The consistency guarantees Brain provides for its operations.

## 1. The model: per-shard linearizable writes, immediately-visible semantic reads

- **Writes within a shard** are linearizable. There is a clear order; later writes see earlier ones.
- **A write's own memory is searchable the moment its ack returns.** The HNSW insert happens inside the write submission, before the metadata commit and before the ack, and the index serves a freshly-inserted vector from a pending buffer at exact cosine until a later flush folds it into the main graph. There is no publication lag to wait out and nothing to opt into.
- **What *is* eventual is everything derived from the write**, not the write: the lexical index's group commit, and the four async derivation stages (auto-edge, temporal-edge, extractor, HyPE).

There is **no `consistency` parameter and no `ReadAfterWrite` mode.** It would have nothing to wait for on the path it was designed for, and it could not deliver what it promised on the paths that really are asynchronous — a flag that blocked until extraction finished would turn every read into a write-latency read.

## 2. Why the semantic index is synchronous

The alternative — insert into the index from a background worker and let reads see an epoch-old snapshot — buys a shorter write path and pays for it with a class of bug the caller cannot work around: `encode` returns success, the immediately following `recall` misses, and nothing in either response says why. Every client then grows a sleep or a retry loop.

Inserting on the write path costs the encode a buffer append, which is cheap because the pending buffer is a flat vector searched by brute force. The brute-force scan is bounded by how often the flush runs, not by corpus size, so the cost stays with the writer instead of leaking into every reader's mental model.

A tombstone is visible on the same terms: `FORGET` marks the id in the pending tombstone set, and the very next search filters it out.

## 3. What remains eventual, and for how long

| Derived state | Visible after | Observable via |
|---|---|---|
| Lexical (tantivy) index entry | the group commit — whichever of `tantivy_commit_n` (256 writes) or `tantivy_commit_ms` (1 s) trips first | the lexical lane of a later RECALL |
| `SimilarTo` auto-edges | the auto-edge worker's next cycle (~100 ms) | `StageCompleted(AutoEdge)` |
| `FollowedBy` temporal edges | the temporal-edge worker's next cycle (~100 ms) | `StageCompleted(TemporalEdge)` |
| Entities / statements / relations | the extractor worker's next cycle (~1 s) plus LLM latency | `StageCompleted(Extractor)` |
| HyPE question vectors | the HyPE stage's completion | `StageCompleted(Hype)` |

Each of these announces itself. A client that needs to act when a derivation lands subscribes to that write's `StageCompleted` events — `ENCODE` with `wait: ack`, then `SUBSCRIBE` scoped to `memory_ids: [the id]` — or sets `wait: derived` on the encode and takes the whole timeline in the response. Both are explicit, bounded, and tell the caller *which* stage it is waiting for; a generic consistency flag could do none of those things.

## 4. The "per-shard" qualifier

Linearizability is per-shard, not global.

A space's data lives on exactly one shard, so for one space there is a clear order over all its operations. Across shards — a namespace-scoped RECALL fanning out, or two spaces that hash differently — each shard has its own ordering and cross-shard ordering is not enforced.

## 5. Why not global linearizability

Global linearizability across shards would require distributed consensus on operation order, coordinated commits, or a global clock. Each adds latency and complexity to every write. Brain's per-shard linearizability plus the space-always-on-one-shard rule sidesteps all three: the only operations that could observe a cross-shard ordering gap are namespace-wide reads, which are rank-merged anyway and have no ordering semantics to violate.

## 6. The transactional consistency

Within a transaction:

- Reads see the transaction's own pending writes (read-your-writes).
- Reads see committed state from before the transaction started.
- Reads don't see other transactions' uncommitted writes.

This is **read committed** with **read-your-writes** within the transaction.

Brain does not provide stronger isolation (snapshot isolation, serializable). Brain's writer-per-shard discipline obviates many of the problems stronger isolation would solve.

## 7. The SUBSCRIBE consistency

SUBSCRIBE delivers events in WAL order per shard. The client sees:

- All events from the requested from_lsn onward.
- In WAL order (per shard).
- At-least-once.

For a client that wants exactly-once: dedupe by LSN.

For cross-shard SUBSCRIBE: events from each shard arrive in their respective WAL orders; cross-shard order isn't strict.

## 8. What a client actually observes

### encode then recall — sees it

```
r = brain.encode("hello world")   # returns on the durable ack
brain.recall("hello world")       # includes the memory
```

The memory's vector is in the index before `encode` returns. No wait, no flag.

What the second call does *not* yet see is the memory's **derived** state: the entities extracted from it, the `SimilarTo` edges to its neighbours, or its HyPE questions. A recall that depends on those should follow the write's `StageCompleted` events rather than retry blindly.

### link then plan — may not see it yet

```
brain.link(A, B, Caused)
brain.plan(start, goal)           # may traverse the pre-edge graph
```

`LINK` is durable on return, and the edge row is committed; what lags is the graph retriever's view of it. Use the edge feed (`EdgeAdded`) if the next step depends on the edge being traversable.

### forget then recall — does not see it

```
brain.forget(memory_id)
brain.recall("...")               # excludes the forgotten memory
```

`FORGET` marks the id in the index's tombstone set synchronously, so the very next search filters it. The cascade that tears down the memory's graph rows is asynchronous, so a statement sourced from it may linger for one worker cycle.

## 9. The "wait for my write" pattern

There is no consistency flag to add, because the thing worth waiting for is a named stage, not a generic watermark. Two supported shapes:

```
# Take the whole derivation timeline in the response.
r = brain.encode(text, wait="derived")
r.trace.stages   # per-stage status, latency and artifacts

# Or stay non-blocking and watch it land.
r = brain.encode(text)                      # wait: ack, the default
brain.subscribe(filter={"memory_ids": [r.memory_id]})
#   → StageCompleted(AutoEdge) / (TemporalEdge) / (Extractor) / (Hype)
```

`wait: derived` is bounded by the shard's trace drain window, so a stalled worker cannot hang the call; a straggler is reported as `Timeout` and still lands in `MEMORY_INSPECT` later.

## 10. Idempotency, not re-reads

A client confirming its own write does not need to read at all: `ENCODE` and `FORGET` are idempotent on `request_id`, and the response carries `memory_id` and `lsn`. Re-issuing the same request returns the same answer rather than writing twice.

## 11. Per-request choice

There is nothing per-request to choose on the read side. The one request-level knob that changes read semantics is `txn_id`, which selects read-your-writes over a transaction's pending buffer — and that is a consequence of being in a transaction, not a consistency preference.

## 12. The "no global timestamps" rule

Brain doesn't have a global timestamp service. Per-shard timestamps come from the local clock; clocks across shards may differ by milliseconds.

For cross-shard ordering questions ("did A on shard X happen before B on shard Y?"), Brain doesn't have a definitive answer. Within a shard, LSN order is the authority.

## 13. The "monotonic" property

For an agent on a single shard:

- The caller's writes are seen in order.
- The caller's reads see a monotonically-advancing view.

In particular: a recall that sees memory X means future recalls will also see X (until X is forgotten).

This is **monotonic reads** for a single client on a single shard. For multi-client or multi-shard, weaker.

## 14. The "session consistency" question

Some systems offer "session consistency" — within a connection, reads always see that connection's own writes.

Brain gives this unconditionally for the memory itself, and to every reader, not just the writing connection: the index insert happens before the ack, so there is no window in which a committed memory is unsearchable. Nothing about the connection has to carry consistency state, because there is no weaker mode to upgrade from.

What a connection does **not** automatically see is the derived state of its own write. That is a property of the derivation being asynchronous, not of session scoping, and a per-connection flag could not change it.

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

- Nothing to configure: the memory is searchable on ack.

For workloads where the client immediately checks its own write:

- Read normally. The memory is there.
- If the check depends on extraction or edges, subscribe to that write's `StageCompleted` events instead of re-reading.

For workloads that are mostly reads with rare writes:

- Nothing changes. There is no per-read wait to avoid.

## 18. The "SUBSCRIBE" alternative

For applications that need to react to writes — and in particular to the *derived* state of a write, which a read cannot wait for — SUBSCRIBE is the right tool. It is push-based and it names which stage completed, so a consumer reacts to the thing it actually depends on rather than to a timer.

A common architecture:

- Writes happen.
- A SUBSCRIBE consumer mirrors the state to a downstream system.
- The downstream system has the up-to-date view.

This avoids the read-after-write coordination — the consumer is always near-real-time.

## 19. The cross-space consistency

Operations across spaces are not expressible; a space is a hard wall. Brain doesn't coordinate.

For applications that need cross-space coordination, layer logic on top: an external coordination mechanism (a queue, a lock service) ensures order.

## 20. The "future stronger consistency" question

A future enhancement could add stronger consistency modes:

- Snapshot isolation.
- Linearizable reads across shards.
- Causal consistency (tracked happens-before).

None is needed today. Within a space — which is one shard — writes are ordered and immediately visible, and the only genuinely asynchronous state announces its own completion. The open questions are all cross-shard, and cross-shard reads are rank-merged, which has no ordering semantics to strengthen.

---

*Continue to [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md) for unresolved questions.*
