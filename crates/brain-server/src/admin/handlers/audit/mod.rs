//! Admin HTTP handlers for `audit` (spec §14/06 §8; sub-task 10.11).
//!
//! Both routes are deferred — no audit-log primitive exists yet.

mod export;
mod query;

pub use export::export;
pub use query::query;
