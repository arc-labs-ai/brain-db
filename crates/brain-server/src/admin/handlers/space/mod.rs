//! Admin HTTP handlers for `space`.
//!
//! All routes deferred — space_id secondary index doesn't exist yet.

mod by_id;
mod list;

pub use by_id::by_id;
pub use list::list;
