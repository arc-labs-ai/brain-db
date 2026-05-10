# 11.01 Worker Architecture

The infrastructure shared by all background workers — the registry, scheduling, and lifecycle.

## 1. The worker registry

Each shard has a registry of workers:

```rust
struct WorkerRegistry {
    workers: HashMap<WorkerKind, WorkerHandle>,
    shard_state: Arc<ShardState>,
}
```

At shard startup, the registry creates each configured worker and starts its loop.

## 2. The worker handle

```rust
struct WorkerHandle {
    name: &'static str,
    task: TaskHandle,            // The Glommio task running the worker
    config: WorkerConfig,        // Interval, batch size, etc.
    metrics: WorkerMetrics,      // Exported metrics
}
```

The handle lets the registry pause, resume, or query a worker.

## 3. The worker config

Each worker has configuration:

```rust
struct WorkerConfig {
    enabled: bool,
    interval_secs: u32,          // How often to run
    batch_size: usize,           // How much per cycle
    max_runtime_ms: u32,         // Soft cap per cycle
    priority: TaskPriority,      // Glommio priority
}
```

Defaults are tuned for typical workloads. Operators can adjust per-worker.

## 4. The worker lifecycle

```
1. Created at shard startup.
2. Worker loop starts.
3. Each cycle:
   a. Wake up (timer or event).
   b. Do one cycle of work (bounded by max_runtime_ms).
   c. Update metrics.
   d. Sleep until next cycle.
4. On shutdown: receives a shutdown signal; finishes current cycle; exits.
```

Worker tasks live for the lifetime of the shard.

## 5. The cycle structure

```rust
async fn one_cycle(state: &ShardState, config: &WorkerConfig) -> Result<()> {
    let start = Instant::now();
    let mut processed = 0;

    while processed < config.batch_size && start.elapsed() < config.max_runtime_ms {
        match do_one_unit_of_work(state).await {
            Ok(true) => processed += 1,
            Ok(false) => break,    // No more work
            Err(e) => return Err(e),
        }

        if (processed % 50) == 0 {
            glommio::yield_now().await;    // Cooperative yield
        }
    }

    // Update metrics
    state.metrics.worker_cycles_total.inc();
    state.metrics.worker_processed.add(processed);

    Ok(())
}
```

The cycle is bounded by both batch size and time. Whichever hits first ends the cycle.

## 6. Yielding within a cycle

Workers yield every ~50 records or every ~10 ms of work, whichever comes first. This keeps them from monopolizing the executor.

The yield discipline matches the substrate's general rule (~100 µs of CPU between yields, [10.07](../10_concurrency_epochs/07_yields.md)).

## 7. Triggers and intervals

Workers run on:

- **Time interval** (most common): "every 5 minutes, do a cycle".
- **Event trigger**: "when the writer commits, check if consolidation is needed".
- **Threshold trigger**: "when tombstone count exceeds X, schedule rebuild".

Most workers use time intervals as the primary mechanism, with thresholds as accelerators.

## 8. The event channel

Workers subscribe to internal events:

```rust
enum ShardEvent {
    WriteCommitted(LSN),
    TombstoneCountChanged(usize),
    ArenaResized,
    ConfigChanged,
}
```

The event channel notifies workers; they decide whether to wake up early.

## 9. Worker isolation

Workers don't share state directly. They access the shard's storage via the same handles as request handlers (separate transactions, MVCC).

Two workers running simultaneously don't conflict because:
- Their writes go through redb's serialized write transactions.
- Their reads use independent MVCC snapshots.

## 10. The single-writer interaction

Background workers that mutate state send their writes to the shard's writer task (same as request handlers do). The writer batches them with concurrent client writes for efficient group commits.

This means workers don't have a separate write path. They use the standard pipeline.

## 11. Rate limiting

Workers self-rate-limit by sleeping between cycles. Default intervals:

- Decay: every 1 hour.
- Access boost: every 10 seconds.
- Consolidation: every 5 minutes (with threshold trigger).
- HNSW maintenance check: every 5 minutes.
- Idempotency cleanup: every 1 hour.
- Slot reclamation: every 10 minutes.
- WAL retention: every 1 minute.

For deployments that want different rates, configuration overrides are supported.

## 12. The "shed load" interaction

When the substrate is overloaded (CPU > 90%), workers shed load:
- Reduce batch sizes.
- Skip non-essential cycles.
- The HNSW rebuild and consolidation are most-likely-to-skip.

Critical workers (idempotency cleanup, WAL retention) continue at reduced rate.

## 13. The "stop" semantics

A worker can be stopped:
- Operator command (`ADMIN_WORKER_STOP <kind>`).
- Configuration disable (`workers.<kind>.enabled = false`).
- Shutdown signal at shard close.

After stop, the worker's loop exits cleanly. In-progress work is committed first.

## 14. The "restart" semantics

A worker can be restarted:
- Operator command (`ADMIN_WORKER_RESTART <kind>`).
- After config change requiring restart.

The worker loop restarts; previous in-flight state is lost (the worker recomputes what's needed).

## 15. The metrics surface

Per-worker metrics:

```
brain_worker_cycles_total{shard=, worker=}
brain_worker_processed_total{shard=, worker=}
brain_worker_errors_total{shard=, worker=}
brain_worker_cycle_duration_ms{shard=, worker=}
brain_worker_last_run_unixtime{shard=, worker=}
brain_worker_pending_work{shard=, worker=}
```

Operators monitor these to detect:
- A worker that stopped (cycles_total stagnant).
- Errors (errors_total rising).
- Long cycles (cycle_duration_ms high).
- Backlog (pending_work growing).

## 16. The configuration surface

```toml
[workers.decay]
enabled = true
interval = "1h"
batch_size = 10000

[workers.consolidation]
enabled = true
interval = "5m"
batch_size = 100
threshold_episodes_in_context = 50

[workers.hnsw_maintenance]
enabled = true
interval = "5m"
tombstone_ratio_threshold = 0.30
recall_threshold = 0.90

# ... etc
```

Each worker has its own section. Defaults are sensible; operators tune as needed.

## 17. The development testing

Workers are exercised in unit tests with sped-up clocks. Integration tests verify end-to-end behaviors (decay reduces salience over time; consolidation produces summaries; etc.).

For production deployments, workers run continuously; their effects are visible in the metrics.

---

*Continue to [`02_decay.md`](02_decay.md) for the decay worker.*
