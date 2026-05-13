//! OpenTelemetry-compatible attribute keys used in tracing spans.
//!
//! Naming mirrors what `brain-server` emits so end-to-end traces
//! stitch cleanly (spec §13/07 §7). The `brain.*` prefix is
//! SDK-specific; `server.*` / `error.*` are OTel semantic
//! conventions.

/// Operation name (e.g. "encode", "recall", "txn_begin").
pub const OP: &str = "brain.operation";

/// Spec §13/04 §3 — UUIDv7 request id.
pub const REQUEST_ID: &str = "brain.request_id";

/// The agent id bound on this connection (AUTH_OK payload).
pub const AGENT_ID: &str = "brain.agent_id";

/// 1-indexed retry attempt counter.
pub const ATTEMPT: &str = "brain.attempt";

/// Wire-protocol error code from spec §03/10. Recorded on ERROR-
/// frame failures.
pub const ERROR_CODE: &str = "brain.error_code";

/// Server endpoint the client is connected to. Uses OTel's
/// `server.address` semantic convention.
pub const SERVER_ADDR: &str = "server.address";

/// The kind of error (`Connect`, `Io`, `Server`, etc.). Uses
/// OTel's `error.type` semantic convention.
pub const ERROR_KIND: &str = "error.type";

// ---- Op-name constants for the by-op breakdown -------------------

pub const OP_ENCODE: &str = "encode";
pub const OP_RECALL: &str = "recall";
pub const OP_PLAN: &str = "plan";
pub const OP_REASON: &str = "reason";
pub const OP_FORGET: &str = "forget";
pub const OP_LINK: &str = "link";
pub const OP_UNLINK: &str = "unlink";
pub const OP_SUBSCRIBE: &str = "subscribe";
pub const OP_TXN_BEGIN: &str = "txn_begin";
pub const OP_TXN_COMMIT: &str = "txn_commit";
pub const OP_TXN_ABORT: &str = "txn_abort";
