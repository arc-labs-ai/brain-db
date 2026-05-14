# Phase 11 — Milestone M4 plan

**Task:** Streaming bodies + Server-Sent Events (server side).

**Phase doc target:**
> Integration test verifies SSE events arrive within 50 ms of emit
> (proves flush discipline); reconnect test verifies `Last-Event-ID`
> carries through.

**Reads:**
- [WHATWG HTML §9.2 (Server-Sent Events)](https://html.spec.whatwg.org/multipage/server-sent-events.html)
- [`http_body_util::StreamBody`](https://docs.rs/http-body-util/0.1/http_body_util/struct.StreamBody.html)
- [hyper 1.x body chunking](https://docs.rs/hyper/1/hyper/body/struct.Frame.html)
- `.claude/research/brain-http-design.md` §4.4 (SSE), §R3 (flush
  discipline pitfall).

---

## 1. Scope

M4 ships **server-side** SSE and the general streaming-body helper
that other handlers can return. The matching SSE client is **deferred**
to a later phase (paired with M5's HTTP-client decision) because:

- Brain has no current consumer for an SSE client. The CLI doesn't
  subscribe; the SDK uses the binary subscribe channel.
- The phase-doc "Last-Event-ID reconnect test" is satisfiable
  server-side: two sequential raw-TCP connections from the test, the
  second carrying the `Last-Event-ID` header. The server reads the
  header from the request, surfaces it to the handler, and the
  handler emits events from that id onward. Pure server-side.
- A real EventSource client (with backoff, retry semantics,
  reconnection storms) is ~500 LOC and needs an HTTP client crate;
  not worth blocking M4 on.

**In scope for M4:**

1. **General streaming body** — `body::stream(impl Stream<Item = Result<Bytes, Error>>) -> ResponseBody`.
2. **SSE encoder** — `SseEvent` struct + per-event `Bytes` encoder.
3. **SSE stream Body adapter** — wraps `impl Stream<Item = SseEvent>` as a `Body`.
4. **SSE response helper** — sets the spec'd headers and uses chunked transfer (no `Content-Length`).
5. **Per-event flush discipline** — each `SseEvent` becomes its own `Body::Frame::data`, so hyper writes it as a chunked-transfer chunk and flushes the TCP buffer. No buffering across events.
6. **`Last-Event-ID` request header surface** — handlers read it from `req.headers().get("Last-Event-ID")`. No special-case code in brain-http — the standard `http::HeaderMap` API works.
7. **Integration tests:** flush-within-50ms test, Last-Event-ID reconnect simulation.

**Out of scope:**

- SSE client (`sse/client.rs`, `sse/retry.rs`) — deferred to a follow-up.
- WebSocket — M6/M7.
- HTTP/2 streams — never in this phase (option 2 design).
- Compression of streaming bodies — not on the v1 roadmap.

---

## 2. New files

```
crates/brain-http/src/
├── body/
│   └── stream.rs                # streaming Body helper (general)
└── sse/
    ├── mod.rs                   # public surface
    ├── event.rs                 # SseEvent struct
    ├── encoder.rs               # event → wire Bytes
    └── stream.rs                # SseStream<S> Body adapter

tests/
├── streaming_smoke.rs           # streaming body round-trip
├── sse_basic.rs                 # event arrives within 50 ms
└── sse_reconnect.rs             # Last-Event-ID carries through
```

Updates:
- `crates/brain-http/src/body/mod.rs` — add `pub mod stream;` and a
  `body::stream(...)` constructor.
- `crates/brain-http/src/lib.rs` — `pub mod sse;` behind the `sse`
  feature flag.

---

## 3. Type signatures

### `body/stream.rs`

```rust
//! General streaming body helper. Adapts any
//! `Stream<Item = Result<Bytes, Error>>` into a `ResponseBody` (the
//! crate's `BoxBody<Bytes, Error>` alias).

use bytes::Bytes;
use futures_core::Stream;
use http_body::Frame;
use http_body_util::{BodyExt, StreamBody};

use crate::body::ResponseBody;

/// Wrap a `Stream` as a streaming HTTP body.
///
/// Each item the stream yields becomes one `Body::Frame::data` →
/// one chunked-transfer chunk on the wire → one TCP flush. Slow
/// consumers naturally backpressure the stream (hyper stops polling
/// `Body::poll_frame` until the previous chunk drains).
pub fn stream<S>(s: S) -> ResponseBody
where
    S: Stream<Item = Result<Bytes, crate::Error>> + Send + 'static,
{
    let framed = s.map(|res| res.map(Frame::data));
    StreamBody::new(framed).boxed()
}
```

Adds `futures-core` to the dep tree (it's transitively pulled by
`tokio` already; we'll declare it explicitly for hygiene).

### `sse/event.rs`

```rust
//! `SseEvent` — the value handlers yield from their event stream.

use std::time::Duration;

/// One SSE event. Spec: WHATWG HTML §9.2.6.
#[derive(Debug, Clone, Default)]
pub struct SseEvent {
    /// `id:` field. If `Some`, clients reflect it in the subsequent
    /// `Last-Event-ID` request header on reconnect. If `None`, no
    /// id line emitted.
    pub id: Option<String>,

    /// `event:` field — custom event name. If `None`, the default
    /// `"message"` event type is implied (no line emitted).
    pub event: Option<String>,

    /// `data:` field. Newlines split across multiple `data:` lines
    /// per the spec. May be empty (rare but valid).
    pub data: String,

    /// `retry:` field — reconnect delay in milliseconds. If `Some`,
    /// emitted as `retry: <millis>`. Clients use this as their
    /// next-reconnect timer.
    pub retry: Option<Duration>,
}

impl SseEvent {
    /// New empty event. Fluent setters via `with_*`.
    #[must_use]
    pub fn new() -> Self { Self::default() }

    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self { self.id = Some(id.into()); self }

    #[must_use]
    pub fn with_event(mut self, event: impl Into<String>) -> Self { self.event = Some(event.into()); self }

    #[must_use]
    pub fn with_data(mut self, data: impl Into<String>) -> Self { self.data = data.into(); self }

    #[must_use]
    pub fn with_retry(mut self, retry: Duration) -> Self { self.retry = Some(retry); self }
}
```

### `sse/encoder.rs`

```rust
//! Encode an `SseEvent` into wire `Bytes`.
//!
//! Wire format per WHATWG HTML §9.2.6:
//!
//! ```text
//! id: <id>\n
//! event: <event>\n
//! data: <line1>\n
//! data: <line2>\n
//! retry: <millis>\n
//! \n
//! ```
//!
//! Multi-line `data` is split into one `data:` line per source line.
//! Empty trailing newline (`\n`) terminates the event — the
//! "dispatch event" trigger on the client.

use bytes::{BufMut, BytesMut};

use crate::sse::event::SseEvent;

const NEWLINE: &[u8] = b"\n";

#[must_use]
pub fn encode(event: &SseEvent) -> bytes::Bytes {
    // Pre-size with a reasonable guess: event keys + data + dispatch newline.
    let est = event.id.as_ref().map_or(0, |s| s.len() + 5)
        + event.event.as_ref().map_or(0, |s| s.len() + 8)
        + event.data.len() + 32 // data: lines × overhead
        + 2;
    let mut buf = BytesMut::with_capacity(est);

    if let Some(id) = &event.id {
        buf.put_slice(b"id: ");
        buf.put_slice(id.as_bytes());
        buf.put_slice(NEWLINE);
    }
    if let Some(name) = &event.event {
        buf.put_slice(b"event: ");
        buf.put_slice(name.as_bytes());
        buf.put_slice(NEWLINE);
    }
    for line in event.data.split('\n') {
        buf.put_slice(b"data: ");
        buf.put_slice(line.as_bytes());
        buf.put_slice(NEWLINE);
    }
    if let Some(retry) = event.retry {
        let _ = write_int(&mut buf, b"retry: ", retry.as_millis() as u64);
    }
    // Empty line terminates the event ("dispatch event" trigger).
    buf.put_slice(NEWLINE);
    buf.freeze()
}

fn write_int(buf: &mut BytesMut, prefix: &[u8], n: u64) -> std::fmt::Result {
    use std::fmt::Write;
    buf.put_slice(prefix);
    let mut tmp = String::with_capacity(20);
    write!(&mut tmp, "{n}")?;
    buf.put_slice(tmp.as_bytes());
    buf.put_slice(NEWLINE);
    Ok(())
}
```

### `sse/stream.rs`

```rust
//! Wrap a `Stream<Item = SseEvent>` as an HTTP `Body`.
//!
//! The key correctness property: **one event ⟹ one Frame**. hyper
//! emits each frame as its own chunked-transfer chunk and flushes
//! the TCP buffer. Batching events into a single Frame defeats the
//! "near-real-time" promise of SSE.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use http_body::{Body, Frame, SizeHint};

use crate::sse::event::SseEvent;
use crate::sse::encoder::encode;

pin_project_lite::pin_project! {
    /// `Body` implementation backed by a stream of [`SseEvent`].
    pub struct SseStream<S> {
        #[pin]
        inner: S,
    }
}

impl<S> SseStream<S> {
    pub fn new(inner: S) -> Self { Self { inner } }
}

impl<S> Body for SseStream<S>
where
    S: Stream<Item = SseEvent> + Send + 'static,
{
    type Data = Bytes;
    type Error = crate::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.project();
        match this.inner.poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(ev)) => Poll::Ready(Some(Ok(Frame::data(encode(&ev))))),
        }
    }

    fn is_end_stream(&self) -> bool { false }

    fn size_hint(&self) -> SizeHint { SizeHint::default() }
}
```

Adds `pin-project-lite` to the dep tree (~150 lines, no transitive
deps; standard for hand-rolling Body impls).

### `sse/mod.rs`

```rust
//! Server-Sent Events for brain-http.
//!
//! Server-side only in M4. Client-side reconnecting EventSource
//! is a follow-up paired with M5's HTTP-client decision.

mod event;
mod encoder;
mod stream;

pub use event::SseEvent;
pub use encoder::encode;
pub use stream::SseStream;

use bytes::Bytes;
use futures_core::Stream;
use http::{Response, StatusCode};
use http_body_util::BodyExt;

use crate::body::ResponseBody;

/// Build a `Response<ResponseBody>` ready to return from a handler.
///
/// Sets the spec'd headers:
/// - `Content-Type: text/event-stream`
/// - `Cache-Control: no-cache`
/// - `X-Accel-Buffering: no` (nginx hint — prevents the reverse
///   proxy from buffering the stream).
///
/// Transfer-encoding is chunked, applied automatically by hyper
/// because the body has no `Content-Length`.
#[must_use]
pub fn response<S>(events: S) -> Response<ResponseBody>
where
    S: Stream<Item = SseEvent> + Send + 'static,
{
    let body = SseStream::new(events).boxed();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(body)
        .expect("static SSE response always builds")
}
```

---

## 4. Critical pitfalls

### 4.1 Per-event flush discipline (R3 from the design report)

The bug pattern: framework wraps a `Stream<Item = SseEvent>` and
internally batches multiple events into one `Bytes` chunk before
yielding a `Frame::data`. Result: events arrive in batches when the
batching buffer fills, not in real time.

Our `SseStream::poll_frame` yields **exactly one event per frame**.
hyper then emits each frame as its own chunked-transfer chunk and
flushes the TCP buffer. The test in §5 verifies this directly:
emit one event, assert the client sees it within 50 ms.

### 4.2 Backpressure shape

If a slow consumer doesn't read, hyper stops polling `Body::poll_frame`
until the previous chunk drains. Our `SseStream` then stops polling
the inner `Stream`. The inner `Stream`'s producer (e.g. a shard
emitting events) must be on a bounded channel so it backpressures
correctly when the SSE consumer is slow.

**This is the application's responsibility**, not brain-http's.
We document the pattern in the `sse/mod.rs` rustdoc: use
`tokio::sync::mpsc::channel(N)` (bounded) for the shard → handler
hop; map `mpsc::Receiver` to `Stream<Item = SseEvent>` via
`tokio_stream::wrappers::ReceiverStream`.

### 4.3 No `Content-Length`

SSE responses must not set `Content-Length` — the body is open-ended.
Hyper handles this automatically because our `BoxBody`'s
`size_hint()` returns the default (no upper bound). We just don't
set the header ourselves.

### 4.4 `tokio::sync::mpsc::error::SendError` ergonomics

Producers will hit channel-closed errors when the consumer
disconnects. The producer task should treat `SendError` as "consumer
went away, stop producing" rather than retry. We document this in
the rustdoc.

---

## 5. Tests

### `tests/streaming_smoke.rs`

- One test: handler returns a stream that emits 5 `Bytes::from_static`
  chunks at 10 ms intervals. Client reads with raw TCP, asserts each
  chunk is visible within 50 ms of its emit time.

### `tests/sse_basic.rs`

- `event_arrives_within_50ms` — handler emits one `SseEvent` then
  sleeps. Client reads the chunked body, parses the SSE event lines,
  asserts the event was visible within 50 ms.
- `multi_event_round_trip` — handler emits 5 events with ids 1..=5.
  Client reads all 5, parses, asserts id sequence and data integrity.
- `multi_line_data` — emit an event with `data = "line1\nline2"`,
  assert the wire format produces two `data:` lines.

### `tests/sse_reconnect.rs`

- `last_event_id_carries_through` — two sequential connections from
  the test:
  1. Connect, read events 1..3, disconnect at id 3.
  2. Reconnect with `Last-Event-ID: 3` header. The test handler
     reads the header, emits events 4..6. Test asserts the second
     connection sees ids 4, 5, 6 — not 1, 2, 3.
- This proves the server surfaces `Last-Event-ID` and the handler
  can react. The test handler does the id arithmetic; brain-http
  itself is just a transport.

---

## 6. Commit shape

```
feat(brain-http): streaming bodies + Server-Sent Events (M4)

Adds the streaming body helper and server-side SSE on top of M2's
server core. Per-event flush discipline is enforced by yielding one
Frame per SseEvent (the bug naive impls have where buffering hides
events for seconds).

New modules:
- body/stream.rs — body::stream(Stream<Item=Result<Bytes,Error>>)
  returns ResponseBody. Thin wrapper over http_body_util::StreamBody.
- sse/event.rs — SseEvent struct with id/event/data/retry fields.
- sse/encoder.rs — encode(&SseEvent) -> Bytes, WHATWG-compliant wire
  format including multi-line data splitting.
- sse/stream.rs — SseStream<S> Body adapter; one event = one frame
  = one chunked-transfer chunk = one TCP flush.
- sse/mod.rs — sse::response(events) helper sets the spec'd headers
  (Content-Type: text/event-stream, Cache-Control: no-cache,
  X-Accel-Buffering: no).

SSE client deferred. No consumer for it today (CLI doesn't
subscribe; SDK uses binary subscribe). Server-side reconnect
semantics work via the standard Request<_>::headers() API — the
handler reads Last-Event-ID itself and resumes appropriately.

Backpressure documented in sse/mod.rs rustdoc: bounded
tokio::sync::mpsc between producer (shard) and SSE handler is the
application's responsibility; brain-http propagates pause via
Body::poll_frame backpressure automatically.

New deps:
- futures-core (explicit; transitively pulled by tokio): Stream
  trait without the noise of futures-util.
- pin-project-lite (~150 LOC, no transitive deps): standard for
  hand-rolled Body impls. Alternative is unsafe Pin handling; we
  pick the audited macro.

Tests (5 integration):
- streaming_smoke: 5 chunks at 10 ms intervals, each visible within
  50 ms.
- sse_basic: event_arrives_within_50ms, multi_event_round_trip,
  multi_line_data.
- sse_reconnect: Last-Event-ID carries through across two
  sequential TCP connections.

just docker-verify green.
```

---

## 7. Done when

- [ ] `body/stream.rs` + `sse/` module compiled and lib tests pass.
- [ ] Streaming smoke test passes — 5 chunks visible inside 50 ms each.
- [ ] SSE basic tests pass — single event within 50 ms.
- [ ] SSE reconnect test passes — `Last-Event-ID` reaches the
      handler and the handler emits the right tail.
- [ ] Two new deps documented in commit message.
- [ ] `just docker-verify` green.
- [ ] Phase doc 11.M4 ticked.

---

## 8. Open questions

1. **Should `sse::response` accept a `Stream<Item = Result<SseEvent, Error>>` instead of `Stream<Item = SseEvent>`?** Error propagation on the stream is real (a producer may fail mid-stream). **Recommendation:** start with the infallible shape; add a `try_response` variant later if a real consumer needs it. Producers that need to fail can emit a final `SseEvent` with `event: "error"` and a `data` payload, then close.

2. **Default retry value in `sse::response`?** The WHATWG spec says clients default to 3 seconds if `retry:` is absent. We could emit `retry: 3000` as the first event automatically. **Recommendation:** don't. Let the handler decide; if they want a default, they emit it. Reduces magic.

3. **Should we support comments (`: keep-alive\n\n`)?** Some SSE clients (Chrome, Firefox) drop connections after ~5 min idle. Comments keep the connection warm without firing the `message` event on the client. **Recommendation:** support via `SseEvent` with a new `comment: Option<String>` field. Or skip and let handlers emit a no-op event. Going with **skip** in M4; revisit if a long-idle stream surfaces.

4. **`X-Accel-Buffering: no` — necessary?** Nginx-only hint. The Brain admin server might sit behind nginx for production. Costs nothing to emit. **Recommendation:** emit.

---

## 9. Risks

- **`pin-project-lite` adds a proc-macro to the dep graph.** ~150 LOC, no transitive deps, widely audited (used by tokio, hyper, axum, every Rust async lib). Risk is low; the alternative is hand-written `Pin` projection with `unsafe`, which violates `CLAUDE.md` §7. Going with pin-project-lite.

- **Per-event allocation.** `encode()` allocates a `BytesMut`, fills it, freezes. One alloc per event. For Brain's expected SSE rate (event streaming for logs/audit/SUBSCRIBE) this is fine. If we ever hit hot-loop SSE (>10k events/sec), we'd switch to a thread-local pool — but that's M10's bench-driven decision.

- **`Last-Event-ID` header parsing edge cases.** The WHATWG spec says the id is an arbitrary UTF-8 string (no newlines). Handlers that interpret it as an integer must defensively parse — we don't help. Documented in the `sse/mod.rs` rustdoc.

- **Multi-line `data` round-trip.** Our encoder splits on `\n`. CR (`\r`) and CRLF need to be normalized too per spec. **Mitigation:** encoder normalizes `\r\n` and `\r` to `\n` on entry. Add a test.

- **Long-idle connection drops.** No keep-alive comment in M4. Most reverse proxies idle-time out at 60 s or longer; Brain's intended SSE consumers run inside the local network. **Mitigation:** document; revisit if production hits drops.
