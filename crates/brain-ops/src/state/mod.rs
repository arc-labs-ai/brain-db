//! Shared per-shard state primitives consumed across the ops crate.
//!
//! These modules hold small, focused state holders that don't fit
//! neatly inside a single handler or worker but are referenced from
//! several:
//!
//! - [`access_buffer`] — RECALL → boost-worker hand-off buffer.
//! - [`idempotency`] — request-hash helpers for the unified write path.
//! - [`txn_lens`] — `ExecutorContext` lens that layers an active txn's
//!   buffered writes on committed state.
//!
//! Grouped here so `crates/brain-ops/src/` only contains top-level
//! orchestration files (`lib.rs`, `dispatch.rs`, `error.rs`,
//! `context.rs`, `test_support.rs`).

pub mod access_buffer;
pub mod ack_codec;
pub mod idempotency;
pub mod txn_lens;
