//! # brain-http
//!
//! HTTP/1.1, WebSocket, and Server-Sent Events transport for the
//! Brain cognitive substrate.
//!
//! Built on [`hyper`] 1.x as the wire substrate. The crate is
//! **HTTP-version-neutral by construction**: the [`Service`] trait,
//! [`Body`] trait, and `Request<B>` / `Response<B>` types all live
//! above the version. HTTP/2 is one feature flag (`hyper/http2`)
//! away when there is a concrete client that needs it.
//!
//! Architecture and milestone breakdown live in
//! `docs/phases/phase-11-brain-http.md`.
//!
//! ## What this crate owns
//!
//! - **Routing** — small `match`-based [`Router`](router) in M2.
//! - **Error mapping** — Brain-specific [`Error`] taxonomy.
//! - **SSE flush discipline** — [`sse`] handlers flush after every
//!   event (the bug frameworks get right and naive implementations
//!   get wrong).
//! - **WebSocket close handshake** — explicit tri-state close machine.
//!
//! ## What hyper owns (so we don't)
//!
//! - HTTP/1.1 wire parsing + encoding.
//! - Keep-alive state machine.
//! - Chunked transfer encoding.
//! - Body backpressure.
//! - Body trait ([`http_body::Body`]) and standard combinators.
//!
//! ## Feature flags
//!
//! - `server` (default) — server accept loop, router, connection
//!   handling. Lands in M2.
//! - `client` — async HTTP client. M5.
//! - `ws` — WebSocket via [`tokio-tungstenite`]. M6/M7.
//! - `sse` — Server-Sent Events. M4.
//! - `tls` — rustls termination at the server. Implies `server`.
//!
//! ## Milestone scope
//!
//! M1 lands the foundation only: error/body/service/observability
//! types. M2 adds the server accept loop and router. See
//! `.claude/plans/brain-http-task-01-skeleton.md` for the M1 plan.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::missing_errors_doc)]

pub mod body;
pub mod error;
pub mod observability;
pub mod service;

// Re-exports — the surface every handler uses.
pub use error::{Error, Result};
pub use http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
};
pub use service::{service_fn, AsyncHandler};
