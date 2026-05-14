# Phase 11 — Milestone M6 plan

**Task:** WebSocket server (HTTP/1.1 Upgrade + tokio-tungstenite).

**Phase doc target:**
> Echo server integration test round-trips text + binary frames;
> close handshake test passes (both initiated-by-us and
> initiated-by-peer); ping/pong control-frame test passes.

**Reads:**
- [RFC 6455](https://datatracker.ietf.org/doc/html/rfc6455) — WebSocket protocol.
- [`tokio-tungstenite` docs](https://docs.rs/tokio-tungstenite/0.21).
- [hyper `Upgrade` API](https://docs.rs/hyper/1/hyper/upgrade/index.html).
- `.claude/research/brain-http-design.md` §4.3 (WebSocket build vs
  buy), §R2 (masking direction), §R10 (close handshake races).

---

## 1. Scope

M6 ships **server-side WebSocket** on top of:

- **Hyper's `Upgrade` mechanism** for the HTTP/1.1 → WS protocol
  switch.
- **`tokio-tungstenite`** for the actual WS frame protocol (framing,
  masking, control frames, close handshake). This was the design-
  report recommendation in §4.3 with the caveat to avoid
  `fastwebsockets` (soundness contested) and `fastwebsockets-mini`
  (incomplete).

We write the **handshake layer** (validate request headers, derive
`Sec-WebSocket-Accept`, build the `101 Switching Protocols`
response) and the **upgrade plumbing** (bridge hyper's `Upgraded`
into a `tokio-tungstenite::WebSocketStream`).

Everything below the upgrade — framing, masking, control frames,
close state machine — is `tokio-tungstenite`'s job. That's
~3 kLOC of audited, RFC-conformant code we don't write.

**In scope for M6:**

1. `Sec-WebSocket-Accept` derivation (base64(sha1(key + magic GUID))).
2. Request validation: `Upgrade: websocket`, `Connection: Upgrade`,
   `Sec-WebSocket-Version: 13`, presence of `Sec-WebSocket-Key`.
3. `101 Switching Protocols` response builder.
4. `OnUpgrade` future — wraps `hyper::upgrade::on(req)` plus the
   `tokio_tungstenite::WebSocketStream::from_raw_socket` call so
   handlers get a ready-to-use `WebSocketStream` after the spawn.
5. Public surface: one ergonomic function `ws::upgrade(req) ->
   Result<(Response, OnUpgrade)>` that handlers call.
6. Re-export `tungstenite::Message` (and `CloseCode`, `Error`) so
   handlers don't import a transitive crate directly.
7. Integration tests using `tokio-tungstenite` as the *client*
   too — echo, control frames, both close directions.

**Out of scope:**

- WebSocket client (M7).
- `permessage-deflate` compression extension (~2 kLOC, no Brain
  consumer yet).
- Subprotocol negotiation (`Sec-WebSocket-Protocol`). We can add a
  per-route subprotocol allow-list in M7 if a real handler needs it.
- WS-over-HTTP/2 (RFC 8441; near-zero adoption in practice).

---

## 2. New files

```
crates/brain-http/src/
└── ws/
    ├── mod.rs                       # public surface (upgrade, types)
    ├── accept_key.rs                # Sec-WebSocket-Accept derivation
    ├── upgrade.rs                   # request validation + 101 response
    └── server.rs                    # OnUpgrade future + WebSocketStream wrap

tests/
├── ws_handshake.rs                  # 101 + Sec-WebSocket-Accept correctness
├── ws_echo.rs                       # text + binary round-trip
├── ws_control_frames.rs             # ping/pong auto-reply, oversized control frame rejected
└── ws_close.rs                      # peer-initiated and server-initiated close
```

Updates:
- `crates/brain-http/src/lib.rs` — `pub mod ws;` behind the `ws`
  feature flag.
- `crates/brain-http/Cargo.toml` — `ws` feature picks up `sha1`,
  `base64`, `tokio-tungstenite`, plus `hyper/server` and
  `hyper-util/tokio` (the I/O bridge for the upgraded stream).
- Workspace `Cargo.toml` — `sha1` + `base64` declared if not already.

---

## 3. Deps

| Dep | Why | Cost |
|---|---|---|
| `sha1` v0.10 | `Sec-WebSocket-Accept` derivation per RFC 6455 §4.2.2. Pure Rust, no deps. | ~500 LOC, no transitive deps |
| `base64` v0.22 | Encode the SHA-1 digest. Pure Rust, no deps. | ~1 kLOC, no transitive deps |
| `tokio-tungstenite` v0.21 | Already declared at workspace level (M1 pre-declared for M6). Now actually used. | ~3 kLOC, pulls `tungstenite` |

All three justified inline in the M6 commit message per
[`AUTONOMY.md`](../../AUTONOMY.md) §2.6.

---

## 4. Type signatures

### `ws/accept_key.rs`

```rust
//! Sec-WebSocket-Accept derivation per RFC 6455 §4.2.2.

const WS_MAGIC_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Compute the `Sec-WebSocket-Accept` value the server returns in
/// the 101 response. Per RFC 6455 §4.2.2:
///
/// ```text
/// accept = base64(SHA1(key + GUID))
/// ```
pub fn derive(client_key: &str) -> String {
    use base64::Engine as _;
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(WS_MAGIC_GUID.as_bytes());
    let digest = hasher.finalize();
    base64::engine::general_purpose::STANDARD.encode(digest)
}
```

Unit test: the RFC's own example.
`client_key = "dGhlIHNhbXBsZSBub25jZQ=="` →
`accept = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="`.

### `ws/upgrade.rs`

```rust
//! WebSocket upgrade-request validation + 101 Switching Protocols
//! response.

use http::{HeaderMap, Method, Request, Response, StatusCode, Version};

use crate::body::{empty, ResponseBody};
use crate::ws::accept_key;

/// Validate a request's WebSocket upgrade headers and build the
/// matching 101 response.
///
/// Per RFC 6455 §4.2.1 the request must be `GET`, HTTP/1.1 (or
/// HTTP/2 with RFC 8441 — not supported here), include
/// `Upgrade: websocket`, `Connection: Upgrade`,
/// `Sec-WebSocket-Version: 13`, and `Sec-WebSocket-Key`.
///
/// Returns the 101 response on success. The caller is responsible
/// for both writing this response back AND consuming the upgrade
/// future via `hyper::upgrade::on(req)` to drive the WS protocol.
/// `ws::server::accept` wraps both halves.
pub fn validate_and_respond<B>(req: &Request<B>) -> crate::Result<Response<ResponseBody>> {
    if req.method() != Method::GET {
        return Err(crate::Error::Upgrade("method must be GET".into()));
    }
    if req.version() != Version::HTTP_11 {
        return Err(crate::Error::Upgrade("HTTP/1.1 required".into()));
    }
    let headers = req.headers();
    check_header(headers, "upgrade", b"websocket")?;
    check_connection_upgrade(headers)?;
    check_header(headers, "sec-websocket-version", b"13")?;
    let key = headers
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| crate::Error::Upgrade("missing sec-websocket-key".into()))?;

    let accept = accept_key::derive(key);
    let resp = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        .header("sec-websocket-accept", accept)
        .body(empty())
        .map_err(crate::Error::Http)?;
    Ok(resp)
}

fn check_header(h: &HeaderMap, name: &str, expected: &[u8]) -> crate::Result<()> {
    let actual = h
        .get(name)
        .ok_or_else(|| crate::Error::Upgrade(format!("missing {name}")))?
        .as_bytes();
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(crate::Error::Upgrade(format!("invalid {name}")))
    }
}

/// `Connection` may be a comma-separated list; accept if one of the
/// values is `upgrade` (case-insensitive).
fn check_connection_upgrade(h: &HeaderMap) -> crate::Result<()> {
    let v = h
        .get("connection")
        .ok_or_else(|| crate::Error::Upgrade("missing connection".into()))?
        .to_str()
        .map_err(|_| crate::Error::Upgrade("non-ascii connection header".into()))?;
    if v.split(',').any(|t| t.trim().eq_ignore_ascii_case("upgrade")) {
        Ok(())
    } else {
        Err(crate::Error::Upgrade("connection must include `upgrade`".into()))
    }
}
```

### `ws/server.rs`

```rust
//! Upgrade plumbing: hyper Upgraded → tokio-tungstenite WebSocketStream.

use std::future::Future;
use std::pin::Pin;

use http::{Request, Response};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio_tungstenite::{tungstenite::protocol::Role, WebSocketStream};

use crate::body::ResponseBody;
use crate::ws::upgrade;

/// Future that yields a fully-upgraded `WebSocketStream` once the
/// peer is on the WebSocket side of the protocol switch.
///
/// Cancel-safe: dropping the future cancels the upgrade. Always
/// `.await` it inside a `tokio::spawn` so the 101 response can be
/// sent first (hyper's upgrade machinery requires the response head
/// to have been written).
pub struct OnUpgrade {
    inner: hyper::upgrade::OnUpgrade,
}

impl OnUpgrade {
    /// Drive the upgrade. Returns the `WebSocketStream` ready for
    /// `next()` / `send()`.
    pub async fn await_upgrade(
        self,
    ) -> crate::Result<WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>> {
        let upgraded = self
            .inner
            .await
            .map_err(|e| crate::Error::Upgrade(format!("hyper upgrade: {e}")))?;
        let stream = WebSocketStream::from_raw_socket(
            TokioIo::new(upgraded),
            Role::Server,
            None, // default WebSocketConfig — sensible v1 limits
        )
        .await;
        Ok(stream)
    }
}

/// Accept a WebSocket upgrade request. Returns the 101 response the
/// handler should return to hyper, plus an `OnUpgrade` future the
/// handler must `tokio::spawn`-and-await to drive the WS protocol.
///
/// Typical usage:
///
/// ```ignore
/// async fn handler(req: Request<Incoming>) -> brain_http::Result<Response<ResponseBody>> {
///     let (response, on_upgrade) = brain_http::ws::accept(req)?;
///     tokio::spawn(async move {
///         match on_upgrade.await_upgrade().await {
///             Ok(mut ws) => echo_loop(ws).await,
///             Err(e) => tracing::warn!(error = %e, "ws upgrade failed"),
///         }
///     });
///     Ok(response)
/// }
/// ```
pub fn accept(
    req: Request<Incoming>,
) -> crate::Result<(Response<ResponseBody>, OnUpgrade)> {
    let response = upgrade::validate_and_respond(&req)?;
    let on_upgrade = OnUpgrade {
        inner: hyper::upgrade::on(req),
    };
    Ok((response, on_upgrade))
}
```

### `ws/mod.rs`

```rust
//! WebSocket server.
//!
//! Server-side only in M6; client lands in M7.
//!
//! HTTP/1.1 `Upgrade: websocket` handshake handled internally
//! (RFC 6455 §4.2). Once upgraded, the frame protocol is driven by
//! [`tokio-tungstenite`].
//!
//! See `accept()` for the handler ergonomics.

mod accept_key;
mod server;
mod upgrade;

pub use accept_key::derive as derive_accept_key;
pub use server::{accept, OnUpgrade};

// Re-exports so handlers don't take a direct dep on tungstenite.
pub use tokio_tungstenite::tungstenite::protocol::CloseFrame;
pub use tokio_tungstenite::tungstenite::Error as WsError;
pub use tokio_tungstenite::tungstenite::Message;
pub use tokio_tungstenite::WebSocketStream;
```

---

## 5. Cargo.toml updates

Workspace `[workspace.dependencies]`:

```toml
sha1 = "0.10"
base64 = "0.22"
```

`crates/brain-http/Cargo.toml`:

```toml
[features]
# … existing entries …
ws = ["dep:tokio-tungstenite", "dep:tokio", "dep:hyper-util", "dep:sha1", "dep:base64", "hyper/server", "hyper/http1"]

[dependencies]
# … existing entries …
sha1 = { workspace = true, optional = true }
base64 = { workspace = true, optional = true }
# tokio-tungstenite already declared as optional in M1.
```

`hyper-util` is now needed by `ws` too (for `TokioIo`). It's
already an optional dep used by the `server` feature; the `ws`
feature pulls it as well.

---

## 6. Tests

Tests run `tokio-tungstenite` as the *client* side too — the
simplest way to exercise WS interop end-to-end without writing a
hand-rolled WS client.

### `tests/ws_handshake.rs`

- `valid_request_yields_101_with_accept_header` — synthetic request
  with the RFC's example key; assert the 101 response carries the
  correct `Sec-WebSocket-Accept`.
- `wrong_method_rejected` — POST instead of GET → `Error::Upgrade`.
- `missing_key_rejected` — no `Sec-WebSocket-Key` → `Error::Upgrade`.
- `wrong_version_rejected` — `Sec-WebSocket-Version: 12`.
- `connection_with_upgrade_in_list_accepted` — `Connection: keep-alive, Upgrade`.

### `tests/ws_echo.rs`

- `text_round_trip` — server echoes a text message; client sends
  "hello", receives "hello".
- `binary_round_trip` — same with `Message::Binary`.
- `multiple_messages_in_sequence` — send 10 messages, receive 10
  echoes in order.

### `tests/ws_control_frames.rs`

- `ping_gets_pong_reply` — `tokio-tungstenite` auto-replies to ping
  with pong. Test sends ping, expects pong.
- `oversized_control_frame_rejected` — WS spec caps control frames
  at 125 bytes. `tokio-tungstenite` enforces this; verify the
  connection closes with the right error.

### `tests/ws_close.rs`

- `peer_initiated_close` — client sends Close frame; server's
  `WebSocketStream::next()` returns `None` after the protocol exchange.
- `server_initiated_close` — server calls `close()` on the stream;
  client receives a Close frame.

---

## 7. Commit shape

```
feat(brain-http): WebSocket server via hyper Upgrade + tokio-tungstenite (M6)

Adds the WebSocket server surface to brain-http. Handles the
HTTP/1.1 → WS protocol switch internally; once upgraded, the
frame protocol is driven by tokio-tungstenite (the design report's
build-vs-buy decision in §4.3).

New modules:
- ws/accept_key.rs: Sec-WebSocket-Accept derivation
  (base64(sha1(key + magic GUID))) per RFC 6455 §4.2.2.
- ws/upgrade.rs: validate request headers (Method::GET, HTTP/1.1,
  Upgrade: websocket, Connection: Upgrade, Sec-WebSocket-Version: 13,
  Sec-WebSocket-Key present); build 101 Switching Protocols response.
- ws/server.rs: `OnUpgrade` future wrapping hyper::upgrade::on +
  tokio_tungstenite::WebSocketStream::from_raw_socket. `accept(req)`
  returns (101 response, OnUpgrade) — the handler spawns and awaits
  OnUpgrade after returning the response.
- ws/mod.rs: re-exports tungstenite::{Message, Error, CloseFrame}
  and WebSocketStream so consumers don't take a transitive dep.

New deps (all behind `ws` feature):
- sha1 v0.10 (~500 LOC, no transitive deps): Sec-WebSocket-Accept
  hash.
- base64 v0.22 (~1 kLOC, no transitive deps): Sec-WebSocket-Accept
  encoding.
- tokio-tungstenite v0.21: framing, masking, control frames, close
  handshake. ~3 kLOC of audited RFC 6455 conformance we don't have
  to write.

What we own: ~250 LOC of upgrade glue.
What tokio-tungstenite owns: ~3 kLOC of frame protocol.

Out of scope: WebSocket client (M7), permessage-deflate compression
(~2 kLOC, no Brain consumer), subprotocol negotiation (add later if
a handler needs Sec-WebSocket-Protocol).

Tests (4 integration files, 11 cases): handshake correctness against
the RFC example, text+binary echo round-trip, ping/pong auto-reply,
peer- and server-initiated close handshake.

just docker-verify green.
```

---

## 8. Done when

- [ ] `ws/{accept_key,upgrade,server,mod}.rs` compile under the
      `ws` feature.
- [ ] Handshake unit tests pass (RFC example value, validation
      paths).
- [ ] Echo integration test passes for text + binary.
- [ ] Control-frame test passes (ping → pong auto-reply).
- [ ] Close handshake works in both directions.
- [ ] `sha1` + `base64` deps justified in commit message.
- [ ] `just docker-verify` green.
- [ ] Phase doc 11.M6 ticked.

---

## 9. Open questions

1. **Should `ws::accept` take `Request<Incoming>` or be generic over
   body type?** M4's body helpers are generic; M2's router is
   generic. But `hyper::upgrade::on()` requires the request to carry
   `Incoming` specifically (the upgrade mechanism is tied to hyper's
   inbound body type). **Recommendation:** specialize to `Incoming`.
   Document why in the rustdoc.

2. **Where does WebSocket fit in the Router?** Today's router takes
   `Request<Incoming>` → handler that returns `Response<ResponseBody>`.
   WS handlers fit naturally: they take the request, call
   `ws::accept`, spawn the upgrade future, return the 101. No router
   changes. **Recommendation:** no special-case route type; the
   feature is "any handler can opt into upgrading."

3. **Default `WebSocketConfig`?** tungstenite has knobs for
   `max_message_size` (default 64 MiB), `max_frame_size` (default
   16 MiB), `accept_unmasked_frames` (default false — must reject
   per RFC). The defaults match what we want. **Recommendation:**
   pass `None` to `from_raw_socket` in M6; expose a customisable
   variant in M8 if benches show value.

4. **Should `Error::Upgrade` carry an `http::StatusCode`?** Today
   it's `String`-only. The handler that propagates an upgrade error
   gets a generic 400 from the router. Some upgrade failures
   warrant 426 (Upgrade Required) per RFC. **Recommendation:** keep
   the String wrapper in M6; refine the status-code mapping in M8
   when we audit response shapes wholesale.

---

## 10. Risks

- **`hyper::upgrade::on` requires the request to *not* be consumed.**
  We pass the request to validate, then `hyper::upgrade::on(req)`
  takes it by value. `ws::accept(req)` consumes `req`, validates,
  then constructs the upgrade future. Make sure the validation path
  borrows `&req` not moves it. The plan above does this correctly
  (`upgrade::validate_and_respond(&req)`).

- **The 101 response must be sent BEFORE the upgrade future is
  awaited.** That's why `accept` returns `(Response, OnUpgrade)` as
  a pair, and the rustdoc tells the handler to `tokio::spawn` the
  upgrade future (which awaits after the 101 has been written by
  hyper) rather than awaiting inline.

- **Close handshake races (R10).** tungstenite handles the
  protocol-level close handshake. We don't add our own state
  machine on top. Tests cover both directions.

- **Mask correctness (R2).** Server-side, we MUST reject unmasked
  data frames from clients. tungstenite enforces this by default
  (`accept_unmasked_frames: false`). Verified by reading the crate
  config.

- **Doctests in the rustdoc.** `accept()`'s example uses `ignore`
  because writing a runnable doctest for an upgrade handler is
  long. Add a real integration test instead.

- **Cargo feature interaction.** `ws` pulls `hyper-util`. The
  `server` feature already pulls it. Both pulling it is fine, but
  ordering matters for `cargo build --no-default-features --features ws`
  to succeed. Verify by adding both feature-combination compile
  checks in `just docker-verify` later.
