//! Planner-side context. Pure data — no async, no I/O. Spec §08/01 §4
//! says the planner has access to:
//!
//! - The request itself (passed as an argument, not here).
//! - Per-shard statistics ([`crate::ShardStats`]).
//! - Configuration ([`crate::PlannerConfig`]).
//! - Agent metadata (quotas) — deferred until a later sub-task adds
//!   the wiring.
//!
//! The planner does **not** have access to the storage layer
//! (spec §08/01 §9 — "planning is computation only; no I/O"). The
//! executor's context (lives in `executor.rs` when 6.7 lands) holds
//! the storage handles separately.

use crate::config::PlannerConfig;
use crate::stats::ShardStats;

#[derive(Debug, Clone, Copy, Default)]
pub struct PlannerContext {
    pub config: PlannerConfig,
    pub stats: ShardStats,
}

// `PlannerConfig` doesn't derive `Default` via the derive macro
// because we wrote a hand-rolled Default with the spec numbers. The
// `Default` derive on `PlannerContext` would normally fail because
// `PlannerConfig: Default` isn't a `derive`-generated impl. Rust
// accepts hand-rolled `Default` impls in derive resolution though, so
// this works as written.
