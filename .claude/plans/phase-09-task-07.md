# Sub-task 9.7 — Per-shard OpsContext + WorkerScheduler

**Reads:** `spec/10_concurrency_epochs/02_single_writer.md` §1-§7, `spec/01_system_architecture/04_layers.md` §5 (execution/workers), `docs/phases/phase-09-glommio-port.md` §4, §5.3, §6, §8.2, §8.5.
**Phase doc:** `docs/phases/phase-09-server.md` §9.7.
**Done when:** Each shard owns an `OpsContext` (per-shard, `!Send`) and a `WorkerScheduler` (per-shard, Glommio-driven). The cascade hotspot from the audit lands: `WriterHandle: Send + Sync` drops; `RealWriterHandle` wires `Wal::append`; redb-backed metadata sink replaces `InMemoryMetadataSink` in recovery.

---

## 1. Why this is the biggest sub-task in Phase 9

Audit §10 budget: "9.7 should be ~2× the LOC of other sub-tasks." It cascades five distinct changes that share scope:

1. **§4** — `brain-planner::WriterHandle` drops `Send + Sync` from the trait; every `+ Send` on future returns goes away. Every impl in brain-ops gets re-typed (it's `!Send` now). The compile-time `Send + Sync` assertions in `brain-ops/src/*` either drop or relax.
2. **§5.3** — `OpsContext` becomes `!Send`. The `Arc<...>` fields that were the source of `Sync` become `Rc<...>` (we're single-threaded inside a Glommio executor now).
3. **§6** — `brain-workers::WorkerScheduler` ports from `tokio::spawn` + `tokio::sync::watch` + `tokio::time::sleep` + `tokio::select!` to `glommio::spawn_local` + `Rc<Cell<bool>>` + `glommio::timer::sleep` + `futures_lite::or`.
4. **§8.2** — Per-worker shutdown via `Rc<Cell<bool>>` flag (set by the scheduler on shard shutdown; observed by every worker between cycles).
5. **§8.5** — Drop the `+ Send` assertion sites cascading from §4.

Plus the brain-server wiring: replace `InMemoryMetadataSink` (9.6's stand-in) with a real `MetadataDb`-backed sink; construct `OpsContext` per shard; register every Phase-8 worker against the per-shard scheduler.

---

## 2. Proposed split — **REVISED: split is not viable**

The split attempted in `.claude/plans/phase-09-task-07a.md` (now deleted)
was: 9.7a = WriterHandle Send-drop only; 9.7b = scheduler port + Arc→Rc;
9.7c = brain-server wire-up.

**The split doesn't compile.** Dropping `Send + Sync` from `WriterHandle`
makes `Arc<dyn WriterHandle>` non-Send, which transitively breaks:

- `ExecutorContext` (brain-planner) — also asserted `Send + Sync`.
- `brain-workers::Worker` trait — declares `Send + Sync + 'static` and
  `+ Send` on `run_cycle`'s return; the impls reference `WorkerContext`
  which holds `Arc<OpsContext>` which contains the now-!Send writer.
- `tokio::spawn(worker_loop(...))` in the scheduler — needs Send futures.

Once `WriterHandle` loses Send+Sync, the *entire* worker stack has to
port to Glommio at the same time, or the build is broken. The audit §10
warned about this ("9.7 is the cascade hotspot, budget ~2× other
sub-tasks") — the split into 7a/7b/7c was wishful thinking.

### Revised plan: 9.7 as one atomic commit

| Phase | Files | LOC |
| ----- | ----- | --: |
| brain-planner | WriterHandle trait + ExecutorContext assertion | ~30 |
| brain-ops | RealWriterHandle + NopWriter + 5 assertion sites + OpsContext Arc→Rc | ~150 |
| brain-workers | Worker trait + WorkerScheduler tokio→glommio + 14 worker yield sites + WorkerContext shutdown port + tests migrated | ~1500 |
| brain-server | per-shard OpsContext construction; real MetadataDb sink replaces InMemoryMetadataSink in recovery; register every Phase-8 worker against per-shard scheduler | ~600 |

**Total: ~2300 LOC. One commit.** Subject:
`feat(brain-server): per-shard OpsContext + worker scheduler (sub-task 9.7)`.

Risk profile: bigger commit means more to revert if something goes
wrong mid-port. Mitigation: stage carefully (each crate's edits done +
verified with `cargo check -p ...` before moving to the next), full
docker-verify before commit.

Estimated effort: 3–6 hours of focused work, plus container build/test
time. The session has accumulated context drift — recommend starting
9.7 in a fresh session with the revised plan as the seed.

If you'd rather do it as one commit: I can collapse the three plans into one. Higher merge risk if the verify gate finds something halfway through.

---

## 3. 9.7a — `WriterHandle` cascade

### 3.1 brain-planner

`crates/brain-planner/src/executor/writer.rs`:
- `pub trait WriterHandle: Send + Sync` → `pub trait WriterHandle`.
- Every method's `Pin<Box<dyn Future<Output = ...> + Send + 'a>>` → `Pin<Box<dyn Future<Output = ...> + 'a>>`.

### 3.2 brain-ops cascades

Every `impl WriterHandle for X` (RealWriterHandle in writer.rs, NopWriter in lib.rs test fixture) drops `+ Send` on every future return type.

Compile-time `Send + Sync` assertions:
- `brain-ops/src/access_buffer.rs:102` — drop.
- `brain-ops/src/context.rs:109` — drop.
- `brain-ops/src/subscribe.rs:467` — relax to no bound (the in-process EventBus stays `Send + Sync` for now; 9.11 splits it).
- `brain-ops/src/writer.rs:140` — drop.

`OpsContext` reshape:
- `Arc<TxnStore>` → `Rc<TxnStore>`.
- `Arc<EventBus>` → `Rc<EventBus>` (until 9.11 splits).
- `Arc<SubscriptionRegistry>` → `Rc<SubscriptionRegistry>`.
- `Arc<AccessBuffer>` → `Rc<AccessBuffer>`.
- Derive `Clone` keeps working (Rc is Clone).
- Removed: the Send+Sync assertion at `context.rs:109`.

This is a wide mechanical change. Most test fixtures across brain-ops/tests/*.rs use `Arc<RealWriterHandle>` → swap to `Rc<RealWriterHandle>`. ~25-30 test files touched.

### 3.3 New tests

- Compile-time `!Send` assertion on `OpsContext` (mirroring the old `Send + Sync` one).
- One integration test verifying that a `RealWriterHandle + Wal` pair works under a single-threaded Glommio executor.

### 3.4 brain-workers cascade

`brain_workers::context::WorkerContext` already holds `Arc<OpsContext>`. After the Arc→Rc change, that becomes `Rc<OpsContext>`. Cascade.

### 3.5 Risk

`Send + Sync` removal is wide-reaching; tests that did `tokio::spawn(async move { ... })` with `Arc<OpsContext>` inside might fail to compile. Those tests are already `#[tokio::test]` — under tokio's multi-threaded test harness, the spawned future must be Send. Two options:
- Move those tests to single-threaded (`#[tokio::test(flavor = "current_thread")]`).
- Wrap the OpsContext fields back to Arc where the test specifically wants cross-thread sharing.

Audit doesn't decide; pick at impl time based on which tests are affected.

---

## 4. 9.7b — `WorkerScheduler` Glommio port

### 4.1 brain-workers

`crates/brain-workers/src/scheduler.rs`:
- `tokio::sync::watch::channel(false)` → `Rc<Cell<bool>>` shutdown flag.
- `tokio::spawn(worker_loop(...))` → `glommio::spawn_local(worker_loop(...))` (returns `Task<()>` — `.detach()` to fire-and-forget, or keep as `Vec<Task<()>>` for the shutdown drain).
- `tokio::time::sleep` → `glommio::timer::sleep`.
- `tokio::time::timeout` → `futures_lite::or` race against a `glommio::timer::sleep`.
- `tokio::select! { sleep, watch.changed() }` → cooperative wait loop: `sleep(remaining).await; if shutdown.get() { break; }` (no select needed — single-threaded).
- Joining a worker: `Task<()>` doesn't have a `.join().await` — `.await` directly to await completion.

`crates/brain-workers/src/context.rs`:
- `pub shutdown: watch::Receiver<bool>` → `pub shutdown: Rc<Cell<bool>>`.
- `is_shutdown()` reads `self.shutdown.get()`.

Per-worker `tokio::task::yield_now().await` → `glommio::executor().yield_if_needed().await`:
- `crates/brain-workers/src/{worker,slot_reclaim,edge_scrub,idempotency_cleanup,wal_retention,decay}.rs` — 6 sites.

### 4.2 Tests

Every `#[tokio::test]` in `crates/brain-workers/tests/` constructs a `WorkerContext` with a watch channel. Each migrates to constructing `Rc<Cell<bool>>` and (if needed) running on a Glommio executor.

But — these are scheduler-level tests. We want them runnable under glommio. The straightforward path: add a `glommio_run` helper similar to brain-storage's, and re-wrap each test.

~12 test files. ~30-50 LOC each on average. ~500 LOC of test churn.

### 4.3 Risk

`brain-workers::scheduler` currently uses `tokio::time::timeout` for the shutdown drain budget. Glommio's `glommio::timer::sleep` doesn't return a "did the wrapped future complete?" boolean — we'd write a `select` between the sleep and the actual await. Manageable but watch for `Pin`/`Unpin` gotchas.

---

## 5. 9.7c — Per-shard OpsContext wired into Shard

### 5.1 brain-server changes

`crates/brain-server/src/shard.rs`:
- Shard struct gains: `ops: OpsContext`, `scheduler: WorkerScheduler`, `metadata_db: Rc<RefCell<MetadataDb>>`, `hnsw_index: ?` (later sub-task may move; tentatively here).
- Spawn flow inside the executor:
  1. `MetadataDb::open(&metadata_path)`.
  2. Build the redb-backed `MetadataSink` adapter (already exists in brain-metadata).
  3. Rerun recovery with the real sink. *Or* skip rerun if a checkpoint marker exists (TBD).
  4. Build `HnswIndex` (rebuilt from arena on first start; resumed from snapshot if present).
  5. Build `OpsContext`.
  6. Construct `WorkerScheduler`; register every Phase-8 worker against it.
  7. Enter main loop.

- New `ShardRequest` variants for the cognitive ops? Or stage that to 9.10's frame dispatcher? **Recommendation:** stage to 9.10. 9.7c's deliverable is "shard has OpsContext + workers running"; the cognitive ops dispatching against OpsContext lands when the frame layer is in.

### 5.2 The recovery rewire

Currently (9.6): `recover(&mut arena, wal_dir, shard_uuid, &mut InMemoryMetadataSink)` — throw-away sink.
Target (9.7c): `recover(&mut arena, wal_dir, shard_uuid, &mut MetadataDbSinkAdapter)` — durable sink.

The `MetadataDbSinkAdapter` exists in brain-metadata as `SinkAdapter` (or similar — confirm at impl time). It implements `brain_storage::recovery::MetadataSink` over a `MetadataDb`.

Recovery becomes the durable warm-start path: a respawn replays + applies to redb, ending with `metadata.next_lsn() == report.next_lsn`. The Wal opens at that LSN.

### 5.3 Worker registration

The Phase-8 schedulers all expect:
```rust
let mut scheduler = WorkerScheduler::new();
scheduler.register(Arc::new(DecayWorker::new(...)), Arc::new(ops))?;
scheduler.register(Arc::new(ConsolidationWorker::new(...)), Arc::new(ops))?;
// ... 12 workers ...
```

After 9.7a's Arc→Rc cascade these become `Rc<dyn Worker>` + `Rc<OpsContext>`. The scheduler holds them inside its task list. 9.7c just wires the 12 workers up; each worker's logic was already shipped in Phase 8.

### 5.4 Cargo

`crates/brain-server/Cargo.toml` Linux-only target gains:
```toml
brain-metadata = { path = "../brain-metadata" }
brain-index = { path = "../brain-index" }
brain-embed = { path = "../brain-embed" }
brain-planner = { path = "../brain-planner" }
brain-ops = { path = "../brain-ops" }
brain-workers = { path = "../brain-workers" }
```

Bringing all the brain-* deps in. macOS host still compiles brain-server because shard.rs is cfg-gated.

### 5.5 Tests

- `shard_workers_run_at_least_once` — spawn shard with a fast-tick worker config (10 ms decay interval); wait 100 ms; assert worker metrics show ≥ 1 cycle.
- `shard_recovery_uses_redb_sink` — write records via the WAL; clean shutdown; respawn; metadata.redb has the expected rows.

---

## 6. Sizing summary

| Sub-task | Source LOC | Test LOC | Commit message |
| -------- | ---------: | -------: | -------------- |
| 9.7a     | ~600       | ~800     | `refactor(brain-planner,brain-ops): WriterHandle !Send + OpsContext Rc cascade (sub-task 9.7a)` |
| 9.7b     | ~400       | ~700     | `refactor(brain-workers): scheduler tokio→glommio port (sub-task 9.7b)` |
| 9.7c     | ~500       | ~300     | `feat(brain-server): per-shard OpsContext + workers (sub-task 9.7c)` |

Total: ~3300 LOC across three commits.

---

## 7. Done criteria (rolled up across 9.7a-c)

- [ ] `WriterHandle` trait has no `Send + Sync` bounds; impls don't carry `+ Send` on futures.
- [ ] `OpsContext` is `!Send` (compile-time assertion).
- [ ] No `tokio::` in brain-workers' `src/` (audit grep gate from §11 of audit doc passes).
- [ ] Per-shard scheduler runs all 12 Phase-8 workers in a Glommio executor.
- [ ] Shard recovery applies to a real `MetadataDb`.
- [ ] All brain-storage, brain-planner, brain-ops, brain-workers, brain-server tests green in container.
- [ ] Audit doc §12 status rows for §4, §5.3, §6.x, §8.2, §8.5 flipped to **done**.
- [ ] Phase doc 9.7 marked `[x]` (or 9.7a / 9.7b / 9.7c marked individually).

---

## 8. What 9.7 explicitly defers

- **Real LLM Summarizer.** Lives in 9.15 (OpenAI/Ollama feature-gated adapter). Until then `DisabledSummarizer` (from Phase 8) stays the wired impl.
- **Real cognitive ops dispatched against OpsContext from the frame.** 9.10 handles frame→shard dispatch.
- **Cross-shard SUBSCRIBE fan-out.** 9.11 — splits the EventBus per audit §8.1.
- **ArcSwap + crossbeam-epoch publication of HNSW.** 9.12 — affects only the read path.
- **PLAN/REASON tombstone filter.** 9.16.

---

*Recommendation: write three sub-plans (9.7a / 9.7b / 9.7c) and land them sequentially. Confirm before drafting them.*
