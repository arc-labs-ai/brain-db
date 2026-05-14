//! HTTP client surface.
//!
//! **Status (Phase 11 M5):** no client. `brain-http` ships server-side
//! transport only. This module exists as the design log for the
//! deferral decision.
//!
//! ## Why no client in v1
//!
//! No consumer in the Brain workspace currently needs an async HTTP
//! client:
//!
//! - `brain-cli` uses a self-contained ~200-LOC blocking client in
//!   its own `http::` module — works, tested via the admin integration
//!   suites, no external deps beyond stdlib.
//! - `brain-sdk-rust` speaks the binary wire protocol over TCP
//!   (`brain-protocol` rkyv frames), not HTTP.
//! - `brain-server`'s only outbound HTTP today is the optional
//!   `reqwest` feature pulled in for the Phase 9.15 LLM summarizer.
//!   That's gated behind `summarizer-openai` / `summarizer-ollama`;
//!   the default build doesn't include it.
//!
//! Building a brain-http client now would be a speculative dependency
//! per [`AUTONOMY.md`] §12 — code that solves no current problem.
//!
//! ## When to revisit
//!
//! Add a client to brain-http when **any one** of these is true:
//!
//! 1. **Phase 12 OTLP push.** If observability ships HTTP-based
//!    OpenTelemetry export and needs a runtime-shareable client
//!    (rather than instantiating a `reqwest::Client` per request),
//!    we need our own.
//! 2. **A new outbound consumer.** Webhook delivery from a worker,
//!    health check fan-out, federation between brain shards — any
//!    new code path that needs async HTTP and where the brain-cli
//!    blocking client is the wrong fit.
//! 3. **`reqwest` becomes painful.** If the summarizer dep tree
//!    audit surfaces something we can't accept, replacing reqwest
//!    with our own thin client on `hyper_util::client::legacy::Client`
//!    is worth the ~700 LOC.
//!
//! ## What the client would look like
//!
//! When we build it, the natural shape is:
//!
//! - `client::Client` — a builder over [`hyper_util::client::legacy::Client`]
//!   with a `Connect` implementation. Plain HTTP via the default
//!   `HttpConnector`; TLS via a feature-gated `hyper-rustls` connector.
//! - `client::RequestBuilder` / `client::Response` — match the
//!   server-side `Request<Body>` / `Response<Body>` ergonomics.
//! - `client::Pool` — keep-alive pooling, idle eviction, per-host
//!   bounds. Mirrors the server's `ServerLimits` shape.
//! - `client::blocking` — a sync facade that wraps the async client
//!   with `tokio::runtime::Runtime` block-on, for `brain-cli` and any
//!   future sync consumer.
//!
//! Roughly 700 LOC production + 400 LOC tests. Out of scope for M5;
//! a full milestone of its own when the trigger fires.
//!
//! ## Why not migrate brain-cli now
//!
//! Path (b1) in the M5 plan: move `brain-cli::http` into
//! `brain-http::client::blocking` verbatim. Same code, different
//! module path. Considered and rejected: it's churn without a real
//! consumer — we'd be moving 200 LOC across the workspace to make
//! the brain-http crate feel "complete." brain-cli's hand-roll is
//! not technical debt; it's exactly the right amount of code for
//! the surface it serves.
//!
//! [`AUTONOMY.md`]: ../../../../../../AUTONOMY.md
