//! Planner-side context. Pure data — no async, no I/O. The planner
//! has access to:
//!
//! - The request itself (passed as an argument, not here).
//! - Per-shard statistics ([`crate::ShardStats`]).
//! - Configuration ([`crate::PlannerConfig`]).
//! - Space metadata (quotas) — deferred until the wiring is added.
//!
//! The planner does **not** have access to the storage layer —
//! planning is computation only; no I/O. The executor's context holds
//! the storage handles separately.

use crate::config::PlannerConfig;
use crate::stats::ShardStats;

#[derive(Debug, Clone, Copy, Default)]
pub struct PlannerContext {
    pub config: PlannerConfig,
    pub stats: ShardStats,
}

// `PlannerConfig` doesn't derive `Default` via the derive macro
// because we wrote a hand-rolled Default with the default numbers. The
// `Default` derive on `PlannerContext` would normally fail because
// `PlannerConfig: Default` isn't a `derive`-generated impl. Rust
// accepts hand-rolled `Default` impls in derive resolution though, so
// this works as written.
