# Sub-task 10.5 — All op methods on `Client`

**Reads:**
- `spec/13_sdk_design/02_core_api.md` §3-§11 (op surfaces).
- `crates/brain-protocol/src/requests/cognitive.rs`,
  `link.rs`, `subscribe.rs`, `txn.rs` — exact request structs.
- `crates/brain-protocol/src/responses/*.rs` — exact response
  shapes.

**Phase doc:** `docs/phases/phase-10-sdk-cli.md` §10.5.

**Done when:** `Client::{encode, recall, plan, reason, forget,
link, unlink, txn_begin/commit/abort, subscribe}` exist and work
end-to-end against a mock server. Each is a builder that auto-
generates a `RequestId` (overridable), runs through `run_op` for
retries, and returns a typed result. Streaming ops collect into
`Vec<T>` for now — 10.6 refactors them into async iterators.

---

## 1. Scope decisions

This is the biggest single sub-task in Phase 10. To keep it
tractable, I split deliverables into "shipped" and "deferred to
10.6 / later":

**Shipped:**

| Op | Method | Streaming? | Notes |
|---|---|---|---|
| ENCODE | `client.encode(text)` | no | full builder |
| RECALL | `client.recall(cue)` | yes → Vec | builder + collect |
| PLAN | `client.plan()` | yes → Vec | builder + collect |
| REASON | `client.reason()` | yes → Vec | builder + collect |
| FORGET | `client.forget(id)` | no | single-id mode |
| LINK | `client.link(src, kind, tgt)` | no | with weight |
| UNLINK | `client.unlink(src, kind, tgt)` | no | |
| TXN_BEGIN | `client.txn_begin()` | no | returns `TxnId` |
| TXN_COMMIT | `client.txn_commit(id)` | no | |
| TXN_ABORT | `client.txn_abort(id)` | no | |
| SUBSCRIBE | `client.subscribe()` | yes → Vec | bounded count helper for tests; real streaming in 10.6 |

**Deferred to later sub-tasks:**

- **10.6** turns RECALL/PLAN/REASON/SUBSCRIBE results into
  `impl Stream<Item = ...>` with backpressure. 10.5 ships them
  as `Vec<...>` first.
- ENCODE_VECTOR_DIRECT — same shape as ENCODE plus a raw-vector
  trailing section. Not strictly required for the spec §13/02 §3
  surface; defer.
- FORGET batch / filter modes (§7 — `forget_batch`, `forget()
  .agent(...).max_age(...)`) — single-id is the primary path;
  batch/filter are post-Phase-10.
- Nested `txn.encode(...)` builder. 10.5 ships
  `client.encode(text).txn(id)` instead, which mirrors the wire
  shape (every request has an optional `txn_id`). The fluent
  `let txn = client.txn().begin()` sugar lands as a Phase-11
  polish.
- Per-op `retry_config(...)` override. 10.3 supports it via
  `ClientConfig.retry`; per-op overrides are nice-to-have, defer.
- Async-cancel via `tokio::select` against a user `CancellationToken`.
  Defer.
- ADMIN ops — separate `Client::admin()` namespace, defer to
  10.8+ (CLI work).

**Why this split:** the phase doc's "Done when" reads
"`client.encode(...)`, `recall(...)`, … all work" — i.e., each
method *exists and round-trips a real request*. Streaming-as-
iterator and txn-builder sugar are explicit later sub-tasks.

---

## 2. Module layout (folder-per-concern)

```
crates/brain-sdk-rust/src/
├── lib.rs                  (+ pub mod ops; + re-exports)
├── client/mod.rs           (+ op method shims that delegate to ops/)
└── ops/                    NEW
    ├── mod.rs              re-exports
    ├── common.rs           shared helpers (build_frame, send-and-read,
    │                        collect-streamed-frames, map ERROR -> ClientError)
    ├── encode.rs           EncodeBuilder + EncodeResult + send()
    ├── recall.rs           RecallBuilder + RecallResult + send() -> Vec
    ├── plan.rs             PlanBuilder + PlanResult + send() -> Vec
    ├── reason.rs           ReasonBuilder + ReasonResult + send() -> Vec
    ├── forget.rs           ForgetBuilder + ForgetResult + send()
    ├── link.rs             LinkBuilder + LinkResult + send()
    ├── unlink.rs           UnlinkBuilder + UnlinkResult + send()
    ├── txn.rs              TxnBegin/Commit/Abort builders
    └── subscribe.rs        SubscribeBuilder + collect(max_events)
```

LOC estimates: common.rs ~150, each op file 80–180, ops total
~1100 LOC. Plus integration tests ~600 LOC.

---

## 3. Builder pattern

Each op is a `Client::xxx(...) -> XxxBuilder`. The builder
carries required fields in its `new`; optional fields via
chained `with_*` setters. `send().await` mints a `RequestId`
(if not overridden), runs through `Client::run_op`, and returns
the typed result.

Example (encode):

```rust
let result = client.encode("text")
    .context(ContextId(42))
    .kind(MemoryKind::Episodic)
    .salience(0.8)
    .edges(vec![EdgeSpec::new(EdgeKind::CausedBy, target_id, 0.9)])
    .send()
    .await?;
// result.memory_id, result.edge_results, result.replayed
```

The builder owns a `&Client` so the send() can call `pool.acquire()`
and `run_op`. `Client` ergonomics: keep `Client` `Clone` (Arc
under the hood), so users hold one `Client` for the whole
process.

Typed result types live alongside the builder — `EncodeResult`,
`RecallResult` (= aggregate of frames), `PlanResult`,
`ReasonResult`, `ForgetResult`, `LinkResult`, `UnlinkResult`,
`TxnBeginResult`. These wrap the wire-domain types in
brain-protocol with friendlier field names (e.g.
`MemoryId` instead of `WireMemoryId`).

---

## 4. Send path & retry

Every op's `send()`:

1. Mint `RequestId` (once, before the retry closure).
2. Wrap with `client.run_op(|| async { ... })` so the same
   request_id is re-sent on retry.
3. Inside the closure:
   - `let mut guard = client.acquire().await?;`
   - Build the request body (use the captured request_id).
   - Allocate a new stream_id via `Connection::next_stream_id()`.
   - Build + write the request frame.
   - Read response frame(s) — single for non-streaming, loop
     until EOS for streaming.
   - Map ERROR opcode → `ClientError::Server { code, message }`.
   - Map response body → typed result.
4. Return.

The Vec-collection for streaming ops respects spec §03/03 §4
EOS — collect frames until `header.flags & FLAG_EOS != 0`.

---

## 5. Error mapping

`common.rs::map_error_frame(payload) -> ClientError`:
- Decode `ResponseBody::Error`.
- Build `ClientError::Server { code: error_code as u16, message }`.

Each op's send() returns the mapped error on `Opcode::Error`.

---

## 6. Tests

### 6.1 Mock-server upgrade

`tests/util/mock.rs` (NEW shared module — `mod common;` at the
top of each test file via `#[path]`) provides:

- `spawn_mock(addr, handler)` — fixture that accepts one
  connection, runs the HELLO/AUTH script, then hands the socket
  to `handler` for op-specific behaviour.
- `expect_request_with_opcode<R>(socket, opcode) -> R` — reads
  one frame, asserts opcode, returns the decoded body.
- `send_response(socket, opcode, body, stream_id, eos)` — writes
  one response frame.

This eliminates the per-test boilerplate from 10.1/10.2's mocks.

### 6.2 Per-op happy-path tests (`tests/ops_*.rs`)

One integration test file per op:

- `tests/ops_encode.rs` — encode round-trip; assert memory_id,
  edge_results, replayed flag wired.
- `tests/ops_recall.rs` — recall with 3 result frames + final
  EOS; assert Vec<MemoryResult> length + sort order.
- `tests/ops_plan.rs` — plan with 2 path frames + EOS.
- `tests/ops_reason.rs` — reason with supporting/contradicting
  evidence.
- `tests/ops_forget.rs` — forget round-trip.
- `tests/ops_link_unlink.rs` — link + unlink round-trips.
- `tests/ops_txn.rs` — begin → commit and begin → abort.
- `tests/ops_subscribe.rs` — collect(N) returns N events then
  stops.

### 6.3 Cross-cutting tests

- `tests/error_mapping.rs` — mock sends ERROR frame; assert
  client returns `ClientError::Server { code, message }`.
- `tests/retry_integration.rs` — mock returns Overloaded ERROR
  once, then ENCODE_RESP. Assert encode succeeds after one retry.

Total ~10 new integration test files. Each ~80-150 LOC.

---

## 7. Mock server scope creep — mitigation

The mock needs to decode each request body to assert wire-shape,
then craft the matching response. The patterns are similar
across ops, so `tests/util/mock.rs` factors them out. Each op
test only writes ~40 LOC of op-specific assertions / canned
responses.

---

## 8. Risks

| Risk | Mitigation |
| ---- | ---------- |
| Big single commit (~1700 LOC) | Acceptable — it's all mechanical wiring against well-defined wire types. No semantic novelty per op. |
| Mock server's complexity creep | Centralize in `tests/util/mock.rs`. Each op test does only request-decode + response-craft (~40 LOC). |
| Streaming-as-Vec design needs re-shape in 10.6 | Documented up front in each builder's `send()` doc-comment. The Vec form is the right shape for "give me everything"; the iterator form will be `send_stream()` and the Vec method stays as a convenience. |
| Edge between Connection's stream_id allocator and the op send | Each op send acquires a fresh stream_id from the guard's connection. Multi-op pipelining (multiple ops in flight on one connection simultaneously) is out of scope for 10.5 — each send holds the guard for the whole request/response cycle. |
| `Connection::stream_mut()` is `pub(crate)` — accessible from `crate::ops::*` because they're in the same crate | Confirmed via the layout — ops/encode.rs etc are submodules of brain-sdk-rust, so pub(crate) is visible. |
| MemoryKind / EdgeKind name collisions with brain-core | The SDK re-exports brain-core's domain types verbatim. Builder setters take `MemoryKind` (domain) and convert to `MemoryKindWire` at send time. |
| Tests touch many opcode/payload paths | Each test is self-contained; failures isolate to one op. |

---

## 9. Done criteria

- [ ] `src/ops/` folder with 9 op files + `common.rs` + `mod.rs`.
- [ ] `Client` gains 11 public methods (encode, recall, plan,
  reason, forget, link, unlink, txn_begin, txn_commit,
  txn_abort, subscribe).
- [ ] Each op's `send()` runs through `Client::run_op` so retries
  fire on retryable errors with the same RequestId.
- [ ] `tests/util/mock.rs` shared via `#[path]` from each ops
  test file.
- [ ] 10+ new integration tests pass.
- [ ] All 36 pre-10.5 tests still pass.
- [ ] `just docker-verify` green.
- [ ] Sub-task 10.5 marked `[x]` in the phase doc.

---

## 10. What 10.5 explicitly defers

- ENCODE_VECTOR_DIRECT.
- Streaming-as-iterator surface for RECALL/PLAN/REASON/SUBSCRIBE
  → 10.6.
- Fluent `let txn = client.txn().begin()` builder sugar → polish
  sub-task.
- FORGET batch / filter modes.
- Per-op retry_config overrides.
- ADMIN ops surface → 10.8+.
- Cancellation tokens.
- Idempotency-key TTL inspection on the client.
- Server-side errors with `retry_after` honored — spec §13/04 §9,
  needs server-side support.

---

*Implement on approval.*
