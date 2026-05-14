# Phase 11 — Milestone M1 plan

**Task:** Crate skeleton + version-neutral types + dep justifications.

**Phase doc target:**
> Crate compiles; integration test asserts a no-op service can be
> wired into hyper's `service_fn`; dep justifications in commit
> message.

**Reads:**
- [hyper 1.x docs](https://hyper.rs/)
- [`http-body-util`](https://docs.rs/http-body-util)
- [`hyper-util`](https://docs.rs/hyper-util)
- [`.claude/research/brain-http-design.md`](../research/brain-http-design.md)
  §5 (module layout), §6 (design patterns), §8 (R9 — body size limits).

---

## 1. Scope

M1 is the smallest shippable step. Three deliverables:

1. **New crate** `crates/brain-http/` registered in workspace, with
   the new dep set landed and justified.
2. **Version-neutral types** in `error/`, `body/`, `service/`,
   `observability/` — the small surface every other milestone builds
   on.
3. **Smoke test** proving the crate integrates with hyper: a no-op
   service is wired into `hyper::service::service_fn`, accepts a
   request, returns a `Response<Empty<Bytes>>`. No actual server
   yet — that's M2.

**Explicitly out of scope for M1:**
- TCP accept loop (M2).
- Router (M2).
- Migration of `brain-server::admin` (M3).
- Streaming bodies and SSE (M4).
- WebSocket (M6-M7).

---

## 2. New files

```
crates/brain-http/
├── Cargo.toml                                # new
├── README.md                                 # one-liner + link to phase doc
└── src/
    ├── lib.rs                                # crate root, docs, re-exports
    ├── error/
    │   ├── mod.rs                            # Error + ErrorKind (thiserror)
    │   └── status.rs                         # StatusCode → Brain Error
    ├── body/
    │   ├── mod.rs                            # re-exports + body helpers
    │   └── limits.rs                         # bounded body reader for R9
    ├── service/
    │   ├── mod.rs                            # service_fn re-export + BoxService
    │   └── handler.rs                        # AsyncHandler trait
    └── observability/
        ├── mod.rs
        └── span.rs                           # tracing span constructors
└── tests/
    └── smoke.rs                              # hyper service_fn round-trip
```

`router/`, `server/`, `ws/`, `sse/`, `tcp/`, `client/` land in M2-M8.
M1 lays the foundation only.

---

## 3. Dependencies

### Workspace `Cargo.toml` additions

Per [`AUTONOMY.md`](../../AUTONOMY.md) §2.6, justifications surface
in the M1 commit message.

```toml
[workspace.dependencies]
# … existing entries …
hyper            = { version = "1.5",  default-features = false, features = ["http1", "server"] }
hyper-util       = { version = "0.1",  features = ["tokio", "server-graceful"] }
http             = "1"
http-body        = "1"
http-body-util   = "0.1"
tokio-tungstenite = { version = "0.21" }  # added in M6; declared here for review
```

### Crate `Cargo.toml`

```toml
[package]
name = "brain-http"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
description = "HTTP/1.1, WebSocket, and SSE transport for the Brain substrate."

[features]
default = ["server"]
# Module gating; full server is in M2.
server  = ["dep:tokio", "dep:hyper-util"]
client  = ["dep:tokio"]
ws      = ["dep:tokio-tungstenite"]
sse     = []
tls     = ["dep:tokio-rustls"]

[dependencies]
brain-core = { path = "../brain-core" }

# Wire stack (all version-neutral; HTTP/2 is a feature flag away)
hyper          = { workspace = true }
hyper-util     = { workspace = true, optional = true }
http           = { workspace = true }
http-body      = { workspace = true }
http-body-util = { workspace = true }
bytes          = { workspace = true }

# Runtime + observability
tokio       = { workspace = true, optional = true, features = ["net", "io-util", "time", "sync", "macros"] }
tracing     = { workspace = true }
thiserror   = { workspace = true }

# Feature-gated
tokio-tungstenite = { workspace = true, optional = true }
tokio-rustls      = { workspace = true, optional = true }

[dev-dependencies]
tokio  = { workspace = true, features = ["macros", "rt-multi-thread", "io-util", "net", "time"] }
anyhow = { workspace = true }
```

### Why these specific crates (commit message text)

- **`hyper` v1**: HTTP/1.1 wire codec + keep-alive state machine +
  chunked transfer + body backpressure. Production-validated at
  Linkerd / TiKV / reqwest scale. Building this ourselves is ~6 weeks
  of CVE-prone work. We enable only `http1` + `server`; HTTP/2 is one
  feature flag away when a client needs it.
- **`hyper-util` v0.1**: the helper crate hyperium split out of hyper
  1.0. We use `TokioIo` (bridges Tokio AsyncRead/Write to hyper's
  `Read`/`Write`) and `server::graceful::GracefulShutdown`. Without
  it we'd re-implement both.
- **`http-body-util` v0.1**: body combinators (`Empty`, `Full`,
  `BoxBody`, `StreamBody`). Tiny, no transitive bloat.
- **`http` v1**: typed HTTP vocabulary. Already pulled transitively
  by hyper; listing it directly is hygiene.
- **`http-body` v1**: the version-neutral body trait. Same story.
- **`tokio-tungstenite` v0.21** (declared, used in M6): RFC 6455
  WebSocket framer + masker + close handshake. Mature; pairs
  cleanly with hyper's `Upgrade`. The fastwebsockets alternative
  was rejected for soundness issues (design report §1.13).

---

## 4. Type signatures

### `lib.rs`

```rust
//! Brain HTTP/WebSocket/SSE transport.
//!
//! Built on hyper 1.x. HTTP-version-neutral by construction; HTTP/2
//! is a feature flag away.
//!
//! See `docs/phases/phase-11-brain-http.md` for the architecture.

#![forbid(unsafe_code)]

pub mod body;
pub mod error;
pub mod observability;
pub mod service;

// Re-exports that consumers use directly.
pub use error::{Error, Result};
pub use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri};
pub use service::{service_fn, AsyncHandler, BoxService};
```

### `error/mod.rs`

```rust
//! Crate-level error type. `thiserror` per CLAUDE.md §7.

use http::StatusCode;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("hyper: {0}")]
    Hyper(#[from] hyper::Error),

    #[error("http: {0}")]
    Http(#[from] http::Error),

    #[error("body too large: {actual} > {limit} bytes")]
    BodyTooLarge { actual: u64, limit: u64 },

    #[error("header too large: {actual} > {limit} bytes")]
    HeaderTooLarge { actual: usize, limit: usize },

    #[error("request timeout after {0:?}")]
    Timeout(std::time::Duration),

    #[error("connection closed")]
    ConnectionClosed,

    #[error("upgrade failed: {0}")]
    Upgrade(String),

    #[error("server error: {0}")]
    Server(StatusCode),

    #[error("client error: {0}")]
    Client(StatusCode),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// The status code a Brain-HTTP server would emit if this error
    /// reached the response builder. Useful in handler error paths.
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::BodyTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::HeaderTooLarge { .. } => StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            Self::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            Self::Server(s) | Self::Client(s) => *s,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}
```

### `body/mod.rs`

```rust
//! Body types and helpers.
//!
//! We don't define a new body trait — `http_body::Body` is the
//! version-neutral abstraction and we use it directly. This module
//! re-exports the combinators most handlers want and adds a few
//! Brain-specific helpers.

pub use http_body::{Body, Frame, SizeHint};
pub use http_body_util::{BodyExt, BoxBody, Empty, Full, StreamBody};

mod limits;
pub use limits::{Limited, LimitedBodyError, MAX_BODY_BYTES};

use bytes::Bytes;

/// A body alias that admin handlers will use for non-streaming
/// responses (JSON, plain text).
pub type StaticBody = Full<Bytes>;

/// Boxed body for handlers that may return any of: Empty, Full,
/// Stream, or an error-mapped variant.
pub type ResponseBody = BoxBody<Bytes, crate::Error>;

/// Construct an empty response body.
pub fn empty() -> ResponseBody {
    Empty::new().map_err(|never| match never {}).boxed()
}

/// Construct a response body from a `Bytes`-like value.
pub fn full(bytes: impl Into<Bytes>) -> ResponseBody {
    Full::new(bytes.into()).map_err(|never| match never {}).boxed()
}
```

### `body/limits.rs`

```rust
//! Bounded body reader. Mitigates R9 from the design report:
//! malicious client declares a huge body, we OOM trying to buffer.

/// 16 MiB default. Matches the existing admin assumption.
pub const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// Read a `Body` to completion with a byte cap.
///
/// Returns `Error::BodyTooLarge` if the body exceeds `limit`.
pub async fn read_to_bytes<B>(body: B, limit: u64) -> crate::Result<bytes::Bytes>
where
    B: http_body::Body<Data = bytes::Bytes>,
    B::Error: Into<crate::Error>,
{
    use http_body_util::BodyExt;
    let upper = body.size_hint().upper().unwrap_or(0);
    if upper > limit {
        return Err(crate::Error::BodyTooLarge { actual: upper, limit });
    }
    let collected = body
        .collect()
        .await
        .map_err(Into::into)?
        .to_bytes();
    if collected.len() as u64 > limit {
        return Err(crate::Error::BodyTooLarge {
            actual: collected.len() as u64,
            limit,
        });
    }
    Ok(collected)
}
```

### `service/mod.rs`

```rust
//! Service trait wrapping.
//!
//! We use `hyper::service::Service` directly (it's re-exported as
//! `tower::Service`). This module provides Brain-specific aliases
//! and a `BoxService` for dynamic dispatch in the router.

pub use hyper::service::{service_fn, Service};

use std::future::Future;
use std::pin::Pin;

mod handler;
pub use handler::AsyncHandler;

/// Boxed Service useful for storing heterogeneous handlers in a
/// `Router`. The router will hold a `Vec<(Method, &str, BoxService)>`
/// in M2.
pub type BoxService<Req, Res> = Box<
    dyn Service<
            Req,
            Response = Res,
            Error = crate::Error,
            Future = Pin<Box<dyn Future<Output = crate::Result<Res>> + Send>>,
        > + Send
        + Sync,
>;
```

### `service/handler.rs`

```rust
//! `AsyncHandler` — the ergonomic shape Brain handlers will use.
//!
//! Differs from `hyper::Service` only in that it's `async fn` rather
//! than `Service::call`. Adapters glue the two.

use std::future::Future;

use http::{Request, Response};
use hyper::body::Incoming;

use crate::body::ResponseBody;

/// What every Brain HTTP handler implements. The router in M2 will
/// adapt any `AsyncHandler` to `hyper::Service`.
pub trait AsyncHandler: Send + Sync + 'static {
    fn call(
        &self,
        req: Request<Incoming>,
    ) -> impl Future<Output = crate::Result<Response<ResponseBody>>> + Send;
}
```

### `observability/span.rs`

```rust
//! Per-request / per-connection span helpers.
//!
//! Used by M2 in the accept loop and by M8 in the per-request
//! middleware. Follows spec §14/03 attribute names.

use http::Request;
use tracing::Span;

pub fn request_span<B>(req: &Request<B>) -> Span {
    tracing::info_span!(
        "http.request",
        http.method   = %req.method(),
        http.path     = %req.uri().path(),
        http.version  = ?req.version(),
        net.peer.ip   = tracing::field::Empty, // populated in M2
        otel.kind     = "server",
    )
}

pub fn connection_span(peer: std::net::SocketAddr) -> Span {
    tracing::info_span!(
        "http.connection",
        net.peer.ip   = %peer.ip(),
        net.peer.port = peer.port(),
        otel.kind     = "server",
    )
}
```

---

## 5. Tests

### `tests/smoke.rs`

A single integration test that proves the foundation is sound. No
TCP, no real server — just confirm we can wrap a function in
`service_fn` and call it.

```rust
//! M1 smoke: prove that the brain-http types compose with hyper's
//! Service infrastructure. M2 wires the actual accept loop.

use brain_http::body::{empty, ResponseBody};
use brain_http::{service_fn, Error};
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use hyper::service::Service;

#[tokio::test]
async fn service_fn_round_trip() {
    let svc = service_fn(handle);
    // Construct a synthetic request with an empty Incoming body.
    // hyper doesn't expose a way to build `Incoming` directly; we
    // instead drive the service with the request shape we'll see
    // from hyper at runtime.
    let req = Request::builder()
        .uri("/healthz")
        .body(empty_incoming())
        .unwrap();
    let resp = svc.call(req).await.expect("handler");
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn handle(_req: Request<Incoming>) -> Result<Response<ResponseBody>, Error> {
    Ok(Response::builder()
        .status(StatusCode::OK)
        .body(empty())
        .unwrap())
}

/// Helper that returns a synthetic `Incoming` for tests. Actual
/// implementation depends on hyper's test API; if `Incoming` proves
/// hard to construct directly, we'll instead test against a
/// `Full<Bytes>` body and the bound will be `B: Body` not
/// `Incoming` specifically.
fn empty_incoming() -> Incoming {
    // hyper 1.x doesn't expose Incoming::empty(). Workarounds in M2
    // will use the full accept loop (real TCP, real Incoming). For
    // M1 we may need to relax `AsyncHandler` to be generic over
    // `Body` rather than fixed to `Incoming`. Decision to make
    // during implementation.
    todo!("see M1 plan §8 open question 1")
}
```

The `todo!()` above is intentional — it surfaces an open design
question (M1 §8.1) that needs resolution during implementation.

### Unit tests (colocated)

- `error::tests::status_code_mapping` — every `Error` variant maps to
  the right `StatusCode`.
- `body::tests::full_round_trip` — `full(b"hello")` collected back to
  `Bytes` equals input.
- `body::limits::tests::rejects_over_limit` — synthetic body with
  declared size > limit returns `Error::BodyTooLarge` without
  buffering.
- `body::limits::tests::accepts_under_limit` — body within limit
  reads to completion.

---

## 6. Commit shape

Per [`AUTONOMY.md`](../../AUTONOMY.md) §5:

```
feat(brain-http): crate skeleton + version-neutral types (M1)

New brain-http crate. M1 lands the foundation everything else builds
on: version-neutral error/body/service/observability types, a thin
layer over hyper 1.x's HTTP machinery.

This is the first commit of Phase 11 — see
docs/phases/phase-11-brain-http.md for the full plan.

New deps:
- hyper v1 (http1 + server features): HTTP/1.1 wire codec, keep-alive,
  chunked transfer, body backpressure. Production-validated at
  Linkerd / TiKV / reqwest scale. HTTP/2 is one feature flag away.
- hyper-util v0.1: TokioIo bridge + graceful shutdown helper. The
  hyperium team's helper split from hyper 1.0.
- http-body-util v0.1: body combinators (Empty, Full, BoxBody,
  StreamBody). Thin.
- http v1, http-body v1, bytes v1: pulled directly for hygiene (all
  transitively required by hyper anyway).

Architecture choice: option-2 from the design report — HTTP/1.1 wire
in v1, but every higher-level abstraction (Service, Body, Router,
request/response types) is HTTP-version-neutral. HTTP/2 slots in
later by enabling hyper's http2 feature; no handler code changes.

The `Service` trait we use is hyper's (= tower's). All Brain handlers
implement `AsyncHandler`, an `async fn` shape we adapt to
`hyper::Service` in the router (lands in M2).

No `unsafe` in brain-http. hyper itself contains unsafe internally;
that's their crate's responsibility, not ours.

Tests:
- service_fn round-trip smoke test.
- error → StatusCode mapping unit tests.
- body limit enforcement unit tests.
```

---

## 7. Done when

- [ ] `crates/brain-http/` compiles and is registered in workspace.
- [ ] Workspace `Cargo.toml` has the new deps with version pins.
- [ ] Smoke test passes (`service_fn` integration with our types).
- [ ] Unit tests pass.
- [ ] `just docker-verify` green.
- [ ] M1 commit lands with dep justifications.
- [ ] Phase doc 11.M1 ticked.

---

## 8. Open questions

1. **Smoke test against `hyper::body::Incoming` or generic `Body`?**
   `Incoming` is hard to construct outside an actual hyper server. If
   the smoke test requires it, we either (a) defer the smoke test to
   M2 where we have a real server, or (b) make `AsyncHandler` generic
   over `B: Body` rather than fixed to `Incoming`, and test against
   `Full<Bytes>` in M1. **Recommendation:** (b). Generic over `Body`
   is more useful anyway (lets handlers be tested with synthetic
   bodies).

2. **`error::Error::Hyper` collapsing hyper's error taxonomy.**
   hyper's `Error` has many variants. Brain's `Error::Hyper(_)`
   wraps them all into one variant. We lose granularity but gain a
   stable Brain-level error vocabulary. Alternative: pattern-match
   on hyper's error to produce specific Brain variants. **Recommendation:**
   start with the collapsed wrapper; revisit in M3 when admin error
   responses need finer mapping.

3. **`MAX_BODY_BYTES` global vs per-route.** 16 MiB default is plenty
   for admin endpoints (all are small JSON). But once we have SSE
   (long-lived bodies) and WebSocket (no Content-Length), the limit
   needs to be context-aware. **Recommendation:** keep `MAX_BODY_BYTES`
   as the default for `read_to_bytes`; SSE/WS will bypass this helper
   entirely (M4 and M6 expose their own body shapes).

---

## 9. Risks

- **hyper 1.x is still settling.** Minor versions (1.0 → 1.5) have
  shipped useful additions but no breaking API changes. We pin to
  `1.5` and re-validate on every minor bump. Mitigation: workspace
  pin + the existing `just docker-verify` runs on every commit.

- **`hyper-util`'s `TokioIo` is the bridge nobody loves.** Required
  because hyper 1.x defined its own `Read`/`Write` traits decoupled
  from Tokio. It's not unsafe, but it does mean one extra wrap per
  connection. Mitigation: this is the standard pattern in every
  hyper 1.x server (axum, etc.); we're not deviating.

- **Workspace dep version skew.** `http`, `http-body`, `bytes` are
  also transitively pulled by `tokio` / future crates. We pin
  explicitly to avoid two versions in the dep graph. Mitigation:
  `cargo tree` audit during M1 verify.

- **Cycle in feature flags.** `default = ["server"]` ⟹
  `server = ["dep:tokio", "dep:hyper-util"]`. If someone disables
  default-features they get a crate that compiles but does almost
  nothing. Mitigation: doc the feature-flag matrix in `lib.rs` and
  in `README.md`. Verify both `--no-default-features` and
  `--all-features` compile in CI.
