# Sub-task 9.14 — Graceful shutdown

**Reads:**
- `spec/01_system_architecture/04_layers.md` (layer interaction; not
  shutdown-specific but relevant for ordering).
- `docs/phases/phase-09-glommio-port.md` §8.2 (per-shard
  `Rc<Cell<bool>>` shutdown flag — locked in 9.7).
- Existing wire-up: `spawn_signal_listener` (ctrl-c only, 9.9 stub);
  `ShutdownSignal` watch-channel (9.9); `shard_main_loop` shutdown
  drain order (9.7b: scheduler → WAL → arena msync); `ShardJoiner`
  (9.4 — one-shot `join()` returning when the executor thread
  exits).

**Phase doc:** orientation §11 sub-task **9.14**.

**Done when:** SIGINT *and* SIGTERM fire the same `ShutdownSignal`;
the binary tears down in deterministic order (stop accepting →
drain inflight → close shards → join executors → exit); a bounded
**drain timer** caps the wait so a stuck task doesn't block exit
indefinitely; the exit code reflects whether drain completed
cleanly.

---

## 1. What's already in place

| Layer | Current shutdown behavior |
| ----- | -------------------------- |
| `ConnectionListener` accept loop | Observes `ShutdownSignal::recv()` (9.9); `serve` returns when fired. |
| `AdminServer` accept loop | Same — observes the same `ShutdownSignal` clone (9.13). |
| Per-connection receiver loop | `select! { shutdown | read }` — fires shutdown arm and exits cleanly (9.10). |
| Per-connection writer loop | Drains the per-conn frame queue, exits when `frame_tx` drops. |
| Per-op sub-tasks | Observe channel close via `send_async` Err. |
| Subscription per-sub tasks (9.11) | Observe `cancel` watch + `broadcast::Receiver::Closed`; emit final EOS. |
| Event-bridge tasks (9.11) | Observe `flume::Receiver` Err when the shard's events Sender drops. |
| Shard main loop | `recv_async` returns Err when every `ShardHandle::tx` Sender drops; then drains scheduler → WAL → arena msync (9.7b). |
| `ShardJoiner::join()` | Blocks until the Glommio executor's OS thread exits. |
| Signal handler | `tokio::signal::ctrl_c().await` only (9.9 stub). |

Most of the machinery is already in place. 9.14 wires the missing
pieces:

1. **SIGTERM** alongside SIGINT.
2. **Drain timer** with a bounded budget (default 30s).
3. **Structured exit ordering + telemetry** in `linux_main::run`.
4. **Per-`ShardJoiner` timeout** so a stuck shard doesn't block the
   whole exit; the binary logs and force-exits if the budget runs out.

---

## 2. Signal handling

Replace the stub:

```rust
fn spawn_signal_listener(trigger: ShutdownTrigger) {
    tokio::spawn(async move {
        let mut sigterm = match tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate()
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "SIGTERM handler install failed; SIGINT-only");
                if let Err(e) = tokio::signal::ctrl_c().await {
                    tracing::error!(error = %e, "ctrl_c handler failed");
                }
                tracing::info!("SIGINT received; signalling shutdown");
                trigger.signal();
                return;
            }
        };
        tokio::select! {
            r = tokio::signal::ctrl_c() => {
                if let Err(e) = r {
                    tracing::error!(error = %e, "ctrl_c handler failed");
                }
                tracing::info!("SIGINT received; signalling shutdown");
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received; signalling shutdown");
            }
        }
        trigger.signal();
    });
}
```

`tokio::signal::unix` is Linux-only (already gated). Falls back to
SIGINT-only if `unix::signal` install fails (e.g. inside a
restricted container).

---

## 3. Structured exit in `linux_main::run`

Replace the current end-of-`block_on` flow:

```rust
let serve_rc = match bound.serve().await { ... };
let _ = admin_handle.await;
serve_rc
```

with:

```rust
async fn graceful_shutdown(
    serve_handle: JoinHandle<io::Result<SocketAddr>>,
    admin_handle: JoinHandle<io::Result<SocketAddr>>,
    drain_budget: Duration,
) -> ExitCode {
    // Phase 1 — wait for the connection listener (which already
    // observed the shutdown signal) and admin server to exit. They
    // both exit promptly; bound by ~50 ms.
    let listener_drain = Duration::from_secs(2);
    let _ = tokio::time::timeout(listener_drain, async {
        let _ = serve_handle.await;
        let _ = admin_handle.await;
    }).await;
    ExitCode::SUCCESS
}
```

Actual structure:

```rust
let serve_rc = runtime.block_on(async move {
    let (trigger, signal) = ShutdownSignal::channel();
    spawn_signal_listener(trigger);

    // ... bind tls, admin, listener as today ...

    let listener_handle = tokio::spawn(async move { bound.serve().await });
    let listener_result = listener_handle.await;
    // serve() returned (either Ok via shutdown signal, or Err via bind error).

    // Phase A — wait for the admin server (already received the
    // same shutdown signal). Bound at 2s.
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        admin_handle,
    ).await;

    match listener_result {
        Ok(Ok(addr)) => {
            tracing::info!(addr = %addr, "connection listener drained");
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "connection listener failed");
            ExitCode::FAILURE
        }
        Err(e) => {
            tracing::error!(error = %e, "connection listener panicked");
            ExitCode::FAILURE
        }
    }
});

// Phase B — close shard channels by dropping the handles, then
// join their executor threads with a per-shard timeout. Out of
// the async runtime so we can `spawn_blocking` cleanly.
let join_rc = shutdown_shards(shards_for_drop, joiners, drain_budget);

if serve_rc == ExitCode::SUCCESS { join_rc } else { serve_rc }
```

The `shards_for_drop` variable is the `Arc<Vec<ShardHandle>>` —
we need to drop it after `runtime.block_on` returns so the
per-shard channels close, but the topology's `shards` is the same
`Arc`. Capture an extra clone outside the closure.

### 3.1 `shutdown_shards`

```rust
fn shutdown_shards(
    shards: Arc<Vec<ShardHandle>>,
    joiners: Vec<ShardJoiner>,
    drain_budget: Duration,
) -> ExitCode {
    // Drop every ShardHandle clone. The `Arc<Vec<ShardHandle>>`
    // held by Topology/AdminState/event_hub may still have
    // outstanding refs; if so, dropping our handle isn't sufficient.
    // The connection / admin tasks already exited (Phase A), so
    // their copies of the Arc are dropped too.
    drop(shards);

    let deadline = Instant::now() + drain_budget;
    let mut rc = ExitCode::SUCCESS;
    for joiner in joiners {
        let shard_id = joiner.shard_id();
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            tracing::error!(
                shard_id,
                "shard drain budget exhausted; thread leaked",
            );
            rc = ExitCode::FAILURE;
            std::mem::forget(joiner); // don't double-warn from Drop
            continue;
        }
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), ShardError>>();
        std::thread::spawn(move || { let _ = tx.send(joiner.join()); });
        match rx.recv_timeout(remaining) {
            Ok(Ok(())) => {
                tracing::info!(shard_id, "shard joined cleanly");
            }
            Ok(Err(e)) => {
                tracing::error!(shard_id, error = %e, "shard join failed");
                rc = ExitCode::FAILURE;
            }
            Err(_) => {
                tracing::error!(shard_id, "shard join timed out");
                rc = ExitCode::FAILURE;
            }
        }
    }
    rc
}
```

`ShardJoiner` gains `pub fn shard_id(&self) -> ShardId` (currently
private field). Mechanical addition.

The `std::mem::forget(joiner)` on timeout skips ShardJoiner's
`Drop` impl, which would otherwise emit a "join never called"
warning. We've already logged the timeout; the forget keeps the
log clean.

### 3.2 Drain budget

`ConnectionLimits` and `WorkerScheduler` both already cap their
own drains:

- WAL group commit: bounded by `wal_config.group_commit.commit_window`
  (default 100 µs).
- Scheduler shutdown: `SHUTDOWN_DRAIN_BUDGET = 5s` (sub-task 9.7's
  audit decision).
- Per-connection reader loop: exits immediately when shutdown fires;
  the in-flight op sub-tasks aren't waited on (they exit when their
  send_async returns Err).

So 9.14's drain budget caps **shard join only**. Default 30s:

```rust
const SHUTDOWN_DRAIN_BUDGET: Duration = Duration::from_secs(30);
```

This is much larger than the worker scheduler's 5s + WAL flush's
~100µs, so a clean shard exit fits comfortably. The budget kicks
in only when something is genuinely stuck (a worker hangs, mmap
syscall stalls, etc.) — and surfaces as a non-zero exit code +
ERROR log line.

---

## 4. Tests

The connection layer's existing `shutdown_signal_stops_accept_loop`
test (9.9) already covers the signal path. 9.14 adds:

1. **`signal_listener_responds_to_sigint`** — spawn the signal
   listener, deliver SIGINT via `nix::libc::raise(libc::SIGINT)`,
   observe `ShutdownSignal::is_signalled() == true` within 1s.
2. **`signal_listener_responds_to_sigterm`** — same shape with
   SIGTERM.
3. **`shutdown_shards_returns_within_budget`** — spawn N real
   shards, drop the handles, call `shutdown_shards(…, 5s)`,
   observe exit within 5s + `rc == SUCCESS`. (Real shards take
   ~100 ms to drain; the 5s budget is comfortable.)
4. **`shutdown_shards_times_out_on_blocked_join`** — spawn one
   shard, *don't* drop the handle (simulating a stuck channel),
   call `shutdown_shards(…, 200ms)`, observe exit within ~250 ms +
   `rc == FAILURE`. (`Drop` warning suppressed via
   `std::mem::forget`.)

Tests 1 + 2 mutate process state (signal delivery), so they need
to be `#[serial]` or run in their own test binary. Pulling
`serial_test` for ~10 LOC of dep is heavyweight; the cleanest
compromise is to put them in a dedicated `tests/shutdown.rs` file
and rely on `cargo test` running test binaries serially (which it
does by default). The same test binary can still parallelise tests
*within* it; we use `--test-threads=1` for this one file via a
`#[cfg(not(test_threads_concurrent))]` guard or just by gating
behind a feature flag. Simplest: skip the signal tests if `CARGO_TEST_THREADS`
isn't 1 — annoyingly fragile. **Decision:** drop tests 1 + 2 from
the integration suite; cover them as unit tests inside
`spawn_signal_listener`'s module with `#[ignore]` so a developer
runs them manually if needed. Tests 3 + 4 (the meaningful drain
behavior) stay.

---

## 5. Module layout

| File | Action | Approx LOC |
| ---- | ------ | ---------- |
| `crates/brain-server/src/shutdown.rs` | new — `graceful_shutdown_shards`, the drain logic; `pub` so tests can call directly | ~150 |
| `crates/brain-server/src/main.rs` | extend — replace `spawn_signal_listener` body with SIGINT+SIGTERM select; restructure end-of-`block_on` to await listener + admin + call `graceful_shutdown_shards` | ~80 delta |
| `crates/brain-server/src/shard.rs` | add `ShardJoiner::shard_id()` accessor | ~5 delta |
| `crates/brain-server/tests/shutdown.rs` | new — 2 integration tests (drain success + drain timeout) | ~150 |

Total: ~385 LOC. Small.

---

## 6. Risks

| Risk | Mitigation |
| ---- | ---------- |
| SIGTERM tokio handler isn't supported on every kernel | We try `tokio::signal::unix::signal(SignalKind::terminate())` and fall back to SIGINT-only if it fails. Logged. |
| ShardJoiner.join() blocks forever on a stuck thread | Per-joiner timeout via `std::sync::mpsc::recv_timeout`. Exit code reflects the failure. |
| `Arc<Vec<ShardHandle>>` cloned into many places (Topology, AdminState, event_hub's bridge tasks) — we can't drop them all in 9.14 | Most clones are inside the *runtime task* — they drop automatically when the task ends. The remaining clone is the `shards_for_drop` we explicitly hold; we drop it in `shutdown_shards`. If a clone leaks (e.g. a panicked bridge task), the shard's request channel stays open and `recv_async` blocks. Mitigated by the per-shard join timeout. |
| Drain timeout fires on a slow but not-stuck shard | Default budget is 30s; spec doesn't prescribe a precise number. Tunable via `cfg.shutdown.drain_budget_secs` in v2; we hard-code 30s in v1. |
| `std::mem::forget(joiner)` on timeout leaks the OS thread | Yes, intentionally — we've already logged the timeout; the alternative (dropping ShardJoiner) emits a second WARN line. v2 could `pthread_kill` the thread; not worth the risk in v1. |
| Test 4 spawns a shard and intentionally doesn't drop the handle — that handle is bound to the test scope's `Vec<ShardHandle>` | The Vec gets dropped at end-of-scope normally. To genuinely *prevent* shard exit, the test calls `std::mem::forget` on the handle Vec before invoking `shutdown_shards`. The 200 ms budget then exhausts and the FAILURE path fires. |

---

## 7. Done criteria

- [ ] `spawn_signal_listener` handles SIGINT *and* SIGTERM.
- [ ] `crates/brain-server/src/shutdown.rs` ships `graceful_shutdown_shards`.
- [ ] `ShardJoiner::shard_id()` accessor added.
- [ ] `linux_main::run` awaits the listener + admin server (bounded
  to 2s), then calls `graceful_shutdown_shards` for shard drain
  (default 30s).
- [ ] Exit code reflects whether all phases completed cleanly.
- [ ] 2 integration tests in `tests/shutdown.rs` pass.
- [ ] All prior wire tests still pass.
- [ ] `just docker-verify` green workspace-wide.
- [ ] Phase doc 9.14 marked `[x]`.

---

## 8. What 9.14 explicitly defers

- **In-flight request drain accounting** — today's per-conn task
  exits when shutdown fires; the in-flight op sub-tasks aren't
  waited on individually (their `send_async` returns Err and they
  exit). For a "wait for in-flight to finish gracefully before
  closing the connection" pass, we'd need to track active op tasks
  per connection. Out of v1 scope; spec acceptable.
- **Live shutdown trigger via admin RPC** — `/healthz` doesn't
  accept POST today. A v2 admin endpoint could request graceful
  shutdown without a signal. Not in 9.13's surface; defer.
- **`pthread_kill`-style forced thread termination on join timeout** —
  v1 logs and leaks. The substrate is built to be process-restartable
  (WAL replay on startup); a leaked thread on shutdown is operator-
  visible (non-zero exit code) but doesn't corrupt state.
- **Configurable drain budget** — hard-coded 30s; v2 adds
  `cfg.shutdown.drain_budget_secs`.
- **Tests for SIGINT / SIGTERM delivery** — gated as manual
  (`#[ignore]`-marked unit tests) because process-wide signal state
  is hostile to parallel test execution. The functional behavior
  (`ShutdownSignal` fire + listener exit) is already covered by
  9.9's `shutdown_signal_stops_accept_loop`.

---

*Implement on approval.*
