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
pub mod entity_ops;
pub mod llm_cache;
pub mod schema;
pub mod sink;
pub mod tables;

pub use db::{MetadataDb, MetadataDbError};
pub use entity_ops::{
    entity_add_alias, entity_get, entity_list_by_type, entity_lookup_by_alias,
    entity_lookup_by_canonical_name, entity_put, entity_remove_alias, entity_rename,
    entity_tombstone, entity_update, normalize_name, EntityOpError,
};
pub use llm_cache::{LlmCacheDb, LlmCacheError, LlmResponse};
