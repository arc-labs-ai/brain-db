//! Worker error taxonomy. Mirrors: errors are
//! counted in `WorkerMetrics::errors_total` and logged via tracing;
//! they don't propagate beyond the scheduler loop.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkerError {
    /// Underlying ops-layer call failed (read/write/index/metadata).
    /// Carries the upstream message so tracing has detail.
    #[error("ops layer error: {0}")]
    Ops(String),

    /// A cycle exceeded its `WorkerConfig::max_runtime` budget. Cycles
    /// are bounded; runtime violations are surfaced rather than ignored.
    #[error("worker cycle exceeded budget: {0}")]
    BudgetExceeded(String),

    /// Catch-all for internal invariants (mostly from `drive_batch`
    /// helper guards).
    #[error("internal: {0}")]
    Internal(String),

    /// A job asked the worker to perform an operation that has no
    /// implementation yet. Surfaced (counted + logged by the scheduler)
    /// rather than silently dropped so an unhandled job never vanishes.
    #[error("unimplemented: {0}")]
    Unimplemented(String),
}
