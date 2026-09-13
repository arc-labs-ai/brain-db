//! Retrieval query executor.
//!
//! Thin module shell: the business logic lives in `logic.rs` and its unit
//! tests in `tests.rs` (kept a child of `logic` so they retain access to
//! the module's private helpers). Re-exports the executor's public API.

mod logic;
pub use logic::*;
