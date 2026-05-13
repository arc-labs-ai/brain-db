# Sub-task 9.10 — Frame dispatcher (Tokio↔Glommio boundary)

**Reads:**
- `spec/01_system_architecture/04_layers.md` (L1 dispatch responsibilities).
- `spec/03_wire_protocol/05_opcodes.md` (full opcode table).
- `spec/03_wire_protocol/06_handshake.md` (HELLO→WELCOME→AUTH→AUTH_OK state machine).
- `spec/03_wire_protocol/07_request_frames.md` + `08_response_frames.md` (payload shapes, already implemented in `brain-protocol`).
- `spec/03_wire_protocol/09_streaming.md` §1–§5 (stream IDs, EOS, single-frame vs streaming responses).
- `spec/03_wire_protocol/10_errors.md` (error code → category mapping).
- `spec/12_sharding_clustering/02_routing.md` (BLAKE3 agent → shard).
- `docs/phases/phase-09-glommio-port.md` §7 (Tokio side) + §8.1 (EventBus topology — `SUBSCRIBE` deferred to 9.11).

**Phase doc:** orientation §11 sub-task **9.10** (frame dispatcher).

**Done when:** `serve_connection` runs the full handshake state machine, dispatches non-streaming request opcodes to the correct shard via `RoutingTable`, awaits the shard's `ResponseBody`, encodes it back to a wire frame, and replies on the same `stream_id`. PING/PONG, BYE, and idle SERVER_PING wire correctly. SUBSCRIBE / CANCEL_STREAM / admin streaming responses stay stubbed (return `ERROR(NotYetImplemented)`); they are 9.11 / 9.13.

---

## 1. Scope split with adjacent sub-tasks

| Concern | Lands in |
| ------- | -------- |
| HELLO/WELCOME/AUTH/AUTH_OK state machine | **9.10** |
| Per-connection state (negotiated version, agent_id, permissions, txn ids) | **9.10** |
| PING → PONG, SERVER_PING → CLIENT_PONG, idle timeout | **9.10** |
| BYE handling (graceful close) | **9.10** |
| `ConnectionListener::bind()` spawns per-shard `LocalExecutor` + holds `Vec<ShardHandle>` | **9.10** |
| Opcode → shard via `RoutingTable::shard_for_agent` / `shard_for_memory` | **9.10** |
| Single-frame request → single-frame response opcodes: ENCODE, FORGET, LINK, UNLINK | **9.10** |
| Streaming responses (RECALL, PLAN, REASON) — *first* frame only, stub the rest as a single EOS frame | **9.10** |
| Transactions (TXN_BEGIN / TXN_COMMIT / TXN_ABORT) | **9.10** |
| SUBSCRIBE / SUBSCRIBE_EVENT fan-out | **9.11** |
| CANCEL_STREAM machinery | **9.11** |
| Real Recall / Plan / Reason streaming (multi-frame batches) | **9.11** / out of scope |
| Per-IP / per-agent connection limits | **9.13** |
| SIGTERM + drain timer | **9.14** |
| Real auth backends (JWT, mTLS verification) | beyond Phase 9 (spec §06/§4 calls out token/mTLS as configurable) |

Single-frame ops are the bulk of the work and unblock end-to-end smoke tests (9.17). Streaming becomes a content-only diff once the seam is right.

---

## 2. Architectural shape

```
                              brain-server::main
                                     │
                       Tokio runtime (multi-thread)
                                     │
              ┌──────────────────────┼──────────────────────────┐
              │                      │                          │
        ConnectionListener   spawn_shard × N     ShutdownTrigger
        (9.9, extended)      (one Glommio       (9.9)
              │              LocalExecutor each)
              │                      │
        accept loop                  Vec<ShardHandle>
              │                      │
       per-connection task ──────────┘
       (one tokio::spawn          flume::Sender<ShardRequest>
        per accepted stream)      (one per shard, cloned into
              │                    ConnState for routing)
              │
   ┌──────────┴───────────┐
   │   ConnState          │
   │  ─────────────────   │
   │  version: u8         │
   │  session_id: [u8;16] │
   │  agent: Option<…>    │  None until AUTH_OK
   │  permissions: …      │
   │  txns: HashMap<…>    │  active txn ids opened on this conn
   │  state: Phase        │  AwaitingHello / AwaitingAuth / Established
   │  next_idle_at: Inst. │  SERVER_PING deadline
   │  shutdown: Signal    │
   └──────────┬───────────┘
              │
       frame_dispatch_loop(stream, conn_state, shards)
              │
        decode RequestBody → handle:
              │
              ├── connection-management (HELLO/AUTH/BYE/PING/CLIENT_PONG) — in-task
              │
              └── data-plane (ENCODE/RECALL/...) — route to shard, await reply
                                     │
                                     ▼
                         ShardHandle::dispatch_op(req, reply_tx)
                                     │
                                     ▼
                         (inside Glommio executor)
                             brain_ops::dispatch(req, &ops) ─► ResponseBody
                                     │
                         reply_tx.send_async(Ok(resp)).await
                                     ▼
                     per-connection task encodes ResponseBody → Frame
                                     │
                                     ▼
                           stream.write_all(frame.encode()).await
```

The boundary primitive stays **`flume::bounded`** (audit §7): runtime-agnostic, `send_async` on the Tokio side and `recv_async` on the Glommio side. The reply path uses a per-request **`flume::bounded(1)` one-shot** carried inside the `ShardRequest::DispatchOp` variant.

---

## 3. The per-connection state machine

```rust
enum ConnPhase {
    AwaitingHello,
    AwaitingAuth { session_id: [u8; 16], chosen_version: u8 },
    Established { agent_id: AgentId, permissions: AgentPermissions, bound_shard: ShardId },
    Closing,
}
```

### 3.1 AwaitingHello → AwaitingAuth

Triggered by a `HELLO` frame (opcode 0x01, stream_id 0):

1. Validate `HELLO.supported_versions` ∩ {1}. Empty → `ERROR(VersionNotSupported)` + close.
2. Allocate `session_id = [u8; 16]` from `rand::random()` (or `uuid::Uuid::new_v4().into_bytes()` to avoid a new dep).
3. Send `WELCOME` with `chosen_version`, `session_id`, `server_features`, `auth_methods = [None]` (real auth backends land later).
4. Transition to `AwaitingAuth`.
5. Start a **30-second auth timer** (spec §06/§6.3). If timer fires before AUTH, close.

### 3.2 AwaitingAuth → Established

Triggered by an `AUTH` frame (opcode 0x02, stream_id 0):

1. `method == None` for v1 (config-gated; default-on in `dev.toml`). Token / Mtls return `ERROR(NoSuchAuthMethod)`.
2. Compute `bound_shard = routing.shard_for_agent(auth.agent_id)`.
3. Build `AgentPermissions { all true }` (v1 dev default — Phase 10's auth pass tightens this).
4. Send `AUTH_OK { agent_id, bound_shard_id, permissions, server_time_unix_nanos }`.
5. Transition to `Established`. Cancel the auth timer.

### 3.3 Established: per-frame dispatch

For each frame:

1. Reject server-bound opcodes that don't belong here (BadOpcode for response-only opcodes).
2. **Connection management** — handled in-task, no shard hop:
   - `PING` (0x10) → `PONG` (0x90) with server timestamp.
   - `CLIENT_PONG` (0x11) → reset idle timer; no reply.
   - `BYE` (0x1F) → reply `BYE` with EOS; flush; transition to `Closing`.
3. **Data-plane ops** — route + dispatch:
   - Decode `RequestBody` via `RequestBody::decode(opcode, &payload)`.
   - Pick the target shard:
     - For requests carrying a `MemoryId` (Forget, Link, Unlink): `routing::shard_for_memory(memory_id)`.
     - Otherwise: use `conn.bound_shard` (the agent's home shard).
   - Build `ShardRequest::DispatchOp { req, reply_tx }`, send via the shard's `flume::Sender`.
   - Await `reply_rx.recv_async()` → `Result<ResponseBody, OpError>`.
   - On Ok: encode response body, build frame with `(matching_response_opcode, EOS, stream_id, payload)`, write to socket.
   - On Err: map `OpError` → `ErrorResponse` via existing `brain_protocol::error` adapters, write `ERROR` frame.
4. **Streaming ops (RECALL/PLAN/REASON)** — 9.10 ships a **single-frame EOS response**. The shard's `dispatch` returns the *first* `ResponseBody::Recall` frame already; we just slap EOS on it. Multi-frame streaming is 9.11.
5. **SUBSCRIBE / UNSUBSCRIBE / CANCEL_STREAM** — return `ERROR(NotYetImplemented, "9.11")` and close the stream (not the connection).

### 3.4 Idle timing

- Reset `next_idle_at = now + cfg.idle_timeout` on every received frame.
- A background `tokio::time::sleep_until(next_idle_at)` fires `SERVER_PING`. If no `CLIENT_PONG` within `cfg.ping_timeout`, close.
- Default values per spec §06/§3.4: idle 300s, ping timeout 30s. Mirror into `ConnectionLimits` (already exists from 9.9).

### 3.5 Concurrency on one connection

Spec §03/02 §5: many concurrent streams per connection. 9.10's implementation lane:

- **Sender loop** owns the socket write half.
- **Receiver loop** owns the socket read half.
- Each frame received either:
  - Is connection-management → handled inline by the receiver loop, response queued.
  - Is a data-plane op → spawned as a sub-task: `tokio::spawn(handle_op(req, shards, write_tx))`.
- Responses from sub-tasks travel through a per-connection `flume::Sender<Frame>` to the sender loop.

This split (split read+write, op fanout) is what lets multiple concurrent ENCODEs over the same connection actually run in parallel on different shards.

The buffered output channel size: 256 frames (~16 KiB per frame × 256 = 4 MiB worst case if everything is full). Tunable via `ConnectionLimits`.

---

## 4. ShardRequest extension

Today's `crates/brain-server/src/shard.rs` `ShardRequest` is:

```rust
enum ShardRequest {
    Ping { reply_tx },
    AllocSlot { reply_tx },
    AppendWalRecord { record, reply_tx },
}
```

9.10 adds:

```rust
DispatchOp {
    req: RequestBody,
    reply_tx: flume::Sender<Result<ResponseBody, OpError>>,
},
```

Handler inside `shard_main_loop`:

```rust
ShardRequest::DispatchOp { req, reply_tx } => {
    let resp = brain_ops::dispatch::dispatch(req, &shard.ops).await;
    let _ = reply_tx.send_async(resp).await;
}
```

`brain_ops::dispatch::dispatch` already returns `Result<ResponseBody, OpError>`. The 9.7b wire-up exists — we just call it.

ShardHandle gains:

```rust
pub async fn dispatch_op(&self, req: RequestBody) -> Result<ResponseBody, DispatchError> {
    let (reply_tx, reply_rx) = flume::bounded(1);
    self.tx.send_async(ShardRequest::DispatchOp { req, reply_tx })
        .await.map_err(|_| DispatchError::ShardDisconnected)?;
    reply_rx.recv_async().await
        .map_err(|_| DispatchError::ShardDisconnected)?
        .map_err(DispatchError::Op)
}

pub enum DispatchError {
    ShardDisconnected,
    Op(OpError),
}
```

---

## 5. Per-shard spawn in main

9.9's `linux_main::run` shipped with `shards: Arc::new(Vec::new())`. 9.10 wires the real spawn:

```rust
let mut shards = Vec::with_capacity(cfg.storage.shard_count);
let mut joiners = Vec::with_capacity(cfg.storage.shard_count);
for shard_id in 0..cfg.storage.shard_count {
    let spawn_cfg = ShardSpawnConfig {
        channel_capacity: cfg.shard.channel_capacity.unwrap_or(1024),
        pin_cpu: cfg.shard.pin_cpu_offset.map(|off| off + shard_id),
        data_dir: cfg.storage.data_dir.clone(),
        arena_initial_capacity_slots: …,
        wal_config: …,
    };
    let (handle, joiner) = spawn_shard(shard_id as u16, spawn_cfg)?;
    shards.push(handle);
    joiners.push(joiner);
}
let shards = Arc::new(shards);
```

`shard_count == 0` is a config error (config.rs already validates `> 0`).

Joiners are kept around so 9.14's shutdown can `for j in joiners { j.join()?; }` after the listener exits. For 9.10 we just hold them; the runtime can panic if the binary exits with un-joined Glommio threads (rare in practice, but flag for 9.14).

The RoutingTable construction:

```rust
let routing = Arc::new(
    RoutingTable::new(cfg.storage.shard_count as u16, /* overrides: */ HashMap::new())?
);
```

The empty `overrides` is correct for v1 (config-driven overrides land later).

---

## 6. New module layout

| File | Action | Approx LOC |
| ---- | ------ | ---------- |
| `crates/brain-server/src/dispatch.rs` | new — `dispatch_frame`, `handle_op_async`, error mapping, per-conn state | ~600 |
| `crates/brain-server/src/connection.rs` | extend — replace stub `serve_connection` with the real loop; add `ConnState`, sender/receiver split | ~300 delta |
| `crates/brain-server/src/shard.rs` | extend — `ShardRequest::DispatchOp` + `ShardHandle::dispatch_op` + `DispatchError` | ~120 delta |
| `crates/brain-server/src/main.rs` | extend — shard spawn loop, RoutingTable construction, pass into listener | ~70 delta |
| `crates/brain-server/Cargo.toml` | add `rand` for session_id (or use `uuid` already in deps) | 1 line |
| `crates/brain-server/tests/dispatch.rs` | new — end-to-end frame round-trip tests | ~400 |

Total: ~1500 LOC. Larger than 9.9 by ~50%; structurally riskier because of the state machine. I'd consider a 9.10a / 9.10b split if the dependency cascade allows — see §10 below.

---

## 7. Error mapping

Spec §10 lists ~80 error codes. We don't need exhaustive coverage in 9.10; we need a deterministic mapping for what we actually emit. Sketch:

```rust
fn op_error_to_wire(e: &OpError) -> (ErrorCode, ErrorCategory) {
    match e {
        OpError::NotFound(_)       => (ErrorCode::MemoryNotFound,     ErrorCategory::NotFound),
        OpError::PermissionDenied  => (ErrorCode::PermissionDenied,   ErrorCategory::Authorization),
        OpError::InvalidArgument(_)=> (ErrorCode::InvalidArgument,    ErrorCategory::Validation),
        OpError::Conflict(_)       => (ErrorCode::IdempotencyMismatch,ErrorCategory::Conflict),
        OpError::NotYetImplemented(_) => (ErrorCode::Unimplemented,   ErrorCategory::Unavailable),
        OpError::Storage(_)        => (ErrorCode::InternalError,      ErrorCategory::Internal),
        // ... fallback
        _                          => (ErrorCode::InternalError,      ErrorCategory::Internal),
    }
}
```

Verify which `ErrorCode` variants actually exist in `brain_protocol::error` during impl; the mapping above is approximate.

Protocol-level errors (BadMagic, BadHeaderCrc, OversizePayload, etc.) come from `Frame::decode_with_max` already — 9.9's stub flow handles them.

---

## 8. Tests (tests/dispatch.rs, ~10 cases)

Each test brings up a fresh listener + a shard (via `start_with_shards` scaffold) on `127.0.0.1:0`, drives a Tokio client over plain TCP. No TLS needed for dispatch tests.

1. **`handshake_completes`** — HELLO → WELCOME, AUTH → AUTH_OK, observe bound_shard, no error frames.
2. **`hello_with_unsupported_version_errors`** — HELLO with `supported_versions = [99]` → `ERROR(VersionNotSupported)` + close.
3. **`ops_before_auth_are_rejected`** — send ENCODE_REQ on stream 1 after WELCOME but before AUTH → `ERROR(NotAuthenticated)`.
4. **`encode_round_trips_through_shard`** — handshake + ENCODE("hello") → ENCODE_RESP with non-NULL memory_id.
5. **`forget_routes_by_memory_id`** — encode on agent A (lands on shard 0), forget the memory_id (also routes to shard 0). Two shards configured; assert no shard-mismatch error.
6. **`ping_pong_with_timestamp`** — PING with client ts → PONG echoes ts + adds server ts.
7. **`bye_closes_gracefully`** — send BYE, receive BYE, observe EOF.
8. **`bad_opcode_errors_stream_not_connection`** — send a client-bound opcode (`ENCODE_RESP`) → `ERROR(BadOpcode)` on the same stream, connection stays open, follow-up PING still works.
9. **`recall_returns_single_frame_eos_in_v1`** — RECALL_REQ → one RECALL_RESP frame with EOS (streaming deferred).
10. **`server_ping_fires_after_idle_timeout`** — connect, complete handshake, wait past the (test-overridden, short) idle timeout, observe SERVER_PING frame.

For most tests, the shard count is 2 so routing is exercised at least minimally. Tests share a tiny `Topology` helper that spawns N shards + builds a RoutingTable, then bootstraps the listener.

The `shard_constructs_full_ops_stack` test from 9.7b's `tests/shard.rs` already does the heavy lifting of "open shard with full OpsContext". 9.10's test scaffold reuses that pattern; the listener is the new piece.

---

## 9. Risks

| Risk | Mitigation |
| ---- | ---------- |
| `brain_ops::dispatch` exit shape (returns ResponseBody) doesn't match a few opcodes — Subscribe returns SubscribeEvent (first event), TxnBegin returns TxnBegin etc. | The mismatch is in opcode-byte mapping (response opcode), not body shape. Add a helper `response_opcode_for(&ResponseBody) -> u8` that pattern-matches on the variant. |
| Spawning many `tokio::spawn` per-frame sub-tasks creates response-frame interleaving without ordering guarantees | Within one stream that's fine (one stream = one in-flight op). The output channel preserves the order responses arrive in, not request order — and that matches spec §03/02 §5.3. |
| Connection-task lifecycle: with read/write split, who shuts down whom? | Use `tokio::sync::oneshot::Sender<()>` for cross-task close + a JoinSet for the per-op sub-tasks. Sender loop drops when receiver loop exits; receiver drops when shutdown fires or read fails. |
| Idle timer + read both block the receiver loop | `tokio::select! { read = ... , _ = idle_timer => { send SERVER_PING }, _ = shutdown.recv() => break }` — three-arm select, but only one read at a time. Same shape as 9.9's stub. |
| Phase-9 dispatch path holds the shard's executor per-request → head-of-line blocking | brain_ops::dispatch is async and the shard's executor is single-threaded; concurrent requests on the same shard queue naturally via the flume channel. That's the design, not a bug. Cross-shard parallelism is what scales. |
| Subscribe handler exists in brain_ops but returns *one* SubscribeEvent; pushing follow-up events into the connection needs the cross-shard EventBus (9.11) | 9.10 stubs Subscribe → ERROR(NotYetImplemented). Audit §8.1 backs this. |
| `RequestBody::decode` might reject opcodes that don't carry a request body (responses, errors) | That's the desired behavior. Decode failure → `ERROR(BadOpcode)`. |
| Frame-dispatcher tests will be flaky if shard spawn is slow | Use `--test-threads=1` if needed; or, pre-spawn one shard for the whole test binary via `OnceCell`. Decide during impl. |

---

## 10. Should this be split into 9.10a + 9.10b?

The natural fault line is **handshake (state machine) vs op dispatch (shard fanout)**:

- **9.10a — handshake only:** HELLO/WELCOME, AUTH/AUTH_OK, PING/PONG, BYE, idle timer. No shard hop. ~600 LOC.
- **9.10b — op dispatch:** `ShardRequest::DispatchOp`, shard spawn loop, op-to-shard routing, ENCODE/FORGET round-trip, error mapping. ~900 LOC.

Arguments for the split:
- 9.10a is testable in isolation (no shards) — fast feedback.
- The state machine is the riskier piece (timer + select + close semantics); landing it independently de-risks 9.10b.

Arguments against:
- 9.10a's "what does the dispatch loop do after AUTH_OK?" is empty until 9.10b; the stub would be `ERROR(NotYetImplemented)` for every op — exactly what 9.9 already ships.
- The shard spawn (multi-Glommio in `linux_main::run`) is small but the routing wire-up needs the full picture; doing it twice is ~30 extra LOC of churn.

**Recommendation:** ship as 9.10 atomically. The state machine and op dispatch share enough scaffolding (ConnState, the read/write split, the sender channel) that splitting forces awkward stubs. The single-commit version is ~1500 LOC; well within precedent (9.7a was bigger).

If implementation hits compile-cascade pain, revert to a 9.10a/b split with that fact documented in the next revision.

---

## 11. Done criteria

- [ ] `crates/brain-server/src/dispatch.rs` ships the per-connection state machine + op dispatch helpers.
- [ ] `connection.rs::serve_connection` is rewritten as a read/write split, drives `ConnState`, spawns per-op sub-tasks, handles PING/BYE/CLIENT_PONG inline.
- [ ] `shard.rs::ShardRequest` gains `DispatchOp`; `ShardHandle::dispatch_op` works.
- [ ] `main.rs::linux_main::run` spawns N shards and builds a `RoutingTable`; both flow into the listener.
- [ ] 10 dispatch integration tests pass.
- [ ] Existing 9.9 connection tests still pass (the stub flow's "ERROR(BadFrame) after any frame" no longer holds — those tests need updating to drive a real handshake or to assert at the wire level only).
- [ ] `just docker-verify` green workspace-wide.
- [ ] Phase doc 9.4 (was) / 9.10 (current) marked `[x]`.

---

## 12. What 9.10 explicitly defers

- **SUBSCRIBE / SUBSCRIBE_EVENT fan-out across shards** — 9.11.
- **CANCEL_STREAM** machinery — 9.11.
- **Real streaming response (multi-frame RECALL / PLAN / REASON / ADMIN_MIGRATE_EMBEDDINGS / ADMIN_LIST_TOMBSTONED)** — 9.11 / out of phase.
- **Real auth backends** (JWT, mTLS subject extraction) — beyond Phase 9.
- **Per-IP / per-agent connection limits** — 9.13.
- **mTLS** — follow-up.
- **SIGTERM-driven drain with `ShardJoiner::join()` ordering** — 9.14.
- **Admin operations** — most are stubbed in `brain_ops::dispatch` already (`NotYetImplemented`); 9.10 surfaces those errors over the wire but doesn't implement the admin operations themselves.

---

*Implement on approval.*
