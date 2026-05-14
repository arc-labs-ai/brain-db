I have enough material. Now I'll produce the comprehensive report.

---

# `brain-http` — Design Report

**Author:** planner agent
**Date:** 2026-05-14
**Status:** for review
**Brain ref:** `CLAUDE.md` §6 (approved crates), §9 (anti-patterns); `AUTONOMY.md` §21 (plan-first)

## §0 — Up-front honesty

The user described this work as "small". It is not small. The surface as specified — HTTP/1.1 server + client + keep-alive, optional HTTP/2, WebSocket RFC 6455, SSE, chunked streaming, raw-TCP helpers, runtime-honest across Brain's Tokio↔Glommio split, ≥10k concurrent connections per core, sub-ms per-frame latency, zero-copy reads — is the surface that took the hyper team multiple years and roughly 35 KLOC to ship `hyper` 1.x, plus `h2`, `httparse`, and `hyper-util` next to it. Calling that "small" because the existing Brain hand-rolls amount to ~750 LOC across `crates/brain-cli/src/http/mod.rs` and `crates/brain-server/src/admin/mod.rs` understates the gap. Those 750 LOC implement the cheapest possible subset: blocking client `GET`/`POST`/`DELETE` with one buffer; async server with `Content-Length: 0..N` bodies, `Connection: close`, hand-drained headers, no chunked, no keep-alive, no streaming, no WebSocket, no SSE. They are correct for what they do; they do almost nothing of what the spec asks.

I'll surface the build vs buy decision against the real cost. The recommendation at the end is a middle path that is honest about which pieces are non-negotiable hand-roll work and which pieces are insanity to write from scratch.

I'll also be explicit about one constraint that I have not seen called out in the Brain docs yet: **hyper requires `Send` futures and types**. Glommio is `!Send`. Putting hyper inside a `LocalExecutor` requires `unsafe impl Send` workarounds (see Glommio's `examples/hyper_server.rs` — it does exactly this, with raw pointer slice construction in `poll_read`). That conflicts with both `CLAUDE.md` §6 (approved crates) and `CLAUDE.md` §7 ("No `unsafe` outside `crates/brain-storage`"). Either we (a) keep HTTP entirely on the Tokio side of the boundary — which Brain already does, the admin server lives on Tokio while shards live on Glommio — or (b) accept a vendored, audited `unsafe impl Send` in `brain-http` as an explicit, surfaced exception. Section §3 below recommends (a).

---

## §1 — Survey of the Rust HTTP / networking ecosystem

I'll cover each project the user listed plus a few peers, with version pinpoints as of May 2026. Numbers are approximate; where I couldn't get a clean figure I say so.

### 1.1 `hyper` 1.x — the foundation

- **Repo:** `github.com/hyperium/hyper`. ~14k stars (May 2026 estimate; was 14.3k in March 2025).
- **Version:** 1.6.x stable. Major-version stability commitment of "at least 3 years" per Sean Monstar's announcement.
- **What it gives:** HTTP/1.1 and HTTP/2 server + client. Sans-runtime in 1.x — `hyper::rt::{Read, Write, Sleep, Timer}` traits are its own; `hyper-util` provides Tokio adapters and a connection pool.
- **What it costs:** `hyper` itself with `http1` + `server` features pulls ~25 crates transitively (`bytes`, `http`, `http-body`, `httparse`, `tokio` (despite "runtime-agnostic", `tokio` shows up via `tokio` feature; without it, you still need an executor). With `hyper-util` it's ~40 crates. With `client-legacy` (pool) more.
- **Production users:** linkerd2-proxy, reqwest, axum, warp, every Rust HTTP framework not named actix-web.
- **Runtime dependency:** technically none in 1.x; practically `hyper-util::rt::TokioIo` to make `tokio::net::TcpStream` implement `hyper::rt::{Read,Write}`. The decision to own its own IO traits (issue #3110, landed for 1.0) was specifically to support io-uring–style completion-based IO, which is exactly the world Brain is in. **This matters for us:** in principle hyper 1.x could accept a Glommio-adapted `Read`/`Write` impl. In practice the futures returned by `hyper::server::conn::http1::Builder::serve_connection` are required to be `Send` because `hyper::body::Body` is `Send`-bounded in places. The Glommio example unsafely papers over this. There is no public, supported "non-Send hyper" mode.
- **LOC:** hyper itself ~25 KLOC; with `h2` add ~20 KLOC; with `hyper-util` add ~5 KLOC.

### 1.2 `axum`

- **Repo:** `github.com/tokio-rs/axum`. ~22k stars.
- **What it gives:** request extractors, typed handlers, `Router`, `tower` middleware, native WebSocket upgrade (`axum::extract::ws`), SSE helpers (`axum::response::sse`).
- **What it costs:** `axum` + `tower` + `tower-http` + `hyper-util` + `hyper` = ~80 crates in `target/`. Compile time on a clean tree: ~45-60 s on a modern laptop.
- **Production users:** Materialize (their HTTP adapter), SurrealDB (HTTP + WS endpoints — they migrated to axum in PR ~Jul 2023), countless smaller services.
- **Runtime dependency:** hard Tokio. Cannot run on Glommio without the same `unsafe impl Send` trick as raw hyper.
- **Trade-off note:** axum gives us SSE and WS for free — `axum::response::sse::Sse<S>` and `axum::extract::ws::WebSocketUpgrade` are mature. If we ever take the "Tokio-side ergonomic framework" path, this is the framework.

### 1.3 `actix-web`

- **Stars:** ~21k.
- **Runtime:** historically had its own `actix-rt`; since v4 (2022) it spawns N single-threaded Tokio runtimes pinned per core — interestingly close to Brain's shard model.
- **What it costs:** the heaviest of the three big frameworks; brings the actor model, codec utilities, ~120 crates with default features.
- **Soundness history:** ~2020 controversy over 100+ `unsafe` uses in `actix-web`/`actix-service`/`actix-codec`/`actix-utils`/`actix-http`. Mostly addressed since the maintainer change, but reading the audit notes is sobering. Brain's `CLAUDE.md` §7 ("No `unsafe` outside `crates/brain-storage`") would force us to vendor + audit any `actix-*` we pulled. **Recommendation: don't.**

### 1.4 `rocket`

- **Stars:** ~25k.
- **Runtime:** Tokio since 0.5.
- **Surface:** request guards, typed forms, code-gen heavy. Strong DX, not optimized for raw throughput, not a great fit for a substrate-internal layer.

### 1.5 `warp`

- **Stars:** ~9.6k.
- **Status:** maintained but slow-moving; v0.4.1 in Aug 2025. Filter combinators are an acquired taste; type errors are punishing. Built on hyper + tower.

### 1.6 `poem` / `viz` / `salvo`

- **`poem`:** ergonomic axum-alike, has middleware, OpenAPI. ~3.8k stars.
- **`viz`:** "fast, flexible, lightweight" hyper-on-tokio. ~1.3k stars.
- **`salvo`:** has HTTP/2, HTTP/3, native OpenAPI. ~3k stars.
- All Tokio-based, all on `hyper` or `hyper-util`. None bring anything Brain needs that `axum` doesn't.

### 1.7 `tide`

- async-std-based. async-std is in maintenance/effectively unmaintained per its repo status as of 2024. **Don't pick this.**

### 1.8 `may-minihttp`

- TechEmpower performer (~585k req/s on TFB hardware per the published rounds).
- Built on `may`, a stackful coroutine library. `may` uses inline asm + stack switching tricks — has well-known soundness concerns (the "stackful coroutines + LLVM" debate, see the Rust internals thread).
- Hard-incompatible with Brain's `CLAUDE.md` §7 unsafe rule.
- **Don't pick this.**

### 1.9 `monoio` + `monoio-http`

- `monoio` (ByteDance — not Cloudflare, the search result was wrong): pure io_uring/epoll/kqueue thread-per-core runtime, ~5k stars. Thread-local data, `!Send` futures.
- `monoio-http`: ByteDance's HTTP/1.1 + (in-progress) HTTP/2 on top of `monoio`. Inspired by hyper but with monoio's own IO traits.
- **ByteDance benchmark claim:** monoio at 4 cores ~2x Tokio, at 16 cores ~3x Tokio. Their gateway claims +20% on optimized NGINX. I haven't independently reproduced these; they're vendor benchmarks.
- **Relevance to Brain:** monoio is the closest pre-existing thing to "an HTTP server that runs natively in a thread-per-core, `!Send` world". We can't directly use monoio — Brain already committed to `glommio`, and they're not interchangeable (different runtimes, different IO futures). But **monoio-http's architecture is the right prior art for writing a Glommio-native HTTP server.** Specifically: own IO traits, sans-io parser, thread-local connection table, `!Send` everywhere.

### 1.10 `glommio`

- Brain's choice. ~3.7k stars. Datadog-originated.
- HTTP support in the repo: only `examples/hyper_server.rs` and `examples/hyper_client.rs`, both of which `unsafe impl Send` their way past hyper's bounds.
- No native HTTP crate. No community crate for HTTP-on-glommio that I could find. **This is the gap brain-http would fill if we go thread-per-core HTTP.**

### 1.11 `may`

- Stackful coroutines. Same soundness concerns as may-minihttp. Skip.

### 1.12 `tokio-tungstenite`

- **Stars:** ~2.0k for the wrapper; the underlying `tungstenite` is ~1.8k.
- Most-downloaded async WebSocket impl on crates.io. Maintained.
- Tokio-bound (sans the `async-tungstenite` cousin which supports multiple runtimes, async-std and smol included; `tungstenite` itself is sync).
- **What we get:** full RFC 6455, handshake, control frames, compression (permessage-deflate as a feature), masking.

### 1.13 `fastwebsockets`

- Cloudflare-originated.
- Reportedly fastest in some benchmarks (1.7-2x `tokio-tungstenite` on echo workloads pre-2025; the gap narrowed after `tokio-tungstenite` 0.26.2 in early 2025).
- Critiqued as "unsound and not thread-safe" by the `tokio-websockets` README's comparison table — claim is the `unsafe` masking SIMD and the `WebSocketWrite` send-half handling don't hold up to strict scrutiny. This claim is contested by Cloudflare; I have not personally audited the diff.
- **Brain implication:** same as actix — pulling `fastwebsockets` would mean vendoring + auditing `unsafe`. Skip unless we want that maintenance.

### 1.14 The "building blocks" — `bytes`, `http`, `httparse`, `h2`

- **`bytes`** (~1.7k stars): refcounted byte buffer. **This is the right primitive for zero-copy.** `BytesMut` for parsers, `Bytes` for owned slices that can be cheaply cloned and split.
- **`http`** (~600 stars): defines `Method`, `Uri`, `HeaderMap`, `Request<T>`, `Response<T>`. **Standard. Use it.** Even hand-rolled HTTP layers use `http` for the type vocabulary; otherwise you spend a week building `HeaderMap` and getting `Host` case-insensitivity wrong.
- **`httparse`** (~1.7k stars, v1.10.1 from March 2025): the SIMD HTTP/1.x push parser. **The gold standard for the parsing pipeline.** Used by every Rust HTTP server that doesn't write its own. Zero allocations, sans-io, ~3 kLOC. ~1.6x slower than C's `picohttpparser` per upstream benchmarks. No runtime deps.
- **`h2`** (~1.5k stars): HTTP/2 server + client. ~20 KLOC. HPACK, stream flow control, framing. Pulls Tokio.
- **`picohttpparser-sys`:** C bindings exist. Faster than `httparse` (~1.6x). Brings a C dep. Not worth it for Brain — `httparse` is fast enough and we lose the no-`unsafe`-outside-storage invariant.
- **`quinn`** (~4k stars): QUIC + HTTP/3 (via `h3`). Out of scope unless the user explicitly wants QUIC.

### 1.15 Summary table

| Crate | LOC | Deps | Tokio-bound? | Recommend? |
|---|---|---|---|---|
| `hyper` 1.x | ~25k | ~25 transitive | effectively, via `hyper-util` | conditionally — see §4 |
| `axum` | ~7k | ~80 transitive | yes | only on Tokio side, only if we want a framework |
| `actix-web` | ~30k | ~120 transitive | yes (custom rt over Tokio) | no — unsafe history |
| `rocket` | ~20k | ~70 transitive | yes | no — DX-first, not perf-first |
| `warp` | ~9k | ~50 transitive | yes | no — declining |
| `poem`/`viz`/`salvo` | varies | varies | yes | no — nothing axum doesn't have |
| `tide` | ~6k | async-std | no (async-std) | no — runtime is moribund |
| `may-minihttp` | ~1k | `may` | no (stackful) | no — soundness |
| `monoio-http` | ~10k | `monoio` | no | no — wrong runtime, but excellent reference |
| `tokio-tungstenite` | ~3k | `tungstenite` + Tokio | yes | yes — if we go axum/Tokio for WS |
| `fastwebsockets` | ~5k | Tokio | yes | no — contested soundness |
| `tungstenite` (sync core) | ~3k | none | no | maybe — for the sans-io masker + framer |
| `httparse` | ~3k | none | no | **yes — non-negotiable** |
| `bytes` | ~3k | none | no | **yes — non-negotiable** |
| `http` | ~6k | none | no | **yes — non-negotiable** |
| `h2` | ~20k | Tokio | yes | no — HTTP/2 not justified, see §4 |

Stars are May 2026 rough estimates; depend on cargo features + version pin for exact deps count.

---

## §2 — How production Rust databases / systems handle HTTP

I checked the systems the user listed. Pattern is consistent: **HTTP is a thin layer over a different core**. Nobody runs their hot data path through `axum`. Nobody hand-rolls HTTP either — they either use `hyper` directly or `axum` on top.

### 2.1 TiKV (`github.com/tikv/tikv`)

- **HTTP surface:** an internal `status_server` for `/metrics`, `/debug/pprof/*`, `/region/*` administrative endpoints. Built on `hyper` 1.x directly (no `axum`). Their `rust-prometheus` library's `example_hyper.rs` documents the pattern they use across the project: `hyper::server::conn::http1::Builder::serve_connection` per accepted stream.
- **Main RPC:** gRPC over `grpcio` (the C++ `grpc-core` bindings), not Rust-native — that's a separate data plane, untouched by the HTTP layer.
- **Takeaway:** TiKV's pattern is `hyper` for ops, custom transport for data. Brain follows the same pattern today: TCP+rkyv for data, hand-rolled HTTP for admin. Whether to upgrade the admin side to hyper or to keep hand-rolling is the §4 decision.

### 2.2 RisingWave (`github.com/risingwavelabs/risingwave`)

- **Meta service:** uses `tonic` (Tokio gRPC) for control plane, with a few HTTP endpoints on `axum`.
- **Frontend (SQL):** primarily PostgreSQL wire protocol — they wrote their own pgwire layer. HTTP is a small sliver, used for management endpoints.
- **Takeaway:** large Rust databases use a framework only where it pays for itself, never on the hot path.

### 2.3 Materialize (`github.com/MaterializeInc/materialize`)

- **HTTP adapter:** `axum`-based, providing a SQL-over-HTTP endpoint plus WebSocket SQL streaming.
- **Their main wire protocol:** pgwire, same as RisingWave.
- **Takeaway:** WebSocket is treated as a streaming-SQL transport. The HTTP framework is `axum`; the data plane is custom.

### 2.4 SurrealDB (`github.com/surrealdb/surrealdb`)

- **Migrated to `axum` in ~mid-2023** (commit `63dd1ea` "metrics HTTP Layer + move to Axum").
- **Surface:** REST API on HTTP + a WebSocket API at `/rpc` for live queries. Both behind the same `axum::Router`, sharing extractors and middleware.
- **Takeaway:** axum's combined HTTP + WS routing is genuinely nice when you want both. If brain-http ever exposes a public WS endpoint (e.g. for SUBSCRIBE streaming over WS as an alternative to the current binary TCP frame), this pattern is worth copying.

### 2.5 Sled (`github.com/spacejam/sled`)

- **HTTP:** none in-crate. The user wires whatever HTTP layer they want on top. Embedded-first design.
- **Takeaway:** for an embeddable library, no HTTP is correct. For a substrate that ships a server binary (Brain does), some HTTP is required.

### 2.6 Vector (`github.com/vectordotdev/vector`)

- **HTTP sources:** `hyper`-based, via Vector's own `HttpSource` framework abstraction.
- **HTTP sinks:** `hyper`-based, via `HttpService` — they explicitly wrote their own pool/batch/retry layer rather than using `reqwest` because they care about backpressure shape.
- **Takeaway:** when you're running an observability pipeline with thousands of HTTP I/Os, the framework's pool/timeout/retry shape becomes load-bearing. Vector built their own on hyper rather than buying `reqwest`. **For Brain, this is a future Phase-N consideration if the SDK gains HTTP transport — out of scope today.**

### 2.7 Linkerd2-proxy (`github.com/linkerd/linkerd2-proxy`)

- **Stack:** Tokio + hyper + tower. They co-developed `h2` for HTTP/2 because no production HTTP/2 implementation existed in 2017.
- **Memory at 4k RPS ingress:** 14-15 MB. Stable. Linkerd publishes no million-RPS number publicly, but operates at internet-scale across the CNCF deployment base.
- **Takeaway:** hyper + tower is the proven stack for production HTTP at high concurrency. Tower's middleware model is genuinely good. **If we go axum, we get tower.** If we go raw hyper, we get to choose whether to bring tower or not.

### 2.8 Cloudflare workerd (`github.com/cloudflare/workerd`)

- **Language:** C++ + KJ library; not directly relevant Rust prior art. Rust enters via `workerd-cxx` for narrow interop.
- **HTTP server inside workerd:** their own `kj::HttpServer` — they re-implemented HTTP rather than use any Rust crate.
- **Takeaway:** at hyperscale, even ports that started as "use the standard library" end up hand-rolled to control allocation and tail latency. Brain is not at hyperscale; this is informative, not directive.

### 2.9 Seastar / ScyllaDB (`github.com/scylladb/seastar`)

- **The prior art for thread-per-core HTTP.** Each core runs a `seastar::http::server`. Connections are accepted on whichever core the kernel routes them to (via `SO_REUSEPORT`); each connection lives entirely on that core. No cross-core data movement, no shared mutexes. Inter-core "remote calls" go through `seastar::smp::submit_to`.
- **Architecture lessons that translate directly to brain-http:**
  - `SO_REUSEPORT` to spread accept across cores. Each core has its own `accept()` loop. (This is exactly what Brain's shards could do — but Brain's accept is currently single-core Tokio; see §5.)
  - Per-connection state lives on one core. No cross-core async on the hot path.
  - HTTP parsing is sans-io: feed bytes in, get events out. Same shape as `httparse`.
  - Bodies and responses are streams of `temporary_buffer<char>` — Seastar's `Bytes` equivalent. `BytesMut` + `Bytes` map cleanly.
- **Source/docs:** `seastar/include/seastar/http/` directory.
- **Takeaway:** if we ever want native Glommio HTTP serving for the data path, Seastar's `http::server` is the design to copy. Glommio is explicitly designed in Seastar's image; the patterns transfer 1:1.

### 2.10 Common pattern across all of these

- **Admin HTTP** = `hyper` or `axum` (a small subset of routes; framework overhead is fine; correctness/maintainability dominates).
- **Hot-path data wire** = custom binary protocol. Sometimes Postgres wire (databases). Sometimes gRPC (control planes). Rarely HTTP, never on the hot path with framework overhead.
- **WebSocket** = used as a streaming alternative to a custom data protocol when a browser is the client, or as an SDK transport.

This is exactly Brain's current pattern. The question for `brain-http` is whether to (a) standardize the admin side on a framework and stop hand-rolling, and (b) introduce WS/SSE as a *new* data-path option, separately or layered.

---

## §3 — The Tokio↔Glommio boundary in practice

This is the most important constraint, and the one I'm most confident about. Let me lay it out carefully.

### 3.1 What Brain looks like today

- `crates/brain-server/src/network/connection.rs` — single Tokio multi-threaded runtime. Accepts TCP, parses Brain binary frames, hands them off to shards.
- `crates/brain-server/src/network/dispatch.rs` — the Tokio↔Glommio dispatcher. The boundary is `flume`-style bounded channels carrying plain `Send` messages (`Frame` bytes, request enums). The Tokio side `oneshot`s a response back.
- `crates/brain-server/src/shard/mod.rs` — `N` Glommio `LocalExecutor` threads, one per shard. Types inside the shard executor are `!Send`. Data structures are thread-local.
- `crates/brain-server/src/admin/mod.rs` — separate Tokio listener on a separate port (`127.0.0.1:9091` by default). Per-connection `tokio::spawn` task. Hand-rolled HTTP/1.1.

The architecture is already split. There's no HTTP running on Glommio today.

### 3.2 What hyper requires

- `hyper` 1.x defines `hyper::rt::{Read, Write, Timer, Sleep, Executor}`. These are intentionally runtime-agnostic.
- **But** the *futures* `hyper::server::conn::http1::Builder::serve_connection_with_upgrades(io, service)` produces are bounded such that, in practice, `io: Send + 'static` and `service::Service::Future: Send + 'static`. This is for the work-stealing case where the connection future might migrate between Tokio worker threads.
- On a single-threaded executor (Tokio's `current_thread` flavor or Glommio's `LocalExecutor`), `Send` is over-strong. But hyper's public API doesn't expose a `!Send` variant.
- **Result:** Glommio's `examples/hyper_server.rs` does `unsafe impl Send for HyperStream {}` on a Glommio `TcpStream` wrapper and uses a raw-pointer slice in `poll_read`. It works because the future never actually moves cross-thread — `LocalExecutor` doesn't migrate. But there is no compiler-checkable guarantee, and the soundness story is "trust me."

### 3.3 What this means for `brain-http`

Three possible postures:

**Posture A — HTTP stays on Tokio only.** `brain-http` exposes a Tokio-side HTTP server + client + WS + SSE. Anything that wants to talk to a shard goes through the existing channel boundary (`crates/brain-server/src/network/dispatch.rs`). The admin server, the metrics endpoint, and any future SDK-facing WS/SSE endpoint live in this Tokio side. **No Glommio entanglement.** This is the most honest path. Aligns with `CLAUDE.md` §9 "Don't add Tokio inside a shard" — we're not, we're keeping HTTP entirely outside shards.

**Posture B — HTTP runs natively on Glommio.** Hand-roll an HTTP server on top of Glommio's `TcpStream` / `TcpListener`. Connections live on the shard core. Saves the channel hop for data-path HTTP, at the cost of writing the HTTP server ourselves (no hyper). This is the path Seastar took. Cost: 4-6k LOC for a credible HTTP/1.1 server, more for WS/SSE. **Postpones to a later phase, if ever.**

**Posture C — Mixed: HTTP on Tokio for admin, raw TCP on Glommio for data, with an optional WS bridge on Tokio that pipes into the existing dispatcher.** Same as A in practice. WS is just an alternative framing on top of the same Tokio→Glommio channel pipeline that `dispatch.rs` already implements for binary frames.

**Recommendation: Posture A/C.** Concretely:
- All HTTP, WS, SSE in `brain-http` run on Tokio.
- The data path (binary `Frame`) stays on its current Tokio→Glommio dispatch.
- WS, when added, is a transport on the Tokio side that produces the same `Frame` enum (or a thin wrapper of it) and dispatches via the same channel.
- SSE is a Tokio-side response shape, fed by a `tokio::sync::mpsc::Receiver` from the shard side (already how `crates/brain-server/src/network/subscribe.rs` pipes SUBSCRIBE events to client connections).

This keeps the `Send` boundary explicit and well-defined. No `unsafe`. No silent cross-thread bugs.

### 3.4 What about io_uring on the HTTP path?

If we want HTTP itself to benefit from io_uring (sub-ms latency, ≥10k connections/core targets), Tokio currently doesn't use io_uring for sockets — `tokio-uring` is experimental and not API-compatible with the regular `tokio::net`. The native path would be Glommio (Posture B) or `monoio` (different runtime, not Brain-approved).

But the user's latency target — "sub-millisecond per round trip excluding network RTT" — is comfortably met by `tokio` + `mio` (epoll). The 10k-connections-per-core target is also met. epoll's per-fd overhead is fine at 10k connections; io_uring's win is at 100k+ or with high syscall volume per fd. **Brain doesn't need io_uring on the HTTP path for these targets.**

This is the kind of trade-off worth surfacing: the wins of io_uring on HTTP are real but quantitative, not qualitative, and they would cost the `Send` clarity. Pick Tokio for HTTP, keep io_uring where it actually matters: storage.

---

## §4 — Build vs buy decision matrix

For each capability, what does "buy" look like, what does "build" look like, what should we do?

### 4.1 HTTP/1.1 server

| Path | Crates | LOC we write | LOC in target/ | Pros | Cons |
|---|---|---|---|---|---|
| Buy: `axum` | axum, hyper, hyper-util, tower, tower-http | ~200 (handlers + wiring) | ~80 crates, ~25 MB target | Mature, WS+SSE bundled, tower middleware | Big dep tree, framework lock-in, harder to control allocations |
| Buy: `hyper` (raw) | hyper, hyper-util, http, bytes, http-body-util | ~600 (router, handlers) | ~25 crates | Maturity without framework overhead, control over allocations | Have to write routing, body helpers, more code than axum |
| Build: hand-roll on tokio + httparse | tokio, http, bytes, httparse | ~2.5k (server, parser glue, response builder, router, keep-alive, chunked encoder, errors) | ~10 crates (we already depend on tokio + bytes) | Zero `unsafe`, full control, fits the "minimalist" Brain ethos, can size to exactly what we need | We own correctness forever — header normalization, encoding edge cases, chunked decoder pitfalls |

**Decision: build, with `httparse` + `http` + `bytes` as building blocks.**

Justification:
- `httparse` is a sans-io parser — using it does not constrain runtime choice. It's the standard.
- `http` is the typed vocabulary used by hyper, axum, and every other framework. Using it lets us speak the same types if we ever swap. Zero runtime deps.
- `bytes` is needed for zero-copy. Already on Brain's approved list implicitly (it's pulled by `tokio` and Brain is happy to use it).
- `hyper` adds ~25 transitive crates and constrains us in the Send story for no benefit on the admin surface. It would buy us HTTP/2 if we wanted it — see §4.7.
- `axum` would be appropriate if we wanted typed extractors and tower middleware. We don't have a use case that needs them. The admin routes today are byte-shoving + JSON.

This is the same call the original hand-roll made; we're not throwing that work away. We're growing it into a real crate.

### 4.2 HTTP/1.1 client

| Path | Crates | LOC | Pros | Cons |
|---|---|---|---|---|
| Buy: `reqwest` | reqwest + hyper + ... | ~100 | Full TLS, redirects, decompression | ~50-crate dep tree for `brain-cli` |
| Buy: `ureq` | ureq (sync) | ~100 | No tokio, sync, small | Sync — CLI is fine with that |
| Buy: `hyper` (raw client) | hyper + hyper-util | ~400 | Async, pooling | Async overhead for one shot |
| Build: hand-roll | std::net, http, httparse, bytes | ~600 (client, parser glue, error mapping) | Tiny, zero deps beyond stdlib + httparse | Have to support chunked decode, keep-alive optionally |

**Decision: build, sync first.** The existing CLI hand-roll (~200 LOC) already covers the blocking GET/POST/DELETE case. Grow it to ~500-700 LOC inside `brain-http::client` to add: response chunked-decoder, header parsing through `httparse`, optional `connection: keep-alive`, and SSE client (for testing the server-side SSE).

For the SDK eventually wanting async HTTP, **leave that as a Phase-N decision.** The SDK isn't shipping in the current phase; introducing `reqwest` for a hypothetical future use is exactly the speculative dependency `AUTONOMY.md` §12 warns against.

### 4.3 WebSocket

| Path | Crates | LOC | Notes |
|---|---|---|---|
| Buy: `tokio-tungstenite` | tokio-tungstenite + tungstenite | ~150 (wiring) | Mature, well-tested, handles control frames, ping/pong, close codes |
| Buy: `fastwebsockets` | fastwebsockets | ~150 | Faster, contested soundness, more `unsafe` |
| Buy: `axum::extract::ws` | axum + hyper + tokio-tungstenite | ~50 | Cleanest if we already have axum |
| Build: hand-roll RFC 6455 | bytes, sha1 (for handshake) | ~1.5-2k | Masking SIMD optional, control frames, fragmentation, close handshake |

**Decision: build a thin RFC 6455 implementation on the sans-io pattern.**

Reasoning:
- The RFC 6455 wire format is small: 14-byte max frame header, masking (XOR with 4-byte key), opcodes (`Continuation`/`Text`/`Binary`/`Close`/`Ping`/`Pong`).
- Hand-rolling at the right scope is ~800 LOC for framer + masker + handshake + control-frame state machine, plus another ~400 for the server `Upgrade` path.
- We do **not** need `permessage-deflate`. That's the compression extension, ~2k extra LOC. Brain's binary frames are already compact; WS-level compression doesn't add value.
- We do **not** need fragmented messages on send. We can fragment-on-receive (RFC mandates we accept fragmented inputs) but send single-frame messages outbound.
- The reason not to buy `tokio-tungstenite`: it pulls a `sha1` impl, a `base64` impl, the `tungstenite` core (~3k LOC of which we'd use ~1.5k), and brings handshake machinery we'd customize anyway. Margin is thin, but the customization shape (we want to be able to feed bytes directly from a `Frame` parser, not from a `tungstenite::Message` round-trip) tips toward build.

If I'm overruled here, `tokio-tungstenite` is the right buy. It is the right level of mature, and the maintainer is responsive. Just don't pick `fastwebsockets` — the soundness question isn't worth it for a substrate with a `no-unsafe-outside-storage` invariant.

### 4.4 SSE

| Path | Crates | LOC | Notes |
|---|---|---|---|
| Buy: `axum::response::sse` | axum + ... | ~50 | Drops in if we go axum |
| Buy: `hyper`'s body streaming | hyper + ... | ~200 | We adapt manually |
| Build: hand-roll | bytes, tokio | ~400 | Trivial; SSE is text format on top of chunked HTTP/1.1 |

**Decision: build.** SSE is genuinely small. The wire format is:
```
event: <type>\n
id: <id>\n
data: <line1>\n
data: <line2>\n
retry: <ms>\n
\n
```
With `Last-Event-ID` header support on reconnect and `text/event-stream` Content-Type. **Critical pitfall** (per the spec sources): you must flush the underlying writer after each event, or buffered I/O will hold events back until the buffer fills. This is one line of code that frameworks get right and naive impls get wrong.

Bonus: SSE doesn't *require* chunked transfer encoding — you can use `Connection: keep-alive` with no `Content-Length` and an indefinite body. But sending `Transfer-Encoding: chunked` is the conventional and well-supported choice; axum does it, hyper does it. We will too.

### 4.5 Streaming bodies (chunked)

| Path | LOC | Notes |
|---|---|---|
| Build chunked encoder | ~80 | `[hex-size]\r\n[bytes]\r\n` repeating, with a final `0\r\n\r\n`. Stateless to encode. |
| Build chunked decoder | ~200 | Hex-len parsing, line handling, trailers (optional, we can reject), EOF handling |

**Decision: build.** It's a tiny state machine. `httparse` handles request chunk size lines via `httparse::parse_chunk_size`.

### 4.6 Raw TCP helpers

Brain's `network` module already has all the TCP helpers it needs — `TcpListener` bind with `SO_REUSEADDR`, `TCP_NODELAY`, `SO_KEEPALIVE` (via `socket2`). `brain-http::tcp` can re-export and lightly wrap these for HTTP-server use (e.g. exposing `SO_REUSEPORT` for multi-core accept if we go there).

**Decision: re-export + thin wrappers.** ~150 LOC.

### 4.7 HTTP/2 — *should we?*

User asked us to discuss the trade-off explicitly. Here's the trade-off.

**To ship HTTP/2 we need:**
- HPACK encoder + decoder (RFC 7541) — ~2 kLOC; this is the "silent killer feature of HTTP/2" per Cloudflare's blog; getting it right around DoS via decompression bombs is non-trivial.
- Frame layer (RFC 7540) — DATA, HEADERS, PRIORITY, RST_STREAM, SETTINGS, PUSH_PROMISE, PING, GOAWAY, WINDOW_UPDATE, CONTINUATION — ~1.5 kLOC.
- Stream multiplexing — request/response/stream-state machine per HTTP/2 stream — ~3 kLOC.
- Flow control — per-stream and per-connection windows, WINDOW_UPDATE propagation — ~1 kLOC.
- Multiplexer scheduling under load (head-of-line avoidance, priority handling) — fiddly.

**Total: ~8-10 kLOC, ALPN integration in TLS, considerable test surface.**

Alternative: pull `h2` (~20 kLOC of someone else's code; battle-tested but Tokio-bound) or `hyper` with `http2` feature.

**Concrete reasons to build HTTP/2 right now:** none. Brain's clients today are `brain-cli`, internal SDK, and Prometheus scrapers. Prometheus's scraper is HTTP/1.1 (it does not use HTTP/2 by default; some setups enable it but no one needs it). The CLI is HTTP/1.1. The admin endpoints are low-volume. HTTP/2's multiplexing wins on browser fan-out and chatty API surfaces — neither applies.

**Decision: do not ship HTTP/2 in `brain-http` v1.** Document the decision in the crate-level rustdoc. Re-evaluate when there's a real client that needs it (e.g. a future gRPC-over-HTTP/2 SDK).

If we change our mind: the cleanest add-back is to add `hyper` with `server`+`http1`+`http2` features and accept the Tokio dependency on the admin side. We already have Tokio in `brain-server`; the marginal cost is ~25 transitive crates.

---

## §5 — Proposed `brain-http` architecture

Brain's folder-per-concern convention: every concern in its own folder, only `mod.rs`/`lib.rs` at the root of `src/`. Inside each folder, file-per-concept again.

```
crates/brain-http/
├── Cargo.toml
├── README.md
├── src/
│   ├── lib.rs                       # crate-level re-exports + crate docs
│   ├── error/
│   │   ├── mod.rs                   # Error type (thiserror), ErrorKind enum
│   │   └── status.rs                # StatusCode helpers, Brain mapping
│   ├── version/
│   │   └── mod.rs                   # HttpVersion enum (Http11 only initially)
│   ├── method/
│   │   └── mod.rs                   # Re-export of http::Method (+ small extras)
│   ├── uri/
│   │   ├── mod.rs                   # Path + query split, percent decode helpers
│   │   └── path_match.rs            # Static + parametric router primitives
│   ├── headers/
│   │   ├── mod.rs                   # HeaderMap wrapper, case-insensitive lookup
│   │   ├── well_known.rs            # Host, Content-Length, Transfer-Encoding, Upgrade, ...
│   │   └── parse.rs                 # Build HeaderMap from httparse output
│   ├── body/
│   │   ├── mod.rs                   # Body trait + concrete types
│   │   ├── empty.rs                 # zero-length body
│   │   ├── bytes_body.rs            # owned-Bytes body
│   │   ├── stream.rs                # Box<dyn Stream<Item=io::Result<Bytes>>>
│   │   └── chunked.rs               # encoder + decoder for chunked transfer
│   ├── http1/
│   │   ├── mod.rs                   # public surface for the protocol
│   │   ├── parser.rs                # httparse driver, BytesMut buffering, sans-io
│   │   ├── encoder.rs               # serializes request/response head to bytes
│   │   ├── conn.rs                  # Connection state machine (read-loop, write-loop)
│   │   ├── keepalive.rs             # keep-alive policy, max-requests, max-age
│   │   └── upgrade.rs               # HTTP/1.1 Upgrade handling (WS lives in ws/)
│   ├── server/
│   │   ├── mod.rs                   # HttpServer builder, bind, serve
│   │   ├── accept.rs                # TcpListener accept loop on Tokio
│   │   ├── service.rs               # Service trait (async fn handle), service_fn
│   │   ├── handler.rs               # ServerHandler helpers
│   │   ├── router.rs                # Router type: method+path → handler
│   │   ├── tls.rs                   # rustls integration (mirrors network/connection.rs §9.9)
│   │   ├── limits.rs                # max body size, max header bytes, request timeout
│   │   └── shutdown.rs              # graceful shutdown integration with ShutdownSignal
│   ├── client/
│   │   ├── mod.rs                   # HttpClient builder
│   │   ├── pool.rs                  # connection pool (keep-alive aware)
│   │   ├── request.rs               # RequestBuilder
│   │   ├── response.rs              # Response, body collection
│   │   └── blocking.rs              # blocking facade (wraps async or std::net)
│   ├── ws/
│   │   ├── mod.rs                   # public WebSocket surface
│   │   ├── handshake.rs             # Sec-WebSocket-Accept generation + validation
│   │   ├── frame.rs                 # 14-byte frame header parser/encoder
│   │   ├── mask.rs                  # XOR mask helper (4-byte key, SIMD optional)
│   │   ├── opcode.rs                # WsOpcode (Text/Binary/Close/Ping/Pong/Continuation)
│   │   ├── codec.rs                 # framed I/O over an AsyncRead/AsyncWrite
│   │   └── close.rs                 # close handshake + status codes
│   ├── sse/
│   │   ├── mod.rs                   # SseEvent struct, SseStream type
│   │   ├── encoder.rs               # event → wire bytes
│   │   ├── client.rs                # SSE client w/ Last-Event-ID reconnect
│   │   └── retry.rs                 # retry-with-backoff helpers
│   ├── tcp/
│   │   ├── mod.rs                   # listener bind helpers (REUSEADDR, REUSEPORT)
│   │   ├── nodelay.rs               # apply TCP_NODELAY + SO_KEEPALIVE
│   │   └── timeout.rs               # AsyncRead/Write with idle timeout
│   └── util/
│       ├── mod.rs                   # internal helpers (no public re-exports)
│       ├── ascii.rs                 # ASCII-only helpers (we own input validation)
│       └── reusable.rs              # BytesMut reclaim helper
└── tests/
    ├── http1_server_smoke.rs
    ├── http1_keepalive.rs
    ├── http1_chunked.rs
    ├── ws_handshake.rs
    ├── ws_echo.rs
    ├── ws_control_frames.rs
    ├── sse_basic.rs
    ├── sse_reconnect.rs
    └── client_get_post.rs
```

### 5.1 Module-by-module: types, deps, what runs where

#### `error/` — `thiserror` per CLAUDE.md §7

```rust
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(ParseError),
    #[error("http: status {0}")]
    Status(http::StatusCode),
    #[error("ws: {0}")]
    WebSocket(WsError),
    #[error("timeout after {0:?}")]
    Timeout(std::time::Duration),
    #[error("body too large: {actual} > {limit}")]
    BodyTooLarge { actual: usize, limit: usize },
    #[error("header too large: {actual} > {limit}")]
    HeaderTooLarge { actual: usize, limit: usize },
    #[error("connection closed")]
    ConnectionClosed,
}
```

#### `version/`, `method/`, `uri/`, `headers/`

Thin wrappers around `http` crate types. We don't reinvent `Method` or `StatusCode`. `headers::parse` is a one-function converter from `httparse::Request::headers` slices to an `http::HeaderMap`, with the explicit checks for control characters and obs-fold.

#### `body/`

The `Body` trait:
```rust
pub trait Body {
    type Error: Into<crate::Error>;
    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) 
        -> Poll<Option<Result<Bytes, Self::Error>>>;
    fn size_hint(&self) -> SizeHint;
}
```
Three concrete impls: `Empty`, `Full(Bytes)`, `BoxBody`. `chunked` is *not* a Body type — it's an encoder/decoder layer applied to a Body. This is the sans-io pattern: chunked encoding is a transformation over a byte stream, separate from the body abstraction.

#### `http1/`

The protocol heart. `parser.rs` wraps `httparse::Request::parse` and `httparse::parse_chunk_size`. State machine:

```
[Idle] --bytes--> [ReadingHeaders] --headers done--> [ReadingBody] --eof/close--> [ResponseReady]
                                                  --keep-alive--> [Idle]
```

`conn.rs` runs the state machine over `tokio::io::AsyncRead + AsyncWrite`. Decoupled from runtime by being generic over those traits.

Runs on Tokio (admin side). Could in principle be reused on Glommio if we ever swap the trait bounds for Glommio's I/O traits — we don't ship that today.

#### `server/`

`HttpServer::bind(addr).serve(router).await`. Internally:
- `accept.rs` runs the `tokio::net::TcpListener::accept` loop. Per-connection `tokio::spawn(http1::Connection::new(stream, service).run())`.
- `service.rs` defines the `Service` trait — async fn `handle(&self, req: Request) -> Result<Response>`. `service_fn(f)` wraps a closure.
- `router.rs` is a small radix trie or a static `match` (we'll start with a static array of `(Method, &str, Handler)` tuples — Brain has ~15 routes today, no need for a trie until we have 100s).
- `limits.rs` enforces max request line, max header block, max body, per-request timeout. All currently hand-rolled in `crates/brain-server/src/admin/mod.rs`; this gathers them.

#### `client/`

Async client first, blocking facade on top. Pool keyed on `(host, port)` with bounded keep-alive count. Falls back to a one-shot connection if pool empty. `brain-cli` migrates from `crates/brain-cli/src/http/mod.rs` to the blocking facade.

#### `ws/`

Sans-io framer in `frame.rs`. State machine in `codec.rs` driven by `AsyncRead+Write`. The server upgrade hook in `http1::upgrade::upgrade_to_ws()` returns an `(WsCodec, AsyncRead+Write)` after parsing the `Sec-WebSocket-Key`/version handshake and emitting the 101 response. `mask.rs` is a single XOR loop with an SIMD path behind a `#[cfg(target_feature = "sse2")]` (kept optional and simple — no `unsafe` block, the wide `u64::from_ne_bytes`-cast trick is enough).

#### `sse/`

`SseEvent { id: Option<String>, event: Option<String>, data: String, retry: Option<Duration> }`. `encoder.rs` produces the bytes; server-side handler returns `Body::Stream(SseStream)` and sets `Content-Type: text/event-stream`, `Cache-Control: no-cache`, `Transfer-Encoding: chunked`. The flush-after-event discipline lives in `SseStream::poll_frame`.

Client `sse/client.rs` connects, reads chunked bytes, parses events line by line, tracks `Last-Event-ID` across reconnects, respects `retry:` for backoff.

#### `tcp/`

Re-exports `tokio::net::TcpListener`/`TcpStream` with the configuration knobs already applied (TCP_NODELAY, SO_REUSEADDR, optional SO_REUSEPORT for multi-listener configs). One small concern: the existing `crates/brain-server/src/network/connection.rs` already does this — `brain-http::tcp` should re-export from there if practical, or vice versa, to avoid two copies of the bind helpers. Concrete: `brain-http::tcp` *is* the new home, and `brain-server::network::connection` imports from it.

#### Crate-level Cargo.toml

```toml
[package]
name = "brain-http"
version = "0.1.0"
edition = "2021"

[features]
default = ["server", "client", "ws", "sse"]
server = []
client = []
ws = []
sse = []
tls = ["dep:tokio-rustls"]

[dependencies]
# already in the workspace
tokio = { workspace = true, features = ["net", "io-util", "time", "sync", "macros"] }
tracing = { workspace = true }
thiserror = { workspace = true }
bytes = "1"
http = "1"
httparse = "1.10"
# only for ws handshake
sha1 = "0.10"
base64 = "0.22"
# only for TLS feature
tokio-rustls = { workspace = true, optional = true }
# Brain-side
brain-core = { path = "../brain-core" }

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

`bytes`, `http`, `httparse`, `sha1`, `base64` are **new deps** that must be justified per `CLAUDE.md` §6 and `AUTONOMY.md` §2.6. Justifications:

- `bytes`: zero-copy buffer, already transitively present via `tokio`. Required for the buffer-reuse pattern.
- `http`: the de-facto typed vocabulary for HTTP. Pure data types, no runtime, ~6kLOC, no other deps. Reuse is cheaper than rebuilding `HeaderMap` + `Method` + `StatusCode`.
- `httparse`: the standard sans-io HTTP/1.x parser. 1.10.1, March 2025. ~3kLOC, no deps, optional SIMD. Building this ourselves is 1-2 weeks of work for a worse result.
- `sha1`: required by RFC 6455 for the `Sec-WebSocket-Accept` derivation. Tiny crate, no deps.
- `base64`: required by RFC 6455 for the same. Tiny.

If the user pushes back on any of these, the alternatives are: do without (don't ship the affected feature), vendor it (copy the source into `brain-http`), or write it from scratch (`sha1`/`base64`/`httparse` are all small enough). The recommendation is justify-and-add.

### 5.2 What runs where

| Module | Runtime | Reason |
|---|---|---|
| `error`, `version`, `method`, `uri`, `headers`, `body`, `http1::parser`, `http1::encoder`, `ws::frame`, `ws::mask`, `ws::handshake`, `sse::encoder` | **runtime-free (sans-io)** | They operate on `&[u8]` and `BytesMut`; no async, no I/O |
| `http1::conn`, `server::accept`, `server::shutdown`, `client::pool`, `ws::codec`, `sse::client`, `tcp` | **Tokio** | These touch sockets and tasks |
| `server::router`, `server::service`, `server::handler`, `server::limits`, `client::request`, `client::response` | **runtime-generic over `AsyncRead+Write`** | Glommio could substitute later if we ever go Posture B |
| `client::blocking` | **`std::net` (sync)** | For `brain-cli` |

The sans-io split is deliberate. It means the parser, framer, masker, and SSE encoder are testable with proptest and fuzzable with cargo-fuzz without spawning any runtime. That fits Brain's testing strategy (`CLAUDE.md` §10).

---

## §6 — Design patterns checklist

These are the patterns that the architecture forces. Each is a small but load-bearing decision.

### 6.1 Separate protocol decoder from transport (sans-io)

The HTTP/1.x parser, the WebSocket framer, the SSE encoder, and the chunked decoder are all pure functions on byte slices. They never touch a socket. The transport layer feeds them bytes from a `BytesMut` buffer and acts on the events they emit. This is the same pattern Brain's `brain-protocol::frame` already follows for the binary wire — `Frame::decode_with_max` operates on `&[u8]` and returns events. Apply it consistently.

Benefit: proptest + cargo-fuzz attach to the parser directly. No need to fake a socket. Faster, more thorough fuzzing.

### 6.2 ServiceFn-style handler trait

```rust
pub trait Service {
    type Future: Future<Output = Result<Response, Error>>;
    fn handle(&self, req: Request) -> Self::Future;
}

pub fn service_fn<F, Fut>(f: F) -> impl Service
where
    F: Fn(Request) -> Fut,
    Fut: Future<Output = Result<Response, Error>>,
{ /* ... */ }
```

Same shape as `hyper::service::Service` (which is `tower::Service` for `Request`). Stays compatible if we ever swap. Crucially: **no tower dep.** We don't need the middleware abstraction; routes are explicit.

### 6.3 Bounded channels at every async boundary

For the SSE/WebSocket path, the writer task receives outbound `Bytes` from an `mpsc::channel(N)` fed by the application. The bound is the backpressure. Same pattern already used by `crates/brain-server/src/network/subscribe.rs`.

### 6.4 Buffer reuse via `bytes::BytesMut` + reclaim

Per-connection: one `BytesMut` for read, one for write. After parsing a request, `read_buf.advance(consumed)` and `read_buf.reserve(MIN_READ_AHEAD)`. `BytesMut` reuses underlying storage when refcount permits, so steady-state allocation per request approaches zero.

This is the pattern that achieves "no allocation in hot path" from `CLAUDE.md` §9.

### 6.5 Connection state machine vs request state machine

Each connection has a finite state: `Idle | ReadingRequest | ProcessingBody | Responding | Closed`. Each request inside a connection has a separate state: `HeadersIn | BodyIn | HeadersOut | BodyOut | Done`. Modeling them as two layered enums (rather than one big flat enum) makes keep-alive transitions trivial: "the connection goes back to Idle when the request goes to Done."

### 6.6 Read-half / write-half split

`tokio::io::split(stream)` gives `(ReadHalf, WriteHalf)`. The HTTP/1.1 connection task drives both halves itself (because HTTP/1.1 is half-duplex per request). WebSocket and SSE *do* want concurrent read+write — for those we spawn a writer task and the original task becomes the reader. Backpressure is on the bounded write channel.

### 6.7 Frame-level zero copy via `Bytes`

The parser produces a `Request<()>` (no body) and an offset into the read buffer. The body is exposed as a `Body` stream that yields `Bytes` slices into the same underlying buffer. `Bytes::clone` is a refcount bump, not a memcpy. Same for outbound: `Response { body: Bytes }` writes the bytes straight to the socket via `write_all_vectored`.

This is what makes "zero-copy on the read path" honest.

### 6.8 Streaming response via `impl Stream<Item = io::Result<Bytes>>`

`Body::Stream(Pin<Box<dyn Stream<Item = io::Result<Bytes>>>>)`. The chunked encoder wraps it; the SSE encoder wraps it; the WebSocket framer wraps it. One abstraction.

### 6.9 Hand-coded routing

Static routes. `match (method, path) { ("GET", "/healthz") => healthz(req), ... }`. For dynamic path segments (`/v1/snapshots/<id>`) the `path::strip_prefix("/v1/snapshots/")` pattern already used in `crates/brain-server/src/admin/snapshot.rs` is fine. No radix trie until route count > ~50.

### 6.10 Limits enforced before reading

The first thing every connection does is read the request line + headers into a `BytesMut` of bounded size. If headers exceed `HTTP_HEADER_BLOCK_MAX`, return `431 Request Header Fields Too Large` and close. Body limits enforced as the body streams in (counter increments per chunk). Timeouts wrap the whole request future with `tokio::time::timeout`.

This is the "don't trust user input" of `CLAUDE.md` §9 made operational.

### 6.11 Fail-stop on protocol errors

Spec invariant 7 ("No silent corruption"). If we parse a request and find a header value with control characters, or a chunked size that's not valid hex, or a WS frame whose payload length exceeds the per-connection limit, we return a structured error, close the connection, and log a `warn!` with a structured field. We never half-process.

### 6.12 Tracing span per request

Following `CLAUDE.md` §14 (observability). Each accepted connection gets a `tracing::span!(Level::DEBUG, "http_conn", peer = %peer_addr)`. Each request inside gets `tracing::span!(Level::INFO, "http_req", method = %m, path = %p)`. Errors carry `error = %e`. Latency is recorded via `Span::record("latency_ms", ...)` at request completion.

### 6.13 Explicit `Connection: close` on errors

Once we've returned a non-2xx that closes the body abruptly, we set `Connection: close` and shut the socket down — never try to recover keep-alive across a malformed-input boundary. This is what every well-behaved HTTP server does and what naive ones get wrong.

### 6.14 `Vary: Accept` / `Content-Type` discipline

For SSE and WS routes specifically, the handler validates `Accept: text/event-stream` (for SSE) and `Upgrade: websocket` + `Sec-WebSocket-Version: 13` (for WS) before accepting. Reject with 400/426 otherwise.

### 6.15 No allocator-per-request

This is the "no allocate in hot path" rule. Concrete patterns:
- Header parsing uses a fixed `[httparse::EMPTY_HEADER; 64]` array; we never allocate a `Vec<Header>` per request.
- The `HeaderMap` we build can be pooled (`hyper-util` does this; we can reuse a thread-local `HeaderMap` for read-only inspection in the routing layer, allocating only when the handler retains it).
- Response writes use vectored I/O: head and body in a single `writev` call, no concat-into-buffer step.

These are the patterns that get us to sub-ms framing latency.

---

## §7 — Implementation plan (milestones)

Each milestone is shippable in isolation, has its own commit run, and leaves the tree green. Sequenced for incremental migration.

### M1 — Crate skeleton + sans-io HTTP/1.1 parser

- **What:** new crate `brain-http`. `error/`, `version/`, `method/`, `uri/`, `headers/`, `body/empty.rs`, `body/bytes_body.rs`, `http1/parser.rs` (httparse driver), `http1/encoder.rs`.
- **Tests:** unit tests on parser with synthetic input. Proptest for header parsing (any byte slice → never panics).
- **Touches:** new crate only. No edits elsewhere.
- **LOC:** ~1500 production, ~600 tests.
- **Justification commit:** add `bytes`, `http`, `httparse` with rationale per `AUTONOMY.md` §2.6.

### M2 — HTTP/1.1 server core on Tokio (no streaming)

- **What:** `tcp/`, `server/accept.rs`, `server/service.rs`, `server/router.rs`, `server/limits.rs`, `server/shutdown.rs`, `http1/conn.rs` for non-keep-alive `Content-Length` bodies.
- **Tests:** integration tests in `tests/http1_server_smoke.rs`. Drive with `std::net` client. Verify `Content-Length` round-trip, 200/400/500, timeouts.
- **Touches:** `brain-http` only. `brain-server::admin` still in place.
- **LOC:** ~1200 production, ~500 tests.

### M3 — Migrate `brain-server::admin` to `brain-http`

- **What:** swap `serve_request` in `crates/brain-server/src/admin/mod.rs` to use `brain-http::server::HttpServer` with the existing handlers wrapped as `Service`. Keep all admin sub-modules (`worker`, `snapshot`, `config_route`, `audit`, `agent`, `shard_route`, `diagnostics`, `rebuild`) — just rewire their `dispatch()` fn signatures to take a `&Request` / return a `Response`.
- **Tests:** existing admin tests must pass unchanged. Add a regression for `Content-Length: 0` POST (the not-implemented routes).
- **Touches:** `crates/brain-server/src/admin/mod.rs` and every file under `crates/brain-server/src/admin/`.
- **LOC:** -400 deleted (the hand-roll), +200 added (the rewiring). Net cleanup.

### M4 — Chunked transfer + streaming bodies + keep-alive

- **What:** `body/chunked.rs`, `body/stream.rs`, `http1/keepalive.rs`. Update `http1/conn.rs` state machine to handle keep-alive transitions.
- **Tests:** chunked decoder fuzz target. Keep-alive integration test: send 100 GETs on one connection. Streaming body integration test: server returns a 10 MB body in 4 KB chunks.
- **Touches:** `brain-http` only.
- **LOC:** ~800 production, ~400 tests.

### M5 — HTTP client (async + blocking)

- **What:** `client/request.rs`, `client/response.rs`, `client/pool.rs`, `client/blocking.rs`.
- **Tests:** integration with brain-http's own server. Cross-test: blocking client → async server, async client → async server.
- **Touches:** `brain-http` only.
- **LOC:** ~1000 production, ~500 tests.

### M6 — Migrate `brain-cli::http` to `brain-http::client::blocking`

- **What:** delete `crates/brain-cli/src/http/mod.rs`, re-export needed types from `brain-http::client::blocking`. Adjust call sites.
- **Tests:** existing CLI integration tests pass.
- **Touches:** `crates/brain-cli/src/http/mod.rs` (deleted), call sites in `brain-cli` (~10 files based on a `grep`).
- **LOC:** -200 deleted, +50 adjustments.

### M7 — Server-Sent Events

- **What:** `sse/` module. Server-side `SseStream` returned as a response body; client-side `EventSource` with reconnect + `Last-Event-ID`.
- **Tests:** unit + integration. Reconnect test that kills the connection mid-stream and verifies the next request carries `Last-Event-ID`.
- **Touches:** `brain-http` only.
- **LOC:** ~600 production, ~400 tests.

### M8 — WebSocket framing + server upgrade

- **What:** `ws/frame.rs`, `ws/mask.rs`, `ws/opcode.rs`, `ws/handshake.rs`, `ws/codec.rs`, `ws/close.rs`. `http1/upgrade.rs` for the 101 path. New deps: `sha1`, `base64`.
- **Tests:** unit on framer (proptest on random byte sequences). Integration: echo server + client via a small test client. Autobahn test-suite (https://github.com/crossbario/autobahn-testsuite) run optional — it's the gold standard for WS interop.
- **Touches:** `brain-http` only.
- **LOC:** ~1500 production, ~800 tests.

### M9 — WebSocket client

- **What:** Client-side upgrade, framer, masker.
- **Tests:** client ↔ server echo, Brain's own server.
- **Touches:** `brain-http` only.
- **LOC:** ~600 production, ~300 tests.

### M10 — Hardening + observability + benches

- **What:** `tracing` spans + OpenTelemetry attributes per CLAUDE.md §14. Criterion benches for parser, framer, masker. Soak test: 10k concurrent connections on a single core. Connection-leak audit.
- **Tests:** Criterion bench wired to `just bench brain-http`. A loadgen harness in `tests/load.rs` (ignored by default; run manually).
- **Touches:** `brain-http` only.
- **LOC:** ~400 production, ~600 tests/benches.

**Total:** ~8-9 kLOC production, ~5 kLOC tests, ~10 milestones. With Brain's commit cadence, 4-6 weeks of focused work. Not "small."

Sequencing notes:
- M1→M2→M3 is the first vertical slice that lets us *delete* the hand-rolled `admin/mod.rs`. If you only want one phase, that's the phase. ~3 kLOC of work.
- M4→M5→M6 cleans up `brain-cli/src/http/mod.rs`.
- M7 is independent and could land any time after M4.
- M8→M9 are the WS pair. M8 alone is useful (server only) for the "browser pushes a request, server pushes events" case.

### Milestone format (example expanded)

> **M1 — Sans-io HTTP/1.1 parser.**
> *Produces:* `brain-http::http1::parser`, `brain-http::http1::encoder`, the foundational types (`Request<()>`, `Response<()>`, `HeaderMap`, body skeletons), the error taxonomy. Runtime-free.
> *Touches:* new crate. Adds to `Cargo.toml` workspace `[members]`.
> *Depends on:* nothing.
> *Acceptance:* `cargo test -p brain-http` passes; `cargo bench -p brain-http parser` shows >1 GB/s parse throughput on a synthetic input; proptest run with 1000 cases on `parse_request_any_bytes_never_panics` passes; verify suite green.
> *LOC estimate:* ~1500 production, ~600 tests.
> *Commit shape:* one commit per submodule (`error`, `http1::parser`, `http1::encoder`, `headers`, `body`), final commit wires `lib.rs`.

---

## §8 — Risks and unknowns

### R1 — `Send` futures and the framework lock-in

**Risk:** if we ever decide to put HTTP on Glommio (Posture B), `hyper`/`axum` are off the table without `unsafe`. Going framework-buy now and rejecting it later is expensive.
**Why it matters:** future "HTTP-on-shard" features (e.g. a per-shard streaming endpoint) would require redoing the layer.
**Mitigation:** the proposed crate is sans-io at its core; everything that depends on a runtime is in `server::accept`, `client::pool`, `ws::codec`, `sse::client`. Swapping Tokio for Glommio in those modules later is feasible (the AsyncRead/Write traits differ, but the sans-io modules are unchanged).

### R2 — WebSocket masking correctness

**Risk:** RFC 6455 requires the client to mask all frames it sends to the server, and the server MUST close immediately on receiving an unmasked frame. The converse: a server that sends masked frames must be rejected by clients. Easy to forget on a hand-roll.
**Why it matters:** silent interop failure with anything that's strict (browsers, well-behaved proxies).
**Mitigation:** put the mask-direction check in a single function (`Frame::validate(role: Role)`), test it explicitly with proptest, run Autobahn's `clientcase`/`servercase` suites.

### R3 — SSE flush discipline

**Risk:** `tokio::io::BufWriter` defers writes until the buffer fills. SSE events held in a 4 KB buffer until a 4 KB threshold = events arrive in batches. Common bug.
**Why it matters:** SSE is supposed to be near-real-time. A 10-second batching delay is a user-visible bug, not a perf regression.
**Mitigation:** the SSE encoder writes directly to the underlying socket (no BufWriter), and we wrap each event in a single `write_all_vectored` to avoid the partial-write race. Test: a `tokio::time::sleep(100ms)` loop that emits events and verifies the client sees each one within 50 ms.

### R4 — Chunked decoder overrun

**Risk:** `httparse::parse_chunk_size` consumes a chunk-size line; we then need to consume exactly that many bytes + the trailing CRLF, then loop. Off-by-one bugs here lead to either reading into the next request (corrupting keep-alive) or dropping bytes.
**Why it matters:** keep-alive disasters are subtle — works in dev, fails on a third request only when the chunk boundary lines up with a TCP segment boundary.
**Mitigation:** proptest the chunked decoder with bytewise-random chunk boundaries. Fuzz target in `fuzz/` per `CLAUDE.md` §10.

### R5 — `httparse` SIMD on non-x86

**Risk:** `httparse` auto-detects SSE4.2/AVX2 on x86. On aarch64 (M-series Macs in dev), it falls back to scalar. Performance characteristics differ; benches that pass on aarch64 may surprise on production x86 — or vice versa.
**Why it matters:** Brain's perf targets are spec'd against Linux x86. Dev on aarch64 doesn't catch SIMD-specific bugs (which `httparse`'s SIMD has occasionally had — e.g., issue history around invalid header chars in 1.8.x).
**Mitigation:** CI runs on Linux x86 per `AUTONOMY.md` §22. Pin `httparse` to a version with no open SIMD-soundness issues at the time of milestone.

### R6 — TLS layering interacts with WS upgrade

**Risk:** the `Upgrade` mechanism in HTTP/1.1 happens *after* TLS is established. Our `server::accept` pipes `TcpStream` → `tokio_rustls::server::TlsStream<TcpStream>` → `http1::Connection`. The connection then "downgrades" to a raw stream for WS frames. Threading the lifetimes and `Pin<Box<dyn AsyncRead + AsyncWrite>>` through this transition is finicky.
**Why it matters:** if we don't get this right, `wss://` upgrades fail or, worse, deadlock at the buffer boundary.
**Mitigation:** model the post-upgrade I/O as a `Pin<Box<dyn AsyncRead + AsyncWrite + Send + Unpin>>` that the `Upgrade` handler returns; test specifically against `tls + ws_handshake + frame_echo`.

### R7 — Glommio cross-call channels

**Risk:** SSE/WS streams need to be fed by the Glommio shard side (events come from inside a shard). Today, `crates/brain-server/src/network/subscribe.rs` does this with a `flume` channel. The producer side runs in Glommio, the consumer in Tokio. Backpressure shape is important — a slow client must not stall a shard.
**Why it matters:** "slow client stalls the database" is the classic streaming bug.
**Mitigation:** bounded channel, with explicit drop policy on overflow (per the SUBSCRIBE pattern already in use). Document the policy in the SSE/WS handler rustdoc. Test under load.

### R8 — Two HTTP servers running concurrently

**Risk:** during M3 migration, we'll have a transition window where the admin server runs on `brain-http` for healthz/metrics but a few routes still use the legacy `serve_request`. Mixing them produces split-brain bugs.
**Why it matters:** existing admin tests depend on specific response bodies; if we migrate piecemeal, tests can pass against either implementation depending on which served the request.
**Mitigation:** migrate atomically — one commit, all routes move at once. Phase doc lists each route and asserts after-migration ownership.

### R9 — `bytes::BytesMut::reserve` panic on huge body

**Risk:** if a malicious client sends a chunked body with declared chunk size near `usize::MAX`, `BytesMut::reserve(chunk_size)` will either OOM or panic. The existing `crates/brain-protocol::frame::decode_with_max` is a precedent for sane bounds — we copy the pattern.
**Why it matters:** trivial DoS otherwise.
**Mitigation:** every reserve goes through a guarded helper that checks against `MAX_BODY_BYTES` (default 16 MiB, configurable) before calling `reserve`. Fuzz target should include "huge declared chunk size" cases.

### R10 — WebSocket close handshake races

**Risk:** the WS close handshake is RFC 6455 §5.5.1 — initiator sends Close, peer echoes Close, both sides close TCP. Races: peer dies mid-handshake, our writer task fights our reader task to send the Close, payload-after-Close gets ignored vs not.
**Why it matters:** half-closed sockets, leaked connections, occasional client-side errors that show up only under disconnect.
**Mitigation:** model the close handshake as a tri-state (`Open | ClosePending(reason) | Closed`). Writer task drains then exits; reader task transitions and terminates. Time-bound the half-closed wait (5 s default).

### R11 — Adding `sha1` + `base64` to the dep graph

**Risk:** these are new deps. `CLAUDE.md` §6 ("Added without justification → reject"). They're tiny and safe but they're still new.
**Why it matters:** the autonomy contract requires a justification in the commit + phase doc.
**Mitigation:** plan-first per `AUTONOMY.md` §21. Surface the dep list before M8 implementation. Both crates are pure-Rust, no-deps. If the user rejects, fall back to vendored impls (each is ~200 LOC).

---

## §9 — Specific recommendation

Build `brain-http` as a Tokio-side HTTP layer using `httparse` + `http` + `bytes` as building blocks. Do **not** pull `hyper`, `axum`, `actix-web`, or `tokio-tungstenite` in v1. Do **not** ship HTTP/2 in v1 — there is no client that needs it. Keep the data plane on the existing binary-TCP+rkyv path; HTTP is for ops and (eventually) browser-facing streaming. Structure the crate as listed in §5, sans-io at the core, with `server`/`client`/`ws`/`sse` modules each opt-in via feature flag. Run all HTTP on Tokio; cross the Tokio↔Glommio boundary the same way the current code does — through bounded channels carrying plain `Send` messages. This honors `CLAUDE.md` §9 ("Don't add Tokio inside a shard"), keeps `CLAUDE.md` §7 ("No `unsafe` outside `brain-storage`") intact, and produces a layer Brain owns end-to-end.

**Start with M1 (the sans-io HTTP/1.1 parser).** It's the smallest piece, has no dependencies on any of the other decisions, and produces an artifact you can verify in isolation: a 1500-LOC parser that hand-handles a `BytesMut` and returns `(Method, Uri, HeaderMap, Body)`-shaped events, with proptest coverage and a Criterion bench. That milestone lets us see whether the sans-io discipline actually buys what we want (zero allocation in the parse hot path, sub-microsecond per-request parse time) before we commit to the bigger surface. If the bench numbers don't land, we revisit and consider `hyper` after all — but with data, not a guess.

---

### Critical files for implementation

- `/Users/dodo/Desktop/brain/CLAUDE.md` — approved-crates policy, no-unsafe-outside-storage, anti-patterns (§6, §7, §9)
- `/Users/dodo/Desktop/brain/crates/brain-server/src/admin/mod.rs` — the hand-rolled HTTP server brain-http replaces (M3 target)
- `/Users/dodo/Desktop/brain/crates/brain-cli/src/http/mod.rs` — the hand-rolled blocking client brain-http replaces (M6 target)
- `/Users/dodo/Desktop/brain/crates/brain-server/src/network/connection.rs` — Tokio↔Glommio boundary patterns to mirror (channels, shutdown, TLS, socket setup)
- `/Users/dodo/Desktop/brain/crates/brain-server/src/network/subscribe.rs` — existing precedent for cross-runtime streaming (template for WS/SSE pipelines)

### Sources

- [hyper.rs](https://hyper.rs/)
- [hyper v1 - seanmonstar](https://seanmonstar.com/blog/hyper-v1/)
- [hyper Polish Period - seanmonstar](https://seanmonstar.com/blog/hyper-polish-period/)
- [Releases · hyperium/hyper](https://github.com/hyperium/hyper/releases)
- [hyper IO traits issue #3110](https://github.com/hyperium/hyper/issues/3110)
- [Using hyper on non-Send Executors · Issue #2341](https://github.com/hyperium/hyper/issues/2341)
- [GitHub - bytedance/monoio](https://github.com/bytedance/monoio)
- [Monoio benchmark](https://github.com/bytedance/monoio/blob/master/docs/en/benchmark.md)
- [Introducing Glommio - Datadog](https://www.datadoghq.com/blog/engineering/introducing-glommio/)
- [GitHub - DataDog/glommio](https://github.com/DataDog/glommio)
- [glommio examples](https://github.com/DataDog/glommio/tree/master/examples)
- [glommio docs](https://docs.rs/glommio/latest/glommio/)
- [The fastest WebSocket implementation](https://c410-f3r.github.io/thoughts/the-fastest-websocket-implementation/)
- [GitHub - snapview/tokio-tungstenite](https://github.com/snapview/tokio-tungstenite)
- [fastwebsockets docs](https://docs.rs/fastwebsockets)
- [tokio-websockets bench README](https://github.com/Gelbpunkt/tokio-websockets/blob/main/benches/README.md)
- [RFC 6455 - The WebSocket Protocol](https://datatracker.ietf.org/doc/html/rfc6455)
- [WebSocket Framing: Masking, Fragmentation and More](https://www.openmymind.net/WebSocket-Framing-Masking-Fragmentation-and-More/)
- [GitHub - seanmonstar/httparse](https://github.com/seanmonstar/httparse)
- [httparse v1.0 - seanmonstar](https://seanmonstar.com/blog/httparse-v1-0/)
- [picohttpparser-sys](https://crates.io/crates/picohttpparser-sys)
- [Round 23 results - TechEmpower Framework Benchmarks](https://www.techempower.com/benchmarks/)
- [GitHub - Xudong-Huang/may](https://github.com/Xudong-Huang/may)
- [Under the hood of Linkerd2-proxy](https://linkerd.io/2020/07/23/under-the-hood-of-linkerds-state-of-the-art-rust-proxy-linkerd2-proxy/)
- [GitHub - tikv/rust-prometheus](https://github.com/tikv/rust-prometheus)
- [ScyllaDB Shard-per-Core Architecture](https://www.scylladb.com/product/technology/shard-per-core-architecture/)
- [Seastar shared-nothing](https://seastar.io/shared-nothing/)
- [Seastar tutorial](https://github.com/scylladb/seastar/blob/master/doc/tutorial.md)
- [SurrealDB Axum docs](https://surrealdb.com/docs/languages/rust/frameworks/axum)
- [Vector HTTP sink util](https://github.com/vectordotdev/vector/blob/master/src/sinks/util/http.rs)
- [GitHub - cloudflare/workerd](https://github.com/cloudflare/workerd)
- [h2 crate docs](https://docs.rs/h2)
- [HPACK silent killer feature - Cloudflare](https://blog.cloudflare.com/hpack-the-silent-killer-feature-of-http-2/)
- [Actix unsafe audit issue](https://github.com/actix/actix-web/issues/289)
- [A sad day for Rust - Steve Klabnik](https://steveklabnik.com/writing/a-sad-day-for-rust/)
- [SSE / chunked encoding issue](https://github.com/quarkusio/quarkus/issues/7094)
- [axum SSE chunked issue](https://github.com/tokio-rs/axum/issues/2594)
