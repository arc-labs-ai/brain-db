# Phase 9 — Tokio → Glommio port audit

Reference document for sub-tasks 9.4–9.14. Catalogues every `tokio::*`
use-site in shard-bound code with a per-site disposition, and locks in
the cross-cutting decisions that span multiple sub-tasks.

**Status:** sub-task 9.2 output. Implementers update each row to
`done` as the port lands; if a row turns out wrong, fix the row and
the matching code together.

**Reading guide:** Section 1 is the summary. Sections 2–10 are the
per-crate inventory. Section 11 is the cross-cutting decisions. Section
12 is open questions for the user (block 9.3+ if non-empty).

---

## 1. Summary table

| Crate | tokio uses in `src/` | tokio uses in `tests/` | Net action |
| ----- | -------------------: | ---------------------: | ---------- |
| `brain-core` | 0 | 0 | none |
| `brain-protocol` | 0 | 0 | none |
| `brain-storage` | 0 | 0 | none — but **see §11.3 WAL group commit** |
| `brain-metadata` | 0 | 0 | none |
| `brain-index` | 0 | 0 | none |
| `brain-embed` | 0 | 0 | none — but **see §11.4 embedder yield discipline** |
| `brain-planner` | 0 in `src/` | many `#[tokio::test]` | drop `+ Send` from `WriterHandle` trait |
| `brain-ops` | 1 file (`subscribe.rs`) + 2 test-attrs | many `#[tokio::test]` | **MOVE** EventBus to connection layer; drop `Send + Sync` cascades |
| `brain-workers` | 7 files | many `#[tokio::test]` | **PORT-GLOMMIO** scheduler + workers (timer/spawn/watch/yield) |

Verdict: the heavy lifting is concentrated in **brain-workers** (scheduler
+ per-worker yields) and **brain-ops/subscribe.rs** (broadcast bus
relocation). Six of the nine crates are runtime-agnostic today.

Tests are universally `#[tokio::test]` and **stay tokio**. Tests do not
run under Glommio; they exercise the runtime-agnostic surfaces (traits,
sync I/O) using a tokio harness for `.await` plumbing.

---

## 2. brain-core / brain-protocol / brain-storage / brain-metadata / brain-index

Zero tokio in `src/`. Zero in `tests/`. Nothing to port at the
crate-source level.

Notes:
- `brain-protocol` defines the wire frame codec; the **async I/O** that
  reads frames lives in `brain-server::connection` (9.9) and uses Tokio
  there. Disposition: **STAY-CONN**.
- `brain-storage` uses sync syscalls (`mmap`, `pwritev2`, `fsync`). See
  §11.3 for the executor-blocking concern.

---

## 3. brain-embed

Zero tokio in `src/`. The `Dispatcher` trait is `Send + Sync` by
design — `Arc<ModelHandle>` is shared across shards by spec
`04/07 §3`. See §11.4 for the per-shard-cache design.

`batch_window_ms` in `config/dev.toml` is currently unused; no batch
loop exists. If/when implemented, it must use a Glommio timer
(§11.4).

---

## 4. brain-planner

| File | Line | Surface | Disposition |
| ---- | ---: | ------- | ----------- |
| `src/executor/writer.rs` | 22 | `pub trait WriterHandle: Send + Sync` | **PORT-LOCAL** — drop both bounds. Each writer is per-shard; cross-thread sharing is impossible. |
| `src/executor/writer.rs` | 27–58 | `+ Send + 'a` on every `Pin<Box<dyn Future>>` return | **PORT-LOCAL** — drop `+ Send`. |
| `tests/*.rs` | many | `#[tokio::test]` | **STAY-TEST** |

**Cascade:** every impl of `WriterHandle` (`RealWriterHandle` in brain-ops,
plus test fixtures in brain-ops/lib.rs, brain-planner tests) loses its
`+ Send` bounds in the future returns. Mechanical search-and-replace —
audit each removal site in 9.7's plan.

The `Send + Sync` assertions in brain-ops (`fn require<T: Send + Sync>()`)
will start failing once we drop the trait bounds. Relax them to plain
`fn require<T>() {}` or delete (the assertion's purpose was to catch
accidental `!Send` types — we now want `!Send` deliberately).

---

## 5. brain-ops

### 5.1 `src/subscribe.rs`

| Line | Use | Disposition |
| ---: | --- | ----------- |
| 51   | `use tokio::sync::broadcast;` | **MOVE** — bus relocates to connection layer (§11.1). |
| 406  | `tokio::time::Instant::now()` | **PORT-GLOMMIO** — `glommio::timer::Instant::now()` once subscribe poll moves to shard-side; or **MOVE** entirely if poll lives in connection layer. |
| 409  | `tokio::time::Instant::now()` | same |
| 417  | `tokio::time::timeout(remaining, receiver.recv()).await` | same |

Recommendation: **MOVE** the SUBSCRIBE poll-and-deliver loop to the
connection layer. The shard-side responsibility shrinks to "publish
`EventEnvelope` to a per-shard local channel"; the connection layer
owns the cross-shard broadcast + per-subscriber bookkeeping (§11.1).

Net effect on `subscribe.rs`: keep `LsnAllocator`, `EventEnvelope`,
`SubscriptionFilter` (pure data); delete the `broadcast::Sender` and
the per-shard `EventBus` struct in favour of a single-consumer
channel into the connection layer.

### 5.2 `src/lib.rs`

| Line | Use | Disposition |
| ---: | --- | ----------- |
| 298, 312 | `#[tokio::test]` | **STAY-TEST** |

Plus several test-only `+ Send` bounds in `NopWriter` impls — these
cascade from the WriterHandle trait change (§4). When the bound drops
in brain-planner, these impls drop `+ Send` too.

### 5.3 Other `src/*.rs`

No direct tokio uses. The `OpsContext` carries `Arc<dyn WriterHandle>`,
`Arc<EventBus>`, etc. — once those traits and types lose `Send + Sync`,
`OpsContext` itself becomes `!Send`. That's the desired Phase 9 shape:
per-shard ops context.

---

## 6. brain-workers — the big port

### 6.1 `src/context.rs`

| Line | Use | Disposition |
| ---: | --- | ----------- |
| 7    | `use tokio::sync::watch;` | **PORT-LOCAL** — replace with `Rc<Cell<bool>>` shutdown flag. Single-threaded shard; watch is overkill. See §11.2. |
| 17   | `pub shutdown: watch::Receiver<bool>` | same |
| 27   | `*self.shutdown.borrow()` | same — becomes `self.shutdown.get()` on `Cell<bool>`. |

### 6.2 `src/scheduler.rs`

| Line | Use | Disposition |
| ---: | --- | ----------- |
| 19   | `use tokio::sync::watch;` | **PORT-LOCAL** (§11.2) |
| 20   | `use tokio::task::JoinHandle;` | **PORT-GLOMMIO** — `glommio::Task<()>` |
| 81   | `tokio::spawn(worker_loop(...))` | **PORT-GLOMMIO** — `glommio::Task::local(worker_loop(...)).detach()` |
| 153  | `tokio::time::timeout(remaining, handle.task).await` | **PORT-GLOMMIO** — `glommio::timer::sleep` racing the task |
| 207  | `tokio::select!` | **PORT-GLOMMIO** — `futures::future::select` or `glommio::executor().yield_*` between manual polls |
| 208  | `tokio::time::sleep(cfg.interval)` | **PORT-GLOMMIO** — `glommio::timer::sleep(cfg.interval)` |

Net rewrite of `scheduler.rs` is roughly 30 lines. The shape stays
identical; only the imports + spawn/sleep/select primitives change.

### 6.3 Per-worker yields

| File | Line | Use | Disposition |
| ---- | ---: | --- | ----------- |
| `src/worker.rs` | 85 | `tokio::task::yield_now().await` | **PORT-GLOMMIO** — `glommio::executor().yield_if_needed().await` |
| `src/slot_reclaim.rs` | 163 | same | same |
| `src/edge_scrub.rs` | 148 | same | same |
| `src/idempotency_cleanup.rs` | 130 | same | same |
| `src/wal_retention.rs` | 246 | same | same |
| `src/decay.rs` | 205 | same | same |

These are all "cooperative yield" points the workers use between batches
of work to keep the scheduler responsive. Mechanical replacement.

### 6.4 Tests

Every `tests/*.rs` uses `#[tokio::test]`. **STAY-TEST**. The tests
exercise the worker traits and the scheduler with a tokio runtime —
they don't validate the Glommio integration. The 9.7 (per-shard
scheduler) plan will add at least one Glommio-side integration test
inside `crates/brain-server/tests/`; the existing tokio tests remain
unchanged.

---

## 7. brain-server

Already on dual-runtime topology by design:
- **Connection layer** (9.9): Tokio. `tokio::net::TcpListener`,
  `tokio::io::AsyncRead/Write`, `tokio::spawn` per connection.
- **Shards** (9.4): Glommio. `LocalExecutor` per shard, pinned to a CPU.

The boundary primitive is a per-shard channel. Recommended:
`flume::bounded` (runtime-agnostic; supports both async send/recv on
the tokio side and on the glommio side without bridging).
**Alternative:** `tokio::sync::mpsc` on the connection-layer end +
`futures::channel::mpsc` on the shard end with manual wakers — more
fiddly. Confirm in 9.10.

---

## 8. Cross-cutting design decisions

These bind multiple sub-tasks. Approve before 9.4 starts.

### 8.1 EventBus topology (affects 9.7, 9.11)

**Decision:** per-shard local + connection-layer fan-out.

```
   Shard 0 Glommio                Connection layer Tokio
   ┌──────────────────────┐       ┌──────────────────────────────┐
   │ writer.publish(env) ─┼──┐    │ SubscriptionRegistry         │
   │                      │  │    │  - HashMap<SubId, mpsc::Tx>  │
   │ LocalEventBus        │  │    │  - HashMap<Filter, Vec<SubId>>│
   │  (single consumer)   │  │    │  - per-shard cross-shard mpsc │
   │       │              │  │    │       ▲                       │
   │       ▼              │  │    │       │ flume::Rx (Tokio side)│
   │   fanout_task ───────┼──┼────┼──────┘                        │
   │   (forwards to       │  │    │                               │
   │    flume::Sender)    │  │    │ Subscriber poll loop:          │
   └──────────────────────┘  │    │   recv → filter → tokio write  │
                             │    └──────────────────────────────┘
                             ▼
                       flume::bounded(1024)   (one per shard)
```

- Per-shard `LocalEventBus` is `Rc<RefCell<Vec<EventEnvelope>>>`-equivalent
  — a single-thread, single-consumer queue. No `Send`/`Sync`.
- One `fanout_task` per shard, spawned via `Task::local`, drains the
  local bus and forwards to the connection-layer registry.
- The registry holds per-subscriber `tokio::sync::mpsc::Sender` and
  dispatches based on the registered filter.
- Drop on overflow: per-subscriber counter, error frame to client on
  reconnect attempt.

Subtask 9.11 implements this; subtask 9.7 reserves the right hook on
the per-shard OpsContext.

### 8.2 Shutdown signal (affects 9.7, 9.14)

**Decision:** per-shard `Rc<Cell<bool>>`, not `watch::channel<bool>`.

Rationale: single-threaded shard means atomic ordering is moot; a
plain `Cell<bool>` suffices. Saves a watch primitive, simpler to
read, no broken-pipe edge cases. The connection layer (Tokio) keeps
`tokio::sync::watch` for the multi-shard fan-out signal.

Per-shard scheduler:
```rust
struct WorkerContext {
    pub ops: Rc<OpsContext>,
    pub shutdown: Rc<Cell<bool>>,
}
```

The shard's accept loop sets `shutdown.set(true)` on SIGINT; every
worker observes between cycles.

### 8.3 WAL group commit semantics (affects 9.6) — **LOCKED: option (b)**

`brain-storage::wal::segment.rs` uses sync
`libc::pwritev2(fd, iov, 1, offset, RWF_DSYNC)`. A sync syscall inside
a Glommio future **blocks the entire shard's executor** for the
duration of the fsync. Tail-latency tests in spec §16/02 will detect
this.

**Decision (locked):** port WAL to io_uring (`IORING_OP_WRITE` +
`IORING_OP_FSYNC`) via Glommio's `DmaFile` / raw uring submission.
Adds **sub-task 9.6a** ("WAL io_uring port") that lands immediately
before 9.6's per-shard hookup. Aligns with spec §05's "io_uring
everywhere" framing.

Out of scope: Glommio's `DmaFile` doesn't expose `RWF_DSYNC` directly;
9.6a wraps the raw uring submission to express the equivalent ordering
guarantee (write+fsync as one ordered pair per group commit).

Rejected:
- (a) Keep sync `pwritev2` — p99 latency hit, real bug under load.
- (c) `run_blocking()` per commit — defeats thread-per-core.

### 8.4 Embedder ownership (affects 9.7)

**Decision:** per-shard `CachingDispatcher` + shared `Arc<ModelHandle>`.

- `Arc<ModelHandle>` is `Send + Sync` already (weights are read-only).
- `CachingDispatcher` becomes `!Send` — its LRU cache lives in `Rc<RefCell<…>>`.
- Each shard constructs its own dispatcher in `Shard::new` with `Arc<ModelHandle>::clone()`.

This works inside Glommio because:
1. `embed_batch` is CPU-bound, not I/O-bound — no syscall stall.
2. A single forward pass on bge-small is ~10 ms on a recent CPU. The shard yields after each batch via `yield_if_needed()` inside the embed loop (added in 9.7's plan).
3. The model itself never moves between shards — only the `Arc` does.

**Open question:** is the `Dispatcher: Send + Sync` trait bound (in
`brain-embed::dispatcher.rs` L38) still necessary? If every dispatcher
clone is per-shard, the bound becomes vestigial. **Recommendation:**
keep the bound. The trait is the substrate's "you can wire your own
embedder" extension point — operators may want to share a remote
inference server across shards. The bound is cheap and doesn't
constrain `CachingDispatcher` (it implements `Send + Sync` via the
inner `Arc<Model>` already).

### 8.5 `+ Send` audit conclusions

Sites carrying `+ Send` bounds that **should drop** them in Phase 9:

1. `brain-planner::WriterHandle` — drop both `Send + Sync` on the trait, drop `+ Send` on every future return (§4).
2. `brain-ops::NopWriter` in `lib.rs:175-258` — cascades from §1.
3. `brain-ops::access_buffer::AccessBuffer` Send+Sync assertion — drop (per-shard).
4. `brain-ops::context::OpsContext` Send+Sync assertion — drop (per-shard).
5. `brain-ops::subscribe::EventBus` Send+Sync assertion (line 467) — drop (per-shard); the connection-layer `SubscriptionRegistry` is the Send+Sync surface.
6. `brain-ops::writer::RealWriterHandle` Send+Sync assertion (line 140) — drop.

Sites that **keep** `+ Send`:

1. `brain-embed::Dispatcher` trait (§8.4 rationale).
2. `brain-embed::cache::EmbedCache` assertion (the cache is the only intentionally-shared `Send + Sync` data structure).

---

## 9. Open questions for the user

**Status: none open.** All §8 decisions are locked.

Decisions taken at audit time (commit `<this commit>`):
1. **§8.3 WAL group commit** — port to io_uring. Adds sub-task **9.6a**.
2. **Cross-shard mpsc** — `flume` (runtime-agnostic, async on both ends).
3. **`rt.rs` shim** — skipped. Ports happen in place.

All §11 status rows update as ports land. New rows added by 9.6a.

---

## 10. Recommended port order

| Sub-task | Phase 9 work | Audit rows touched |
| -------- | ------------ | ------------------ |
| 9.3 (routing) | independent | none |
| 9.4 (shard scaffold) | Glommio LocalExecutor plumbing, channel boundary | §7, §8.2 (shutdown) |
| 9.5 (real arena) | brain-storage arena types | none |
| 9.6a (WAL io_uring port) | brain-storage WAL → io_uring submission | **§8.3** |
| 9.6 (real WAL hookup) | brain-storage WAL → shard (uring-backed after 9.6a) | depends on 9.6a |
| 9.7 (per-shard OpsContext + scheduler) | **biggest port** | §4, §5.3, §6.1, §6.2, §6.3, §8.2, §8.5 |
| 9.8 (wire Phase-8 seams) | summarizer/rebuild/wal-retention/snapshot/cache-evict | none new |
| 9.9 (connection layer + TLS) | brain-server::connection | §7 (Tokio side) |
| 9.10 (frame dispatcher) | Tokio↔Glommio channel boundary | §7 |
| 9.11 (cross-shard SUBSCRIBE) | EventBus relocation + SubscriptionRegistry | §5.1, §8.1 |
| 9.12 (ArcSwap + crossbeam-epoch) | per-shard publication | none |
| 9.13 (health + metrics) | admin HTTP server | none |
| 9.14 (graceful shutdown) | shutdown signal propagation | §8.2 |
| 9.15 (Summarizer adapter) | OpenAI/Ollama backend | none |
| 9.16 (tombstone filter) | brain-planner change | none |
| 9.17 (E2E smoke) | acceptance gate | none |
| 9.18 (phase exit) | docs/phases checklist, tag | none |

**9.7 is the cascade hotspot.** It absorbs §4 (WriterHandle bound
drop), §5.3 (OpsContext becomes `!Send`), §6 (scheduler + worker port),
§8.2 (shutdown), §8.5 (assertion cleanup). The plan for 9.7 should
budget ~2× the LOC of other sub-tasks.

---

## 11. CI/grep guard (post-Phase-9)

Once the port is complete, add to `just verify`:

```bash
# No tokio in shard-bound src/, except brain-server/src/connection.rs and tests.
rg -n 'tokio::' \
    crates/{brain-ops,brain-workers,brain-embed,brain-planner,\
            brain-metadata,brain-index,brain-storage,brain-core}/src \
    && echo "FAIL: tokio leaked into shard code" && exit 1
```

This codifies the audit. Spec §10/02 says the discipline isn't
enforced by the type system; the grep is the next-best thing.

---

## 12. Status table (live; update as ports land)

| Row | Status | Sub-task |
| --- | :----: | -------- |
| §4 `WriterHandle: Send + Sync` drop | TODO | 9.7 |
| §5.1 EventBus relocation | TODO | 9.11 |
| §5.2 brain-ops test-attrs stay | n/a (STAY-TEST) | — |
| §5.3 OpsContext `!Send` | TODO | 9.7 |
| §6.1 WorkerContext shutdown port | TODO | 9.7 |
| §6.2 Scheduler tokio→glommio | TODO | 9.7 |
| §6.3 Worker yields tokio→glommio | TODO | 9.7 |
| §8.1 EventBus topology decision | LOCKED | 9.11 |
| §8.2 Shutdown signal: `Rc<Cell<bool>>` | LOCKED | 9.7 / 9.14 |
| §8.3 WAL group commit (io_uring port) | **done** (9.6a) | 9.6a |
| §8.4 Embedder ownership | LOCKED | 9.7 |
| §8.5 `+ Send` assertion drops | TODO | 9.7 |
| CI grep guard | TODO | 9.18 |

When a row turns `done`, update its disposition (`done`, `superseded`,
or `revised`) and link the commit hash.
