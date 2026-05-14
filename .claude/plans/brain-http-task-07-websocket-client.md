# Phase 11 — Milestone M7 plan

**Task:** WebSocket client.

**Phase doc target:**
> Client ↔ brain-http server echo round-trip passes.

**Reads:**
- `tokio_tungstenite::connect_async` docs.
- M6's `ws/server.rs` for the symmetric API shape.

---

## 1. Scope

M7 ships a thin client wrapper around
`tokio_tungstenite::connect_async`. The wrapper exists for three
reasons:

1. **API symmetry.** Server-side is `brain_http::ws::accept`; client-
   side should be `brain_http::ws::connect`. Same crate, same module,
   same vocabulary.
2. **Brain-flavoured error type.** `tungstenite::Error` collapses
   into our `crate::Error::Upgrade` / `crate::Error::Hyper`-shaped
   taxonomy at the boundary.
3. **A place to hang future customisations.** Custom request
   headers (auth, brand `User-Agent`), connect timeout, TLS config —
   today they're a single argument to `connect`; in the future they
   become a builder.

**In scope:**

- `ws::connect(url)` — basic `ws://` connection. Returns a
  `WebSocketStream` ready to `send`/`next`.
- `ws::ConnectBuilder` — builder with `.header(name, value)` for
  custom headers (auth bearer, agent id, etc.) and `.connect_timeout(d)`
  for a wall-clock cap on the handshake.
- Error mapping from `tungstenite::Error` to `crate::Error`.

**Out of scope:**

- `wss://` (TLS). Need `hyper-rustls` or `tokio-rustls` wired into a
  `Connect` impl. Defer until a real consumer needs it; the
  scaffolding lands when M5's client decision is revisited.
- Connection pooling. WebSocket is long-lived; pooling makes no
  sense. (Different story from HTTP client pooling.)
- Reconnect-with-backoff. That's a per-consumer policy, not a
  transport feature. The SSE plan §M4 made the same call.

---

## 2. New files

```
crates/brain-http/src/
└── ws/
    └── client.rs                    # connect(), ConnectBuilder

tests/
└── ws_client.rs                     # client ↔ server echo round-trip
```

Updates:
- `crates/brain-http/src/ws/mod.rs` — `pub use client::{connect, ConnectBuilder, Connected}`.

---

## 3. Type signatures

### `ws/client.rs`

```rust
//! WebSocket client.
//!
//! Thin wrapper around `tokio_tungstenite::connect_async`. Builder
//! shape so we can grow custom headers, connect timeout, and a TLS
//! config later without breaking call sites.

use std::time::Duration;

use http::{HeaderName, HeaderValue, Request as HttpRequest, Response as HttpResponse};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request as TungReq;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;

/// Result of a successful connect. Carries the WS stream plus the
/// raw 101 response (for inspecting server-side headers like
/// `Sec-WebSocket-Protocol` once subprotocol negotiation lands).
pub struct Connected {
    pub stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    pub response: HttpResponse<Option<Vec<u8>>>,
}

/// Connect to a WebSocket server with default settings (no custom
/// headers, no timeout).
///
/// # Errors
///
/// Returns [`crate::Error::Upgrade`] on handshake failure (bad
/// status, malformed `Sec-WebSocket-Accept`), [`crate::Error::Io`] on
/// socket-level failure, or [`crate::Error::Timeout`] if a builder-
/// configured connect timeout fires.
pub async fn connect(url: &str) -> crate::Result<Connected> {
    ConnectBuilder::new(url).connect().await
}

/// Builder for a WebSocket client connection.
pub struct ConnectBuilder<'a> {
    url: &'a str,
    headers: Vec<(HeaderName, HeaderValue)>,
    connect_timeout: Option<Duration>,
}

impl<'a> ConnectBuilder<'a> {
    /// Start a connection to `url`.
    #[must_use]
    pub fn new(url: &'a str) -> Self {
        Self {
            url,
            headers: Vec::new(),
            connect_timeout: None,
        }
    }

    /// Add a header to the HTTP/1.1 Upgrade request. Common uses:
    /// `Authorization: Bearer …`, `User-Agent: my-app/1.0`.
    ///
    /// `Host`, `Upgrade`, `Connection`, `Sec-WebSocket-*` are owned
    /// by tungstenite — overriding them is unsupported and may
    /// produce a malformed handshake.
    #[must_use]
    pub fn header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.push((name, value));
        self
    }

    /// Wall-clock cap on the connect + handshake. Defaults to no
    /// timeout.
    #[must_use]
    pub fn connect_timeout(mut self, d: Duration) -> Self {
        self.connect_timeout = Some(d);
        self
    }

    /// Drive the connection.
    ///
    /// # Errors
    ///
    /// See [`connect`].
    pub async fn connect(self) -> crate::Result<Connected> {
        let mut req: TungReq = self
            .url
            .into_client_request()
            .map_err(|e| crate::Error::Upgrade(format!("bad url: {e}")))?;
        for (name, value) in self.headers {
            req.headers_mut().append(name, value);
        }
        let fut = tokio_tungstenite::connect_async(req);
        let (stream, response) = match self.connect_timeout {
            Some(d) => tokio::time::timeout(d, fut)
                .await
                .map_err(|_| crate::Error::Timeout(d))?
                .map_err(map_ws_error)?,
            None => fut.await.map_err(map_ws_error)?,
        };
        Ok(Connected { stream, response })
    }
}

fn map_ws_error(e: tokio_tungstenite::tungstenite::Error) -> crate::Error {
    use tokio_tungstenite::tungstenite::Error as Te;
    match e {
        Te::Io(io) => crate::Error::Io(io),
        other => crate::Error::Upgrade(other.to_string()),
    }
}
```

### `ws/mod.rs` — additions

```rust
mod client;
pub use client::{connect, ConnectBuilder, Connected};
```

---

## 4. Tests

### `tests/ws_client.rs`

End-to-end echo: start a brain-http server with an echo handler,
connect via `brain_http::ws::connect`, send text + binary, receive
echoes, close cleanly.

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_server_echo_round_trip() {
    let router = Router::new().get("/ws", echo_handler);
    let server = TestServer::start(router).await;
    let url = format!("ws://{}/ws", server.addr());

    let mut conn = brain_http::ws::connect(&url).await.expect("connect");
    assert_eq!(conn.response.status(), 101);

    conn.stream.send(Message::Text("hello".into())).await.expect("send");
    let reply = conn.stream.next().await.expect("recv").expect("ok");
    assert_eq!(reply, Message::Text("hello".into()));

    conn.stream.close(None).await.expect("close");
    server.shutdown().await.expect("shutdown");
}
```

Plus a `connect_timeout_fires_on_unreachable` test that points the
client at a non-listening port and verifies the timeout error.

Plus a `custom_header_propagates` test that registers a server
handler asserting `req.headers().get("x-test-token")` matches.

---

## 5. Commit shape

```
feat(brain-http): WebSocket client (M7)

Thin wrapper around tokio_tungstenite::connect_async. ~100 LOC of
ergonomics on top of ~3 kLOC of tungstenite. API symmetry with M6's
ws::accept on the server side.

New module:
- ws/client.rs: ws::connect(url) for the default path; ws::ConnectBuilder
  for custom headers + connect timeout.
- `Connected` struct carries the WebSocketStream + raw 101 response
  for inspecting server headers (Sec-WebSocket-Protocol, etc).
- Error mapping: tungstenite::Error → crate::Error::Io for socket
  failures, ::Upgrade for handshake failures, ::Timeout for
  builder-configured timeout.

Out of scope: wss:// (TLS) — defer until a real consumer needs it;
connection pooling — WS is long-lived; reconnect-with-backoff —
that's a per-consumer policy, not a transport feature.

Tests (3 integration):
- client_server_echo_round_trip — end-to-end echo via brain-http's
  own ws::accept + ws::connect.
- connect_timeout_fires_on_unreachable — verify Timeout error.
- custom_header_propagates — Authorization-style header reaches the
  server handler.

just docker-verify green.
```

---

## 6. Done when

- [ ] `ws/client.rs` compiles under the `ws` feature.
- [ ] 3 integration tests pass.
- [ ] `just docker-verify` green.
- [ ] Phase doc 11.M7 ticked.

---

## 7. Open questions

1. **Should `connect()` take an `http::Uri` instead of `&str`?**
   `into_client_request` accepts both. **Recommendation:** `&str` for
   the basic call; the builder accepts whatever
   `IntoClientRequest`-implementing type the caller has.

2. **Auto-redirect on 3xx during handshake?** RFC 6455 doesn't speak
   to this. Some clients follow redirects on the initial GET.
   **Recommendation:** no redirect support in M7. If a server
   redirects WS upgrades, the caller can re-call `connect` with the
   new URL.

3. **Should `Connected` flatten into a tuple?** Returning a struct
   beats a 2-tuple because it gives the fields names. Same shape as
   M6's `ws::accept` returning a struct.

---

## 8. Risks

- **`MaybeTlsStream<TcpStream>` even though we don't enable TLS.**
  `tokio-tungstenite::connect_async` returns this type unconditionally
  — TLS support is gated by tungstenite's own features (`native-tls`
  / `rustls-tls-*`). We don't enable any of those in M7; the type
  alias just means "could be TLS in some configuration." Documented
  in the `Connected` rustdoc.

- **Reconnect storms.** If the application loops `while connect()`
  on failure with no backoff, it can hammer the server. M7 doesn't
  provide a `connect_with_backoff` helper because that's per-consumer
  policy. Documented in the `connect` rustdoc.

- **Header capitalisation.** Some servers care about exact header
  casing. tungstenite normalises to lowercase. If a server breaks on
  that, it's the server's bug, but worth a note.

- **Doctest gotchas.** As in M4, the `connect()` rustdoc example
  uses `ignore` because writing a runnable doctest that spins up a
  server is too much for a docstring.
