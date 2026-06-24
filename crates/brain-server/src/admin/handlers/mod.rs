//! Admin route handlers.
//!
//! One module per route family. Multi-action families (snapshot,
//! worker, config, audit, agent, shard, diagnostics) are folders with
//! one file per action; single-action handlers (healthz, metrics,
//! rebuild) are flat files inside `handlers/`.
//!
//! Each handler module is internal to `crate::admin` and consumed
//! from the `crate::admin::router` family of `build_*` functions.

pub mod agent;
pub mod api_keys;
pub mod audit;
pub mod config;
pub mod diagnostics;
pub mod extract;
pub mod healthz;
pub mod metrics;
pub mod readyz;
pub mod rebuild;
pub mod shard;
pub mod snapshot;
pub mod worker;
