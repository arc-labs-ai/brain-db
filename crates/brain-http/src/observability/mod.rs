//! Tracing integration helpers.
//!
//! Per-connection and per-request span constructors follow the OTel
//! semantic-convention attribute names from spec §14/03. M8 wires
//! these into the actual server pipeline; M1 just exposes the
//! constructors so handlers can attach span context as soon as they
//! exist.

mod span;
pub use span::{connection_span, request_span};
