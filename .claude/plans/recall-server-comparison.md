# recall-server organization — what to borrow, what to skip

`arc-labs/recall/crates/recall-server` is a mature axum-based REST
server (163 source files). It demonstrates a clean way to organize
a Rust server crate. Below: which of its ideas apply to brain-server,
which don't, and why.

---

## recall-server's organizing skeleton

```
src/
├── main.rs              32 LOC — runtime + bootstrap::run() only
├── lib.rs                    — declares the modules
├── app/                      — Application + Router + AppState
│   ├── application.rs        Application::{build, run}
│   ├── router.rs             build_router() with Tower middleware stack
│   ├── state.rs              AppState<S> generic over storage backend
│   └── mod.rs
├── bootstrap/                — startup orchestration
│   ├── mod.rs::run()         single public entry-point
│   ├── observability.rs      tracing subscriber init
│   ├── otel.rs               OTLP exporter
│   ├── pools.rs              storage pool construction
│   ├── providers.rs          LLM / embedder selection
│   └── worker.rs             background worker spawn
├── config/                   — env-var-loaded config, split by section
│   ├── deployment.rs
│   ├── log.rs
│   ├── providers.rs
│   ├── secrets.rs
│   ├── server.rs
│   └── mod.rs
├── http/                     — HTTP-layer infrastructure
│   ├── constants/            api / namespace / plan
│   ├── cursor.rs             opaque pagination encoding
│   ├── error/                ApiError, codes, impls
│   ├── extractors/           pagination, scope_filter, time_range, …
│   ├── middleware/           auth, idempotency, rate_limit, security_headers
│   ├── response.rs           ApiJson, PaginatedApiJson envelopes
│   └── span.rs               handler_event! macros
├── routes/                   — one sub-dir per resource group
│   ├── memories/   …
│   ├── entities/   …
│   ├── pipeline/   …
│   └── router.rs             merge all sub-routers
├── domain/                   — JSON request/response body types
├── services/                 — multi-step orchestration
├── identity/                 — auth + tenancy
├── messaging/                — control-plane event bus, write-job SSE
├── email/                    — sender + templates
└── crypto.rs                 — AES-256-GCM helpers
```

The headline pattern: **main.rs is 32 lines.** Every concern has a
home; the entry point doesn't reach for any of them directly.

---

## What translates to brain-server

Brain-server is a binary-protocol server (Glommio + Tokio bridge), not
REST. Most of recall's layers don't map. But two big wins do:

### 1. **`main.rs` → `bootstrap/` + thin main** (high value)

brain-server's `main.rs` is **488 LOC**. It performs:

1. CLI arg parsing.
2. Tracing subscriber init (fmt + optional JSON, optional OTel).
3. Config load + validation.
4. Summarizer factory wiring.
5. Tokio runtime construction.
6. Inside the runtime: shutdown signal channel, signal listener spawn,
   TLS build, ConnectionMetrics construction, AdminServer construction
   + bind + spawn, ConnectionListener construction + bind + spawn,
   serve() await, admin drain timeout, shard graceful shutdown.

That's 5+ orthogonal concerns. recall's pattern splits this into:

```
bootstrap/
├── mod.rs              run() — orchestration only
├── tracing.rs          init_tracing(LogConfig)
├── tls.rs              build_tls(TlsConfig) — currently in tls.rs root
├── shards.rs           spawn_shards(cfg, summarizer) + ShardSpawn helpers
├── admin.rs            build_admin(cfg, state) → AdminServer (binds inside)
├── listener.rs         build_listener(cfg, tls, topology, metrics) → ConnectionListener
├── shutdown.rs         signal handler + graceful drain — currently in shutdown.rs
└── summarizer.rs       summarizer factory — currently inside main.rs
```

After: `main.rs` would be ~40 LOC (CLI parse + Tokio runtime build +
`bootstrap::run(cfg).await`). The existing `tls.rs`, `shutdown.rs`,
`llm/` would slot under bootstrap/ naturally.

**Risk**: low. Pure relocation. The runtime model and shard
discipline don't change.

### 2. **`config.rs` → `config/`** (high value)

brain-server's `config.rs` is **578 LOC** of `[server]`, `[storage]`,
`[shard]`, `[hnsw]`, `[embedder]`, `[workers]`, `[logging]`,
`[tracing]`, `[auth]`, `[summarizer]` sections plus parsers
(`parse_human_bytes`, `coerce_leaf`, `set_path`, `deserialize_*`).

Recall's `config/` mirrors its env-var sections one-per-file. Brain
can mirror its TOML sections:

```
config/
├── mod.rs              Config struct + load + validation
├── server.rs           ServerConfig + TlsConfig
├── storage.rs          StorageConfig + ShardConfig
├── index.rs            HnswConfig + EmbedderConfig
├── workers.rs          WorkersConfig
├── observability.rs    LoggingConfig + TracingConfig
├── auth.rs             AuthConfig
├── summarizer.rs       SummarizerConfig (currently inline)
└── parse.rs            parse_human_bytes, coerce_leaf, set_path
```

Each section file: ~50–80 LOC. mod.rs: ~100 LOC for the top-level
`Config` + `load_from_file` + `apply_overrides`.

**Risk**: low. Section structs are independent; only `parse.rs`
helpers cross-cut.

### 3. **`admin.rs` (463 LOC) — partial: keep monolithic but lift constants** (low value)

recall's `http/constants/`, `http/error/`, `http/response.rs` are
beautiful in an HTTP REST context. brain-server's admin.rs is a
hand-rolled HTTP/1.1 implementation intentionally kept tight (no
hyper, no Tower). The constants we *do* have (path strings, status
codes, JSON body shapes) could live in
`admin/{routes,response,handlers}.rs`, but the win is small.

**Recommendation: skip**. The cost of admin/ split outweighs the
~30% navigability gain. Revisit if admin grows past 1k LOC.

---

## What does NOT translate

| recall pattern | Why it doesn't fit brain |
|---|---|
| `app/router.rs` Tower middleware stack | Brain's connection-listener has a single FSM dispatch (HELLO → AUTH → Established → frame-dispatch). The "stack" is opcodes + idempotency lookup + auth-phase guard, all already in `dispatch.rs`. Wrapping in a Tower-like layer would force boxed-Service ceremony for zero behaviour gain. |
| `routes/{memories,entities,pipeline,…}` | Brain has 30 wire opcodes dispatched in `dispatch.rs`. They're not "routes" — they're stream operations with shard-affinity. Splitting `dispatch.rs` further would over-fragment the FSM. |
| `services/` | Brain's "service layer" is the brain-ops crate. Don't duplicate. |
| `domain/{request,response}` JSON types | Brain has the brain-protocol crate (rkyv-encoded binary). The recently-shipped refactor D already gave it `requests/` + `responses/` sub-modules. |
| `http/middleware/{auth,rate_limit,idempotency,security_headers}` | Brain's auth is per-connection FSM, idempotency lives in brain-ops, rate-limiting is a connection-limit (spec §03/06). No mid-stream middleware needed. |
| `http/extractors/` | Brain wire frames are rkyv-decoded; there are no query strings to extract. |
| `identity/`, `tenancy/`, `email/`, `crypto/` | Out of spec scope for v1 (auth is `AuthMethod::None` per spec §03/06 §3.1). |
| `messaging/` (in-process event bus + write-job SSE) | Brain has `brain-ops/subscribe` (SUBSCRIBE bridge). Don't duplicate. |
| OTLP / `otel.rs` | Brain has it in cargo deps (`opentelemetry`) but no exporter yet — that's a Phase 14 observability task. |

---

## Spec considerations

Any refactor must preserve:

- **Single-writer-per-shard** discipline (CLAUDE.md §5 inv. 2).
- **Glommio executor per shard** model (CLAUDE.md §9: no Tokio inside a shard).
- **ConnectionListener** structure (spec §10).
- **Hand-rolled HTTP/1.1 for admin** (decided in 9.13 — no hyper).
- **Tokio↔Glommio boundary** at `dispatch.rs` (sub-task 9.10).

The two recommended refactors (bootstrap/, config/) touch only
main.rs, config.rs, and the standalone helper files. They don't
cross any of the above lines.

---

## Recommended sequence (if you choose to act)

| # | Refactor | LOC moved | Files added | Risk |
|---|---|---|---|---|
| 1 | `main.rs` → `bootstrap/` + thin main | ~450 | ~6 | Low |
| 2 | `config.rs` → `config/` | ~580 | ~8 | Low |
| 3 | (skip) `admin.rs` split | — | — | — |

Both refactors are pure relocation with import-scoping. ~2 commits,
~30 min each, docker-verify between.

After: brain-server's `src/` becomes:

```
src/
├── main.rs               ~40 LOC
├── lib.rs                module declarations
├── bootstrap/            6 files — startup orchestration
├── config/               8 files — config sections + parsers
├── shard.rs              ~975 LOC (core shard runtime)
├── shard_adapters.rs     ~735 LOC (worker adapter glue)
├── connection.rs         ~780 LOC (Tokio listener)
├── dispatch.rs           ~700 LOC (frame FSM + boundary)
├── subscribe.rs          ~430 LOC (SUBSCRIBE bridge)
├── admin.rs              ~460 LOC (admin HTTP/1.1)
├── routing.rs            ~280 LOC (RoutingTable)
├── llm/                  6 files (summarizer adapters, already split)
└── tests/                in-process E2E fixtures
```

The "infra" (bootstrap, config) is grouped; the per-feature concerns
(shard, connection, dispatch, subscribe, admin, routing) stay at the
root as cohesive units — which matches recall's pattern where
`app/`, `bootstrap/`, `http/`, `routes/` are dirs but
single-concern feature files stay at the root.

---

*Awaiting direction on whether to execute refactor 1, 2, or both.*
