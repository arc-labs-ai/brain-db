# Sub-task 9.4 — Shard scaffold (Glommio LocalExecutor + channel boundary)

**Reads:** `spec/01_system_architecture/05_hardware.md`, `spec/10_concurrency_epochs/02_single_writer.md`, audit (`docs/phases/phase-09-glommio-port.md`) §7 + §8.2.
**Phase doc:** `docs/phases/phase-09-server.md` §9.4 (was §9.2; orientation renumbered).
**Done when:** A Glommio `LocalExecutor` runs per shard on its own OS thread, drains a `flume` request channel, dispatches stub `Ping` requests, and shuts down cleanly on channel close. `ShardHandle` is `Send + Sync` and usable from the Tokio connection layer.

---

## 1. Scope

The skeleton on top of which 9.5–9.7 hang the real arena/WAL/OpsContext. **No real arena, no WAL, no metadata, no HNSW, no workers, no real ops.** Just:

- One OS thread per shard, pinned to a CPU (optional).
- One `glommio::LocalExecutor` per thread.
- A `flume::bounded` request channel — Tokio side sends, Glommio side receives.
- A `Ping` request that the shard replies to (validates the boundary works end-to-end).
- Graceful shutdown by dropping the last sender → channel closes → shard loop exits → thread joins.

**Out of scope (handled by later sub-tasks):**
- Real `Shard` data — Arena (9.5), WAL (9.6/9.6a), MetadataDb (9.7), HnswIndex (9.7).
- Real request types — Frame dispatch (9.10) replaces `Ping` with `ShardRequest::Frame { req, reply_tx }`.
- Workers — per-shard scheduler (9.7).
- `Rc<Cell<bool>>` shutdown flag — audit §8.2; lands with workers in 9.7. Channel-close suffices for 9.4 because there's nothing mid-cycle to preempt.
- CPU pinning across NUMA — single placement strategy for v1.

---

## 2. The Linux-only / macOS-buildable split

Glommio compiles only on Linux. We don't want brain-server to stop building on macOS during dev — config/routing tests must still pass on host. So:

| Item | Gate |
| ---- | ---- |
| `glommio` dep | `[target.'cfg(target_os = "linux")'.dependencies] glommio.workspace = true` |
| `shard.rs` module | `#[cfg(target_os = "linux")] mod shard;` in main.rs |
| Integration test | `#[cfg(target_os = "linux")]` on each `#[test]` |
| `flume` dep | unconditional — used in both layers later |
| `ShardHandle` type | also Linux-only; the connection-layer fake (for non-Linux compile) gets stubbed in 9.9 if needed |

On macOS:
- `cargo test -p brain-server` skips shard tests (cfg-gated out). 39 config/routing tests still pass.
- `cargo check -p brain-server` succeeds because glommio isn't pulled.

On Linux (dev container):
- `cargo test -p brain-server` compiles glommio + runs the shard integration tests.

---

## 3. Surface

```rust
// crates/brain-server/src/shard.rs  (Linux-only module)

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use brain_core::ShardId;
use flume::{Receiver, Sender};
use glommio::{LocalExecutorBuilder, Placement};
use parking_lot::Mutex;
use tracing::{debug, info, warn};

/// How the Tokio side talks to the Glommio side. v1 carries one
/// variant; 9.10 extends it with `Frame { req, reply_tx }`.
pub(crate) enum ShardRequest {
    /// Trivial round-trip; the shard replies with `()`.
    Ping { reply_tx: flume::Sender<()> },
}

/// Sent from the connection layer to spawn a shard.
#[derive(Clone, Debug)]
pub struct ShardSpawnConfig {
    /// Capacity of the request channel. 1024 is the v1 default.
    pub channel_capacity: usize,
    /// CPU to pin the shard's executor to. `None` = let the OS schedule.
    /// `Some(n)` must be a valid CPU index on the host.
    pub pin_cpu: Option<usize>,
}

impl Default for ShardSpawnConfig {
    fn default() -> Self {
        Self { channel_capacity: 1024, pin_cpu: None }
    }
}

/// Handle the Tokio connection layer holds. Cloneable, `Send + Sync`.
/// Each clone increments the sender count; the shard's thread joins
/// only after every clone (including any in-flight replies) drops.
#[derive(Clone)]
pub struct ShardHandle {
    shard_id: ShardId,
    tx: Sender<ShardRequest>,
    /// Lazily joined: the last handle drop joins the OS thread.
    join: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

impl ShardHandle {
    #[must_use]
    pub fn shard_id(&self) -> ShardId { self.shard_id }

    /// Round-trip Ping. Returns when the shard has replied.
    /// Errors if the shard has shut down (channel closed).
    pub async fn ping(&self) -> Result<(), ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx.send_async(ShardRequest::Ping { reply_tx }).await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx.recv_async().await
            .map_err(|_| ShardError::ShardDisconnected)?;
        Ok(())
    }

    /// Drop this handle's send capability. When the last handle drops,
    /// the channel closes; the shard's loop sees `Err(Disconnected)`
    /// and exits. Use this to initiate shutdown; the thread joins via
    /// `Drop` on the last `Arc<Mutex<JoinHandle>>` clone.
    pub fn shutdown(self) { drop(self); }
}

/// Drop on the last handle joins the OS thread.
impl Drop for ShardHandle {
    fn drop(&mut self) {
        if Arc::strong_count(&self.join) == 1 {
            if let Some(handle) = self.join.lock().take() {
                debug!(shard_id = self.shard_id, "joining shard thread");
                let _ = handle.join();
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    #[error("shard has shut down or is unreachable")]
    ShardDisconnected,
    #[error("failed to launch Glommio executor: {0}")]
    Spawn(String),
}

/// Spawn a shard on a dedicated OS thread.
pub fn spawn_shard(shard_id: ShardId, cfg: ShardSpawnConfig)
    -> Result<ShardHandle, ShardError>;

// Internal:
async fn shard_main_loop(shard_id: ShardId, rx: Receiver<ShardRequest>) {
    info!(shard_id, "shard executor entering main loop");
    while let Ok(req) = rx.recv_async().await {
        match req {
            ShardRequest::Ping { reply_tx } => {
                if reply_tx.send_async(()).await.is_err() {
                    warn!(shard_id, "Ping reply dropped (caller gone)");
                }
            }
        }
    }
    info!(shard_id, "shard main loop exiting (channel closed)");
}
```

The `spawn_shard` body uses Glommio's `LocalExecutorBuilder::spawn(closure)` which returns a `thread::JoinHandle<T>` directly — no manual `thread::spawn`:

```rust
pub fn spawn_shard(shard_id: ShardId, cfg: ShardSpawnConfig)
    -> Result<ShardHandle, ShardError>
{
    let (tx, rx) = flume::bounded(cfg.channel_capacity);
    let placement = match cfg.pin_cpu {
        Some(cpu) => Placement::Fixed(cpu),
        None => Placement::Unbound,
    };
    let join = LocalExecutorBuilder::new(placement)
        .name(&format!("brain-shard-{shard_id}"))
        .spawn(move || async move {
            shard_main_loop(shard_id, rx).await;
        })
        .map_err(|e| ShardError::Spawn(e.to_string()))?;
    Ok(ShardHandle {
        shard_id,
        tx,
        join: Arc::new(Mutex::new(Some(join))),
    })
}
```

`Placement::Fixed(cpu)` pins; `Placement::Unbound` lets the OS schedule. If `pin_cpu` is out of range Glommio errors at spawn — surface as `ShardError::Spawn`.

---

## 4. Send/Sync assertions

The connection layer (Tokio) shares `ShardHandle` across threads. Compile-time guarantee:

```rust
const _: fn() = || {
    fn require<T: Send + Sync>() {}
    require::<ShardHandle>();
};
```

The internals (`tx: flume::Sender`, `Arc<Mutex<JoinHandle>>`) are all `Send + Sync` already.

The `ShardRequest::Ping`'s `reply_tx` is a `flume::Sender<()>` — also `Send + Sync`. Future variants (Frame { … }) will need the same property; flume gives it for free.

---

## 5. Cargo.toml changes

Workspace (`Cargo.toml`):
```toml
[workspace.dependencies]
flume = { version = "0.11", default-features = false, features = ["async"] }
```

Brain-server crate (`crates/brain-server/Cargo.toml`):
```toml
[dependencies]
flume.workspace = true
parking_lot.workspace = true

[target.'cfg(target_os = "linux")'.dependencies]
glommio.workspace = true
```

`parking_lot` already in workspace; just bring it in for the `Mutex<JoinHandle>`.

---

## 6. Tests

### 6.1 Compile-time `Send + Sync`
Non-Linux compatible — runs on macOS too.

```rust
#[test]
fn shard_handle_is_send_sync() {
    fn require<T: Send + Sync>() {}
    require::<ShardHandle>();
}
```

Wait — `ShardHandle` is itself `#[cfg(target_os = "linux")]`. Move this assertion into the Linux-gated test file.

### 6.2 Linux-only integration tests (in `crates/brain-server/tests/shard.rs`)

```rust
#![cfg(target_os = "linux")]

// 1. ping_roundtrips
//    spawn → ping → reply → drop handle → thread joins.
// 2. multiple_pings_in_sequence
//    spawn → 100× ping → all reply in order.
// 3. concurrent_handles_share_shard
//    spawn → clone handle → both ping in parallel via tokio::join! → both succeed.
// 4. drop_handle_shuts_down
//    spawn → drop handle (no shutdown call) → thread joins (assert via attempting
//    to clone and ping post-drop, which fails because the handle is gone).
//    Actually simpler: spawn → store handle → drop → poll thread join with timeout.
// 5. pin_to_invalid_cpu_errors
//    spawn with pin_cpu = Some(usize::MAX) → ShardError::Spawn.
```

Each test runs `#[tokio::test]` on the Tokio side — the test process is Tokio + a Glommio child thread. We deliberately exercise the cross-runtime boundary. (Per audit §6.4: tests stay tokio-driven; Glommio is invoked transitively via `spawn_shard`.)

### 6.3 Unit tests inside `shard.rs`
Linux-only.

```rust
// 6. shard_request_ping_variant_carries_sender
//    sanity check on enum shape.
// 7. spawn_with_unbound_placement_succeeds
//    spawn → handle returned → drop → done.
```

Sizing: ~5 integration tests + 2 unit tests.

---

## 7. The Tokio↔Glommio handshake

Critical detail: `flume::Sender::send_async()` and `flume::Receiver::recv_async()` are both runtime-agnostic. flume implements `Future` directly; both sides poll it natively. No `block_in_place` or `spawn_blocking` needed.

The reply channel (`flume::bounded(1)` per call) likewise polls in either runtime. The Tokio side's `.await` is driven by tokio's reactor; the Glommio side's `.await` is driven by glommio's reactor. Both reactors are happy.

This is **the** justification for picking flume over `tokio::sync::mpsc` (which only polls under tokio).

---

## 8. Risks

| Risk | Mitigation |
| ---- | ---------- |
| `glommio` v0.9 API differs from what's planned | Plan defers to the actual surface at impl time. If `LocalExecutorBuilder::spawn(closure)` is named differently, fix at impl. Crate version is pinned in workspace. |
| flume's async feature isn't enabled by default | Cargo entry explicitly sets `features = ["async"]`. |
| `Placement::Fixed(n)` panics or errors on out-of-range | Test 6.2 #5 confirms behavior; error path returns `ShardError::Spawn`. |
| Dropping last handle while a Ping is in flight races | flume's `Sender` drop closes the channel only when **all** senders drop; the reply channel is per-call. The shard's `recv_async` returns `Err` cleanly. No partial state. |
| macOS dev unable to verify the integration tests | Plan ships verified `cargo check + clippy + 39 host tests`. The Linux container runs the full set; document in commit + audit. |
| Glommio dep chain fails to compile in the dev container (like the earlier gemm-f16 issue from 9.1) | Glommio has no candle/gemm deps. Verify with `cargo check -p brain-server --target x86_64-unknown-linux-gnu` if cross-compile is set up, else verify in container. If it fails, surface to user. |

---

## 9. File-by-file

| File | Action | LOC |
| ---- | ------ | --- |
| `Cargo.toml` (workspace)             | Edit  | +1 line (flume) |
| `crates/brain-server/Cargo.toml`     | Edit  | +5 lines (deps + target gate) |
| `crates/brain-server/src/shard.rs`   | NEW (Linux-only) | ~180 LOC + ~50 unit tests |
| `crates/brain-server/src/main.rs`    | Edit  | +1 `#[cfg(target_os = "linux")] mod shard;` |
| `crates/brain-server/tests/shard.rs` | NEW (Linux-only) | ~140 LOC integration |

Total: ~370 LOC impl + tests. Single commit.

Commit subject: `feat(brain-server): shard scaffold (sub-task 9.4)`.

---

## 10. Verification plan

1. macOS host: `cargo check -p brain-server && cargo test -p brain-server` — 39 tests pass (shard tests skipped via cfg).
2. Linux container: `docker run … cargo test -p brain-server` — 39 + ~7 = ~46 tests pass.
3. macOS host: `cargo clippy -p brain-server --all-targets -- -D warnings` — clean.
4. macOS host: `cargo fmt -p brain-server -- --check` — clean.

If step 2 fails because the container can't compile glommio (or its deps), surface to user before committing. Step 2 is the gate for declaring 9.4 done — the macOS-only verification is necessary but not sufficient.

---

## 11. Done criteria

- [ ] `shard.rs` ships under `#[cfg(target_os = "linux")]`.
- [ ] flume + parking_lot wired into Cargo.toml; glommio target-gated.
- [ ] `ShardHandle: Send + Sync` compile-time assertion in place.
- [ ] 5 integration tests + 2 unit tests pass on Linux.
- [ ] macOS host: cargo check / test / clippy / fmt all green.
- [ ] Linux container: full test suite green. **Required.** If container build fails for non-9.4 reasons (e.g. candle/gemm), surface and decide.
- [ ] Commit on `feature/brain-server`.
- [ ] Phase doc 9.4 marked `[x]`.
- [ ] Audit doc §6 / §7 / §8.2 status rows updated where 9.4 contributes (channel boundary established; no scheduler/worker changes yet).

---

## 12. What 9.4 explicitly *doesn't* set up

So we don't accidentally smuggle later work into the scaffold:

- The `Shard` struct (data) — that's 9.5+ work. 9.4 has a free function `spawn_shard` and an internal `shard_main_loop`, no `pub struct Shard`.
- Per-shard `OpsContext` — 9.7.
- Workers + scheduler — 9.7.
- Routing-table-to-handle-map (`HashMap<ShardId, ShardHandle>`) — that's the connection layer's bookkeeping in 9.9.
- Frame parsing — 9.10.

Resist the urge. The scaffold's only job is "the channel boundary works".

---

*Implement on approval.*
