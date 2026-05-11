//! Executor side of the planner. Async functions that consume a
//! plan + an [`ExecutorContext`] and produce a Rust-side result.
//!
//! Spec §08/08 §1: "The executor is async (returns futures). Each
//! `execute_*` method orchestrates the steps in the plan."

pub mod context;
pub mod error;
pub mod recall;
pub mod result;

pub use context::ExecutorContext;
pub use error::ExecError;
pub use recall::execute_recall;
pub use result::{RecallHit, RecallResult};
