//! Tracing integration helpers.
//!
//! Per-connection and per-request span constructors follow the OTel
//! semantic-convention attribute names. The server wires
//! these into the actual pipeline; this module exposes the
//! constructors so handlers can attach span context as soon as they
//! exist.

mod span;
pub use span::{connection_span, record_status, request_span};
