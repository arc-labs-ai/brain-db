# Sub-task 10.6 — Streaming via async iterators

**Reads:**
- `spec/13_sdk_design/05_streams.md` §1, §2, §3, §5, §10-§12.
- §4 / §13-§16 (flow-control window, keep-alives, fan-out,
  stream metrics) — deferred to later sub-tasks.

**Phase doc:** `docs/phases/phase-10-sdk-cli.md` §10.6.

**Done when:** `client.subscribe()` and the streaming RECALL /
PLAN / REASON ops expose an `impl Stream<Item = Result<T,
ClientError>>` surface alongside the 10.5 Vec-collecting form.
Back-pressure works: the SDK only reads the next frame off the
socket when the user polls `.next()`. Dropping the stream
detaches; close is best-effort.

---

## 1. What 10.6 actually delivers

10.5 shipped `Vec<T>` collection for the four streaming ops. 10.6
adds the async-iterator surface without breaking that — both
forms stay available so existing callers don't churn:

- `Stream<Item = Result<MemoryResult, ClientError>>` for RECALL
- `Stream<Item = Result<PlanStep, ClientError>>` for PLAN
- `Stream<Item = Result<InferenceStep, ClientError>>` for REASON
- `Stream<Item = Result<SubscriptionEvent, ClientError>>` for
  SUBSCRIBE

Shape per spec §13/05 §11 (Rust):

```rust
use futures::StreamExt;
let mut stream = client.recall("cue").top_k(10000).send_stream().await?;
while let Some(item) = stream.next().await {
    let result = item?;
    process(result);
}
```

**Method names:**
- Keep `.send()` (Vec form) for the simple "give me everything"
  use-case.
- Add `.send_stream()` that returns `impl Stream`.
- For SUBSCRIBE only, the natural API is streaming-first; we add
  `.send_stream()` and keep `.collect(n)` from 10.5 for tests.

**Back-pressure mechanism:** the stream owns the `PoolGuard` for
its lifetime. Reads are demand-driven — each `next()` poll reads
one frame off the socket. If the user stops polling, no reads
happen, the socket's recv buffer fills, TCP backpressure
propagates to the server.

---

## 2. Why a single `Stream` type per op (not a shared one)

Each op's frame body is different (RecallResponseFrame vs
PlanResponseFrame vs etc.). A single generic stream type would
need a Box<dyn FnMut(...)> for the per-op decoder; cleaner to
have a thin op-specific `*Stream` struct that wraps a shared
inner type.

Layout:

```
src/ops/
├── stream.rs          NEW — shared FrameStream<T> + impl Stream
├── recall.rs          + RecallBuilder::send_stream() -> RecallStream
├── plan.rs            + PlanBuilder::send_stream()   -> PlanStream
├── reason.rs          + ReasonBuilder::send_stream() -> ReasonStream
└── subscribe.rs       + SubscribeBuilder::send_stream() -> SubscribeStream
```

`FrameStream<T>` carries:
- The held `PoolGuard` (back-pressure anchor).
- A `Vec<T>` buffer of items decoded from the current frame but
  not yet yielded.
- A "stream ended" flag.
- A `read_next_frame_into_buffer` future that decodes one frame
  on each poll-and-refill.

Per-op streams are newtypes wrapping `FrameStream<Inner>` for
distinct `Item =` impls.

---

## 3. The poll loop

```
Stream::poll_next:
  if buffer is non-empty: return Poll::Ready(Some(Ok(item)))
  if ended: return Poll::Ready(None)
  poll the in-flight read-frame future:
    Pending  → Pending
    Ready(Err(e))         → Ready(Some(Err(e)))
    Ready(Ok(frame))      →
      decode body, push items into buffer
      if FLAG_EOS set:    set ended flag
      re-call poll_next to immediately yield first buffered item
```

The `read_next_frame_into_buffer` future is built by storing the
guard + a `Pin<Box<dyn Future>>` field that's recreated when the
previous one completes. Standard async-iterator pattern.

Retry is **out of scope for streams**. SUBSCRIBE in particular
has subtle resume semantics (spec §05 §8 / §07/8) — restarting
mid-stream would lose ordering or skip events. We document this:
streams don't retry; transient errors surface to the caller.

---

## 4. Tests

### 4.1 Unit (`ops/stream.rs::tests`)
- `Stream<Item = Result<…>>` shape via `futures::StreamExt`.
- Buffered items: a single frame with 3 items yields three
  `next()` returns.
- EOS: the final frame's items yield, then `None`.
- Decode-error: bad payload returns `Some(Err(Protocol))` and
  ends the stream.

### 4.2 Integration (per streaming op)
- `tests/ops_recall_stream.rs`: 3 mid-stream frames + 1 EOS,
  total 6 items expected from `send_stream().collect().await`.
- `tests/ops_subscribe_stream.rs`: 5 individual SubscribeEvent
  frames; the stream yields all 5 then is held alive until
  dropped (test drops it explicitly).
- For PLAN / REASON: skip dedicated integration tests in 10.6
  (the unit + recall integration prove the shape; per-op tests
  exist from 10.5).

### 4.3 Back-pressure smoke
- Mock that emits 10 frames as fast as possible.
- Client reads 3 frames, awaits 200 ms, reads the remaining 7.
- Assert mock observed `TcpStream::write_all` blocking somewhere
  in the middle (best-effort — depends on kernel buffer sizes).
- Pragmatic alternative: assert correctness of the result count,
  document that the underlying TCP backpressure works because
  we don't read until polled. (Skipping the kernel-buffer
  observation test in 10.6; revisit in 11.x benchmarks.)

---

## 5. Risks

| Risk | Mitigation |
| ---- | ---------- |
| Pin / `Pin<Box<dyn Future>>` machinery is fiddly | Use `futures::stream::poll_fn` or `async_stream` crate. `async_stream` is workspace-friendly; check Cargo.toml. If unavailable, hand-roll. |
| Holding a `PoolGuard` across `.await` boundaries is fine (guards are `Send` if the connection is) | Connection's `TcpStream` is `Send`. PoolGuard's lifetime is bound to `'static` if the Stream owns the guard via `Arc<Pool>` clone. Need to redesign to avoid lifetime issues — use `Pool::acquire_owned` style returning `OwnedPoolGuard`. |
| `send_stream` doesn't fit `Client::run_op`'s closure shape (closure must return a future that resolves once; streams resolve per-item) | Streams skip `run_op` entirely. Each builder's `send_stream` is a one-shot: acquire connection, send request frame, return the stream. Document the no-retry contract. |
| Dropping a stream mid-flight leaves a partially-drained pool guard | Drop hands the connection back through normal `PoolGuard::Drop`. The next request on that connection may see leftover frames from the abandoned stream — defensive: 10.6's stream sends a STREAM_CLOSE on drop. But STREAM_CLOSE isn't fully wired server-side. We accept the limitation: dropped streams leave the connection in an inconsistent state, and the SDK closes it (forced-drop) rather than returning to the pool. |
| Same-pool concurrency with non-stream ops | Streams hold a guard for their lifetime; users wanting parallel ops must let the stream finish or use a multi-connection pool. Documented in `Pool::max_connections`. |

---

## 6. PoolGuard ownership for streams

Current 10.5 `PoolGuard` is `'a`-tied to the calling op's
scope. A stream that outlives the op-method call needs an owned
guard.

**Refactor:** introduce `OwnedPoolGuard` that holds an
`Arc<Pool>` clone alongside the connection. The pool releases by
sending back through an Arc'd channel instead of borrowing. Used
only by streams; `acquire()` keeps returning the borrow-based
`PoolGuard`.

Layout adjustment:

```
src/pool/
├── mod.rs              (+ acquire_owned() -> OwnedPoolGuard)
└── guard.rs            (+ OwnedPoolGuard struct)
```

---

## 7. What 10.6 explicitly defers

- Reconnect / resume on disconnect (spec §13/05 §8) — major
  semantics work; 11.x.
- Keep-alive frames on streams (§13) — needs server-side
  emission (spec §03/06). Defer.
- Stream metrics (§15) — 10.7.
- Multi-shard fan-out (§16) — v2.
- Streaming RECALL when K is huge enough to break a single
  frame (§10) — the protocol layer already splits; the SDK's
  10.5 collect-form handles it. Streaming form benefits the
  large-K case naturally.
- `STREAM_CLOSE` on drop — server-side cleanup needs design;
  10.6 closes the underlying connection instead (less efficient
  but safe).
- `start_from` LSN — already in `SubscribeBuilder::start_lsn`
  from 10.5; we just preserve it.

---

## 8. Done criteria

- [ ] `src/ops/stream.rs` ships `FrameStream<T>` + four newtype
  wrappers.
- [ ] `Pool::acquire_owned()` + `OwnedPoolGuard`.
- [ ] `RecallBuilder::send_stream() -> RecallStream` (and the
  three other ops).
- [ ] 10.5's `.send()` / `.collect()` forms still work.
- [ ] 3+ unit tests in `ops/stream.rs::tests`.
- [ ] 2+ integration tests (`tests/ops_recall_stream.rs`,
  `tests/ops_subscribe_stream.rs`).
- [ ] All 45 pre-10.6 tests still pass.
- [ ] `just docker-verify` green.
- [ ] Sub-task 10.6 marked `[x]` in `docs/phases/phase-10-sdk-cli.md`.

---

*Implement on approval.*
