//! # brain-index
//!
//! Approximate-nearest-neighbour index for Brain, wrapping `hnsw_rs::Hnsw`
//! with the parameters and lifecycle (build, search, snapshot, rebuild)
//! defined in `spec/09_indexing/`.
//!
//! This crate is a **closed leaf**: vectors in, candidates out. It has
//! no dependency on `brain-storage` or `brain-metadata`; the cross-crate
//! composition (rebuilding the HNSW from arena slots + active-memory
//! scans) lives in a higher-layer crate from Phase 7 onward.
//!
//! ## Current surface (sub-task 4.1)
//!
//! - [`IndexParams`] — HNSW knobs with spec defaults
//!   (`M=16, ef_construction=200, ef_search=64, ef_search_max=500`).
//! - [`HnswIndex<D>`] — const-generic over vector dim. Production use
//!   pins `D = `[`VECTOR_DIM`] (= 384 for BGE-small).
//!
//! Later sub-tasks add `MemoryId` mapping (4.2), tombstone filtering
//! (4.3/4.4), persistence (4.5), rebuild (4.6), the recall benchmark
//! (4.7), and the `ArcSwap` concurrency wrapper (4.8).

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod entity_hnsw;
pub mod graph_retriever;
pub mod hnsw;
pub mod idmap;
pub mod params;
pub mod persistence;
pub mod rebuild;
pub mod semantic_retriever;
pub mod shared;
pub mod statement_hnsw;
pub mod tantivy_shard;
pub mod tombstones;

pub use entity_hnsw::{
    EntityHnswError, EntityHnswIndex, EntityHnswParams, RebuildReport as EntityRebuildReport,
};
pub use graph_retriever::{
    proximity_score, validate_depth as validate_graph_depth, Direction, GraphAnchor, GraphError,
    GraphQuery, GraphRetriever, GraphRetrieverConfig, DEFAULT_DEPTH as GRAPH_DEFAULT_DEPTH,
    DEFAULT_MAX_BRANCHING as GRAPH_DEFAULT_MAX_BRANCHING,
    DEFAULT_TIMEOUT_MS as GRAPH_DEFAULT_TIMEOUT_MS, DEFAULT_TOP_K as GRAPH_DEFAULT_TOP_K,
    MAX_DEPTH_HARD_CAP as GRAPH_MAX_DEPTH_HARD_CAP,
};
pub use hnsw::{HnswError, HnswIndex};
pub use idmap::{IdMap, IdMapError};
pub use params::{IndexParams, IndexParamsError, MAX_LAYER, VECTOR_DIM};
pub use rebuild::RebuildReport;
pub use semantic_retriever::{
    project_memory_hits, project_statement_hits,
    validate_filters_for_scope as validate_semantic_filters, SemanticError, SemanticFilters,
    SemanticFiltersConfigSlot, SemanticQuery, SemanticRetriever, SemanticRetrieverConfig,
    SemanticScope, DEFAULT_EF_SEARCH as SEMANTIC_DEFAULT_EF_SEARCH,
    DEFAULT_TIMEOUT_MS as SEMANTIC_DEFAULT_TIMEOUT_MS, DEFAULT_TOP_K as SEMANTIC_DEFAULT_TOP_K,
    EF_SEARCH_MAX as SEMANTIC_EF_SEARCH_MAX, VECTOR_DIM as SEMANTIC_VECTOR_DIM,
};
pub use shared::{FlushReport, PendingEntry, SharedHnsw, Writer};
pub use statement_hnsw::{
    RebuildReport as StatementRebuildReport, StatementHnswError, StatementHnswIndex,
    StatementHnswParams,
};
pub use tantivy_shard::{
    build_analyzer, memory_text_schema, schema_payload_json, statements_schema, BrainSchemaPayload,
    BrainTokenizer, IndexHandle, IndexStatus, LexicalError, LexicalFilters, LexicalQuery,
    LexicalRetriever, LexicalRetrieverConfig, LexicalScope, RankedItem, RankedItemId,
    RebuildReason as TantivyRebuildReason, TantivyLexicalRetriever, TantivyShard,
    TantivyShardError, TantivyShardStartup, BRAIN_SCHEMA_VERSION, BRAIN_TOKENIZER_NAME,
};
pub use tombstones::TombstoneBitmap;
