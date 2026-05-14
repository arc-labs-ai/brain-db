# Phase 11 — Milestone M3 plan

**Task:** Migrate `brain-server::admin` to `brain-http`.

**Phase doc target:**
> All existing admin integration tests pass; admin hand-roll deleted
> (~500 LOC out, ~150 LOC in for the rewiring).

**Reads:**
- All files under `crates/brain-server/src/admin/`:
  `mod.rs`, `snapshot.rs`, `rebuild.rs`, `worker.rs`, `config_route.rs`,
  `audit.rs`, `agent.rs`, `shard_route.rs`, `diagnostics.rs`.
- `crates/brain-server/tests/admin.rs` — integration tests that must
  pass unchanged.
- `crates/brain-server/src/main.rs` lines 200-235 — current admin
  wiring.
- M2 plan §3 — `Router` + `HttpServer` + `ShutdownSignal` API.

---

## 1. Scope

M3 is the payoff for M1+M2. The 500 LOC of hand-rolled HTTP parsing /
header drain / `write_response` in `admin/mod.rs` becomes ~50 LOC of
router construction. Each admin sub-module's `dispatch<W>(stream, …)`
function becomes a handler returning `Response<ResponseBody>` —
shorter and clearer than the existing write-to-stream pattern.

**Public contract preserved:**

- `crates/brain-server/src/admin/mod.rs` still exports `AdminServer`,
  `BoundAdminServer`, `AdminState`, `BuildInfo`. Same constructor
  signature. Same `bind()`/`serve()`/`local_addr()` methods.
- Every existing admin route returns the same status code and body
  bytes it did before.
- All 5 integration tests in `crates/brain-server/tests/admin.rs` and
  the e2e tests in `crates/brain-server/tests/cli_e2e.rs` pass
  unchanged.

**Things that change internally:**

- The hand-rolled request-line parser, header drain, and
  `write_response`/`write_not_implemented` helpers in `admin/mod.rs`
  are deleted.
- Each `admin/<family>.rs` sub-module's `dispatch<W>(stream, method,
  path, query, state)` becomes `handle(req: Request<Incoming>, state:
  Arc<AdminState>) -> Result<Response<ResponseBody>>`. Same internal
  logic; different signature.
- Routes are registered in a new `admin/router.rs` that builds a
  `brain_http::router::Router<Incoming>`.
- The accept loop is `brain_http::server::HttpServer`.

**Out of scope:**

- New routes. M3 only migrates existing ones.
- New limits, timeouts, or auth. M3 preserves existing semantics
  byte-for-byte.
- Per-request span instrumentation beyond what `brain-http::observability`
  already wires (full OTel attribute set is Phase 12).

---

## 2. Migration strategy

### 2.1 The shutdown bridge

The existing `AdminServer::new(addr, state, shutdown_signal)` takes
the `brain-server::network::connection::ShutdownSignal` type — a
project-internal signal already wired through `main.rs`.

`brain_http::server` has its own `ShutdownSignal`. We bridge:

```rust
// inside AdminServer::serve
let (http_handle, http_signal) = brain_http::server::channel();
let server_shutdown = self.shutdown.clone();   // brain-server signal

// Bridge task: when brain-server signal fires, fire brain-http's.
tokio::spawn(async move {
    server_shutdown.recv().await;
    http_handle.shutdown();
});

// Build and run brain-http server with `http_signal`.
```

Net: one extra spawn per AdminServer; the bridge task exits when the
brain-server signal fires.

### 2.2 State injection

The brain-http router takes plain closures. We inject `Arc<AdminState>`
by capturing it in each handler closure:

```rust
fn build_admin_router(state: Arc<AdminState>) -> Router<Incoming> {
    let s = state.clone();
    let r = Router::new()
        .get("/healthz", move |req| healthz(req))
        .get("/metrics", {
            let s = s.clone();
            move |req| metrics::handle(req, s.clone())
        })
        .get("/v1/workers", {
            let s = s.clone();
            move |req| worker::list(req, s.clone())
        })
        // ... etc.
    r
}
```

The captures look noisy but they're the cost of state injection in a
typed-Router system without extractors. Brain has 15 routes — we'll
write the function once and never touch it again. axum's typed
extractors solve this elegantly but at the cost we already rejected
(80 transitive deps).

### 2.3 Per-handler signature change

Every `dispatch<W>(stream, method, path, query, state) -> Option<io::Result<()>>`
becomes either:

```rust
// For exact routes (one method, one path):
pub async fn handle(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>>
```

or, for prefix routes that dispatch internally on path tail or method:

```rust
// For prefix routes that handle multiple methods or sub-paths:
pub async fn handle_prefix(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>>
```

The handler reads method/path/query from `req` and returns a typed
`Response` rather than writing bytes to a stream.

### 2.4 Helpers we keep

Two of the existing helpers are still useful and move into `admin/util.rs`:

- `write_not_implemented(deferred_to, detail)` — returns the 501 with
  `{"error":"not_implemented","deferred_to":"...","detail":"..."}`. Now
  returns `Response<ResponseBody>` instead of taking a stream.
- A small `json_response(status, body)` helper that wraps the
  `Response::builder()` boilerplate for JSON responses.

The custom request-line parser, header drain, and `write_response`
function — all gone.

---

## 3. Per-file migration

### `admin/mod.rs`

**Before (~500 LOC):**
- AdminState, BuildInfo, AdminServer, BoundAdminServer (KEEP).
- `serve_request` — hand-rolled HTTP/1.1 parser (~80 LOC) — DELETE.
- `parse_request_line`, `split_path_query`, `read_line` (~40 LOC) — DELETE.
- `format_metrics` (~110 LOC) — MOVE to `admin/metrics.rs` (new), keeps
  same logic, just becomes a handler.
- `write_response`, `write_not_implemented` (~40 LOC) — DELETE (replaced
  by `Response::builder()` + new `admin/util.rs::not_implemented`).

**After (~150 LOC):**
- AdminState, BuildInfo, AdminServer, BoundAdminServer with thin
  wrappers around brain-http types.
- `AdminServer::bind()` builds the router + binds the listener.
- `AdminServer::serve()` runs brain-http's accept loop + the
  shutdown bridge.

### `admin/router.rs` (NEW, ~80 LOC)

Builds the `Router<Incoming>` with all routes. One function:
`build(state: Arc<AdminState>) -> Router<Incoming>`.

### `admin/metrics.rs` (NEW, ~120 LOC)

Move the `format_metrics` body out of `admin/mod.rs` into a dedicated
file. Becomes:
```rust
pub async fn handle(_req: Request<Incoming>, state: Arc<AdminState>)
    -> brain_http::Result<Response<ResponseBody>>
```

### `admin/util.rs` (NEW, ~40 LOC)

```rust
pub fn json_response(status: StatusCode, body: String)
    -> Response<ResponseBody>;
pub fn text_response(status: StatusCode, body: &str)
    -> Response<ResponseBody>;
pub fn not_implemented(deferred_to: &str, detail: &str)
    -> Response<ResponseBody>;
```

### `admin/snapshot.rs`

- Was: `dispatch<W>(stream, method, path, query, state) -> Option<io::Result<()>>`.
- Now: split into 3 handlers (or one prefix-handler that internally
  dispatches on method + path tail).
- Recommendation: one prefix-handler for `/v1/snapshots*`. Internal
  match on (method, path):
  - POST `/v1/snapshots` → create
  - GET `/v1/snapshots` → list
  - DELETE `/v1/snapshots/{id}` → delete (path tail parses id)

### `admin/rebuild.rs`

- One handler: POST `/v1/rebuild-ann`.

### `admin/worker.rs`

- Two handlers:
  - GET `/v1/workers` (exact)
  - POST prefix `/v1/workers/` (parses name/action from path tail,
    returns 501 from `util::not_implemented`)

### `admin/config_route.rs`

- Three handlers:
  - GET `/v1/config` (exact, reads `?key=...` for sub-tree)
  - POST `/v1/config/reload` (exact, 501)
  - POST `/v1/config` (exact, 501)

### `admin/audit.rs`

- Two handlers:
  - GET `/v1/audit` (exact, 501)
  - GET `/v1/audit/export` (exact, 501)

### `admin/agent.rs`

- Two handlers:
  - GET `/v1/agents` (exact, 501)
  - prefix `/v1/agents/` (GET → 501, DELETE → 501; internal dispatch)

### `admin/shard_route.rs`

- Three handlers:
  - GET `/v1/shards` (exact)
  - POST `/v1/shards` (exact, 501)
  - DELETE prefix `/v1/shards/` (501)

### `admin/diagnostics.rs`

- Two handlers:
  - POST `/v1/diagnostics/profile` (exact, 501)
  - GET `/v1/diagnostics/debug-snapshot` (exact)

---

## 4. Router definition (preview)

```rust
// admin/router.rs
use std::sync::Arc;
use brain_http::router::Router;
use hyper::body::Incoming;
use http::Method;

use crate::admin::{
    agent, audit, config_route, diagnostics, metrics, rebuild,
    shard_route, snapshot, worker, AdminState,
};

pub fn build(state: Arc<AdminState>) -> Router<Incoming> {
    let r = Router::new();

    // /healthz — string OK, no state.
    let r = r.get("/healthz", |_| async move {
        Ok(brain_http::body::full(bytes::Bytes::from_static(b"ok\n")))
            .map(|body| http::Response::builder()
                .status(200)
                .header("content-type", "text/plain; charset=utf-8")
                .body(body).unwrap())
    });

    // /metrics — Prometheus text exposition.
    let r = with_state(r, Method::GET, "/metrics", state.clone(), metrics::handle);

    // Snapshot family — prefix dispatch handles POST /, GET /, DELETE /{id}.
    let r = with_state_prefix(r, Method::POST, "/v1/snapshots",
        state.clone(), snapshot::handle);
    let r = with_state_prefix(r, Method::GET, "/v1/snapshots",
        state.clone(), snapshot::handle);
    let r = with_state_prefix(r, Method::DELETE, "/v1/snapshots/",
        state.clone(), snapshot::handle);

    // ... etc, one entry per route ...

    r
}

fn with_state<F, Fut>(
    r: Router<Incoming>,
    method: Method,
    path: &'static str,
    state: Arc<AdminState>,
    handler: F,
) -> Router<Incoming>
where
    F: Fn(http::Request<Incoming>, Arc<AdminState>) -> Fut + Send + Sync + Copy + 'static,
    Fut: std::future::Future<Output = brain_http::Result<http::Response<brain_http::body::ResponseBody>>> + Send + 'static,
{
    r.route(method, path, move |req| handler(req, state.clone()))
}

// ... + with_state_prefix variant
```

The `with_state` helper hides the `Arc::clone` boilerplate. The
`Copy` bound on `F` is so handler-function-pointers (not closures)
can be passed by value into the route registration. All our handlers
are `pub async fn`s — these satisfy `Copy`.

---

## 5. Tests

### Existing tests that must pass

- `crates/brain-server/tests/admin.rs` (6 tests):
  - `healthz_returns_ok`
  - `metrics_emits_build_info_and_up`
  - `metrics_increments_connections_total_on_accept`
  - `metrics_emits_worker_counters`
  - `bad_path_returns_400` → now returns 404 from brain-http (behaviour
    change — see §6 below).
- All admin route integration tests in
  `crates/brain-cli/tests/{worker,config,audit,agent,shard,diagnostics}.rs`
  (~30 tests across the families) — these go through brain-cli to
  the admin server, so they pass if the routes return the right
  bodies.
- `crates/brain-server/tests/cli_e2e.rs` and `sdk_e2e.rs` (11 tests).

### New regression test

Add one regression test in `crates/brain-server/tests/admin.rs`:
- `admin_listen_addr_matches_config` — bind on `127.0.0.1:0`, assert
  `local_addr()` returns a port > 0. Sanity that the brain-http path
  preserves the binding API.

### Behaviour delta to handle

The existing `serve_request` returns `400 Bad Request` for unknown
paths. brain-http's `Router` returns `404 Not Found` for unknown paths
(more correct per RFC 9110 §15.5.5 vs §15.5.1). The existing
`bad_path_returns_400` test asserts 400. **Decision:** update the test
to assert 404. Document the wire-behaviour change in the M3 commit
message.

---

## 6. Commit shape

```
feat(brain-server): migrate admin to brain-http (M3)

Replaces the 500-LOC hand-rolled HTTP/1.1 admin server with a thin
wrapper around brain-http::server::HttpServer. All routes preserved;
all existing tests pass unchanged save one.

Deleted from admin/mod.rs:
- serve_request hand-roll (~80 LOC)
- parse_request_line, split_path_query, read_line helpers (~40 LOC)
- write_response, write_not_implemented helpers (~40 LOC)
- format_metrics writeln-chain — moved to admin/metrics.rs as a
  handler returning Response<ResponseBody>

New:
- admin/router.rs — single function building the Router from
  AdminState.
- admin/util.rs — json_response / text_response / not_implemented
  helpers (Response<ResponseBody> returners).
- admin/metrics.rs — Prometheus text exposition as a handler.

Each admin sub-module's dispatch<W>(stream, method, path, query,
state) -> Option<io::Result<()>> becomes:
  handle(req: Request<Incoming>, state: Arc<AdminState>)
    -> brain_http::Result<Response<ResponseBody>>

Wire-behaviour delta: unknown paths now return 404 (brain-http's
Router behaviour, RFC 9110 §15.5.5) instead of 400. One test updated;
external scrapers and brain-cli are unaffected since they don't hit
unknown paths.

The brain-server -> brain-http shutdown signal bridge runs as a
spawned task: when the brain-server ShutdownSignal fires, it
triggers brain-http's ShutdownHandle. The brain-http accept loop
then drains in-flight connections (30 s timeout) before returning.

Net diff: ~500 LOC out, ~250 LOC in.
```

---

## 7. Done when

- [ ] `admin/mod.rs` shrunk to ~150 LOC, hand-roll deleted.
- [ ] `admin/router.rs`, `admin/util.rs`, `admin/metrics.rs` added.
- [ ] Each sub-module's `dispatch<W>` converted to `handle(req, state)`.
- [ ] `bad_path_returns_400` test renamed/updated to assert 404.
- [ ] `just docker-verify` green — all admin, cli, e2e tests pass.
- [ ] M3 commit lands with the wire-behaviour delta documented.
- [ ] Phase doc 11.M3 ticked.

---

## 8. Open questions

1. **Should `/healthz` be exact or prefix?** Existing code exact-matches
   `/healthz`. Some healthcheck systems poke `/healthz?something=x`.
   The query string is fine on exact match (Router compares path only,
   ignoring query). **Recommendation:** exact, no change.

2. **Should `metrics::handle` be an `async fn` or sync?** The original
   `format_metrics` was async because it awaited `shard.scheduler_snapshot()`.
   We preserve that. **Recommendation:** async, matches existing.

3. **Status: `405` for `bad_path` test vs `404`?** I said 404 above.
   Method-mismatched-but-path-matched is 405; path-not-matched-at-all
   is 404. The existing test hits a totally unknown path, so 404.
   **Recommendation:** 404.

4. **Should we factor the with_state helper into brain-http?** Tempting
   — it's the obvious next step for ergonomics. But it's a closure-
   capture pattern that depends on handler type. Better to leave it
   in `brain-server::admin` as application code; if a second consumer
   of brain-http needs the same pattern, then refactor. **Recommendation:**
   keep it admin-local for now.

---

## 9. Risks

- **Risk: Handler-closure type erasure errors.** Rust's closure type
  inference around `Fn + Send + Sync + 'static` plus generic over
  `Request<B>` is a common source of frustrating compile errors. M2
  already faced this. Mitigation: copy the M2 pattern (`Box<dyn Fn>`,
  explicit `Pin<Box<dyn Future + Send>>` return). Use `with_state`
  helpers in `admin/router.rs` to localize the friction.

- **Risk: Body collection in admin handlers.** Most admin handlers
  don't read a body. The few that do (none in v1 — every admin POST
  has an empty body) would use `brain_http::body::read_to_bytes(req.into_body(), MAX_BODY_BYTES)`.
  Stays consistent with brain-http patterns.

- **Risk: Test API drift.** The existing `tests/admin.rs` calls
  `start_admin_only()` and `start_admin_with_shards(n)`, which use
  `admin::AdminServer::new(...).bind()...serve()`. Preserving the
  exact API shape ensures these tests don't even need to be edited
  beyond the 400→404 update.

- **Risk: The metrics endpoint's content-type discipline.** The
  Prometheus text format requires `text/plain; version=0.0.4;
  charset=utf-8`. The existing hand-roll sets it; new
  `metrics::handle` must too. Check after migration.
