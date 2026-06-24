//! # brain-metadata
//!
//! redb-backed metadata store: agents, contexts, memory metadata, edges,
//! idempotency table, and the durable LSN checkpoint. WAL recovery
//! lives in `recovery/` (one file per WalPayload family), implementing
//! `brain_storage::recovery::MetadataSink` for `MetadataDb`.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod api_keys;
pub mod audit;
pub mod cascade;
pub mod db;
pub mod entity;
pub mod extraction;
pub mod extractor;
pub mod hype;
pub mod llm_cache;
pub mod recovery;
pub mod relation;
pub mod schema;
pub mod statement;
pub mod statement_question;
pub mod storage_version;
pub mod system_schema;
pub mod tables;

// Item-level re-exports surface frequently-used functions and types at the
// crate root so callers can import them directly instead of walking the
// per-domain directories on every import.
pub use api_keys::{
    api_key_create, api_key_list_for_agent, api_key_lookup_by_hash, api_key_lookup_by_secret,
    api_key_revoke, api_key_touch_last_used, hash_secret, ApiKeyDb, ApiKeyError, ResolvedScope,
};
pub use audit::ops::{
    audit_by_extractor, audit_by_memory, audit_get, audit_recent, audit_recent_failures,
    audit_write, AuditOpError,
};
pub use db::{MetadataDb, MetadataDbError};
pub use entity::ops::{
    entity_add_alias, entity_get, entity_list_by_type, entity_lookup_by_alias,
    entity_lookup_by_canonical_name, entity_put, entity_remove_alias, entity_rename,
    entity_resolve_canonical_all_types, entity_resolve_canonical_all_types_wtxn,
    entity_resolve_scored, entity_tombstone, entity_update, normalize_name, EntityOpError,
    READ_RESOLVE_TRIGRAM_FLOOR,
};
pub use entity::review::{
    enqueue_merge_proposal, list_proposals_by_status, proposal_get, proposal_get_inside_wtxn,
    update_proposal_recheck, update_proposal_status, MergeReviewError,
};
pub use entity::trigram::{
    candidates_for_query, extract_trigrams, index_entity_trigrams, jaccard,
    lookup_candidates_by_trigram, remove_entity_trigrams, trigrams_of_entity, TrigramOpError,
};
pub use entity::types::{
    entity_type_intern, entity_type_label_qnames, entity_type_lookup_by_name, EntityTypeOpError,
};
pub use extraction::queue::{
    extraction_queue_drain, extraction_queue_enqueue, extraction_queue_len,
    extraction_queue_remove, ExtractionQueueError,
};
pub use extractor::ops::{
    extractor_get, extractor_intern, extractor_list, extractor_lookup_by_qname,
    extractor_set_enabled, ExtractorOpError,
};
pub use hype::ops::{
    hype_has_vectors, hype_iter_all_vectors, hype_neighborhood_hash_get,
    hype_neighborhood_hash_put, hype_vector_put, hype_vectors_delete_memory, HypeOpError,
    HypeRebuildRow,
};
pub use llm_cache::{
    sweep_expired as llm_cache_sweep_expired, LlmCacheDb, LlmCacheError, LlmResponse,
};
pub use relation::ops::{
    relation_create, relation_get, relation_history, relation_list_from, relation_list_to,
    relation_supersede, relation_tombstone, relations_with_evidence, RelationListFilter,
    RelationOpError,
};
pub use relation::traversal::{
    traverse, TraversalConfig, TraversalDirection, TraversalPath, TraversalStep,
    DEFAULT_MAX_BRANCHING, DEFAULT_MAX_DEPTH, MAX_BRANCHING, MAX_DEPTH, MAX_TOTAL_VISITED,
};
pub use relation::types::{
    relation_type_embedding_get, relation_type_embedding_put, relation_type_get,
    relation_type_intern, relation_type_list, relation_type_lookup_by_qname, RelationTypeOpError,
};
pub use schema::apply::{apply_schema_definitions, SchemaApplyError};
pub use schema::kind::{
    kind_behavior, kind_intern, kind_list, render_declared_kinds_block, KindOpError,
};
pub use schema::predicate::{
    predicate_embedding_get, predicate_embedding_put, predicate_get, predicate_intern,
    predicate_list, predicate_lookup_by_qname, predicate_review_list, predicate_review_record,
    render_declared_predicates_block, PredicateOpError,
};
pub use schema::store::{
    schema_active, schema_active_row, schema_get, schema_list, schema_namespaces, schema_upload,
    SchemaStoreError,
};
pub use statement::{
    allocate_evidence_overflow, evidence_overflow_load, pack_evidence_entries, pack_evidence_ids,
    read_evidence_entries_w, read_evidence_ids, read_evidence_ids_w, reclaim_evidence_overflow,
    statement_create, statement_get, statement_history, statement_list, statement_retract,
    statement_supersede, statement_tombstone, statements_contradicting, StatementListFilter,
    StatementOpError, DEFAULT_LIST_LIMIT,
};
pub use system_schema::{seed_system_schema, SystemSchemaError, SYSTEM_SCHEMA_SOURCE};
pub use tables::extractor_audit::{
    audit_count as pipeline_audit_count, extraction_attempts as pipeline_extraction_attempts,
    extraction_retry_due as pipeline_extraction_retry_due, failure_class,
    has_extracted as pipeline_has_extracted, pipeline_audit_entry, pipeline_status,
    record_extracted as pipeline_record_extracted, retry_backoff_nanos, tier_status,
    ExtractorItemCounts, ExtractorPipelineAuditEntry, ExtractorPipelineAuditError,
    EXTRACTOR_PIPELINE_AUDIT_TABLE,
};
pub use tables::schema_version::{
    SchemaVersionRow, SCHEMA_ACTIVE_VERSIONS_TABLE, SCHEMA_VERSIONS_TABLE, VALIDATOR_VERSION,
};
