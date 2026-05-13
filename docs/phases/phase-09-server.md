# Phase 9 — `brain-server`: End-to-End Wire-Up

## Goal

A runnable substrate. TCP connection layer (Tokio) accepts clients; per-shard Glommio executors handle requests; cross-shard routing works; graceful shutdown is clean. After this phase, you can `cargo run --bin brain-server`, point a client at it, and exercise every operation end-to-end.

## Prerequisites

- [x] Phase 8 complete.

## Reading list

1. [`spec/01_system_architecture/00_purpose.md`](../../spec/01_system_architecture/00_purpose.md)
2. [`spec/01_system_architecture/04_layers.md`](../../spec/01_system_architecture/04_layers.md)
3. [`spec/01_system_architecture/03_primitives.md`](../../spec/01_system_architecture/03_primitives.md)
4. [`spec/01_system_architecture/05_hardware.md`](../../spec/01_system_architecture/05_hardware.md)
5. [`spec/01_system_architecture/04_layers.md`](../../spec/01_system_architecture/04_layers.md)
6. [`spec/01_system_architecture/04_layers.md`](../../spec/01_system_architecture/04_layers.md)
7. [`spec/12_sharding_clustering/00_purpose.md`](../../spec/12_sharding_clustering/00_purpose.md)
8. [`spec/12_sharding_clustering/01_shard_model.md`](../../spec/12_sharding_clustering/01_shard_model.md)
9. [`spec/12_sharding_clustering/02_routing.md`](../../spec/12_sharding_clustering/02_routing.md)
10. [`spec/10_concurrency_epochs/00_purpose.md`](../../spec/10_concurrency_epochs/00_purpose.md)
11. [`spec/10_concurrency_epochs/02_single_writer.md`](../../spec/10_concurrency_epochs/02_single_writer.md)
12. [`spec/10_concurrency_epochs/05_arc_swap.md`](../../spec/10_concurrency_epochs/05_arc_swap.md)
13. [`spec/10_concurrency_epochs/06_crossbeam_epoch.md`](../../spec/10_concurrency_epochs/06_crossbeam_epoch.md)

## Outputs

- `crates/brain-server` is a fully working binary.
- Config TOML loaded; multi-shard topology set up.
- Tokio connection layer + Glommio shard executors.
- Cross-shard routing via BLAKE3(agent_id) → shard.
- Graceful shutdown.
- Tag: `phase-9-complete`.

## Sub-tasks

### Task 9.1 — Config loading  [x]
**Reads:** `config/dev.toml`
**Writes:** `crates/brain-server/src/config.rs`
**Done when:** Config struct deserializes from TOML; env var overrides supported; missing required fields produce clear errors.

### Task 9.2 — Tokio/Glommio port audit  [x]
**Reads:** every shard-bound crate's `src/`; spec §01/04, §10/02.
**Writes:** `docs/phases/phase-09-glommio-port.md`
**Done when:** Every `tokio::*` use-site in shard-bound code has a
disposition (STAY-CONN / STAY-TEST / PORT-GLOMMIO / PORT-LOCAL / MOVE /
DELETE / QUESTION). Cross-cutting decisions locked. Open questions
surfaced.

> The original phase doc listed "Shard executor (Glommio)" as 9.2; the
> Phase 9 orientation (`.claude/plans/phase-09.md`) renumbered. The
> shard executor scaffold is now **9.4**. See the orientation for the
> updated 18-sub-task projection.

### Task 9.4 — Shard scaffold (Glommio LocalExecutor + channel boundary)  [x]
**Reads:** `spec/01_system_architecture/05_hardware.md`, audit §7/§8.2
**Writes:** `crates/brain-server/src/shard.rs`, `crates/brain-server/tests/shard.rs`
**Done when:** A Glommio `LocalExecutor` per shard, on its own OS thread,
drains a flume request channel, replies to stub `Ping` requests, and is
joinable on shutdown. `ShardHandle: Send + Sync`. Linux-gated;
macOS still compiles brain-server (shard module cfg-gated).

> **Scaffold only.** Real arena/WAL/metadata/HNSW/workers land in 9.5–9.7.

### Task 9.8 — Wire Phase-8 seams to real impls  [x]
**Reads:** plan `phase-09-task-08.md`, audit §4 + §8.5,
  spec §05/09 §3 (checkpoint sequence), §05/10 (retention), §11/04 §7
  (rebuild source), §11/07 / §11/08 §6 (worker seams).
**Writes:** `crates/brain-workers/src/{hnsw_maint,wal_retention,snapshot,cache_evict}.rs`
  (drop `Send + Sync` from the 4 source traits, drop `+ Send` from
  future-return aliases); `crates/brain-workers/src/lib.rs` (re-export
  the source future types so adapters can name them);
  `crates/brain-server/src/shard_adapters.rs` (new — 3 real adapters +
  5 unit tests); `crates/brain-server/src/shard.rs` (Shard.arena +
  Shard.wal switched to `Rc<RefCell<…>>`; `register_phase8_workers`
  takes 4 `Arc<dyn …>` parameters; spawn flow builds adapters inside
  the Glommio closure); `crates/brain-server/tests/shard.rs` (mirror
  the shard_adapters module path).
**Done when:** `RebuildSource`/`WalRetentionSource`/`SnapshotSource`/
  `CacheEvictionSource` traits drop `Send + Sync`; real adapters
  `ArenaRebuildSource`, `WalDirRetentionSource`, `ShardSnapshotSource`
  ship; `CacheEvictionSource` stays `DisabledCacheEvictionSource`
  (waiting on 9.10's `CachingDispatcher`); `Shard.arena` +
  `Shard.wal` switch to `Rc<RefCell<…>>` so adapters can share state
  with the main loop; 31 brain-server tests + full workspace green
  (`just docker-verify`).

### Task 9.7b — Per-shard OpsContext + workers wired in  [x]
**Reads:** plan `phase-09-task-07b.md`, audit §5.3 + §6 + §8.2.
**Writes:** `crates/brain-server/Cargo.toml` (6 new Linux deps + parking_lot),
  `crates/brain-server/src/shard.rs` (full per-shard stack inside the
  Glommio closure + 12-worker registration + shutdown drain),
  `crates/brain-server/tests/shard.rs` (2 smoke tests).
**Done when:** `spawn_shard` constructs `MetadataDb` → `recover()` (real
sink) → `SharedHnsw` → `NopDispatcher` → `RealWriterHandle` →
`ExecutorContext` → `OpsContext` → `Wal` → `WorkerScheduler` registering
all 12 Phase-8 workers. Shutdown drains scheduler → WAL → arena msync.

### Task 9.7a — WriterHandle Send drop + WorkerScheduler Glommio port  [x]
> Sub-task 9.7 originally planned a 9.7a/b/c split. The dependency cascade
> made the split non-viable (audit §4 + §6 are one change). This commit
> drops Send + Sync from `brain_planner::WriterHandle`, ports
> `brain_workers::WorkerScheduler` from tokio to Glommio, and updates
> every cascading site. `brain-server`'s per-shard OpsContext wire-up
> moves to a follow-up sub-task (was the original 9.7c).

**Reads:** audit `docs/phases/phase-09-glommio-port.md` §4 + §6 + §8.2 + §8.5.
**Writes:** `crates/brain-planner/src/executor/{writer,context}.rs`;
  `crates/brain-ops/src/{writer,lib,context,subscribe,access_buffer,txn}.rs`;
  `crates/brain-workers/src/{worker,context,scheduler,*}.rs` + every test
  file migrated to a `glommio_run` harness; `+ Send` stripped from test
  fixtures' WriterHandle impls.
**Done when:** `WriterHandle` is `!Send + !Sync`; `WorkerScheduler` runs
on Glommio (no `tokio::spawn`); `WorkerContext.shutdown` is
`Arc<AtomicBool>` not `tokio::sync::watch`; 992 tests green in container.

### Task 9.6 — Real WAL hookup  [x]
**Reads:** `spec/05_storage_arena_wal/06_wal_durability.md` §1, §11; `spec/05_storage_arena_wal/08_recovery.md` §§1–7; `spec/12_sharding_clustering/01_shard_model.md` §1–§5.
**Writes:** `crates/brain-storage/src/wal/{segment,wal}.rs` (new `open_for_append` + `open_existing`); `crates/brain-server/src/shard.rs` (Wal field, recovery on spawn, `AppendWalRecord` handler); `crates/brain-server/tests/shard.rs` (4 new integration tests).
**Done when:** Each shard owns a real `Wal` on disk under `<data_dir>/<shard_id>/wal/`; recovers on respawn via `brain_storage::recovery::recover` (with `InMemoryMetadataSink` stand-in — 9.7 swaps in `MetadataDb`); `AppendWalRecord` exercises `Wal::append` end-to-end.

### Task 9.6a — WAL io_uring port  [x]
**Reads:** `spec/05_storage_arena_wal/06_wal_durability.md`, `docs/spec-deviations.md` SD-2.8-2/SD-2.9-1.
**Writes:** `crates/brain-storage/src/wal/{segment,group_commit,wal,checkpoint,reader,recovery}.rs`, `crates/brain-storage/tests/random_kill.rs`, `crates/brain-metadata/tests/recovery_integration.rs`.
**Done when:** WAL writes go through Glommio io_uring (`BufferedFile::write_at` + `fdatasync`); committer is a `spawn_local` coroutine on the shard executor; `Wal::append` is `async fn(&self)`. SD-2.8-2 + SD-2.9-1 reconciled; new SD-2.8-2-b documents the two-syscall fsync.

### Task 9.5 — Real arena hookup  [x]
**Reads:** `spec/05_storage_arena_wal/02_arena_layout.md`, `spec/12_sharding_clustering/01_shard_model.md` §1–§5.
**Writes:** `crates/brain-server/src/shard.rs`, `crates/brain-server/tests/shard.rs`.
**Done when:** Each shard owns a real `ArenaFile` + `SlotAllocator` on disk
under `<data_dir>/<shard_id>/`; persists UUID across restarts; stub
`AllocSlot` op returns sequential `(idx, version)` pairs from the executor.

### Task 9.9 — Connection layer (Tokio + optional rustls)  [x]
> Was originally numbered 9.3 in this phase doc; the orientation
> (`.claude/plans/phase-09.md` §11) renumbered after routing landed
> early as 9.3.

**Reads:** plan `phase-09-task-09.md`, `spec/01_system_architecture/04_layers.md` (L1),
  `spec/03_wire_protocol/02_transport.md` (TCP + TLS),
  `spec/03_wire_protocol/03_frame_header.md` (frame layout),
  audit `docs/phases/phase-09-glommio-port.md` §7 (Tokio side locked).
**Writes:** `Cargo.toml` (workspace deps: tokio-rustls, rustls, rustls-pemfile, rcgen, socket2);
  `crates/brain-server/Cargo.toml` (Linux deps: tokio + rustls stack + socket2 + rcgen dev-dep);
  `crates/brain-server/src/connection.rs` (new — `ConnectionListener` two-step
  `new`/`bind`/`serve`, `ShutdownSignal` over `tokio::sync::watch`, per-connection
  task with frame I/O helpers and TCP option setup);
  `crates/brain-server/src/tls.rs` (new — `load_server_tls_config` w/ aws-lc-rs
  provider install, TLS 1.3 only, ALPN `brain/1`);
  `crates/brain-server/src/main.rs` (Linux async main built around
  `tokio::runtime::Builder::new_multi_thread`, non-Linux stays sync stub);
  `crates/brain-server/tests/connection.rs` (new — 6 integration tests).
**Done when:** `ConnectionListener::new(addr, tls, shards, limits, signal).bind()?.serve().await`
  binds + accepts on Linux; SO_REUSEADDR / TCP_NODELAY / SO_KEEPALIVE applied
  per spec §03/02 §1.2; optional rustls TLS 1.3 with `brain/1` ALPN; per-frame
  read timeout enforced; well-formed frames receive `ERROR(BadFrame)` then
  close (9.10 plugs in the real dispatcher); ctrl-c → ShutdownTrigger →
  serve() exits cleanly; 6 connection tests + workspace verify green.

### Task 9.10 — Frame dispatcher (Tokio↔Glommio boundary)  [x]
> Was originally numbered 9.4 in this phase doc; orientation §11
> renumbered after routing landed early as 9.3.

**Reads:** plan `phase-09-task-10.md`, `spec/01_system_architecture/04_layers.md`,
  `spec/03_wire_protocol/05_opcodes.md`, `spec/03_wire_protocol/06_handshake.md`,
  `spec/03_wire_protocol/07_request_frames.md` + `08_response_frames.md`,
  `spec/03_wire_protocol/09_streaming.md` §1–§5,
  `spec/12_sharding_clustering/02_routing.md`,
  audit `docs/phases/phase-09-glommio-port.md` §7.
**Writes:** `crates/brain-server/src/dispatch.rs` (new — `ConnPhase` state
  machine, handshake / PING / BYE handlers, `Topology`, op routing,
  `IdleTimer` + `Tick`, OpError→wire mapping);
  `crates/brain-server/src/connection.rs` (rewritten `serve_connection`
  with reader/writer split via `tokio::io::split`, per-conn flume queue,
  `tokio::spawn`-ed op sub-tasks; handshake deadline + idle timer arms);
  `crates/brain-server/src/shard.rs` (`ShardRequest::DispatchOp`,
  `ShardHandle::dispatch_op`, `DispatchError`);
  `crates/brain-server/src/main.rs` (Linux path now spawns N shards,
  builds `RoutingTable` + `ServerCapabilities` + `Topology`, joins
  shards on shutdown);
  `crates/brain-server/tests/dispatch.rs` (new — 10 integration tests).
**Done when:** TCP/TLS clients can complete HELLO/WELCOME/AUTH/AUTH_OK,
  send ENCODE / FORGET / RECALL / PLAN / REASON / LINK / UNLINK /
  TXN_* opcodes and receive a wire-shaped response (single-frame EOS
  in 9.10; multi-frame streaming is 9.11); PING→PONG, BYE→BYE, and
  idle SERVER_PING all work; per-frame and handshake timeouts fire;
  ops routed via `MemoryId.shard()` (where applicable) or
  `routing.shard_for_agent(agent_id)`; 39 brain-server tests pass
  (+10 dispatch integration tests on top of 9.9's 35).

### Task 9.11 — Cross-shard SUBSCRIBE fan-out  [x]
**Reads:** plan `phase-09-task-11.md`, `spec/09_cognitive_operations/09_subscribe.md`,
  `spec/03_wire_protocol/05_opcodes.md` §1.3, `spec/03_wire_protocol/09_streaming.md`,
  audit `docs/phases/phase-09-glommio-port.md` §8.1.
**Writes:** `crates/brain-server/src/subscribe.rs` (new — `ShardEventHub`,
  `SubscriptionRegistry`, per-sub task);
  `crates/brain-server/src/shard.rs` (`ShardHandle::events()` flume
  Receiver; Glommio closure spawns a `fanout_task` draining
  `OpsContext::events`);
  `crates/brain-server/src/dispatch.rs` (`Action::Subscribe` /
  `Action::CancelSubscribe` variants, dispatch_frame routes SUBSCRIBE
  / UNSUBSCRIBE / CANCEL_STREAM through registry);
  `crates/brain-server/src/connection.rs` (`ShardEventHub` field on
  `ConnectionListener`; per-conn `SubscriptionRegistry` in
  `serve_connection`; new helpers);
  `crates/brain-ops/src/{lib,subscribe}.rs` (`parse_filter` made
  public);
  `crates/brain-server/tests/subscribe.rs` (new — 5 integration tests).
**Done when:** Clients can SUBSCRIBE post-AUTH and receive
  SUBSCRIBE_EVENT frames on the chosen stream; UNSUBSCRIBE +
  CANCEL_STREAM both emit acks on their own stream + a final EOS on
  the subscription stream; `from_lsn` rejected as `LsnTooOld`;
  duplicate stream_id rejected; 5 subscribe integration tests pass on
  top of 9.9/9.10's existing wire tests. Audit §8.1 status row →
  **done**.

### Task 9.5 — Cross-shard routing  [x]
> Landed early as **sub-task 9.3** per the orientation's renumbering.
> See `crates/brain-server/src/routing.rs`.

**Reads:** `spec/12_sharding_clustering/02_routing.md`
**Writes:** `crates/brain-server/src/routing.rs`
**What to build:**
- `agent_id_to_shard(agent_id, num_shards) -> ShardId` via BLAKE3.
- `MemoryId.shard()` shortcuts routing for ops that already have a memory ID.

### Task 9.12 — ArcSwap shared state + crossbeam-epoch reclamation  [x]
> Was numbered 9.6 + 9.7 in this phase doc originally; orientation §11
> renumbered to 9.12 as the consolidated sub-task.

**Reads:** plan `phase-09-task-12.md`,
  `spec/10_concurrency_epochs/05_arc_swap.md`,
  `spec/10_concurrency_epochs/06_crossbeam_epoch.md`,
  `docs/spec-deviations.md` SD-4.8-1 (HNSW RwLock fallback, locked).
**Writes:** `crates/brain-server/Cargo.toml` (add `arc-swap`);
  `crates/brain-server/src/dispatch.rs` (`Topology.routing` becomes
  `Arc<ArcSwap<RoutingTable>>`, `dispatch_frame` uses `load_full()`);
  `crates/brain-server/src/main.rs` (construct via
  `ArcSwap::from_pointee`); `crates/brain-server/src/routing.rs` (new
  unit test); test scaffolds in `tests/{connection,dispatch,subscribe}.rs`;
  `docs/spec-deviations.md` (new **SD-10.6-1** documenting why
  first-party code intentionally doesn't use `crossbeam-epoch` under
  single-writer-per-shard).
**Done when:** `Topology.routing` is an `Arc<ArcSwap<RoutingTable>>`;
  reads use `load_full()`; a follow-up `store()` is visible to a
  fresh `shard_for_agent` call (unit test);
  `crossbeam-epoch` non-use is documented as SD-10.6-1; ArcSwap
  use for HNSW remains deferred via SD-4.8-1.

### Task 9.13 — Health + metrics endpoints  [x]
> Was numbered 9.8 in the phase doc originally; orientation §11
> renumbered to 9.13.

**Reads:** plan `phase-09-task-13.md`,
  `spec/14_observability_ops/01_metrics.md`.
**Writes:** `crates/brain-server/src/admin.rs` (new — `AdminServer`,
  `AdminState`, `BuildInfo`, hand-rolled minimal HTTP/1.1,
  `/healthz` + `/metrics` handlers, Prometheus exposition builder);
  `crates/brain-server/src/connection.rs` (`ConnectionMetrics` +
  RAII `ConnectionGuard`; `ConnectionListener::new` gains a
  `metrics: Arc<ConnectionMetrics>` parameter; accept loop
  increments `total` + `active`);
  `crates/brain-server/src/shard.rs`
  (`ShardRequest::SchedulerSnapshot`,
  `ShardHandle::scheduler_snapshot`);
  `crates/brain-workers/src/scheduler.rs`
  (`WorkerScheduler::metrics_snapshot`);
  `crates/brain-server/src/main.rs` (spawn admin + connection
  servers under the same `ShutdownSignal`);
  `crates/brain-server/tests/admin.rs` (new — 5 integration tests).
**Done when:** the server binds a separate HTTP listener on
  `cfg.server.metrics_addr`; `GET /healthz` returns `200 OK\nok`;
  `GET /metrics` returns Prometheus exposition for the metrics
  already counted first-party (build_info, up, shards_total,
  connections active/total, process_uptime, worker counters);
  hand-rolled HTTP parser rejects non-GET and unknown paths with
  400; 5 integration tests pass.

### Task 9.14 — Graceful shutdown  [x]
> Was numbered 9.9 in the phase doc originally; orientation §11
> renumbered to 9.14.

**Reads:** plan `phase-09-task-14.md`,
  `spec/01_system_architecture/04_layers.md`,
  audit `docs/phases/phase-09-glommio-port.md` §8.2.
**Writes:** `crates/brain-server/src/shutdown.rs` (new —
  `graceful_shutdown_shards` with per-shard timed `join()`);
  `crates/brain-server/src/main.rs` (`spawn_signal_listener`
  handles SIGINT *and* SIGTERM via `tokio::signal::unix`; falls
  back to SIGINT-only if SIGTERM install fails;
  `linux_main::run` keeps a `shards_for_drain` clone, spawns the
  listener as a `JoinHandle`, awaits listener + admin under a
  2 s drain bound, then calls
  `graceful_shutdown_shards(..., DEFAULT_SHARD_DRAIN_BUDGET=30s)`);
  `crates/brain-server/src/shard.rs` (`ShardJoiner::shard_id()`
  accessor); `crates/brain-server/tests/shutdown.rs` (new — 3
  integration tests).
**Done when:** SIGINT *and* SIGTERM fire the same `ShutdownSignal`;
  exit ordering is deterministic (accept loops exit → tasks drain
  → shards close → joiners run with a per-shard timeout);
  `ExitCode::FAILURE` surfaces if any drain phase times out
  (observability-friendly); 3 integration tests pass + all prior
  wire tests still pass.

### Task 9.15 — OpenAI / Ollama Summarizer adapter (feature-gated)  [x]
**Reads:** plan `phase-09-task-15.md`,
  `spec/11_background_workers/03_consolidation.md` §6, §7,
  `spec/11_background_workers/09_failure_modes.md` §6.
**Writes:** `crates/brain-server/src/llm/{mod,prompt,bridge,openai,ollama,factory}.rs`
  (new module tree); `crates/brain-server/src/config.rs`
  (`SummarizerConfig` + `SummarizerBackend`); `config/dev.toml`
  (new `[summarizer] backend = "disabled"` section);
  `crates/brain-server/Cargo.toml` (`summarizer-openai` +
  `summarizer-ollama` Cargo features; optional `reqwest` +
  `serde_json` deps); workspace `Cargo.toml` (`reqwest` 0.12 with
  rustls-tls); `crates/brain-server/src/shard.rs`
  (`ShardSpawnConfig.summarizer`; `register_phase8_workers` takes
  `Arc<dyn Summarizer>`); `crates/brain-server/src/main.rs`
  (`build_summarizer(&cfg)` once at startup, cloned into each shard);
  `crates/brain-server/tests/summarizer.rs` (new — 6 mock-server
  integration tests).
**Done when:** `cfg.summarizer.backend = "openai"` /  `"ollama"`
  constructs a real `Arc<dyn Summarizer>` backed by a shared
  bridge Tokio runtime (avoids the Glommio↔Tokio reactor collision);
  default `disabled` keeps `DisabledSummarizer`; feature-gated tests
  exercise round-trip + 4xx error mapping against a hand-rolled
  mock HTTP server.

### Task 9.16 — PLAN / REASON tombstone filter  [x]
**Reads:** plan `phase-09-task-16.md`,
  `spec/16_benchmarks_acceptance/01_correctness_criteria.md` §12
  (Tombstone correctness MUST).
**Writes:** `crates/brain-planner/src/executor/{path,reason}.rs`
  (per-neighbour `ctx.index.is_tombstoned` retain + tombstoned
  start/goal/seed → empty-endpoint short-circuit);
  `crates/brain-planner/tests/{path_executor,reason_executor}.rs`
  (5 new tests covering chain exclusion + entry short-circuits).
**Done when:** spec §16/01 §12 acceptance test passes: `FORGET m;
  PLAN through chain incl. m` returns paths that avoid `m`.
  Tombstoned start / goal / seed yield empty endpoint sets; the
  BFS short-circuits to `NoPathFound` / empty supporting+
  contradicting (matches `search_active`'s silent-filter behavior
  for ByText endpoints).

### Task 9.17 — End-to-end smoke test  [x]
> Was numbered 9.10 in this phase doc originally; orientation §11
> renumbered. The "uses `brain-sdk-rust`" original framing was
> rescoped — see plan §1 — because brain-sdk-rust is a placeholder
> (Phase 13) and the shard scaffold uses `NopDispatcher`
> (zero-vector embeddings, no content-correctness assertions
> possible).

**Reads:** plan `phase-09-task-17.md`.
**Writes:** `crates/brain-server/tests/e2e.rs` (new — 4 wire-smoke
  tests using the in-process scaffold pattern from
  `tests/dispatch.rs` / `tests/admin.rs`).
**Done when:** the test binary spins up shards + listener + admin
  server on `127.0.0.1:0`, drives an in-process client through:
  - `encode_recall_forget_recall_round_trip` — full lifecycle wire
    shapes (HELLO → AUTH → ENCODE → RECALL → FORGET → RECALL → BYE).
  - `repeated_encode_recall_is_stable` — 100 round-trips don't
    leak, deadlock, or panic.
  - `metrics_endpoint_reflects_traffic` — `/metrics` shows
    `brain_connections_total >= 2` after two clients connect.
  - `bye_and_shutdown_drain_cleanly` — BYE + `server.stop()` →
    `graceful_shutdown_shards` within the default 30 s budget.
  All 4 pass; full workspace verify still green.

## Phase exit checklist

- [ ] All sub-tasks complete.
- [ ] `just verify` green.
- [ ] `cargo run --bin brain-server` accepts a connection from a sample client.
- [ ] E2E smoke test passes 100 iterations.
- [ ] `just run-server` boots in < 5 seconds with empty data.
- [ ] Tag `phase-9-complete`.
