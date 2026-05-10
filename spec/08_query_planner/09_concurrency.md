# 08.09 Concurrency

How concurrent executions interact within a shard and across shards.

## 1. The Glommio executor model

Each shard runs on a dedicated OS thread, pinned to a CPU core. On that thread, a Glommio executor schedules many async tasks.

Tasks include:
- Connection handlers.
- Request executors (one per in-flight request).
- The writer task.
- Background workers (decay, consolidation).

Glommio multiplexes them via cooperative async scheduling. No OS-level context switches; just future-state machine resumption.

## 2. The single-writer pattern

Within a shard:
- Many request executors run concurrently (reads in particular).
- One writer task processes mutations sequentially.

Read-only request executors (RECALL, PLAN, REASON) don't touch the writer. They open read transactions on storage and run independently.

Write-bearing request executors (ENCODE, FORGET, LINK) send their write operations to the writer via a channel and await acks.

## 3. Channel-based writer communication

```rust
// Executor side:
let ack = self.send_to_writer(op).await?;

// Writer side:
loop {
    let op = self.queue.recv().await;
    let ack = self.process(op).await;
    op.ack_tx.send(ack).await;
}
```

The channel is bounded (default 1024). If full, executors await until space opens. This provides backpressure naturally.

## 4. Group commits

The writer task batches operations:

```rust
async fn writer_loop(&mut self) {
    loop {
        // Drain at least 1 op
        let mut batch = vec![self.queue.recv().await];
        
        // Try to gather more, with a brief timeout
        let timeout = sleep(Duration::from_micros(100));
        loop {
            select! {
                op = self.queue.try_recv() => batch.push(op?),
                _ = &mut timeout => break,
                _ = (batch.len() >= 64) => break,
            }
        }

        // Process the batch as one group commit
        self.process_batch(batch).await;
    }
}
```

The 100 µs window plus 64-op cap balance latency vs throughput:
- Light load: ~100 µs added latency per write.
- Heavy load: 64 ops per commit, ~5 µs per write.

## 5. Cooperative yielding

Within long-running steps, executors yield:

- Every ~100 µs of CPU work.
- At every I/O await point.
- Explicitly via `tokio::task::yield_now()` (or Glommio equivalent).

Yields let other tasks make progress. Without them, a single heavy request could starve others.

## 6. The reader-writer interaction

Readers (executors handling RECALL, etc.):
- Open redb read transactions on storage.
- Run searches and lookups concurrently.
- Don't block writers (MVCC).

Writers:
- Process operations sequentially.
- Hold redb write transactions briefly.
- Don't block readers (MVCC).

The single-writer-per-shard discipline means there's no writer-vs-writer contention.

## 7. Cross-shard concurrency

Cross-shard queries fan out:

```rust
async fn cross_shard_recall(&self, plan: RecallPlan) -> Result<...> {
    let futures = plan.shards.iter().map(|s| self.search_shard(s));
    let results = futures::future::try_join_all(futures).await?;
    self.merge(results)
}
```

Each shard's search runs on its own Glommio executor (different thread, different core). Truly parallel.

The merge runs on the originating executor (the one handling the request).

## 8. Cross-shard communication

For an in-process substrate (single binary), cross-shard calls are direct method calls — no serialization, no network.

For a distributed substrate (clustered), cross-shard calls go over the network. The wire protocol carries the requests and responses. Latency is higher (~1 ms typical intra-datacenter).

## 9. The orchestrator pattern

For complex queries (PLAN, REASON), the orchestrator:

```rust
async fn orchestrate(&self, plan: PlanPlan) -> Result<...> {
    // Step 1: parallel embeddings
    let (start_emb, goal_emb) = futures::join!(
        self.embedder.embed(&plan.starting_state),
        self.embedder.embed(&plan.goal_text)
    );

    // Step 2: parallel RECALLs
    let (start_recall, goal_recall) = futures::join!(
        self.recall(start_emb),
        self.recall(goal_emb)
    );

    // Step 3: traversal (sequential)
    let traversal = self.traverse(start_recall, goal_recall, &plan).await?;

    // Step 4: scoring
    let scored = self.score_paths(traversal);

    Ok(scored)
}
```

Sequential steps are awaited in turn; parallel steps run concurrently.

## 10. The "fan out then gather" cost

Fan-out across N shards:

```
Latency = max(per_shard_latency) + merge_overhead
```

The latency is dominated by the slowest shard. If one shard is overloaded, the whole query waits.

The substrate has timeouts per-shard (default 5 sec per shard call). If a shard times out, partial results are returned.

## 11. Background work scheduling

Background workers (decay, consolidation, maintenance) also run on the same Glommio executors. They yield generously to keep request latency low.

The substrate prioritizes:
- High: request executors.
- Medium: writer task.
- Low: background workers.

This is enforced through scheduling hints (Glommio supports task priorities).

## 12. The "isolation" guarantee

Each shard's state is isolated:

- A shard can't read another shard's storage directly.
- Cross-shard queries go through the wire protocol or a distributed-call interface.
- A shard's failure doesn't cascade to other shards.

This isolation means shards are good failure boundaries.

## 13. Connection-level concurrency

A single TCP connection can carry multiple in-flight requests (different stream IDs). The connection layer demultiplexes and sends responses on the right stream.

Per-connection limits (max concurrent streams) prevent runaway request submission. Default: 1024 streams per connection.

## 14. The "stop the world" rare events

Some operations briefly stop the world on a shard:

- Arena growth: the writer pauses briefly while the arena is mmapped to a new size.
- Snapshot creation: the writer pauses while the metadata is checkpointed.
- HNSW rebuild swap: a microsecond-level swap of the HNSW reference.

These events are rare and brief. They don't affect throughput meaningfully.

## 15. Measurement

The substrate measures concurrency:

- Active executor task count per shard.
- Writer queue depth.
- Per-request waiting time (in queue, not yet executing).

These metrics help operators tune capacity.

## 16. The "small concurrency" advantage

Brain's per-shard concurrency is intentionally limited:

- A handful of cores per shard.
- A few hundred to a few thousand in-flight requests per shard.

This gives:
- Predictable latency (no thread pool exhaustion).
- Easy resource accounting.
- Simple debugging.

For higher throughput, add more shards. Sharding is the scaling lever.

## 17. The "no shared mutable state across shards" rule

A core invariant: shards don't share mutable state. Each shard's:
- Storage is independent.
- HNSW is independent.
- Writer task is independent.

Cross-shard queries communicate via messages (function calls or RPC), never shared memory.

This makes shards independent failure and scaling units. It also makes the codebase simpler — no cross-shard locks, no global state.

---

*Continue to [`10_failure_modes.md`](10_failure_modes.md) for failure modes.*
