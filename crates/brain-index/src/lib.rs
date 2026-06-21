//! # brain-index
//!
//! Approximate-nearest-neighbour index for Brain, wrapping `hnsw_rs::Hnsw`
//! with the parameters and lifecycle (build, search, snapshot, rebuild).
//!
//! This crate is a **closed leaf**: vectors in, candidates out. It has
//! no dependency on `brain-storage` or `brain-metadata`; the cross-crate
//! composition (rebuilding the HNSW from arena slots + active-memory
//! scans) lives in a higher-layer crate.
//!
//! - [`IndexParams`] — HNSW knobs with defaults
//!   (`M=16, ef_construction=200, ef_search=64, ef_search_max=500`).
//! - [`HnswIndex`] — full-precision HNSW over the [`VECTOR_DIM`]-dim
//!   (= 384 for BGE-small) memory embeddings, scoring exact cosine.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod arena_reader;
pub mod entity_hnsw;
pub mod graph_retriever;
pub mod hnsw;
pub mod hype_hnsw;
pub mod idmap;
pub mod params;
pub mod persistence;
pub mod pq;
pub mod rebuild;
pub mod semantic_retriever;
pub mod shared;
pub mod statement_hnsw;
pub mod statement_question_hnsw;
pub mod tantivy_shard;
pub mod tombstones;

pub use arena_reader::{null_arena_reader, ArenaReader, NullArenaReader};

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
pub use hype_hnsw::{
    hype_default_params, HypeHnswError, HypeHnswIndex, RebuildReport as HypeRebuildReport,
};
pub use idmap::{IdMap, IdMapError};
pub use params::{IndexParams, IndexParamsError, MAX_LAYER, VECTOR_DIM};
pub use pq::{
    bootstrap_codebook, Codebook, CodebookError, EncodeError, KmeansError, Lut, PqDist, PqParams,
    PqParamsError, SdcTable, BOOTSTRAP_M, PQ_BITS_V1, PQ_CENTROIDS_PER_SUBSPACE,
};
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
