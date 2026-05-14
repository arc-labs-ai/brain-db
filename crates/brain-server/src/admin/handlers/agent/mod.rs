//! Admin HTTP handlers for `agent` (spec §14/06 §10; sub-task 10.11).
//!
//! All routes deferred — agent_id secondary index doesn't exist yet.

mod by_id;
mod list;

pub use by_id::by_id;
pub use list::list;
