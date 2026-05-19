# Phase 11 — `brain-http` (foundation HTTP/WS/SSE layer)

> **Roadmap impact:** this is a new Phase 11. The observability /
> benchmarks / acceptance phase that was previously Phase 11 becomes
> **Phase 12**. Reason: `brain-http` is foundational — Phase 12's
> metrics, SSE log streaming, and HTTP-served dashboards all depend
> on a real HTTP layer. Building observability on the existing
> 500-LOC hand-roll and refactoring later is the wrong sequence.

## Goal

Ship a Brain-owned HTTP transport crate that replaces the two existing
hand-rolled HTTP surfaces (`brain-server::admin` and `brain-cli::http`)
and provides WebSocket + Server-Sent Events as first-class capabilities.

Built on **hyper 1.x as the wire substrate**. We own the application-
level decisions (routing, error mapping, SSE flush policy, WebSocket
close handshake); hyper owns the wire-level mechanics (HTTP/1.1
parsing, keep-alive, chunked transfer encoding, body backpressure,
optional HTTP/2 path).

The crate is **HTTP-version-neutral by construction** because hyper
is. The `Service` trait, `Body` trait, and `http::Request<B>` /
`http::Response<B>` types all live above the version. HTTP/2 is one
feature flag away when we want it.

## Why hyper, not axum, not hand-roll

Three options considered; decision is hyper-raw. Tabulated decision
matrix lives in
[`.claude/research/brain-http-design.md`](../../.claude/research/brain-http-design.md);
short summary:

- **Hand-roll** (~8-9 kLOC, 4-6 weeks): expensive ownership of the
  HTTP/1.1 parser, keep-alive state machine, chunked encoder, HTTP/2
  framing if we ever want it. We'd own a CVE surface that the hyper
  team already owns at scale.
- **axum** (~0.5-1 kLOC, 1-2 weeks): framework over hyper. Adds ~80
  transitive deps. Bundles middleware machinery we don't use (Brain
  has 15 admin routes, no middleware, no CORS, no compression). Net
  negative ergonomically for our surface size.
- **hyper-raw** (~2.5-3 kLOC, 2-3 weeks): we get the hard parts
  (parser, keep-alive, chunked, HTTP/2-ready, body backpressure)
  from hyper; we own the design decisions (routing shape, error
  mapping, SSE flush, WS close handshake). Half the dep tree of
  axum. Production users behind the wire layer: TiKV, Linkerd,
  reqwest, every Rust HTTP shop including axum itself.

## Prerequisites

- [x] Phase 10 complete (Rust SDK & CLI shipped). `brain-cli::http`
      and `brain-server::admin` are the migration targets.

## Reading list

1. [`.claude/research/brain-http-design.md`](../../.claude/research/brain-http-design.md)
   — the full design report (918 lines).
2. [hyper 1.x docs](https://hyper.rs/) — Builder, service_fn,
   server::conn::http1, body::Incoming, upgrade.
3. [`crates/brain-server/src/admin/mod.rs`](../../crates/brain-server/src/admin/mod.rs)
   — current hand-rolled HTTP/1.1 server (500 LOC, M3 migration target).
4. [`crates/brain-cli/src/http/mod.rs`](../../crates/brain-cli/src/http/mod.rs)
   — current hand-rolled blocking HTTP client (200 LOC).
5. [`crates/brain-server/src/network/connection.rs`](../../crates/brain-server/src/network/connection.rs)
   — TCP listener / TLS / shutdown pattern. Helpers re-home into
   `brain-http::tcp` during M2.
6. [`crates/brain-server/src/network/subscribe.rs`](../../crates/brain-server/src/network/subscribe.rs)
   — cross-runtime streaming pattern (template for SSE / WS).
7. [`CLAUDE.md`](../../CLAUDE.md) §6 (approved crates — hyper added
   per justification in the M1 commit), §7 (no `unsafe` outside
   `brain-storage`), §9 (no Tokio inside a shard — HTTP stays on the
   Tokio side of the Brain boundary).

## Outputs

- New crate `crates/brain-http/`.
- All HTTP runs on Tokio (Posture A from the design report — no
  Glommio entanglement). Data plane stays on its existing
  Tokio→Glommio binary channel.
- `brain-server::admin` rewired to use `brain-http::server`.
  Hand-roll deleted.
- `brain-cli::http` migration decision: most likely keep the existing
  200-LOC blocking client for now (works, no value in churn) and
  defer to a later phase if/when async client is needed. Documented
  in M5.
- WebSocket server + client (via `tokio-tungstenite`, integrated
  through hyper's `Upgrade`).
- SSE server + client (on hyper's `Body`, with explicit flush
  discipline).
- Criterion benchmarks, Tracing spans per connection lifecycle.
- Tag: `phase-11-complete`.

## Version-agnostic by construction

The five abstractions that survive into a future HTTP/2 addition
without rewriting handlers — they're not Brain-specific designs, they
come from hyper:

1. **`http::Request<Body>` / `http::Response<Body>`** — typed shapes
   from the `http` crate. HTTP/1.1 and HTTP/2 both produce them.
2. **`Body` trait** — `http_body::Body` is the version-neutral body
   abstraction. We use `http_body_util` combinators (`Empty`, `Full`,
   `BoxBody`, `StreamBody`).
3. **`Service` trait** — `tower::Service` (re-exported via `hyper`).
   `service_fn` wraps a closure.
4. **`Router`** — ours. Matches `(Method, &str)` → `Handler`. Pure
   routing; no wire-format knowledge. Built as a small match-based
   dispatcher in M2.
5. **Connection acceptance** — `hyper::server::conn::http1::Builder`
   today, swappable to `hyper-util::server::conn::auto::Builder`
   (ALPN-negotiated HTTP/1.1 or HTTP/2) when we want HTTP/2. Same
   handler code either way.

**Things that stay HTTP/1.1-specific in v1:**

- Connection acceptance uses `http1::Builder` (single-version).
- WebSocket Upgrade is an HTTP/1.1 mechanism (RFC 6455). HTTP/2's
  WebSocket equivalent (RFC 8441) has near-zero adoption; skip.

**When HTTP/2 lands (future phase):**

- Enable `hyper`'s `http2` feature flag.
- Swap `http1::Builder::serve_connection` for `auto::Builder` in
  `server/accept.rs`.
- Add ALPN to the TLS layer.
- No `Service` or handler changes.

## Crate dependencies (justifications)

Per [`CLAUDE.md`](../../CLAUDE.md) §6, each new dep justified inline
in the M1 commit.

| Dep | Why | Cost |
|---|---|---|
| `hyper` v1 | HTTP/1.1 wire codec, keep-alive state machine, chunked encoder, body backpressure, HTTP/2-ready foundation. Production-validated at Linkerd / TiKV / reqwest scale. Building this ourselves is 4-6 weeks of CVE-prone work. | ~25 kLOC, ~25 transitive deps |
| `hyper-util` v0.1 | Required helpers: `TokioIo` (Tokio↔hyper I/O bridge), `GracefulShutdown`. The hyperium team split these out of hyper 1.0 to keep the core minimal. | thin, no new transitive deps |
| `http-body-util` v0.1 | Body combinators: `Empty`, `Full`, `BoxBody`, `StreamBody`. Pairs with hyper. | thin |
| `http` v1 | Typed HTTP vocabulary (`Method`, `StatusCode`, `HeaderMap`, `Request<B>`, `Response<B>`). hyper re-exports from this. | already pulled transitively |
| `bytes` v1 | Zero-copy buffer (hyper requires it). | already in tree via tokio |
| `tokio-tungstenite` v0.21 | WebSocket framing + masking + close handshake, RFC 6455. Pairs with hyper's `Upgrade` for the handshake. Mature; the maintainer is responsive. | ~3 kLOC, no extra heavy deps |

Existing deps used: `tokio`, `tracing`, `thiserror`, `tokio-rustls`
(behind a `tls` feature flag).

**No new deps for:** routing (~100 LOC match), SSE (~150 LOC on
`hyper::body`), TCP helpers (re-home from `brain-server::network`).

## Module layout

Folder-per-concern. Every concern in its own folder; only `lib.rs` at
the root of `src/`.

```
crates/brain-http/
├── Cargo.toml
├── README.md
├── src/
│   ├── lib.rs                       # crate-level re-exports + docs
│   │
│   ├── error/                       # version-neutral
│   │   ├── mod.rs                   # Error, ErrorKind (thiserror)
│   │   └── status.rs                # StatusCode → Brain Error mapping
│   │
│   ├── body/                        # version-neutral helpers
│   │   ├── mod.rs                   # re-exports from http_body_util
│   │   ├── stream.rs                # StreamBody helpers
│   │   └── limits.rs                # bounded body reader
│   │
│   ├── service/                     # version-neutral
│   │   ├── mod.rs                   # service_fn, BoxService, types
│   │   └── handler.rs               # AsyncHandler trait + adapters
│   │
│   ├── router/                      # version-neutral
│   │   ├── mod.rs                   # Router type
│   │   ├── route.rs                 # (Method, path, Handler) entry
│   │   └── matcher.rs               # static + parametric matching
│   │
│   ├── server/                      # Tokio + hyper
│   │   ├── mod.rs                   # HttpServer builder + serve
│   │   ├── accept.rs                # TcpListener accept + per-connection spawn
│   │   ├── connection.rs            # hyper http1::Builder wrap
│   │   ├── limits.rs                # max body / header / request timeout
│   │   ├── shutdown.rs              # graceful shutdown via hyper-util
│   │   └── tls.rs                   # rustls feature gate
│   │
│   ├── client/                      # decision in M5 — see below
│   │   └── mod.rs                   # placeholder until M5 lands
│   │
│   ├── ws/                          # WebSocket
│   │   ├── mod.rs                   # public surface
│   │   ├── upgrade.rs               # hyper Upgrade handler → tungstenite
│   │   ├── server.rs                # WsServer<F> wrapper
│   │   └── client.rs                # WsClient (M8)
│   │
│   ├── sse/                         # Server-Sent Events
│   │   ├── mod.rs                   # SseEvent struct, SseStream body
│   │   ├── encoder.rs               # event → wire bytes
│   │   ├── stream.rs                # impl Body for SseStream
│   │   └── client.rs                # EventSource w/ Last-Event-ID reconnect
│   │
│   ├── tcp/                         # Tokio TCP helpers
│   │   ├── mod.rs                   # bind helpers
│   │   ├── socket.rs                # TCP_NODELAY, SO_REUSEADDR/PORT, KEEPALIVE
│   │   └── timeout.rs               # idle-read timeout wrapper
│   │
│   └── observability/               # tracing integration
│       ├── mod.rs
│       └── span.rs                  # per-connection / per-request span helpers
└── tests/
    ├── server_smoke.rs
    ├── server_keepalive.rs
    ├── server_router.rs
    ├── server_streaming.rs
    ├── ws_handshake.rs
    ├── ws_echo.rs
    ├── ws_control_frames.rs
    ├── sse_basic.rs
    └── sse_reconnect.rs
```

## Sub-tasks (milestones)

Each milestone is shippable in isolation. Verify suite green after every
milestone. Plan-first per [`AUTONOMY.md`](../../AUTONOMY.md) §21.

### M1 — Crate skeleton + version-neutral types + dep justifications
**Reads:** hyper 1.x docs (Builder, service_fn, body::Incoming).
**Writes:** new `brain-http` crate registered in workspace. `error/`,
  `body/` (re-exports + helpers), `service/`, `observability/`.
  Workspace `Cargo.toml` updated with new deps.
**Done when:** crate compiles; integration test asserts a no-op
  service can be wired into hyper's `service_fn`; dep justifications
  in commit message.

### M2 — Server core (accept loop + Router + Connection)
**Reads:** `crates/brain-server/src/network/connection.rs` for the
  TCP setup + shutdown patterns to mirror.
**Writes:** `tcp/`, `server/accept.rs`, `server/connection.rs`,
  `router/`, `server/limits.rs`, `server/shutdown.rs`.
**Done when:** integration test issues GET/POST and round-trips
  bodies via hyper; keep-alive works automatically (free from hyper);
  graceful shutdown drains in-flight requests.

### M3 — Migrate `brain-server::admin` to `brain-http`
**Reads:** every file under `crates/brain-server/src/admin/`.
**Writes:** each admin sub-module (`worker`, `snapshot`,
  `config_route`, `audit`, `agent`, `shard_route`, `diagnostics`,
  `rebuild`) becomes a `Service` function. Delete the hand-rolled
  request parser, header drain, and `write_response` helper.
**Done when:** all existing admin integration tests pass; admin
  hand-roll deleted (~500 LOC out, ~150 LOC in for the rewiring).

### M4 — Streaming bodies + SSE
**Reads:** WHATWG `EventSource`; design report §4.4 + R3 (flush
  discipline pitfall).
**Writes:** `body/stream.rs`, `sse/` module.
**Done when:** integration test verifies SSE events arrive within
  50 ms of emit (proves flush discipline); reconnect test verifies
  `Last-Event-ID` carries through.

### M5 — HTTP client decision ✅
**Reads:** `crates/brain-cli/src/http/mod.rs`.
**Decision (shipped):** path (a) — `brain-cli::http` stays as-is
  (~200 LOC blocking GET/POST/DELETE, zero external deps,
  well-tested via the admin integration suites). brain-http does
  NOT expose a client in v1.
**Writes:** `crates/brain-http/src/client/mod.rs` — a pure-rustdoc
  module that documents the deferral, the three trigger conditions
  for revisiting (Phase 12 OTLP push / new outbound consumer /
  `reqwest` summarizer dep audit), and the natural shape when a
  client is added (`hyper_util::client::legacy::Client` over a
  feature-gated `Connect` impl, with a blocking facade wrapping a
  Tokio runtime).
**Done when:** decision documented; `brain-cli::http` unchanged;
  the `client` feature flag stays declared (it's the future
  on-switch) but compiles to a docs-only module.

### M6 — WebSocket server (Upgrade + tokio-tungstenite)
**Reads:** RFC 6455; `tokio-tungstenite` docs; hyper `Upgrade` example.
**Writes:** `ws/upgrade.rs`, `ws/server.rs`.
**Done when:** echo server integration test round-trips text + binary
  frames; close handshake test passes (both initiated-by-us and
  initiated-by-peer); ping/pong control-frame test passes.

### M7 — WebSocket client
**Writes:** `ws/client.rs` — thin wrapper around `tokio_tungstenite::
  connect_async`.
**Done when:** client ↔ brain-http server echo round-trip passes.

### M8 — Hardening, observability, benches ✅
**Reads:** [`CLAUDE.md`](../../CLAUDE.md) §14 (tracing/OTel pattern).
**Writes:** observability wiring + criterion benches + load test.
**Shipped:**
- `accept.rs` per-connection task `.instrument()`-ed with
  `connection_span(peer)`; `connection.rs::handle_request` wrapped
  in `request_span(&req)`; `http.response.status_code` recorded
  after the handler returns via new `observability::record_status`.
- Criterion benches: `benches/router.rs` (exact / prefix / 404
  fallback), `benches/sse_encoder.rs` (small / multi-line / full
  event), `benches/end_to_end.rs` (GET round-trip over loopback).
  `cargo bench -p brain-http` produces stable numbers.
- `tests/load.rs` — `#[ignore]`-d 10k-concurrent-connections /
  5-minute load test, operator-invoked
  (`cargo test ... -- --ignored load_10k --nocapture`). Linux-only;
  requires `ulimit -n` ≥20k.

**Done when:** all three workstreams above shipped; clippy `-D
warnings` green; load-test infra present (numbers in commit message
on operator run).

**Total: 8 milestones**, ~2.5-3 kLOC production + ~1.5 kLOC tests.
Realistic timeline: 2-3 weeks of focused work.

## Phase exit checklist

- [x] All 8 milestones complete.
- [x] Verify suite (fmt-check + build + clippy + test + check-skills) green.
- [x] `brain-server::admin` hand-roll deleted (M3).
- [x] Admin endpoints unchanged externally (all 47 existing tests pass).
- [x] WebSocket server (M6) + client (M7) smoke-tested round-trip.
- [x] SSE server smoke-tested with `Last-Event-ID` reconnect (M4).
- [x] Criterion baselines available via `cargo bench -p brain-http` (M8).
- [ ] Tag `phase-11-complete`.  *(awaiting user signal)*

## Notes

- **Strict ordering.** M1 → M2 → M3 is the first vertical slice; on
  shipping M3 we delete the brain-server admin hand-roll. After M3,
  Phase 12 (observability) can start in parallel with M4-M8 because
  metrics + structured logs don't need WebSocket or SSE.
- **No HTTP/2 in this phase.** hyper supports it via a feature flag;
  enabling it is a one-line change once there's a concrete client
  that needs it. Phase 11 ships with `http1` feature only.
- **brain-cli HTTP stays hand-rolled in v1.** No churn there. The
  200-LOC blocking client works for the CLI's needs (low rate, no
  concurrency, no streaming). M5 documents the decision; revisit when
  the SDK or another consumer needs async HTTP.
- **No `unsafe`.** Per [`CLAUDE.md`](../../CLAUDE.md) §7, brain-http
  is outside `brain-storage` and cannot use `unsafe`. hyper itself
  contains `unsafe` internally — that's their crate's responsibility,
  not ours. Brain's invariant is about *our* code.
- **Runtime stays Tokio.** Posture A from the design report §3.3.
  Anything that needs to cross into a shard goes through the existing
  `brain-server/network/dispatch.rs` channel pattern.
