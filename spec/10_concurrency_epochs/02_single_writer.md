# 10.02 Single-Writer per Shard

The single most important concurrency invariant: each shard has exactly one writer task.

## 1. The discipline

For each shard:

- Exactly one writer task is responsible for all mutations.
- All mutating operations (ENCODE, FORGET, LINK, UNLINK) flow through this task.
- The writer is on the shard's dedicated executor.

Other tasks on the shard handle reads, connection management, etc., but they don't mutate shard state directly. They send write operations to the writer via a channel.

## 2. Why this works

The writer-per-shard model gives several benefits:

- **No writer-vs-writer locking.** Within a shard, there's only one writer. No contention.
- **Implicit serialization.** The writer processes operations sequentially. WAL records are appended in a single, well-defined order.
- **Simple control flow.** No "what if two writers see different states" reasoning.
- **Predictable throughput.** Writer's per-second rate is clear (limited by group commit and storage).

## 3. The writer's responsibilities

```rust
struct Writer {
    shard_id: ShardId,
    queue: Receiver<WriteOp>,
    storage: StorageHandle,
    publisher: Publisher,
}

impl Writer {
    async fn run(&mut self) {
        loop {
            let batch = self.collect_batch().await;
            let result = self.process_batch(batch).await;
            self.send_acks(result).await;
        }
    }
}
```

Per-batch:

1. Collect operations from the queue.
2. Apply them in WAL order.
3. fsync the WAL (durability barrier).
4. Apply to in-memory derived state (metadata, HNSW).
5. Send acks to the originators.

## 4. The queue

```rust
let (tx, rx) = bounded::<WriteOp>(1024);
```

Bounded queue with backpressure. If the queue is full:

- Senders block (in async terms, await space).
- After timeout, they error out (`WriterOverloaded`).

The bound (1024) is tunable. Default is conservative; sustained queue depth >100 is a warning sign.

## 5. The batching window

The writer batches multiple operations:

```rust
async fn collect_batch(&self) -> Vec<WriteOp> {
    let first = self.queue.recv().await;
    let mut batch = vec![first];

    let timeout = sleep(Duration::from_micros(100));
    loop {
        select! {
            op = self.queue.try_recv() => {
                if let Ok(op) = op {
                    batch.push(op);
                    if batch.len() >= 64 { break; }
                }
            }
            _ = &mut timeout => break,
        }
    }
    batch
}
```

The 100 µs window + 64 op cap balance latency vs throughput.

For light load: ~100 µs added latency, batches of 1-2 ops.

For heavy load: 64-op batches every ~6 ms (160 batches/sec × 64 = 10K ops/sec).

## 6. Group commit

A batch is processed as a single group commit:

1. Append all WAL records (one per operation).
2. Single fsync (one disk write for the whole batch).
3. Apply all operations to derived state.
4. Send acks for all operations.

Group commit amortizes the fsync cost. Without it, each operation pays ~0.3 ms for fsync; with batches of 32, that's ~10 µs per operation.

## 7. The "writer is the source of truth" rule

The writer is the single source of truth for shard state. Other tasks observe state through:

- redb read transactions: see committed data.
- ArcSwap.load() on the HNSW: see published data.
- Direct mmap reads: see arena bytes.

No task other than the writer modifies the canonical state. There are caches and derived structures, but they all derive from what the writer has produced.

## 8. The cross-task communication

Other tasks send write operations to the writer:

```rust
async fn execute_encode(&self, plan: EncodePlan) -> Result<EncodeResponse> {
    // ... preparation ...

    let (ack_tx, ack_rx) = oneshot::channel();
    let op = WriteOp::Encode { plan, ack: ack_tx };
    self.writer_tx.send(op).await?;

    let result = ack_rx.await?;
    Ok(self.build_response(result))
}
```

The executor task waits on the ack. It's blocked (async-blocked, not OS-blocked) until the writer processes the operation.

For batch group commits: many executors await acks; the writer broadcasts after the commit.

## 9. The writer's failure modes

If the writer task crashes (panic):

- The substrate logs the panic.
- The shard is marked unhealthy.
- New write operations fail.
- Reads continue (the writer wasn't doing them).
- Operator action: investigate, restart the shard.

Restart involves:
- Replaying the WAL.
- Re-creating the writer task.
- Resuming.

In-flight operations that were waiting on acks see the connection close; clients can retry.

## 10. The "no two writers" invariant

The substrate enforces single-writer per shard:

- Per-shard struct contains the writer's queue.
- Only the writer task has the receiver end.
- No other task can directly mutate shard state.

In debug builds, assertions check this:

```rust
debug_assert!(thread_id() == writer_thread_id, "Mutation outside writer task");
```

In release builds, the structural guarantees (private receivers, etc.) prevent it.

## 11. The "writer doesn't read directly" subtlety

The writer task can read shard state too — it needs to look up things to apply operations. It does this via the same primitives readers use (redb read transactions, etc.) — within the writer task.

So the writer is also a reader, but it's "the" reader that's also doing writes.

## 12. The cooperative-yield within writer

The writer task yields cooperatively:

- Between batches.
- During large batches (every ~10 ops).
- During I/O (fsync, etc.).

Yields let other tasks (readers, background workers) run. Without them, a busy writer would starve everything else on the shard.

## 13. The writer's resource bound

The writer task's resource use:

- CPU: ~30-50% of the shard's core under sustained load.
- Memory: ~few MB (in-flight ops, batches).
- I/O: bounded by WAL fsync rate.

Other tasks share the remaining CPU. For a 16-thread server with 16 shards: each shard uses about half a core for writes; the rest is for reads.

## 14. The exception: external writers (replication)

For HA / replication, a "follower" shard takes writes from a "leader" shard, not from clients. The follower's writer applies replicated operations.

In v1, Brain doesn't have replication. A future addition would extend the writer model:

- A leader writer takes client requests.
- A follower writer takes replicated operations.
- Both apply to the same shard's state — but they're never both active at once (one is leader, one is follower).

## 15. The implications for SDK

The SDK:

- Doesn't manage writer state.
- Submits requests; awaits responses.
- Doesn't need to know about writers.

The single-writer is a substrate-internal detail. Clients see a "submit operation" interface.

## 16. The implications for testing

Tests can:

- Spawn a single-shard substrate.
- Send concurrent operations.
- Verify ordering: operations are linearizable per shard.

Tests can't:

- Create multiple writers per shard.
- Bypass the writer.

The substrate's API doesn't expose direct mutation; it always goes through the writer.

## 17. The "writer pause" pattern

Some operations need to briefly pause the writer:

- Snapshot creation: writer pauses while files are linked.
- Schema migration: writer pauses while migrations run.

The pause is implemented as: an "admin" message in the writer's queue takes priority and runs synchronously, blocking other operations. After it completes, the writer resumes normal processing.

The pause is brief (typically < 100 ms). Clients see a temporary latency spike.

## 18. The writer-ready check

When a shard is starting up (recovery in progress), the writer isn't yet ready. Operations submitted to a not-ready writer get queued; when ready, they're processed.

If the queue grows too long during startup, new operations are rejected with `ShardNotReady`.

## 19. The throughput math

Single-writer throughput per shard:

- 10K ops/sec sustained (limited by WAL fsync + redb commits).
- 30-50K ops/sec burst (with full batching, no fsync stall).

Scaling beyond per-shard limits: more shards. Per-shard performance is bounded; the substrate's total throughput is N × per-shard.

## 20. The summary

Single-writer-per-shard is the keystone of Brain's concurrency model. It:

- Eliminates writer-vs-writer contention.
- Provides a clear ordering for operations.
- Simplifies reasoning about consistency.
- Bounds per-shard throughput predictably.

The trade-off — bounded per-shard throughput — is acceptable because Brain scales by adding shards.

---

*Continue to [`03_epochs.md`](03_epochs.md) for epoch-based reclamation.*
