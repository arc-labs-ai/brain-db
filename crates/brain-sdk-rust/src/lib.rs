//! # brain-sdk-rust
//!
//! Idiomatic async Rust SDK for the Brain cognitive substrate.
//!
//! ## What 10.1 ships
//!
//! - [`Client`] — single-connection async entry point. `Client::connect`
//!   opens a TCP socket, drives the spec §03/06 handshake (HELLO →
//!   WELCOME → AUTH → AUTH_OK), and returns a usable client.
//! - [`ClientConfig`] with spec §13/02 §14 defaults.
//! - [`ClientError`] — `#[non_exhaustive]` error taxonomy.
//!
//! Op methods (encode / recall / plan / reason / forget / link /
//! txn / subscribe), the connection pool, retry-with-backoff, and
//! the streaming surface land in 10.2 → 10.6. See
//! `docs/phases/phase-10-sdk-cli.md`.
//!
//! ## Layout
//!
//! Every concern under `src/` lives in its own folder; only
//! `lib.rs` sits at the crate root. See
//! `.claude/plans/phase-10-task-01.md` §3 for the rationale.
//!
//! ## Spec reference
//!
//! See `spec/13_sdk_design/` for the authoritative SDK design.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod client;
pub mod config;
pub mod error;
pub mod pool;
pub mod proto;
pub mod retry;

pub use client::Client;
pub use config::{AuthMethod, ClientConfig};
pub use error::ClientError;
pub use pool::{Connection, Pool, PoolConfig, PoolGuard};
pub use proto::handshake::{ClientIdentity, NegotiatedSession};
pub use retry::RetryConfig;
