//! Reciprocal Rank Fusion.
//!
//! Thin shell: business logic in `logic.rs`, unit tests in `tests.rs`
//! (kept a child of `logic` so they retain access to private helpers).
//! Re-exports the module's public API.

mod logic;
pub use logic::*;
