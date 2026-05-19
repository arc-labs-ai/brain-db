//! Thin wrappers over [`comfy_table`].
//!
//! The renderers in `crate::render` never touch comfy-table directly —
//! they reach for [`build_table`], the cell helpers in [`cells`], and
//! [`middle_truncate`] so the look of every table in the project stays in
//! sync as the conventions evolve.

pub mod builder;
pub mod cells;
pub mod truncate;

pub use builder::build_table;
pub use cells::{
    confidence_cell, entity_id_cell, kv_row, predicate_cell, score_cell, short_id_cell,
};
pub use truncate::middle_truncate;
