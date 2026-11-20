# 14.02 Writer Model

> **TL;DR.** Brain's writer-side concurrency contract: one writer handle per shard, reached concurrently by a bounded number of in-flight requests whose commits serialize structurally (the redb transaction body holds no await on a single-threaded executor); a contiguous durable watermark that records what recovery may skip; reclamation by `Arc` refcount over whole published index generations (no epoch protocol); and atomic publication (prepare changes off-graph, ArcSwap them in).

## One Writer per Shard

The most important concurrency invariant: each shard has exactly one writer handle, and at most one open redb write transaction.

## 1. The discipline

For each shard:

- Exactly one writer handle is responsible for all mutations.
- All mutating operations (ENCODE, FORGET, LINK, UNLINK) go through it.
- Every request runs on the shard's dedicated executor.

Other tasks on the shard handle reads, connection management, etc., but they don't mutate shard state directly; they call the writer handle.

The handle is shared, not owned by a single task. The shard's receive loop admits each dispatch under a permit and detaches it onto the same executor, so several requests may be inside the writer at once, at different stages.

## 2. Why this works

The one-writer-per-shard model gives several benefits:

- **No writer-vs-writer locking.** Within a shard there is one writer handle and one metadata file. No lock is taken to order writes against each other.
- **Structural serialization.** The region that opens the redb write transaction, applies every phase, stamps the idempotency row and commits is fully synchronous. On a single-threaded executor a task inside it cannot yield, so a second request cannot enter it. Writes serialize because there is nowhere for the executor to go, not because anything waits.
- **Concurrency where it pays.** The stages that *do* await — the WAL append and its group-commit fsync — overlap across requests. That is what gives the group committer more than one record to fold into a single `fdatasync`.
- **Simple control flow.** No "what if two writers see different states" reasoning.
- **Predictable throughput.** The per-second rate is bounded by group commit and storage, not by lock contention.

The one invariant everything rests on: **no await point between `begin_write` and `commit`.** Introducing one would let a second request open a transaction on the same file and deadlock the shard.

## 3. What one write does

Per request, in order:

1. Check the durable idempotency cache; a replay returns the stored ack without touching storage.
2. Append the write's WAL records as a single batch — one typed payload for a single-phase write, `TxnBegin` + N payloads + `TxnCommit` for a multi-phase one — and await the group-commit fsync. The drain reserves the assigned LSN range.
3. Apply HNSW side effects. These run *before* the transaction opens, so the transaction's lifetime stays minimal and an HNSW failure abandons the write before any metadata commits.
4. Open the redb write transaction; apply every phase; stamp the idempotency row in the same transaction; commit. No await anywhere inside.
5. Resolve the reserved LSN range against the shard's durable watermark.
6. Publish events, enqueue post-commit worker stages, return the ack.

Durability is observed at step 2; visibility and the client's ack at step 4.

## 4. The admission bound

The shard admits a bounded number of concurrent dispatches, held as permits:

```rust
fn max_in_flight_for_wal(cfg: &WalConfig) -> usize {
    (cfg.group_commit.max_batch_bytes / WAL_RECORD_BUDGET_BYTES).max(2)
}
```

The bound is derived, not configured separately. `WAL_RECORD_BUDGET_BYTES` is 4 KiB: the dominant write record is an `EncodePayload`, whose 384-element `f32` vector alone is 1536 bytes, plus memory text, the rkyv response payload, inline edges, a 32-byte header and a footer. One page sits comfortably above that floor and stays honest about the text.

At the default 60 KiB batch budget the bound is **15**. The floor of two is the smallest bound that means anything: one would reproduce serial dispatch, zero would wedge the shard. Because the bound tracks the operator-tunable WAL configuration, widening the commit window and its batch budget gives the matching concurrency.

When every permit is out the shard parks on `recv_async()`. Requests stay in the channel and are served in arrival order; none is shed. Parking the receive also parks the shard's cheap requests (Ping, slot allocation, snapshot admin). That is deliberate: in-order service of a saturated shard is preferable to a second scheduling lane with unreasoned starvation behavior.

## 5. The WAL-drain channel

The writer is `Send + Sync` behind an `Arc`; the shard's `Wal` is `!Sync` (interior `RefCell` plus a Glommio-bound committer task). A bounded `flume` channel bridges them, drained by a loop on the shard's own executor that calls `Wal::append_many`.

Default capacity is 1024 records. A full channel awaits a slot before the batch is enqueued, so backpressure propagates to the submitting request rather than growing without bound.

The whole batch for one write crosses this boundary in a single hop, and the underlying WAL writes the records back-to-back, so the group committer can fold them into one `fdatasync`.

## 6. Group commit

The WAL's group committer owns the active segment and batches records from all concurrent appenders:

1. Await the first record of a batch.
2. Wait up to the commit window (100 µs default) for more, or until the buffer reaches `max_batch_bytes` (60 KiB default).
3. One `flush_durable` — a single `pwritev2(RWF_DSYNC)` — for the whole batch.
4. Signal every pending ack with its LSN.

Group commit amortizes the fsync cost. Without it, each operation pays ~0.3 ms for fsync; with batches of 32, that's ~10 µs per operation. Concurrent dispatch is what makes a batch of more than one reachable under load: a second appender has to be able to arrive while the first is waiting.

Errors are sticky. After a failed flush the WAL is latched broken; every in-flight handle and every later append gets `WalBroken` for the life of the committer.

## 7. The "writer is the source of truth" rule

The writer handle is the single source of truth for shard state. Other tasks observe state through:

- redb read transactions: see committed data.
- ArcSwap.load() on the HNSW: see published data.
- Direct mmap reads: see arena bytes.

Nothing modifies the canonical state except through the writer handle. There are caches and derived structures, but they all derive from what it has produced.

Note which store is canonical for a memory's vector while the process is up: redb, not the arena. The live write path puts the write-time vector in redb and never writes it into an arena slot; the arena is filled by replay ([`../08_storage/03_write_path.md`](../08_storage/03_write_path.md) §5). A runtime index rebuild must therefore read redb — rebuilding from the arena would silently drop every memory encoded in the current run.

## 8. The cross-task communication

A request calls the writer handle directly and awaits the result:

```rust
async fn execute_encode(&self, plan: EncodePlan) -> Result<EncodeResponse> {
    // ... embed, reserve a MemoryId, build the phases ...

    let ack = self.writer.submit(write).await?;
    Ok(self.build_response(ack))
}
```

There is no writer inbox and no per-operation reply channel: the dispatch *is* the write. The request is async-blocked (not OS-blocked) across its WAL fsync, and resumes to run the transaction itself.

One channel does exist on this path, and it is a different thing: the bounded WAL-drain channel of §5, which exists only because the `Send + Sync` writer handle cannot hold the `!Sync` `Wal`. Records cross it; requests do not.

A group commit therefore has several *requests* awaiting their own acks from the committer, each of which then goes on to commit its own transaction. The committer signals them individually when the batch is durable.

## 9. Write-path failure modes

If a dispatch panics:

- Glommio logs the panic for that detached task.
- The request's reply channel drops; the caller sees the connection close and can retry.
- Its reserved LSN range is abandoned by the `Drop` guard, so the durable watermark is not left pinned.
- Other in-flight requests and the shard's receive loop continue.

If the shard's executor thread itself dies, the shard's channel closes, its receive loop exits, and the shard is gone until restarted. Restart involves:

- Replaying the WAL from the checkpoint's `durable_lsn`.
- Re-seeding the slot counter from the persisted high-water mark.
- Re-opening the WAL and signalling readiness before any request is accepted.

In-flight operations that were waiting on acks see the connection close; clients can retry, and the idempotency row makes a retry of an already-committed write return the original ack rather than writing twice.

## 10. The "one transaction at a time" invariant

Brain enforces one open write transaction per shard:

- Each shard owns exactly one metadata file and one writer handle.
- Only the shard's own executor runs dispatches for that shard; the handle is never reached from another thread.
- The transaction body is synchronous, so the executor cannot interleave a second request into it.
- No task mutates shard state except through the writer handle.

There is no runtime lock and no debug assertion enforcing this. The guarantee is the absence of an await point, which is why §2's rule is stated as an invariant rather than a style preference.

## 11. The "the writer doesn't read directly" subtlety

A write also reads shard state — it needs to look up things to apply operations. It does this via the same primitives readers use (redb read transactions, etc.), inside the dispatch.

So a write is also a reader, but one that is also doing writes.

## 12. The cooperative-yield within a write

A write yields cooperatively:

- At the WAL append and its group-commit fsync.
- At any await in the request's own pre- and post-commit work.
- Never between `begin_write` and `commit`.

Yields let other tasks (readers, background workers, other in-flight writes) run. The WAL fsync is the longest yield on the path, and it is precisely the point at which another request can reach the log and join the same commit batch.

## 13. The write path's resource bound

Resource use for the write path on one shard:

- CPU: ~30-50% of the shard's core under sustained load.
- Memory: a few MB (in-flight requests, WAL-drain channel, batch buffers), bounded by the admission permits and the 1024-record drain channel.
- I/O: bounded by WAL fsync rate.

Other tasks share the remaining CPU. For a 16-thread server with 16 shards: each shard uses about half a core for writes; the rest is for reads.

## 13a. The durable watermark

Each shard publishes a single number: the highest WAL LSN such that every record at or below it is accounted for in redb. Recovery skips everything at or below it, and a checkpoint records it as `durable_lsn`, so the number has to be a **contiguous committed prefix** and not a maximum.

The distinction is load-bearing exactly because commits are not ordered by LSN. Two requests may append in the order (A, B) and commit in the order (B, A). A watermark that took the maximum committed LSN would publish B's position while A was still unapplied; a crash in that window would let recovery skip A's record, and A would be silently lost despite its ack.

The mechanism:

- The WAL drain **reserves** each writer batch's inclusive LSN range as the LSNs are assigned. The drain is sequential, so reservations arrive in ascending order and the queue's front is the minimum outstanding LSN.
- After a successful `commit()`, the write **commits** its range: the range leaves the outstanding queue and raises the highest-committed mark.
- A write that fails anywhere after its records are durable **abandons** its range, released by a `Drop` guard so a cancelled or early-returning future cannot pin the frontier forever.
- The published frontier is `min(highest_committed, front.first - 1)`, or `highest_committed` when nothing is outstanding.

Three sources of LSNs never reach a redb commit, and none of them may be left holding a reservation — a reservation that nothing resolves would hold the frontier down for the life of the shard:

- **Checkpoint records.** `CHECKPOINT_BEGIN` / `CHECKPOINT_END` are appended directly by the checkpoint path and mutate no redb state, so they are never reserved at all.
- **Subscribe-event records.** Change-feed events ride the WAL under the same record kinds as durable writes, distinguished by a flag. The drain reserves a batch only when every record in it is unflagged, so these are never reserved either. Recovery skips them on the same flag.
- **Writes that fail before commit.** These *are* reserved — the failure happens after the LSNs are assigned — and are then abandoned by the `Drop` guard. A later real commit closes the prefix across the gap, which matches recovery's treatment of the failed write. The drain abandons the range itself if the submitting task has disappeared before it could be handed the LSNs, since nothing would otherwise resolve it.

Snapshot code reads the frontier between awaits while the writer and the drain are updating range state. The frontier is therefore an atomic, published beside a mutex-guarded reservation queue: the queue is protected, and the read never waits on an in-flight write.

## 14. The exception: external writers (replication)

For HA / replication, a "follower" shard takes writes from a "leader" shard, not from clients. The follower's writer applies replicated operations.

Brain doesn't currently have replication. A future addition would extend the writer model:

- A leader writer takes client requests.
- A follower writer takes replicated operations.
- Both apply to the same shard's state — but they're never both active at once (one is leader, one is follower).

## 15. The implications for clients

Clients:

- Doesn't manage writer state.
- Submits requests; awaits responses.
- Doesn't need to know about writers.

The writer model is an internal detail. Clients see a "submit operation" interface.

## 16. The implications for testing

Tests can:

- Spawn a single-shard server.
- Send concurrent operations.
- Verify that each write's effects are all-or-nothing per shard.

Tests can't:

- Open two write transactions on one shard's metadata file.
- Bypass the writer handle.

Brain's API doesn't expose direct mutation; it always goes through the writer.

Note what is *not* promised: concurrent requests to one shard are not ordered by arrival. Two writes that overlap may commit in either order, and a test that asserts LSN order implies commit order is asserting something the model does not provide.

## 17. Snapshots do not pause writes

Taking a snapshot does not drain in-flight writes and does not stop the shard accepting new ones. The sequence appends `CHECKPOINT_BEGIN`, `msync`s the arena, then writes `CHECKPOINT_END` carrying the durable watermark clamped to the WAL tail sampled at `CHECKPOINT_BEGIN`.

The watermark is what makes the pause unnecessary. A write that is WAL-durable but whose redb commit has not run sits strictly above the frontier, so the checkpoint does not claim it and recovery replays it. A drain would buy consistency the watermark already guarantees, at the cost of a stall.

## 18. The shard-ready check

A shard signals readiness only once its WAL is open; a WAL IO failure, or the executor thread exiting before it signalled, fails the spawn rather than leaving a dead shard serving requests. Recovery runs before the shard's receive loop starts, so there is no window in which writes are accepted against un-replayed state.

## 19. The throughput math

Per-shard throughput:

- 10K ops/sec sustained (limited by WAL fsync + redb commits).
- 30-50K ops/sec burst (with full batching, no fsync stall).

The sustained figure is a commit-rate bound, not a dispatch bound: requests overlap, but every one of them passes through the same serialized transaction region, and the redb commit is a real fsync.

Scaling beyond per-shard limits: more shards. Per-shard performance is bounded; Brain's total throughput is N × per-shard.

## 20. The summary

One writer per shard, with an await-free commit, is the keystone of Brain's concurrency model. It:

- Eliminates writer-vs-writer contention without a lock.
- Lets requests overlap wherever they are waiting on I/O, so group commit has records to group.
- Makes each write's effects atomic.
- Bounds per-shard throughput predictably.

The trade-off — bounded per-shard throughput, and a commit order that need not match LSN order — is acceptable because Brain scales by adding shards, and because the durable watermark tracks a contiguous prefix rather than assuming commits are ordered.

---

## Reclamation

How Brain frees index state that may have in-flight readers.

## 1. The problem

When the writer replaces a data structure (the HNSW), readers in flight may still be using the old version. The old version can't be freed immediately — readers would be left holding a dangling view.

Brain can't keep all old versions forever either; that's a leak.

Brain requires a way to know "when is it safe to free this old version?" The answer is `Arc`'s reference count, and that is the whole of the mechanism.

## 2. Published generations, not reclamation epochs

The memory index is an `ArcSwap<MainEpoch>`, where a `MainEpoch` is an immutable published HNSW plus a generation number. Publishing builds a new `MainEpoch`, bumps `epoch_id` with `wrapping_add(1)`, and `store`s it.

"Epoch" in Brain names that **published generation** — the identity of one immutable snapshot of the graph. It does not name a reclamation interval, and nothing pins to it. A reader calls `load()`, gets an `Arc` to whichever generation was current, and holds it for the duration of its search; the generation it holds cannot be mutated, because a publish replaces the whole structure rather than editing it.

Reclamation follows from that directly: when the last reader drops its `Arc`, the old `MainEpoch` — the graph and everything in it — is dropped. There is no free-list, no deferred-destroy queue, no grace period to wait out, and no pin for the writer to watch.

## 3. No epoch-based reclamation protocol

Brain does not use an epoch-based reclamation protocol, and does not use `crossbeam-epoch`. There are no reader pins, no `Guard`, no `defer_destroy`, no global reclamation counter, and no writer-side wait for readers to leave an interval.

That is not a gap to be filled later. Epoch-based reclamation earns its complexity when a structure is mutated in place while lock-free readers walk it, so individual nodes become unreachable at a moment when readers may still be inside them. Brain's index is never mutated in place under readers: it is replaced wholesale. Once the unit of reclamation is the entire published structure, `Arc`'s refcount already answers the only question an epoch protocol would have answered, and answers it exactly rather than conservatively.

This matches [09.04 Indexing Concurrency](../09_indexing/04_concurrency.md) §5: reclamation here needs no epoch protocol.

## 4. Where the generation number is used

`epoch_id` is observability and reconciliation state, not a safety mechanism:

- A flush reports the generation it published, so the maintenance worker can tell whether its rebuild landed.
- A reader can ask which generation it is seeing.

It is deliberately a wrapping counter. Nothing compares two generation numbers for ordering across a wrap, and nothing holds a generation number as a capability, so wrap-around is a non-event rather than a hazard to size a counter against.

## 5. Tombstones: nodes are not removed at all

HNSW has no efficient eager deletion, so a forgotten memory is **tombstoned** rather than removed. Its node stays in the graph and is filtered out at search time.

The filter state is a bit-packed bitmap over the internal `u32` ids that the graph library returns — `Vec<u64>`, 64 bits per word, growing lazily so a fresh bitmap allocates nothing. At the per-shard ceiling of 10M memories it costs ~1.25 MB, where a `Vec<bool>` would cost 10 MB. Every operation is idempotent: setting an already-set bit, or clearing an already-clear one, is a no-op and does not perturb the count. The count itself is a running counter updated only on real bit transitions, so reading it is O(1) rather than an O(N/64) population count per request.

Above the published bitmap sits a **pending tombstone overlay** — a set of ids tombstoned since the last publish. The overlay wins everywhere: a search excludes those ids from the main graph's results before merging in the pending tier, and the presence and tombstone predicates consult the overlay first.

So there is no per-node free to schedule, and no window in which a reader might visit a node whose memory has been released. A tombstoned node's memory is reclaimed only when the whole generation is replaced: the next rebuild simply omits the tombstoned memories, so the new index starts with a fresh, empty bitmap and the old nodes go away with the old `MainEpoch`.

## 6. The two tiers a reader sees

A reader's view is the published generation plus the pending buffer:

- **Main** — the immutable published HNSW, searched under the `Arc` from `load()`, with the pending tombstone overlay and the caller's filter applied to candidates.
- **Pending** — recent inserts not yet folded in, held with full-precision vectors and brute-forced at exact cosine, behind an `RwLock`.

Results from the two tiers are merged and deduplicated by `MemoryId`, preferring the pending score on a collision so the latest vector wins. Both tiers are cosine similarity, so the merge is a plain descending sort.

This is why publication is not a visibility gate: a freshly-inserted vector is found in the pending tier before any publish folds it into the main graph.

## 7. The safety property

The invariant, stated without epochs:

```
A reader holding an Arc to generation E
  searches a structure that is never mutated,
  and whose memory is released only after that Arc is dropped.
```

It is enforced by the type system and `Arc`, not by a protocol Brain has to get right at runtime. The writer handle is deliberately not `Clone`, which enforces one writer per shard at the type level.

## 8. The performance characteristics

- `ArcSwap::load()`: an atomic read plus a refcount bump.
- `store()`: an atomic pointer swap.
- Dropping the last `Arc` to an old generation: one deallocation of the whole graph, paid by whichever reader happens to be last.

The publish cost is dominated by *building* the new index, not by publishing it. The swap itself is lock-free and wait-free: a publish never blocks a reader, a read never blocks a publish, and a publish completes in bounded time regardless of how many readers are active.

## 9. The one cost this model does carry

Because the unit of reclamation is the whole structure, both the old and the new index are live between the swap and the moment the last reader drops the old `Arc`. Memory peaks across that window.

An epoch protocol with per-node reclamation would trade that peak for a steady-state cost — and for the obligation to reason about pins, grace periods and stuck readers. Brain takes the peak. It is bounded by one extra copy of the graph and by the duration of the longest in-flight search; a long-pinned reader cannot accumulate unbounded garbage here, because there is nothing to accumulate beyond the single superseded generation each publish leaves behind.

## 10. The implication for higher-level code

Executors and planners never see reclamation at all. They call a search; they get results. There is no guard to scope, no pin to release, and no lifetime to thread through the query layer.

## 11. The summary

Reclamation of index state is `Arc` refcounting over whole published generations:

- The writer builds a new index and `ArcSwap`s it in.
- Readers hold an `Arc` to the generation they loaded; it is immutable for as long as they hold it.
- The old generation is dropped when its last reader lets go.
- Tombstoned nodes are filtered, never individually freed; they disappear with the generation that contained them.

The mechanism is deliberately smaller than the problem epoch-based reclamation solves, because Brain never creates that problem: it does not mutate a structure in place while lock-free readers walk it.

---

## Atomic Publication

How the writer makes new state visible to readers atomically.

## 1. The publication concept

When the writer changes a data structure, readers shouldn't see partial state. They should see either the old state or the new state — never an inconsistent in-between.

**Publication** is the moment when the writer makes the new state available. After publication, future readers see the new state; readers who started before continue with the old.

## 2. Where publication is needed

In Brain:

- **HNSW state**: when the writer adds nodes or rebuilds the graph, readers must see consistent snapshots.
- **Routing tables**: when shards are added/removed, requests must be routed consistently.
- **Configuration**: when settings change, operations should see one config or the other.

Each of these uses ArcSwap for atomic publication.

## 3. The ArcSwap pattern

```rust
use arc_swap::ArcSwap;

struct Shard {
    hnsw: ArcSwap<HnswState>,
    // ...
}

// Reader:
fn search(&self, query: &[f32]) -> Vec<...> {
    let hnsw = self.hnsw.load();        // Atomic load of Arc
    hnsw.search(query, ...)
}

// Writer:
fn publish_new_hnsw(&self, new_hnsw: Arc<HnswState>) {
    self.hnsw.store(new_hnsw);          // Atomic store
}
```

The `load` returns an `Arc<HnswState>`; the reader holds it for the duration of the search. Even if the writer publishes a new state during the search, the reader's reference is still valid.

The `store` replaces the Arc atomically. Subsequent loads see the new state.

## 4. The "build then publish" pattern

The writer:

1. Builds the new state in isolation (no other task sees it).
2. Wraps it in an Arc.
3. Atomically swaps it in via ArcSwap.store.
4. Drops its own reference to the old Arc (the readers may still hold references; Arc's refcount handles this).

When the last reader drops its reference, the old state is dropped.

## 5. The cost of publication

ArcSwap.store:

- Atomic write of a pointer.
- ~50 ns.

ArcSwap.load:

- Atomic read of a pointer.
- ~10-50 ns.

Cheap enough to be in any code path.

## 6. The frequency of publication

For HNSW, publication happens:

- Periodically (every 10 ms by default).
- After every major change (e.g., a maintenance rebuild's swap).
- On-demand for read-after-write (the writer publishes immediately).

Periodic publication amortizes the build cost. Each publication captures recent inserts.

## 7. The pending buffer pattern

Between publications, the writer accumulates changes in a "pending buffer":

```rust
struct WriterState {
    published: Arc<HnswState>,
    pending: Vec<PendingInsert>,
}
```

On each insert, the writer adds to `pending`. Periodically, it merges:

```rust
fn merge_pending(&mut self) {
    let mut new_state = (*self.published).clone();   // Deep clone
    for insert in self.pending.drain(..) {
        new_state.apply(insert);
    }
    let new_arc = Arc::new(new_state);
    self.hnsw_swap.store(new_arc.clone());
    self.published = new_arc;
}
```

This is "publication via copy-on-write".

## 8. The "deep clone" cost

Cloning an HNSW state for ~1M nodes is expensive (~150 MB of memory + the clone time). Brain does not actually deep-clone for routine inserts.

Instead, the HNSW supports incremental publication:

- The writer mutates a private "draft" state.
- The draft shares immutable parts with the published state (structural sharing via Arc).
- When ready to publish, the new state is wrapped in an Arc and swapped.

This is the "persistent data structure" pattern. The HNSW implementation (or Brain's wrapper around hnsw_rs) supports this.

## 9. The implementation reality

In practice:

- The published main is an immutable `MainEpoch` — an HNSW plus a generation number — behind an `ArcSwap`.
- A publish builds a new index, wraps it in a new `MainEpoch` with the generation bumped, and `store`s it. The published index is never mutated in place.
- Routine inserts do not publish at all. They land in the pending buffer, where a search brute-forces them at exact cosine, and are folded into a new main only when a flush rebuild runs.

So ArcSwap is used for every publish, not only for full rebuilds, because every publish replaces the whole structure. The generation number tells the maintenance worker which publish landed; it is not what makes a write visible.

## 10. The "rebuild swap" path

When the maintenance worker rebuilds the HNSW:

1. Build the new HNSW in the background (takes seconds).
2. Wrap in an Arc.
3. ArcSwap.store the new Arc.
4. Old HNSW's Arc count drops; when no readers hold it, it's freed.

The old HNSW may live a few hundred milliseconds after the swap (until in-flight searches complete). Memory peaks during this window (both old and new are live).

## 11. The "atomic swap" semantics

ArcSwap's swap is **lock-free** and **wait-free**:

- A swap doesn't block any reader.
- A read doesn't block any swap.
- Multiple readers can load concurrently.
- A swap completes in bounded time, regardless of reader count.

This is critical for predictable latency.

## 12. The "many publications" cost

If the writer publishes very frequently (e.g., on every insert), the cost is:

- Per-publish: ~50 ns for the swap, plus the Arc clone overhead.
- Per-read: same cost (load is symmetric to store).

But the writer would be allocating and freeing Arcs constantly — heap pressure.

So Brain does not publish per-insert. Publications are batched: after a window of inserts, one publication.

## 13. The publication ordering

Publications on different shards are independent. Each shard's publications happen on its own schedule.

Within a shard, publications are ordered by the writer's progress. There's a clear "before" and "after" for each publication.

## 14. The reader's view during publication

A reader holds a reference loaded before a publication:

- The reader's view doesn't change during the publication.
- The publication updates the "current" reference; the reader's old reference is unchanged.
- After the reader drops its reference, the old state's Arc count drops.

## 15. The "publication can fail" question

If the writer can't allocate the new state (OOM during clone), the publication is skipped. The old state remains active. The writer logs the error and tries again later.

In practice, OOM in this path is very rare. Brain provisions generously.

## 16. The "no GC pause" guarantee

Publication doesn't have a "stop-the-world" pause. Readers continue running during publication. The swap is atomic, but it's a single instruction; no waiting.

This contrasts with traditional GC pauses in some systems. Brain doesn't have GC pauses.

## 17. Publication is not on the read path

Publication folds the writer's pending entries into the published main graph. It is **not** what makes a write visible: a search merges the pending buffer with the published main, so a freshly-inserted vector is found before any publication happens.

That is why there is no read-after-write wait and no `consistency` request parameter. Publication is a structure-and-cost concern — it keeps the brute-forced pending set small and the main graph current — and a reader never has to know its cadence.

## 18. The publication LSN

Each publication has an associated LSN — the WAL LSN at the time of publication. Brain tracks a per-shard "published LSN" as an atomic.

It is operational and replay state, not a read gate: checkpointing uses it to know what is safely folded, and subscribe-replay uses LSNs to resume. No read consults it to decide whether it may proceed.

## 19. The "publish nothing" case

Sometimes the writer has nothing new to publish (no recent operations). The publish-cycle skips: no allocation, no swap.

This keeps idle shards lightweight.

## 20. The summary

Publication via ArcSwap:

- Atomic, lock-free, wait-free.
- Cheap in steady state.
- Provides clear before/after semantics.
- Plays well with Arc-based memory management.

It's the right primitive for "swap the entire current view of a data structure", and that is the only granularity Brain reclaims at. There is no finer-grained reclamation within a structure: tombstoned nodes are filtered rather than freed, and they go away with the generation that held them (§5 under Reclamation).

---

*Continue to [`03_lock_free_primitives.md`](03_lock_free_primitives.md) for ArcSwap details.*
