# 07 — Background workers

**Audience:** anyone tuning a worker's cadence, adding a new
worker, reading a `/v1/admin/workers` dashboard at 2 AM, or
wondering why an ENCODE went fast but a salience update lagged
by ten minutes.

**Goal:** by the end you should know what every worker does, who
schedules them, how they share an executor with request
handlers without starving each other, and what knobs an operator
gets.

This chapter assumes [01 — System architecture](01-system-architecture.md)
(one Glommio executor per shard, workers run *on* it) and
[03 — Arena and WAL](03-arena-and-wal.md) (the durability
boundary the workers operate behind).

---

## Why workers exist

A request handler does just enough to make a state change
durable: write the WAL record, update redb, insert into HNSW,
publish the new memory. Lots of work the substrate also wants
done is deliberately *not* on the hot path:

- Salience decay (slow, every hour).
- Consolidating similar episodic memories into semantic ones.
- Rebuilding the HNSW after enough tombstones accumulate.
- Pruning the 24-hour idempotency table.
- Reclaiming slots after the FORGET grace expires.
- Deleting old WAL segments after they've been checkpointed.

Each of these is a cycle: wake on an interval, do bounded work,
record metrics, sleep. The shape is identical across the twelve
substrate workers and the eight knowledge-layer workers — what
differs is the cadence, the source of work, and what the cycle
actually does.

Twenty workers is a lot, but the registry is `enum`-flat and
each worker is a single file in
`crates/brain-workers/src/workers/`. The infrastructure that
makes them composable lives in two files: `worker.rs` (the
trait + the batch-driver helper) and `scheduler.rs` (the
per-shard registry).

---

## Where they run

**On the shard's Glommio executor**, not on a separate thread.
This is a deliberate choice and worth a paragraph.

The textbook description in distributed-database literature has
"background workers" on dedicated cores, kept apart from
request-serving cores. We don't. Three reasons:

- **Per-shard isolation costs nothing in our setup.** Each
  shard's executor is already pinned (or unbound) to its own OS
  thread; running its workers on the same thread doesn't
  introduce cross-core contention because the *only* core the
  shard touches is its own.
- **Glommio's cooperative scheduling makes this safe.** A
  worker that calls `glommio::executor().yield_if_needed()`
  yields like any other future. Long-running batches are
  *bounded* (see below) and yield frequently. A worker hogging
  CPU for tens of seconds is a bug, not a design point.
- **Shared shard-local state.** Workers want to read the arena,
  scan redb, mutate the HNSW. Putting them on a separate core
  would mean serialising every access through a channel —
  exactly the cross-core hop we're trying to avoid.

The cost is that workers and request handlers share a single
core's CPU budget. The mitigations are (a) bounded batch
sizes, (b) cooperative yields every 50 units of work, and (c)
the operator's ability to pause any worker. Sections below.

---

## The `Worker` trait

`crates/brain-workers/src/worker.rs:26`:

```rust
pub trait Worker: 'static {
    fn name(&self) -> &'static str;
    fn kind(&self) -> WorkerKind;
    fn config(&self) -> WorkerConfig;

    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>>;
}
```

Four obligations. The interesting one is `run_cycle`:

- **Returns `Pin<Box<dyn Future>>`** rather than using
  `async-trait`. We avoid the dep; the boxing cost is paid once
  per cycle, not per work unit.
- **Returns `usize`** — units processed this cycle. The
  scheduler adds it to the worker's `processed_total` metric.
- **Bounded.** Implementations must respect `batch_size` and
  `max_runtime` from `WorkerConfig`. Most workers delegate to
  `drive_batch` (next section) for this discipline.
- **Takes `&self`, not `&mut self`.** Workers use interior
  mutability (`AtomicU64` counters, locks inside a `WorkerContext`
  field) rather than exclusive borrow — the scheduler doesn't
  want to hold an `&mut` on every worker for the duration of
  its cycle
  (`crates/brain-workers/src/worker.rs:21`).

### `drive_batch` — the cycle-shape helper

`crates/brain-workers/src/worker.rs:56`:

```rust
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
    let mut processed = 0;
    loop {
        if processed >= cfg.batch_size { break; }
        if start.elapsed() >= cfg.max_runtime { break; }
        if ctx.is_shutdown() { break; }

        match unit(ctx).await? {
            true  => processed += 1,
            false => break,                // nothing else to do; sleep early
        }
        if processed % YIELD_EVERY == 0 {
            glommio::executor().yield_if_needed().await;
        }
    }
    Ok(processed)
}
```

Four termination conditions:

1. **Hit `batch_size`.** Soft cap on units per cycle.
2. **Hit `max_runtime`.** Soft cap on wall-clock time per cycle.
3. **Shutdown signalled.** Drop the rest, return what we did.
4. **No more work.** Sleep until next interval.

And one *yield* condition: every 50 units processed
(`YIELD_EVERY = 50`,
`crates/brain-workers/src/worker.rs:16`), explicitly yield to
the executor so request handlers (or other workers) can run.
This is what keeps a 10 000-unit decay cycle from blocking a
RECALL for tens of milliseconds.

---

## The scheduler

`WorkerScheduler`
(`crates/brain-workers/src/scheduler.rs:77`) is one per shard.
Construction is sync; `register(...)` must be called from
inside a Glommio executor because it spawns a task there:

```rust
pub fn register(
    &mut self,
    worker: Arc<dyn Worker>,
    ops: Arc<OpsContext>,
) -> Result<(), WorkerError> {
    …
    let task = glommio::spawn_local(worker_loop(
        worker, ctx, metrics.clone(), controls.clone(),
    ));
    self.handles.insert(name, WorkerHandle { … });
    Ok(())
}
```

Each worker gets its own `glommio::Task<()>` running `worker_loop`,
which is the canonical loop:

```
loop:
  if shutdown: break
  if !disabled and !paused:
    metrics.cycle_started_at = now()
    let processed = worker.run_cycle(&ctx).await
    metrics.processed_total += processed
    metrics.last_cycle_duration_ms = ...
    metrics.cycles_completed += 1
  select:
    on sleep(interval):     continue
    on wake_rx.recv():      continue   (run-now request)
    on shutdown_flag flip:  break
```

A few things worth knowing:

- **Single-task per worker.** The registry rejects duplicate
  names; the type system doesn't prevent it but the runtime
  check does
  (`crates/brain-workers/src/scheduler.rs:104`).
- **Disabled workers stay registered.** Their `worker_loop`
  ticks on its interval but never calls `run_cycle`. This is
  what `ADMIN_WORKER_STOP` flips
  (`crates/brain-workers/src/config.rs:69`). Operators can
  re-enable without restarting.
- **`WorkerControls`** carry the `paused: AtomicBool` and a
  `flume::bounded(1)` wake channel
  (`crates/brain-workers/src/scheduler.rs:47`). `pause`,
  `resume`, and `run_now` each map to one operation through
  these.
- **Shutdown is cooperative.** A per-shard `Arc<AtomicBool>` is
  flipped; each worker's `ctx.is_shutdown()` check returns
  `true` on the next polling boundary. The scheduler then
  awaits every task with a 5 s soft budget
  (`crates/brain-workers/src/scheduler.rs:35`); tasks still
  alive after that get `Task::cancel`.

### `WorkerContext` — what a worker sees

`crates/brain-workers/src/context.rs`:

```rust
pub struct WorkerContext {
    pub ops: Arc<OpsContext>,
    pub shutdown: Arc<AtomicBool>,
}
```

Workers reach the per-shard state — embedder, index, metadata,
writer, summarizer, all of it — through the same `OpsContext`
([chapter 01](01-system-architecture.md)) request handlers
use. Same handle bag, same access paths, same single-writer
discipline.

That last point is important: a worker that wants to *mutate*
metadata or HNSW must respect the same `&mut self`-driven
writer trait the handlers use. Two writers per shard isn't
possible by construction, so a long-running worker mutation
serialises against in-flight handlers naturally.

---

## `WorkerConfig` and the defaults table

Every worker has the same four knobs
(`crates/brain-workers/src/config.rs:66`):

```rust
pub struct WorkerConfig {
    pub enabled: bool,
    pub interval: Duration,        // sleep between cycles
    pub batch_size: usize,         // soft cap units/cycle
    pub max_runtime: Duration,     // soft cap wall-clock/cycle
}
```

The defaults are pinned in `WorkerConfig::defaults_for`
(`crates/brain-workers/src/config.rs:85`). Here's the substrate
twelve, plus the knowledge-layer eight:

| Worker | Default interval | Batch | Max runtime | Default enabled |
|---|---|---|---|---|
| `decay` | 1 h | 10 000 | 5 s | yes |
| `access_boost` | 10 s | 1 000 | 0.5 s | yes |
| `consolidation` | 5 m | 100 | 10 s | yes |
| `hnsw_maintenance` | 5 m | 1 | 60 s | yes |
| `idempotency_cleanup` | 1 h | 10 000 | 5 s | yes |
| `slot_reclamation` | 10 m | 1 000 | 5 s | yes |
| `wal_retention` | 1 m | 100 | 2 s | yes |
| `edge_scrub` | 30 m | 5 000 | 5 s | yes |
| `counter_reconcile` | 1 h | 1 | 30 s | yes |
| `statistics` | 5 m | 1 | 5 s | yes |
| `embedder_cache_evict` | 1 m | 5 000 | 2 s | yes |
| `snapshot` | 1 h | 1 | 5 m | **no** |
| `backfill` | 1 s | 256 | 20 s | yes |
| `forget_cascade` | 1 s | 256 | 10 s | yes |
| `schema_migration` | 1 s | 128 | 30 s | yes |
| `supersession_sweeper` | 24 h | 256 | 30 s | yes |
| `audit_log_sweeper` | 24 h | 1 024 | 30 s | yes |
| `llm_cache_sweeper` | 1 h | 1 024 | 10 s | yes |
| `stale_extraction_detector` | 1 h | 512 | 10 s | yes |
| `entity_gc` | 24 h | 256 | 30 s | **no** |

Two workers are *opt-in* — `snapshot` and `entity_gc`. The
snapshot worker is destructive-adjacent (deletes old snapshots)
and operators opt-in via configuration. `entity_gc` is
similarly cautious — entity garbage collection has subtle
correctness questions in the knowledge layer that operators
should turn on explicitly.

---

## The substrate twelve

What each one does, in one paragraph, with the source citation.

### `decay`

Lowers `salience` on every memory according to the Ebbinghaus
forgetting curve. Half-lives per `MemoryKind`
(`crates/brain-workers/src/workers/decay.rs:36`):

- `Episodic` — 30 days.
- `Semantic` — 365 days.
- `Consolidated` — 90 days.

Default interval 1 hour, batch 10 000 memories per cycle.
Decayed values are only written when the delta exceeds
`MIN_DELTA_FOR_WRITE` — avoids re-rewriting every row for a
0.00001 change.

### `access_boost`

Reads the `AccessBuffer` (memories touched since last cycle) and
bumps their salience by a factor. Default 10 s interval, 1 000
units per cycle. The cheap counterpart to `decay`: every recall
nudges the cued memory's score up; `decay` pulls everything
down.

### `consolidation`

Clusters similar episodic memories and emits a single
consolidated semantic memory referencing them as
`DERIVED_FROM`. Default interval 5 m, batch 100, max runtime
10 s. Threshold
`DEFAULT_SIMILARITY_THRESHOLD = 0.6`
(`crates/brain-workers/src/workers/consolidation.rs:55`). The
"sleep" analogue from cognitive science.

Consolidation creates new memories and writes new edges; both
go through the normal WAL/HNSW path. If a `Summarizer` is
configured (the LLM optional dep), the consolidated memory's
text is an LLM-generated summary of the cluster; otherwise it's
a concatenation.

### `hnsw_maintenance`

Reads the index's stats and either does nothing, schedules a
rebuild "soon," or runs a full rebuild
(`crates/brain-workers/src/workers/hnsw_maint.rs:87`).

Thresholds (`crates/brain-workers/src/workers/hnsw_maint.rs:66`):

- `tombstone_full_rebuild = 0.30` — full rebuild above 30 %
  tombstones.
- `tombstone_schedule = 0.15` — schedule a rebuild above 15 %.
- `recall_full_rebuild = 0.90` — full rebuild below 90 %
  recall@K (when sampling is wired).
- `recall_schedule = 0.93` — schedule below 93 %.

When it rebuilds, it pulls a `(MemoryId, vector)` snapshot from
the `RebuildSource`, calls `HnswIndex::rebuild`, then
`SharedHnsw::swap()`s the new index in. The catch-up between
"snapshot taken" and "swap done" is what the
`backfill`/`forget_cascade` patterns address downstream.

Batch is `1` because the cycle is monolithic. `max_runtime` is
60 s — enough for a 1 M-node shard's rebuild.

### `idempotency_cleanup`

Prunes expired entries from `IDEMPOTENCY_TABLE`. Default TTL
24 h
(`crates/brain-workers/src/workers/idempotency_cleanup.rs:23`).
Interval 1 h, batch 10 000, max runtime 5 s. The single biggest
growth source on a write-heavy shard, if this worker is paused.

### `slot_reclamation`

Reclaims tombstoned slots after the FORGET grace period (default
**7 days**,
`crates/brain-workers/src/workers/slot_reclaim.rs:42`). Bumps
`slot_version` in the arena, clears flags, writes a `Reclaim`
WAL record. The grace period is what keeps stale `MemoryId`s
from accidentally hitting a fresh allocation —
[chapter 03](03-arena-and-wal.md) covers the version-check side
of this.

### `wal_retention`

Deletes WAL segments older than the latest committed checkpoint,
keeping a configurable retention count (default 4 segments —
~1 GiB per shard at 256 MiB segments). Interval 1 m, batch 100,
max runtime 2 s. Without this, the WAL grows monotonically.

### `edge_scrub`

Walks the edge tables looking for dangling references (edges
pointing at tombstoned or reclaimed memories) and drops them.
30-minute interval — edges are the most-derived data and don't
need fast catchup.

### `counter_reconcile`

Reconciles the cached counters on `MemoryMetadata`
(`edges_out_count`, `edges_in_count`, etc.) by full-scanning the
relevant tables. Batch 1 (full-scan is monolithic), max runtime
30 s. The sink writes don't maintain these counters online — see
the deliberate placeholder at
`crates/brain-metadata/src/sink.rs:27`. This worker brings them
back to truth.

### `statistics`

Updates per-shard statistics (memory count, agent count,
salience distribution, etc.) used by the query planner for
strategy selection. Interval 5 m. Plain reads + a single redb
write per cycle.

### `embedder_cache_evict`

Removes embedder LRU entries older than
`DEFAULT_CACHE_MAX_AGE`. Interval 1 m, batch 5 000. Mostly
redundant with LRU itself; this is the time-based companion to
size-based eviction, for caches where the working set is small
but stale entries should age out anyway.

### `snapshot`

Takes a consistent snapshot (arena + redb + WAL pointer) with a
`max_count = 7` and `max_age = 30 days` retention policy
(`crates/brain-workers/src/workers/snapshot.rs:58`). **Disabled
by default** — operators opt in via configuration. Interval 1 h
when enabled. Snapshots are the basis for cold backup and the
fast cold-start path for the HNSW (see
[chapter 04](04-hnsw-index.md)).

The retention decision is a pure function
(`crates/brain-workers/src/workers/snapshot.rs:75`):

```
keep snapshot if:
  age < max_age AND idx < max_count
delete if:
  too old (age >= max_age) OR too many (idx >= max_count)
```

---

## The knowledge-layer eight

Active only when a schema is declared. The same scheduler hosts
them; the same `WorkerConfig` shape governs them.

| Worker | What it does |
|---|---|
| `backfill` | Re-runs extractors over memories that pre-date the current schema. Admin-triggered; idle when no work is queued. |
| `forget_cascade` | When a memory is hard-forgotten, drops every derived entity / statement / relation that depended on it. |
| `schema_migration` | Applies queued schema-migration steps in bounded batches. |
| `supersession_sweeper` | Reaps superseded statement/relation versions after their grace period. Daily cadence. |
| `audit_log_sweeper` | Trims the extraction-audit + entity-resolution-audit tables. Daily cadence. |
| `llm_cache_sweeper` | Evicts LLM extractor cache entries past their TTL. Hourly. |
| `stale_extraction_detector` | Flags memories whose extraction was run against an old extractor version, for backfill. Hourly. |
| `entity_gc` | Reclaims tombstoned entities after their grace. Daily; **disabled by default.** |

The knowledge-layer workers are covered in more detail in
[09 — Knowledge layer](09-knowledge-layer.md) and
[10 — Extractors](10-extractors.md). This chapter just registers
that they exist and share the same scheduler.

---

## How a cycle looks in practice

Let's trace the `decay` worker through one cycle in detail.

```
T = 0       worker_loop wakes from sleep(interval = 1 h)
T = 0       ctx.shutdown? no.  config.enabled? yes.  paused? no.
T = 0       run_cycle(&ctx).await begins
            │
            │ drive_batch is called
            │  │
            │  ├─ for each unit (up to batch_size=10000, or 5 s):
            │  │     pull next memory from a redb cursor
            │  │     compute decayed salience(kind, age, last_access)
            │  │     if delta >= MIN_DELTA_FOR_WRITE:
            │  │         write UpdateSalience WAL record
            │  │         redb txn: update memories table
            │  │     yield_if_needed every 50 units
            │  │
            │  └─ return processed_count
            │
T = ~3 s    run_cycle returns Ok(10 000)
            metrics.cycles_completed += 1
            metrics.processed_total  += 10 000
            metrics.last_cycle_duration_ms = ~3 000
T = ~3 s    sleep(interval - elapsed) until ~T = 1 h
```

Every step yields to the executor; an arriving `ENCODE` between
two decay units gets handled with at most one decay unit's worth
of latency (~300 µs on average). The 50-unit yield discipline is
what makes this true regardless of `batch_size`.

If shutdown fires during the cycle, `ctx.is_shutdown()` catches
it at the top of the `drive_batch` loop and the cycle returns
early with the partial count.

---

## Failure modes

**A worker's `run_cycle` returns `Err`.** The scheduler logs
the error and continues. `metrics.last_error` is updated.
Persistent failures show up as non-increasing `cycles_completed`
relative to wall time — a useful alert signal.

**A worker panics.** Glommio kills the task; the loop is gone.
Other workers in the same scheduler keep running. Operators
should treat this as a bug and restart the shard; v1 has no
automatic re-spawn.

**A worker hits `max_runtime` every cycle.** Means the batch
isn't draining. The metric `processed_total` keeps growing, but
the queue is growing faster. Two responses: tune the cadence
(more frequent cycles), or raise `batch_size` / `max_runtime`.
The defaults assume a reasonable steady-state load; sustained
high-write deployments may need tuning.

**The shutdown drain budget expires.** 5 s is enough for any
single cycle (every worker's `max_runtime` is ≤ 60 s and we're
*not* waiting for a full cycle — we're waiting for the next
`yield_if_needed` check). A worker that takes longer is either
panicking (covered above) or stuck on an `.await` that doesn't
observe the shutdown signal. The scheduler calls
`Task::cancel`, which drops the future, which drops any
state-holding-but-unblocking thing it had.

**Paused worker forgotten.** Operator pauses `decay` for an
investigation, forgets to resume. Salience values stay frozen;
recall quality slowly drifts. The pause is *intentional* (so
we don't auto-resume), but the admin UI should show a "paused
since T" badge prominently.

**Worker re-uses the same `WorkerKind` name.** Rejected by
`register()`
(`crates/brain-workers/src/scheduler.rs:104`). If you're adding
a worker that's "like decay but different," give it a different
`WorkerKind` variant.

---

## What an operator can do

Three admin verbs map onto the `WorkerControls`
(`crates/brain-workers/src/scheduler.rs:135`):

- **Pause** — flip `controls.paused`. The loop keeps ticking
  but skips `run_cycle`. Useful when investigating a hot shard.
- **Resume** — clear `controls.paused` and kick `wake_tx`. The
  loop doesn't wait out the current sleep.
- **Run now** — kick `wake_tx`. The loop wakes from its
  current sleep and runs `run_cycle` once outside the interval
  schedule. No-op if the worker is paused (run-now means "wake
  early," not "ignore pause").

Plus a static set of admin endpoints:

- `/v1/admin/workers` — list, with `name`, `kind`, `interval`,
  `cycles_completed`, `processed_total`, `last_cycle_duration_ms`,
  `last_error`, `paused`, `enabled`.
- `/v1/admin/workers/<name>/pause`, `/resume`, `/run-now`.
- The same controls are also accessible via the wire protocol's
  admin opcodes.

Metrics are exported per-worker through the admin HTTP `/metrics`
endpoint. The names are `brain_worker_*` and each row carries a
`worker_name` and `worker_kind` label.

---

## Configuration & tuning

Every worker's four knobs (`enabled`, `interval`, `batch_size`,
`max_runtime`) can be overridden per-deployment via the
`[workers]` section of the TOML config — fields named
`<worker>_interval_sec`, etc.
(`crates/brain-server/src/config/mod.rs` near line 100).

Standing rules:

- **Don't disable `wal_retention`.** WAL grows monotonically;
  disk fills.
- **Don't disable `idempotency_cleanup`.** Same — table grows
  monotonically.
- **`snapshot` is opt-in, but you almost certainly want it on
  in production.** Without snapshots, every cold start is a
  full HNSW rebuild — 30 s per 1 M-node shard.
- **`hnsw_maintenance` thresholds are correctness-adjacent.**
  If you raise `tombstone_full_rebuild` past 0.5 the index's
  recall starts to degrade visibly. Lower thresholds are safer
  but cost CPU on rebuild.
- **`decay` half-lives are domain choices.** Increase them for
  archival-leaning agents; decrease for ephemeral chat-like
  agents. The cost is in client-visible salience drift.
- **Watch `last_cycle_duration_ms` against `max_runtime`.** A
  worker frequently hitting its `max_runtime` cap means
  its queue is growing; either raise `max_runtime`, raise
  `batch_size`, or shorten `interval`.
- **Don't pause more than one worker for "performance."** Each
  worker has a job. If the shard is overwhelmed, scale shards
  or scale shard count; pausing maintenance is a temporary
  measure.

---

## Where it lives in the code

| Topic | Path |
|---|---|
| `Worker` trait + `drive_batch` | `crates/brain-workers/src/worker.rs` |
| `WorkerConfig`, `WorkerKind`, defaults table | `crates/brain-workers/src/config.rs` |
| `WorkerContext` (handle bag + shutdown) | `crates/brain-workers/src/context.rs` |
| `WorkerScheduler`, `worker_loop`, controls | `crates/brain-workers/src/scheduler.rs` |
| `WorkerMetrics`, `Snapshot` | `crates/brain-workers/src/metrics.rs` |
| `Summarizer` trait (consolidation's LLM hook) | `crates/brain-workers/src/summarizer.rs` |
| Decay | `crates/brain-workers/src/workers/decay.rs` |
| Access boost | `crates/brain-workers/src/workers/access_boost.rs` |
| Consolidation | `crates/brain-workers/src/workers/consolidation.rs` |
| HNSW maintenance + thresholds | `crates/brain-workers/src/workers/hnsw_maint.rs` |
| Idempotency cleanup | `crates/brain-workers/src/workers/idempotency_cleanup.rs` |
| Slot reclamation | `crates/brain-workers/src/workers/slot_reclaim.rs` |
| WAL retention | `crates/brain-workers/src/workers/wal_retention.rs` |
| Edge scrub | `crates/brain-workers/src/workers/edge_scrub.rs` |
| Counter reconcile | `crates/brain-workers/src/workers/counter_reconcile.rs` |
| Statistics | `crates/brain-workers/src/workers/statistics.rs` |
| Cache evict | `crates/brain-workers/src/workers/cache_evict.rs` |
| Snapshot + retention | `crates/brain-workers/src/workers/snapshot.rs` |
| Knowledge-layer workers | `crates/brain-workers/src/workers/backfill.rs`, `forget_cascade.rs`, `schema_migration.rs`, `supersession_sweeper.rs`, `audit_log_sweeper.rs`, `llm_cache_sweeper.rs`, `stale_extraction_detector.rs`, `entity_gc.rs` |

---

## Further reading

- [01 — System architecture](01-system-architecture.md) for how
  the per-shard executor hosts both request handlers and these
  workers.
- [03 — Arena and WAL](03-arena-and-wal.md) for what
  `slot_reclamation`, `wal_retention`, and `snapshot` operate
  on.
- [04 — HNSW index](04-hnsw-index.md) for the index that
  `hnsw_maintenance` rebuilds and `snapshot` persists.
- [05 — redb metadata](05-redb-metadata.md) for the tables that
  `idempotency_cleanup`, `counter_reconcile`, `edge_scrub`, and
  the knowledge-layer sweepers all operate on.
- [09 — Knowledge layer](09-knowledge-layer.md) and
  [10 — Extractors](10-extractors.md) for the eight
  knowledge-layer workers in their proper context.
