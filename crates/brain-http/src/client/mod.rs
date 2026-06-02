//! HTTP client surface.
//!
//! **Status:** no client. `brain-http` ships server-side transport
//! only. The engine has no in-workspace outbound HTTP consumer:
//! external clients live in their own repos, and the
//! only outbound HTTP in this crate is the LLM summarizer's optional
//! `reqwest` dep, gated behind `summarizer-openai` /
//! `summarizer-ollama`. Building a client here today would be
//! speculative.
//!
//! ## When to revisit
//!
//! Add a client when any of:
//!
//! 1. **OTLP push.** Observability ships HTTP-based OpenTelemetry
//!    export needing a runtime-shareable client.
//! 2. **A new outbound consumer.** Webhook delivery from a worker,
//!    health-check fan-out, federation between brain shards.
//! 3. **`reqwest` becomes painful.** If the summarizer dep tree
//!    audit surfaces something we can't accept, we replace it with
//!    a thin client on `hyper_util::client::legacy::Client`.
//!
//! Natural shape when it lands: a builder over
//! [`hyper_util::client::legacy::Client`] with HTTP/TLS connectors,
//! a pool mirroring the server's `ServerLimits`, and a small
//! blocking facade for sync consumers. Roughly 700 LOC + tests.
