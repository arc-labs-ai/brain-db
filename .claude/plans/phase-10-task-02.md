# Sub-task 10.2 — Connection pool

**Reads:**
- `spec/13_sdk_design/03_connection.md` §1, §2, §4, §5, §13, §14.
- Re-skim §6 (reconnection), §7 (failover), §17 (backoff) to mark
  the boundaries — those land in 10.3 + later.

**Phase doc:** `docs/phases/phase-10-sdk-cli.md` §10.2.

**Done when:** A `Pool` type maintains 1–8 connections per server
(min/max configurable), reaps idle ones past `idle_timeout`, and
exposes `warm_up()` that pre-establishes the configured min.
`Client` becomes a thin wrapper around the pool; `Client::connect(addr)`
keeps its 10.1 contract (a Client backed by a pool of size 1).
`Client::warm_up()` and `Client::builder()` are the new public
surface.

---

## 1. What 10.2 delivers

Spec §13/03 §1: "The Client maintains a pool of connections per
server." 10.1 hardcoded a single connection because the pool
hadn't landed yet. 10.2 makes Client pool-aware without breaking
10.1's contract.

**Two refactors + one new type:**

1. **Extract `Connection` from `Client`.** Move the single-TCP
   fields (stream, session, next_stream_id, agent_id) into a new
   `Connection` type under `pool/`. `Connection::open(addr, cfg,
   agent_id)` does the same work `Client::connect_with` did in
   10.1. The handshake FSM (`proto::handshake::complete_handshake`)
   stays put — `Connection` calls it.

2. **`Pool` type** (new, under `pool/`):
   - Holds `Vec<Pooled>` where `Pooled = { conn: Connection,
     last_used: Instant, in_use: bool }`.
   - `Pool::new(config, addr) -> Pool`.
   - `Pool::warm_up(&self) -> Result<(), ClientError>` opens
     `config.min_connections` connections in parallel.
   - `Pool::acquire(&self) -> Result<PoolGuard<'_>, ClientError>`
     hands out a free connection, opens a new one if all in-use
     and we're under `max_connections`, errors with
     `ClientError::Overloaded` if at cap.
   - `PoolGuard` is a RAII handle: derefs to `&mut Connection`,
     marks the slot free on drop, updates `last_used`.
   - Background idle reaper task (tokio task spawned by `Pool::new`)
     periodically closes connections idle past `idle_timeout`,
     keeping at least `min_connections` alive.

3. **`Client` reshape**:
   - Internally holds `Arc<Pool>`.
   - `Client::connect(addr)` still works — constructs a
     `PoolConfig` with `min = 1, max = 1` and returns a
     `Client(Arc<Pool>)`.
   - `Client::builder()` exposes the full surface for callers who
     want larger pools.
   - `Client::warm_up()` proxies to the pool.
   - 10.5's op methods will go through `pool.acquire()` per call.
   - `Client::bye()` becomes `Client::close()` (signal pool to
     drain — see §6 below).

**What 10.2 does NOT deliver:**

- Multi-server / failover (spec §13/03 §7) — single bootstrap
  address only.
- Routing table / shard-aware addressing (§8) — single-server in
  v1.
- Bootstrap discovery (§9) — v2 cluster work.
- TLS (§11) — separate sub-task post-Phase-10.
- Reconnection on drop (§6, §17) — overlaps with 10.3's retry
  surface; deferred there.
- Lifecycle hooks (§10) → 10.7 observability.
- Connection metrics (§16) → 10.7.
- Circuit breaker (§18) — v2.
- SHUTDOWN-frame handling (§15) — needs server-side support; v2.
- Graceful drop of in-flight requests (§14) — basic close()
  ships; in-flight tracking lands with 10.5.

---

## 2. PoolConfig

```rust
pub struct PoolConfig {
    pub min_connections: u32,        // default 1
    pub max_connections: u32,        // default 8
    pub idle_timeout: Duration,      // default 5 min
    pub acquire_timeout: Duration,   // default 30 s (= request timeout)
    pub keepalive_interval: Duration, // default 30 s (spec §5)
                                      //   placeholder; 10.6 wires server PING
}
```

Defaults come from spec §13/03 §1 (min 1, max 8), §5 (5 min idle,
30 s keep-alive). `acquire_timeout` is new — bounds how long
`acquire()` blocks waiting for a free connection before
returning `Overloaded`.

`ClientConfig` (added in 10.1) gains a `pool: PoolConfig` field.
Default for 10.1's `ClientConfig::default()` stays equivalent —
the existing tests pass without changes because pool defaults
collapse to single-connection behavior.

---

## 3. Module layout

```
crates/brain-sdk-rust/src/
├── lib.rs                       (re-exports updated)
├── client/
│   └── mod.rs                   thin Client wrapping Arc<Pool>
├── config/
│   └── mod.rs                   ClientConfig grows `pool: PoolConfig`
├── error/
│   └── mod.rs                   + `Overloaded`, `PoolClosed` variants
├── proto/                       (unchanged)
└── pool/                        NEW
    ├── mod.rs                   Pool + acquire/release + reaper task
    ├── connection.rs            Connection (extracted from Client)
    ├── config.rs                PoolConfig + defaults
    └── guard.rs                 PoolGuard (RAII checkout)
```

LOC estimates: `pool/mod.rs` ~300, `connection.rs` ~150 (mostly
moved code), `config.rs` ~60, `guard.rs` ~80. `client/mod.rs`
shrinks from ~140 to ~120 (thinner; same surface). Tests:
`tests/pool.rs` adds ~200 LOC.

---

## 4. Concurrency model

Pool internals: `parking_lot::Mutex<Vec<Slot>>` where
`Slot = { connection: Connection, state: SlotState }` and
`SlotState ∈ { Idle, InUse, Closed }`.

`acquire()` algorithm:
1. Lock the slots mutex.
2. Find first `Idle` slot → mark `InUse`, return guard.
3. If none idle and `len() < max`, drop the lock, open new
   connection, re-lock + push, return guard.
4. If at cap, wait on a `tokio::sync::Notify` until released or
   `acquire_timeout` fires.

Idle reaper: a single `tokio::spawn`'d task per pool. Wakes
every `idle_timeout / 4`, scans slots, closes any `Idle` slot
past `last_used + idle_timeout` while respecting `min_connections`.
Task is cancelled via a `tokio::sync::watch` channel signalled by
`Pool::close`.

---

## 5. Tests

### 5.1 Unit: `pool/config.rs::defaults_match_spec`
- min=1, max=8, idle=5min, keepalive=30s.

### 5.2 Unit: `pool/mod.rs::acquire_then_release_marks_idle`
- White-box mock connection (trait-object behind `Connection`
  for testability? Not yet — see §6 risks). Use a thin in-pool
  test that bypasses the network: `Pool::new_in_memory` with a
  ready-made `Connection` injected.
- Actually defer this: 10.2's pool always owns real
  `Connection`s. The in-memory variant is a future testability
  add-on.

### 5.3 Integration: `tests/pool.rs::warm_up_opens_min_connections`
- Mock server that accepts N HELLO+AUTH cycles, then idles.
- `Pool::new(addr, PoolConfig { min: 3, max: 8, .. })`.
- `pool.warm_up().await?;`
- Assert mock server saw exactly 3 connect+handshake sequences.

### 5.4 Integration: `tests/pool.rs::acquire_reuses_idle_connection`
- Mock server accepts 1 connection, runs handshake, then idles.
- `let g1 = pool.acquire().await?; drop(g1); let g2 = pool.acquire().await?;`
- Assert mock server saw only 1 handshake.

### 5.5 Integration: `tests/pool.rs::acquire_blocks_then_succeeds_when_released`
- Pool size max=1.
- Acquire two concurrent acquire futures; first wins, second
  waits, succeeds after first guard drops.

### 5.6 Integration: `tests/pool.rs::acquire_overloaded_at_cap`
- Pool size max=1, `acquire_timeout = 100ms`.
- Acquire + hold; second `acquire()` returns
  `ClientError::Overloaded` within ~110ms.

### 5.7 Integration: `tests/pool.rs::idle_reaper_closes_stale_connection`
- Pool min=0, max=2, `idle_timeout = 200ms`.
- Acquire + release a connection.
- Wait 500ms.
- Assert connection count drops to 0; mock server saw a FIN.

### 5.8 Existing: `tests/handshake.rs` keeps passing
- `Client::connect(addr)` and `Client::bye()` should not change
  observable behavior. Internal refactor only.

---

## 6. Risks

| Risk | Mitigation |
| ---- | ---------- |
| Big-bang refactor of 10.1's `Client` could regress | 10.1's tests are kept as-is and must pass unchanged. They exercise `Client::connect` + `Client::bye` end-to-end. |
| Per-pool reaper task accumulates if many Pools are created | `Pool::close()` signals the watch channel, and `Drop for Pool` issues `tokio::spawn(reaper_shutdown)` (best-effort). Document the contract: pools are long-lived; don't spam them. |
| `acquire_timeout` competing with the per-request `timeout` | Different concepts: acquire = wait for slot, request = wait for response. Document in the field docs. |
| Connection might be dead when `acquire()` returns it (idle TCP RST) | 10.2 doesn't yet validate idle connections (spec §5). Connections fail their next I/O and return `ClientError::Closed`; 10.3 retries the request on a fresh connection. Add a TODO comment. |
| `parking_lot::Mutex` vs `tokio::sync::Mutex` | Use `parking_lot::Mutex` — slot bookkeeping is non-blocking; only `Notify::notified().await` crosses the await boundary, and we drop the lock first. |
| Mock-server tests need an "accept N connections" mode | Extract a small helper into a `tests/mock.rs` module shared by 10.1's `handshake.rs` and 10.2's `pool.rs`. ~80 LOC, drops duplication. |

---

## 7. Done criteria

- [ ] `pool/` folder with `mod.rs`, `connection.rs`, `config.rs`,
  `guard.rs`. Folder-per-concern preserved.
- [ ] `Connection` extracted, `Client` thin Pool wrapper.
- [ ] `ClientConfig.pool: PoolConfig` added; defaults unchanged
  in observable behavior.
- [ ] `Client::builder()` / `Client::warm_up()` public.
- [ ] `ClientError::Overloaded` / `ClientError::PoolClosed`
  variants added.
- [ ] 10.1's tests pass without modification.
- [ ] New 7 tests in §5 pass under docker-verify.
- [ ] Sub-task 10.2 marked `[x]` in `docs/phases/phase-10-sdk-cli.md`.

---

## 8. What 10.2 explicitly defers

- Reconnection on drop (10.3).
- Multi-server / failover (post-Phase-10).
- Routing-aware send (v2).
- Bootstrap discovery (v2).
- TLS (post-Phase-10).
- Connection metrics + lifecycle hooks (10.7).
- Circuit breaker (v2).
- SHUTDOWN-frame handling (v2).
- In-flight tracking for graceful close (10.5 once op methods
  exist).

---

*Implement on approval.*
