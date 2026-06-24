//! redb table definitions and value types, one module per table.
//!
//! The catalog is 13 domain tables plus one internal `__schema_meta`
//! from [`crate::storage_version`].

pub mod agent;
pub mod api_keys;
pub mod audit;
pub mod checkpoint;
pub mod context;
pub mod contradiction;
pub mod edge;
pub mod entity;
pub mod entity_type;
pub mod extraction_queue;
pub mod extractor;
pub mod extractor_audit;
pub mod fingerprint;
pub mod hype;
pub mod idempotency;
pub mod kind;
pub mod memory;
pub mod merge;
pub mod merge_review_queue;
pub mod model_fingerprint;
pub mod next_lsn;
pub mod predicate;
pub mod relation;
pub mod relation_type;
pub mod schema_version;
pub mod slot_version;
pub mod statement;
pub mod statement_question;
pub mod text;
pub mod worker_checkpoints;

/// Boilerplate `redb::Value` impl for an rkyv-archived struct.
///
/// Each value type in the phase bodies uses the same encoding
/// pattern (rkyv with `check_bytes`, deserialize-on-read, type_name
/// versioned with `::v1`). This macro emits that impl from the type
/// name and a stable `type_name` string.
///
/// Mirrors the per-file impl in substrate tables (`agent.rs`,
/// `memory.rs`); collapsed into a macro here because 11 opaque-body
/// value structs share the exact same body.
#[macro_export]
macro_rules! impl_redb_rkyv_value {
    ($ty:ty, $type_name:literal) => {
        impl ::redb::Value for $ty {
            type SelfType<'a> = $ty;
            type AsBytes<'a> = ::std::vec::Vec<u8>;

            fn fixed_width() -> Option<usize> {
                None
            }

            fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
            where
                Self: 'a,
            {
                let mut buf = ::rkyv::AlignedVec::with_capacity(data.len());
                buf.extend_from_slice(data);
                ::rkyv::from_bytes::<$ty>(&buf).expect(concat!(
                    stringify!($ty),
                    " bytes failed rkyv validation; redb file is corrupt"
                ))
            }

            fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
            where
                Self: 'a,
                Self: 'b,
            {
                ::rkyv::to_bytes::<_, 256>(value)
                    .expect(concat!(stringify!($ty), " is rkyv-serializable"))
                    .into_vec()
            }

            fn type_name() -> ::redb::TypeName {
                ::redb::TypeName::new($type_name)
            }
        }
    };
}

/// Open every main-metadata-DB table inside `wtxn` so redb materializes
/// them. After this returns, every read path can call `open_table(T)`
/// without a `TableDoesNotExist` fallback — the contract enforced by
/// [`crate::storage_version::open_or_init_schema`].
///
/// Tables that live in their own redb files (api_keys, llm_cache)
/// self-init inside their own `open()` constructors and are NOT listed
/// here.
pub fn materialize_all_tables(wtxn: &::redb::WriteTransaction) -> Result<(), ::redb::TableError> {
    use agent::AGENTS_TABLE;
    use audit::{
        ENTITY_RESOLUTION_AUDIT_TABLE, EXTRACTOR_AUDIT_BY_EXTRACTOR_TABLE,
        EXTRACTOR_AUDIT_BY_MEMORY_TABLE, EXTRACTOR_AUDIT_BY_TIME_TABLE, EXTRACTOR_AUDIT_TABLE,
    };
    use checkpoint::CHECKPOINTS_TABLE;
    use context::{AGENT_CONTEXTS_TABLE, CONTEXTS_TABLE, CONTEXT_NAMES_TABLE};
    use contradiction::STATEMENT_CONTRADICTION_AUDIT_TABLE;
    use edge::{EDGES_REVERSE_TABLE, EDGES_TABLE};
    use entity::{
        ENTITIES_TABLE, ENTITY_ALIASES_TABLE, ENTITY_BY_CANONICAL_NAME_TABLE,
        ENTITY_MENTIONS_TABLE, ENTITY_TRIGRAMS_TABLE, ENTITY_VECTORS_TABLE,
    };
    use entity_type::ENTITY_TYPES_TABLE;
    use extraction_queue::EXTRACTION_QUEUE_TABLE;
    use extractor::{EXTRACTORS_BY_QNAME_TABLE, EXTRACTORS_TABLE};
    use extractor_audit::EXTRACTOR_PIPELINE_AUDIT_TABLE;
    use fingerprint::FINGERPRINTS_TABLE;
    use idempotency::IDEMPOTENCY_TABLE;
    use kind::{KINDS_BY_BYTE_TABLE, KINDS_TABLE};
    use memory::{MEMORIES_BY_AGENT_TIMELINE_TABLE, MEMORIES_TABLE};
    use merge::{ENTITY_MERGE_AUDIT_OVERFLOW, MERGE_LOG_TABLE};
    use merge_review_queue::{MERGE_REVIEW_BY_STATUS_TABLE, MERGE_REVIEW_QUEUE_TABLE};
    use model_fingerprint::MODEL_FINGERPRINTS_TABLE;
    use next_lsn::NEXT_LSN_TABLE;
    use predicate::{PREDICATES_BY_QNAME_TABLE, PREDICATES_TABLE, PREDICATE_EMBEDDINGS_TABLE};
    use relation::{
        RELATION_BY_EVIDENCE_TABLE, RELATION_METADATA_TABLE, RELATION_TYPE_EMBEDDINGS_TABLE,
    };
    use relation_type::{RELATION_TYPES_BY_QNAME_TABLE, RELATION_TYPES_TABLE};
    use schema_version::{SCHEMA_ACTIVE_VERSIONS_TABLE, SCHEMA_VERSIONS_TABLE};
    use slot_version::SLOT_VERSIONS_TABLE;
    use statement::{
        EVIDENCE_OVERFLOW_TABLE, STATEMENTS_BY_EVENT_TIME_TABLE, STATEMENTS_BY_EVIDENCE_TABLE,
        STATEMENTS_BY_OBJECT_ENTITY_TABLE, STATEMENTS_BY_PREDICATE_TABLE,
        STATEMENTS_BY_SUBJECT_TABLE, STATEMENTS_TABLE, STATEMENT_CHAIN_TABLE,
        STATEMENT_EMBED_QUEUE_TABLE,
    };
    use statement_question::STATEMENT_QUESTION_VECTORS_TABLE;
    use text::TEXTS_TABLE;
    use worker_checkpoints::WORKER_CHECKPOINTS_TABLE;

    let _ = wtxn.open_table(AGENTS_TABLE)?;
    let _ = wtxn.open_table(EXTRACTOR_AUDIT_TABLE)?;
    let _ = wtxn.open_table(EXTRACTOR_AUDIT_BY_MEMORY_TABLE)?;
    let _ = wtxn.open_table(EXTRACTOR_AUDIT_BY_EXTRACTOR_TABLE)?;
    let _ = wtxn.open_table(EXTRACTOR_AUDIT_BY_TIME_TABLE)?;
    let _ = wtxn.open_table(ENTITY_RESOLUTION_AUDIT_TABLE)?;
    let _ = wtxn.open_table(STATEMENT_CONTRADICTION_AUDIT_TABLE)?;
    let _ = wtxn.open_table(CHECKPOINTS_TABLE)?;
    let _ = wtxn.open_table(CONTEXTS_TABLE)?;
    let _ = wtxn.open_table(CONTEXT_NAMES_TABLE)?;
    let _ = wtxn.open_table(AGENT_CONTEXTS_TABLE)?;
    let _ = wtxn.open_table(EDGES_TABLE)?;
    let _ = wtxn.open_table(EDGES_REVERSE_TABLE)?;
    let _ = wtxn.open_table(ENTITY_TYPES_TABLE)?;
    let _ = wtxn.open_table(ENTITIES_TABLE)?;
    let _ = wtxn.open_table(ENTITY_BY_CANONICAL_NAME_TABLE)?;
    let _ = wtxn.open_table(ENTITY_ALIASES_TABLE)?;
    let _ = wtxn.open_table(ENTITY_TRIGRAMS_TABLE)?;
    let _ = wtxn.open_table(ENTITY_MENTIONS_TABLE)?;
    let _ = wtxn.open_table(ENTITY_VECTORS_TABLE)?;
    let _ = wtxn.open_table(EXTRACTION_QUEUE_TABLE)?;
    let _ = wtxn.open_table(EXTRACTORS_TABLE)?;
    let _ = wtxn.open_table(EXTRACTORS_BY_QNAME_TABLE)?;
    let _ = wtxn.open_table(EXTRACTOR_PIPELINE_AUDIT_TABLE)?;
    let _ = wtxn.open_table(FINGERPRINTS_TABLE)?;
    let _ = wtxn.open_table(IDEMPOTENCY_TABLE)?;
    let _ = wtxn.open_table(KINDS_TABLE)?;
    let _ = wtxn.open_table(KINDS_BY_BYTE_TABLE)?;
    let _ = wtxn.open_table(MEMORIES_TABLE)?;
    let _ = wtxn.open_table(MEMORIES_BY_AGENT_TIMELINE_TABLE)?;
    let _ = wtxn.open_table(MERGE_LOG_TABLE)?;
    let _ = wtxn.open_table(ENTITY_MERGE_AUDIT_OVERFLOW)?;
    let _ = wtxn.open_table(MERGE_REVIEW_QUEUE_TABLE)?;
    let _ = wtxn.open_table(MERGE_REVIEW_BY_STATUS_TABLE)?;
    let _ = wtxn.open_table(MODEL_FINGERPRINTS_TABLE)?;
    let _ = wtxn.open_table(NEXT_LSN_TABLE)?;
    let _ = wtxn.open_table(PREDICATES_TABLE)?;
    let _ = wtxn.open_table(PREDICATES_BY_QNAME_TABLE)?;
    let _ = wtxn.open_table(PREDICATE_EMBEDDINGS_TABLE)?;
    let _ = wtxn.open_table(RELATION_METADATA_TABLE)?;
    let _ = wtxn.open_table(RELATION_BY_EVIDENCE_TABLE)?;
    let _ = wtxn.open_table(RELATION_TYPES_TABLE)?;
    let _ = wtxn.open_table(RELATION_TYPES_BY_QNAME_TABLE)?;
    let _ = wtxn.open_table(RELATION_TYPE_EMBEDDINGS_TABLE)?;
    let _ = wtxn.open_table(SCHEMA_VERSIONS_TABLE)?;
    let _ = wtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
    let _ = wtxn.open_table(SLOT_VERSIONS_TABLE)?;
    let _ = wtxn.open_table(STATEMENTS_TABLE)?;
    let _ = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
    let _ = wtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE)?;
    let _ = wtxn.open_table(STATEMENTS_BY_OBJECT_ENTITY_TABLE)?;
    let _ = wtxn.open_table(STATEMENTS_BY_EVENT_TIME_TABLE)?;
    let _ = wtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE)?;
    let _ = wtxn.open_table(STATEMENT_CHAIN_TABLE)?;
    let _ = wtxn.open_table(EVIDENCE_OVERFLOW_TABLE)?;
    let _ = wtxn.open_table(STATEMENT_EMBED_QUEUE_TABLE)?;
    let _ = wtxn.open_table(STATEMENT_QUESTION_VECTORS_TABLE)?;
    let _ = wtxn.open_table(TEXTS_TABLE)?;
    let _ = wtxn.open_table(WORKER_CHECKPOINTS_TABLE)?;
    Ok(())
}

#[cfg(all(test, not(miri)))]
pub(crate) fn fresh_db(dir: &tempfile::TempDir) -> redb::Database {
    let db = redb::Database::create(dir.path().join("test.redb")).expect("create redb");
    // Materialize every table once on creation so read-only tests
    // (counting rows on empty tables, missing-key lookups, etc.)
    // don't trip TableDoesNotExist. Re-opening an existing table is a
    // no-op, so this stays idempotent across repeated calls.
    {
        let wtxn = db.begin_write().expect("begin_write");
        materialize_all_tables(&wtxn).expect("materialize");
        wtxn.commit().expect("commit");
    }
    db
}
