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
pub mod entity_merge_ops;
pub mod entity_ops;
pub mod entity_type_ops;
pub mod llm_cache;
pub mod predicate_ops;
pub mod relation_ops;
pub mod relation_traversal;
pub mod relation_type_ops;
pub mod schema;
pub mod schema_apply;
pub mod schema_store;
pub mod sink;
pub mod statement_ops;
pub mod system_schema;
pub mod tables;
pub mod trigram_ops;

pub use db::{MetadataDb, MetadataDbError};
pub use entity_ops::{
    entity_add_alias, entity_get, entity_list_by_type, entity_lookup_by_alias,
    entity_lookup_by_canonical_name, entity_put, entity_remove_alias, entity_rename,
    entity_tombstone, entity_update, normalize_name, EntityOpError,
};
pub use llm_cache::{LlmCacheDb, LlmCacheError, LlmResponse};
pub use predicate_ops::{
    predicate_get, predicate_intern, predicate_list, predicate_lookup_by_qname, PredicateOpError,
};
pub use relation_ops::{
    relation_create, relation_get, relation_history, relation_list_from, relation_list_to,
    relation_supersede, relation_tombstone, relations_with_evidence, RelationListFilter,
    RelationOpError,
};
pub use relation_traversal::{
    traverse, TraversalConfig, TraversalDirection, TraversalPath, TraversalStep,
    DEFAULT_MAX_BRANCHING, DEFAULT_MAX_DEPTH, MAX_BRANCHING, MAX_DEPTH, MAX_TOTAL_VISITED,
};
pub use relation_type_ops::{
    relation_type_get, relation_type_intern, relation_type_list, relation_type_lookup_by_qname,
    RelationTypeOpError,
};
pub use entity_type_ops::{entity_type_intern, entity_type_lookup_by_name, EntityTypeOpError};
pub use schema_apply::{apply_schema_definitions, SchemaApplyError};
pub use schema_store::{
    schema_active, schema_active_row, schema_get, schema_list, schema_namespaces, schema_upload,
    SchemaStoreError,
};
pub use system_schema::{seed_system_schema, SystemSchemaError, SYSTEM_SCHEMA_SOURCE};
pub use tables::knowledge::schema_version::{
    SchemaVersionRow, SCHEMA_ACTIVE_VERSIONS_TABLE, SCHEMA_VERSIONS_TABLE, VALIDATOR_VERSION,
};
pub use statement_ops::{
    allocate_evidence_overflow, evidence_overflow_load, statement_create, statement_get,
    statement_history, statement_list, statement_retract, statement_supersede,
    statement_tombstone, statements_contradicting, StatementListFilter, StatementOpError,
    DEFAULT_LIST_LIMIT,
};
pub use trigram_ops::{
    candidates_for_query, extract_trigrams, index_entity_trigrams, jaccard,
    lookup_candidates_by_trigram, remove_entity_trigrams, trigrams_of_entity, TrigramOpError,
};
