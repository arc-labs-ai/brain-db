# Sub-task 8.1 — `Worker` trait & scheduler

**Spec:** `spec/11_background_workers/00_purpose.md`, `01_worker_architecture.md`
**Phase doc:** `docs/phases/phase-08-workers.md` §8.1
**Done when:**
- `trait Worker { name(), config(), run_cycle() }` is defined.
- A scheduler runs each registered worker on its interval; each cycle yields cooperatively.

---

## 1. Scope and intent

8.1 builds the **infrastructure** for the 12 workers that follow (8.2 – 8.13). No worker logic ships in this sub-task — just:

- The `Worker` trait.
- `WorkerConfig` (enabled, interval, batch_size, max_runtime).
- `WorkerContext` (the handle bag workers consume — `Arc<OpsContext>` plus a shutdown signal).
- `WorkerMetrics` (atomic counters; spec §15).
- `WorkerScheduler` — registers workers, spawns one tokio task per worker, supports graceful shutdown.
- A `drive_batch` helper that implements the spec §5 "bounded cycle with periodic yields" pattern so individual workers don't reinvent it.

**Runtime:** tokio for v1. The shard's Glommio executor lands in Phase 9 when the connection layer is built; the trait is shaped so the Glommio swap is a runtime-substitution, not a redesign. Spec §4's task-priority story is a Phase 9 concern — workers in v1 just run on the default tokio runtime.

---

## 2. Public surface (in `crates/brain-workers`)

```rust
// crates/brain-workers/src/error.rs  (NEW)
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("ops layer error: {0}")]
    Ops(String),
    #[error("worker cycle exceeded budget: {0}")]
    BudgetExceeded(String),
    #[error("internal: {0}")]
    Internal(String),
}

// crates/brain-workers/src/config.rs  (NEW)
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub enabled: bool,
    pub interval: Duration,
    pub batch_size: usize,
    pub max_runtime: Duration,
}
impl WorkerConfig {
    pub fn defaults_for(kind: WorkerKind) -> Self { ... }  // per spec §11
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum WorkerKind {
    Decay, AccessBoost, Consolidation, HnswMaintenance,
    IdempotencyCleanup, SlotReclamation, WalRetention,
    EdgeScrub, CounterReconcile, Statistics,
    EmbedderCacheEvict, Snapshot,
}

// crates/brain-workers/src/context.rs  (NEW)
#[derive(Clone)]
pub struct WorkerContext {
    pub ops: Arc<brain_ops::OpsContext>,
    /// shutdown signal — workers check this between units of work.
    pub shutdown: tokio::sync::watch::Receiver<bool>,
}

// crates/brain-workers/src/metrics.rs  (NEW)
#[derive(Default, Debug)]
pub struct WorkerMetrics {
    pub cycles_total:           AtomicU64,
    pub processed_total:        AtomicU64,
    pub errors_total:           AtomicU64,
    pub last_cycle_duration_ms: AtomicU64,
    pub last_run_unix_secs:     AtomicU64,
    pub pending_work_estimate:  AtomicU64,
}

// crates/brain-workers/src/worker.rs  (NEW)
pub trait Worker: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn kind(&self) -> WorkerKind;
    fn config(&self) -> WorkerConfig;
    /// Execute one bounded cycle. Returns the number of units processed
    /// (counter input for `processed_total`). Workers typically call
    /// [`drive_batch`] to honour spec §5's batch_size + max_runtime
    /// bound + yield-every-50 rule.
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + Send + 'a>>;
}

/// Helper implementing the spec §5 / §6 cooperative-batch pattern.
/// `unit` is "do one piece of work, return Ok(true) if more remains".
pub async fn drive_batch<F, Fut>(
    cfg: &WorkerConfig,
    ctx: &WorkerContext,
    mut unit: F,
) -> Result<usize, WorkerError>
where
    F: FnMut(&WorkerContext) -> Fut,
    Fut: Future<Output = Result<bool, WorkerError>>,
{
    let start = Instant::now();
    let mut processed = 0usize;
    while processed < cfg.batch_size
        && start.elapsed() < cfg.max_runtime
        && !*ctx.shutdown.borrow()
    {
        match unit(ctx).await? {
            true => processed += 1,
            false => break,
        }
        if processed % 50 == 0 { tokio::task::yield_now().await; }
    }
    Ok(processed)
}

// crates/brain-workers/src/scheduler.rs  (NEW)
pub struct WorkerScheduler {
    handles: HashMap<&'static str, WorkerHandle>,
    shutdown: tokio::sync::watch::Sender<bool>,
}
pub struct WorkerHandle {
    pub name: &'static str,
    pub kind: WorkerKind,
    pub metrics: Arc<WorkerMetrics>,
    task: tokio::task::JoinHandle<()>,
}
impl WorkerScheduler {
    pub fn new() -> Self;
    /// Spawn the worker's loop task. Registry uses `name()` as the key.
    pub fn register(&mut self, worker: Arc<dyn Worker>, ops: Arc<brain_ops::OpsContext>) -> Result<(), WorkerError>;
    pub fn metrics(&self, name: &str) -> Option<Arc<WorkerMetrics>>;
    pub fn names(&self) -> Vec<&'static str>;
    /// Signal shutdown and await every spawned task.
    pub async fn shutdown(self) -> Result<(), WorkerError>;
}
```

`lib.rs` removes the `SPEC_REFERENCE` placeholder and re-exports the surface above.

---

## 3. Cycle / loop semantics (matches spec §5 + §6)

1. Scheduler.register():
   - Creates `Arc<WorkerMetrics>` and an `Arc::clone` of the worker.
   - Spawns a tokio task running `worker_loop(worker, metrics, ctx)`.
2. `worker_loop`:
   - Reads `cfg = worker.config()`.
   - If `cfg.enabled == false` → sleep on the interval, never call `run_cycle`.
   - Else loop:
     - Check shutdown.
     - Call `worker.run_cycle(&ctx)` with stopwatch.
     - On Ok(n): bump cycles_total, processed_total += n, record duration + last_run.
     - On Err(e): bump errors_total, log via tracing.
     - `tokio::select!` on sleep(interval) vs shutdown.changed().
3. Shutdown: scheduler flips watch::Sender to `true`, then awaits each task's JoinHandle (with a soft 5s budget — beyond that, log and proceed; tasks abort on drop).

---

## 4. Default config table (spec §11)

```rust
match kind {
    Decay              => WorkerConfig { enabled: true, interval: 1h,   batch_size: 10_000, max_runtime: 5s   },
    AccessBoost        => WorkerConfig { enabled: true, interval: 10s,  batch_size: 1_000,  max_runtime: 500ms},
    Consolidation      => WorkerConfig { enabled: true, interval: 5m,   batch_size: 100,    max_runtime: 10s  },
    HnswMaintenance    => WorkerConfig { enabled: true, interval: 5m,   batch_size: 1,      max_runtime: 60s  },
    IdempotencyCleanup => WorkerConfig { enabled: true, interval: 1h,   batch_size: 10_000, max_runtime: 5s   },
    SlotReclamation    => WorkerConfig { enabled: true, interval: 10m,  batch_size: 1_000,  max_runtime: 5s   },
    WalRetention       => WorkerConfig { enabled: true, interval: 1m,   batch_size: 100,    max_runtime: 2s   },
    EdgeScrub          => WorkerConfig { enabled: true, interval: 30m,  batch_size: 5_000,  max_runtime: 5s   },
    CounterReconcile   => WorkerConfig { enabled: true, interval: 1h,   batch_size: 1,      max_runtime: 30s  },
    Statistics         => WorkerConfig { enabled: true, interval: 5m,   batch_size: 1,      max_runtime: 5s   },
    EmbedderCacheEvict => WorkerConfig { enabled: true, interval: 1m,   batch_size: 5_000,  max_runtime: 2s   },
    Snapshot           => WorkerConfig { enabled: false, interval: 1h,  batch_size: 1,      max_runtime: 5min },
}
```

(Defaults are baseline; per-worker sub-tasks may tune. Snapshot defaults off until 8.13 wires it.)

---

## 5. Tests (`crates/brain-workers/tests/scheduler.rs`)

A `TestWorker` fixture lets us exercise the infrastructure without depending on the real ops layer for this sub-task. We can build a minimal `OpsContext` via the existing in-crate fixture pattern (TempDir, MetadataDb, MockDispatcher) — same shape brain-ops tests use.

### Trait conformance (3)
1. `worker_runs_at_least_once` — register a worker with interval=20ms, sleep 100ms, observe ≥ 2 cycles.
2. `worker_run_unit_count_feeds_processed_total` — TestWorker returns `Ok(N)` per cycle; metrics reflect `N × cycles`.
3. `disabled_worker_never_executes` — `enabled=false` worker registered; sleep; cycles_total stays at 0.

### Batching helper (4)
4. `drive_batch_respects_batch_size_bound` — unit always returns `Ok(true)`, batch_size=10, max_runtime=1h → processed exactly 10.
5. `drive_batch_respects_max_runtime` — unit sleeps 20ms per call; batch_size=1000, max_runtime=80ms → processed roughly 4±1.
6. `drive_batch_stops_on_unit_false` — unit returns `false` on iteration 3 → processed == 3.
7. `drive_batch_yields_to_runtime` — verify the helper is `await`able through a yield point (smoke test: doesn't deadlock).

### Lifecycle (3)
8. `shutdown_waits_for_in_progress_cycle` — slow worker (200ms cycle); call shutdown after 50ms; worker's last cycle completes before the JoinHandle resolves.
9. `errors_increment_errors_total_and_continue` — worker returns `Err` on cycle 1, `Ok` on cycle 2; both observed; errors_total == 1, cycles_total == 2.
10. `scheduler_lists_registered_workers_by_name` — register 3 distinct workers, `names()` returns all 3.

### Multiple workers (1)
11. `multiple_workers_run_independently` — two workers with distinct intervals; metrics for each advance at their own rate.

---

## 6. Cargo.toml additions

```toml
[dependencies]
brain-core = { path = "../brain-core" }
brain-ops  = { path = "../brain-ops" }
thiserror.workspace = true
tracing.workspace = true
tokio = { version = "1", features = ["sync", "time", "rt", "macros"] }

[dev-dependencies]
brain-protocol  = { path = "../brain-protocol" }
brain-embed     = { path = "../brain-embed" }
brain-index     = { path = "../brain-index" }
brain-metadata  = { path = "../brain-metadata" }
brain-planner   = { path = "../brain-planner" }
parking_lot.workspace = true
tempfile.workspace = true
uuid.workspace = true
tokio = { version = "1", features = ["sync", "time", "rt-multi-thread", "macros"] }
```

---

## 7. File-by-file plan

| File                                            | Action | Notes |
| ----------------------------------------------- | ------ | ----- |
| `crates/brain-workers/Cargo.toml`               | Edit   | Add deps above |
| `crates/brain-workers/src/lib.rs`               | Rewrite | Remove `SPEC_REFERENCE` stub; module declarations + re-exports |
| `crates/brain-workers/src/error.rs`             | NEW    | `WorkerError` |
| `crates/brain-workers/src/config.rs`            | NEW    | `WorkerKind`, `WorkerConfig`, defaults table |
| `crates/brain-workers/src/context.rs`           | NEW    | `WorkerContext` |
| `crates/brain-workers/src/metrics.rs`           | NEW    | `WorkerMetrics` |
| `crates/brain-workers/src/worker.rs`            | NEW    | `Worker` trait, `drive_batch` helper |
| `crates/brain-workers/src/scheduler.rs`         | NEW    | `WorkerScheduler`, `WorkerHandle`, internal `worker_loop` |
| `crates/brain-workers/tests/scheduler.rs`       | NEW    | 11 tests |

---

## 8. Out-of-scope (deferred)

- Glommio task priority (spec §4) — Phase 9.
- Event-channel triggers (spec §7-§8 `ShardEvent`) — when consolidation / HNSW workers actually need them in 8.4 / 8.5.
- Operator commands (`ADMIN_WORKER_STOP`, `_RESTART`) — spec §13-§14; admin ops are Phase 9.
- Per-worker TOML config plumbing (spec §16) — server config plumbing is Phase 9. Tests inject `WorkerConfig` directly.
- Load-shedding (spec §12) — defer to Phase 9 admission control.

---

## 9. Risks

| Risk                                          | Mitigation |
| --------------------------------------------- | ---------- |
| Tokio runtime tied to scheduler binary        | Scheduler is a library; binary picks the runtime. Tests use `#[tokio::test(flavor = "multi_thread")]` for the multi-worker case |
| Shutdown deadlock if a worker is stuck        | 5s soft budget on `shutdown()`; log + proceed beyond that |
| Test timing flakiness (intervals = ms)        | Use generous bounds (≥2 cycles not ==2); poll metrics with a bounded retry loop instead of fixed sleeps |
| Spec says "trait Worker { run_cycle &mut self }" | Our `run_cycle(&self)` is more permissive (interior mutability where needed). Functionally equivalent; doc-comment notes the deviation |

---

## 10. Done criteria

- [ ] 8 new files in `crates/brain-workers/src/`.
- [ ] 11 tests in `tests/scheduler.rs`, all passing.
- [ ] Existing `SPEC_REFERENCE` stub test gone (replaced by real tests).
- [ ] `cargo test --workspace` green.
- [ ] clippy + fmt clean.
- [ ] No wire / spec changes.
- [ ] Commit subject: `feat(brain-workers): Worker trait + scheduler (sub-task 8.1)`.

~500 LOC of impl + ~400 LOC of tests, single commit.
