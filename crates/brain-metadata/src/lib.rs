//! # brain-metadata
//!
//! redb-backed metadata store: agents, contexts, memory metadata, edges,
//! idempotency table, and the durable LSN checkpoint. Phase 2's
//! [`brain_storage::recovery::MetadataSink`] trait gets its real impl
//! here (sub-task 3.11).
//!
//! See `spec/07_metadata_graph/` for the authoritative design.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod db;
pub mod schema;
pub mod sink;
pub mod tables;

pub use db::{MetadataDb, MetadataDbError};
