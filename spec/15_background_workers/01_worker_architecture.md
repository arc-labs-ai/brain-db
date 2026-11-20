# 15.01 Worker Architecture

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

Four knobs, shared by every worker:

```rust
pub struct WorkerConfig {
    /// A disabled worker stays registered (so it is still introspectable)
    /// and its loop keeps ticking, but never calls `run_cycle`.
    pub enabled: bool,
    pub interval: Duration,      // Sleep between cycles
    pub batch_size: usize,       // Soft cap on units of work per cycle
    pub max_runtime: Duration,   // Soft cap on wall-clock per cycle
}
```

There is **no priority field**. Glommio exposes no per-task priority; what a
worker's scheduling is set by is the task queue it runs in, which is a property
of the scheduler rather than of the worker (below).

`WorkerConfig::defaults_for(kind)` is the one cadence table, and it decides
`enabled` as well as timing: `entity_gc`, `statement_reclaim` and
`predicate_gc` ship **off**, because reclaiming rows a read already filters out
is something an operator opts into. Which cadences an operator can override,
and which are a property of the build, is §16.

**Where workers run.** A shard is one Glommio executor, and until it was
changed every worker reached it through plain `spawn_local` — which means the
executor's *default* task queue, the one that serves incoming requests.
Glommio schedules queues by share, so a derivation burst and a client's ENCODE
ranked as equals, and derivation is not a polite neighbour: each finished
provider call brings a JSON decode, a projection, a redb commit and an HNSW
insert back onto that lane. Measured on one shard with 120 memories, a
128-wide extractor fan-out returned `503 engine.unavailable` on 9 of 120
writes.

`WorkerScheduler` therefore creates one queue of its own on first `register`
and spawns every worker into it with `spawn_local_into`:

```rust
create_task_queue(Shares::Static(250), Latency::NotImportant, "brain-workers")
```

250 shares against the default queue's 1000. Under contention workers get
roughly a fifth of the lane; with the request path idle they get all of it,
because shares only bind when something else wants to run. An acked write is a
promise already kept, while derivation is enrichment allowed to arrive late.

Splitting the two across executors is not available: `OpsContext` is `!Send`
by design — that is the share-nothing thread-per-core model — so a worker and
the requests it competes with necessarily share one executor. Priority is the
only lever that exists here.

Share accounting cannot preempt work that never yields, so one worker needed a
second mechanism: the classifier tier's GLiNER forward pass is ~3.8 s of
uninterruptible CPU per micro-batch with no await point inside it, and it runs
on Glommio's blocking pool rather than the reactor thread. See
[11. Extractor tiers](../11_extractors/01_extractor_tiers.md).

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

The yield discipline matches Brain's general rule (~100 µs of CPU between yields, per [14. Concurrency — yields](../14_concurrency/04_yields.md)).

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
- index maintenance check: every 5 minutes.
- Idempotency cleanup: every 1 hour.
- Slot reclamation: every 10 minutes.
- WAL retention: every 1 minute.

Ten of these cadences can be overridden from `[workers]`; the rest are a
property of the build (§16).

## 12. What happens under load

There is **no CPU-triggered load shedding**: nothing samples CPU, and no
worker reduces its batch size or skips a cycle because the shard is busy.
Three mechanisms cover the ground instead, and they are worth distinguishing
because only the first is automatic:

- **The share split (§3).** Workers run in a 250-share queue against the
  request path's 1000, so under contention they are scheduled less and the
  request path keeps its budget. This is the whole of the automatic response.
- **`max_runtime`.** A soft cap per cycle, checked between units of work, so
  one cycle cannot run unboundedly long. It cannot interrupt a single unit —
  which is why the classifier's forward pass is offloaded rather than capped.
- **The operator.** `POST /v1/workers/{name}/stop` pauses a worker's
  `run_cycle` while its loop keeps ticking, so it resumes without restarting
  the shard; `start` resumes it and `run-now` triggers one cycle.

## 13. The "stop" semantics

A worker can be stopped:
- Operator command (`POST /v1/workers/{name}/stop`).
- Configuration disable (`workers.<kind>.enabled = false`).
- Shutdown signal at shard close.

After stop, the worker's loop exits cleanly. In-progress work is committed first.

## 14. The "restart" semantics

A worker can be restarted:
- Operator command (`POST /v1/workers/{name}/start`, or `run-now` for a single cycle).
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

Two shapes, and the difference matters: the maintenance workers share one flat
block of cadences, while the typed-graph and derivation workers each get a
section with their own knobs.

```toml
[workers]
# Sleep between cycles, in seconds. Omitting a key leaves the built-in
# cadence. Each value must be >= 1; a zero is refused at boot.
consolidation_interval_sec = 300
hnsw_maintenance_interval_sec = 300
idempotency_cleanup_interval_sec = 3600
slot_reclamation_interval_sec = 600
wal_retention_interval_sec = 60
edge_scrub_interval_sec = 1800
counter_reconciliation_interval_sec = 3600
statistics_update_interval_sec = 300
embedder_cache_eviction_interval_sec = 60
snapshot_interval_sec = 3600

[workers.auto_edge]
enabled = true
interval_ms = 100
batch_size = 256
similarity_threshold = 0.85
top_k = 5
ef_search = 64
channel_capacity = 1024

[workers.supersession_sweeper]
enabled = false
retention_seconds = 0      # 0 keeps superseded rows forever
```

Rules the shape implies:

- **A cadence is read once, at registration**, and kept for the life of the
  process, so changing one needs a restart. The ten keys are held in a
  fixed-width table paired with the worker kind each governs, precisely so a
  key cannot be added to the file and never arrive at a scheduler — an
  operator who edits a value that reaches nothing sees no error and no change.
- **`access_boost` has no key.** Eleven maintenance workers, ten cadences:
  its tick is a property of the build.
- **Unknown keys are refused** (`deny_unknown_fields`), so a typo fails boot
  rather than being silently ignored.
- **Not every knob is configurable.** The audit-log sweeper's 90-day
  retention, the entity GC's 30-day grace, and the stale-extraction detector's
  cadence are code defaults with no section to set them from. The LLM response
  cache's TTL is per extractor, declared as `cache_ttl` in the extractor
  document, not an operator knob at all.

## 17. The development testing

Workers are exercised in unit tests with sped-up clocks. Integration tests verify end-to-end behaviors (decay reduces salience over time; consolidation produces summaries; etc.).

For production deployments, workers run continuously; their effects are visible in the metrics.

---

*Continue to [`02_memory_maintenance.md`](02_memory_maintenance.md) for memory maintenance.*
