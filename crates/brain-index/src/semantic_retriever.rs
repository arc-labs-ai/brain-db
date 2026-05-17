//! Semantic retriever trait + value types (phase 23.1).
//!
//! The trait surface defined here matches
//! `spec/23_retrievers/03_semantic_retriever.md`. The
//! production impl (`BrainSemanticRetriever`) lives in
//! `brain-ops::ops::retrievers::semantic` because it needs
//! both the HNSW handles defined here and the `MetadataDb`
//! read paths exposed by `brain-metadata` — and we keep
//! `brain-index` free of the `brain-metadata` → `brain-storage`
//! → `glommio` Linux-only transitive dep so this crate stays
//! buildable on a developer's macOS host.

use std::ops::RangeInclusive;

use brain_core::knowledge::StatementKind;
use brain_core::{AgentId, MemoryKind, PredicateId};

use crate::tantivy_shard::{RankedItem, RankedItemId};

pub use crate::tantivy_shard::RankedItem as SemanticRankedItem;
pub use crate::tantivy_shard::RankedItemId as SemanticRankedItemId;

/// 384-dim vectors per BGE-small (§05/05).
pub const VECTOR_DIM: usize = 384;

/// Substrate HNSW default `ef_search` (§06/02).
pub const DEFAULT_EF_SEARCH: usize = 64;

/// Hard cap on `ef_search` (§06/02 §5).
pub const EF_SEARCH_MAX: usize = 500;

/// Default top-k (§23/03 §3).
pub const DEFAULT_TOP_K: usize = 64;

/// Default timeout (§23/03 §3).
pub const DEFAULT_TIMEOUT_MS: u32 = 50;

/// The semantic-retrieval trait. Object-safe; consumers hold
/// an `Arc<dyn SemanticRetriever>`.
pub trait SemanticRetriever: Send + Sync {
    fn retrieve(
        &self,
        query: &SemanticQuery,
        scope: SemanticScope,
        config: &SemanticRetrieverConfig,
    ) -> Result<Vec<RankedItem>, SemanticError>;
}

/// Query input — either a pre-embedded 384-d vector or raw
/// text that the retriever embeds on demand.
#[derive(Debug, Clone)]
pub enum SemanticQuery {
    /// Caller-provided 384-d L2-normalised vector.
    Vector(Box<[f32; VECTOR_DIM]>),
    /// Text input; the retriever calls into `brain-embed`'s
    /// `Dispatcher` to encode it before searching.
    Text(String),
}

/// Which corpus to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticScope {
    /// Substrate memory HNSW (§06). Returns `RankedItemId::Memory`.
    Memory,
    /// Statement HNSW (§26/00). Returns `RankedItemId::Statement`.
    Statement,
    /// Both corpora; results merged by descending cosine.
    Both,
}

/// Filters applied either as HNSW push-down (memory scope,
/// per §23/03 §5) or post-search.
#[derive(Debug, Clone, Default)]
pub struct SemanticFilters {
    pub agent_id: Option<AgentId>,
    pub memory_kind: Option<MemoryKind>,
    pub statement_kind: Option<StatementKind>,
    pub predicate_id: Option<PredicateId>,
    pub confidence_bucket: Option<RangeInclusive<u8>>,
    pub created_at_ms: Option<RangeInclusive<u64>>,
    pub extracted_at_ms: Option<RangeInclusive<u64>>,
}

/// HNSW search config + post-search cuts.
#[derive(Debug, Clone)]
pub struct SemanticRetrieverConfig {
    pub top_k: usize,
    pub ef_search: usize,
    pub similarity_threshold: f32,
    pub timeout_ms: u32,
    pub filters: SemanticFiltersConfigSlot,
}

/// Inline-friendly wrapper. We pass filters through the
/// config (not the query) so callers can hold a single value
/// across multiple retrieve calls while varying the query
/// text. `Default` is filter-free.
#[derive(Debug, Clone, Default)]
pub struct SemanticFiltersConfigSlot(pub SemanticFilters);

impl Default for SemanticRetrieverConfig {
    fn default() -> Self {
        Self {
            top_k: DEFAULT_TOP_K,
            ef_search: DEFAULT_EF_SEARCH,
            similarity_threshold: 0.0,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            filters: SemanticFiltersConfigSlot::default(),
        }
    }
}

/// Error taxonomy (§23/03 §7).
#[derive(Debug, thiserror::Error)]
pub enum SemanticError {
    #[error("index unavailable (rebuild in progress)")]
    IndexUnavailable,
    #[error("query parse failed: {0}")]
    QueryParseFailed(String),
    #[error("query timed out after {0} ms")]
    Timeout(u32),
    #[error("embedder fingerprint mismatch")]
    EmbedderFingerprintMismatch,
    #[error("embedder failure: {0}")]
    EmbedderFailure(String),
    #[error("internal: {0}")]
    Internal(String),
}

/// Validate scope + filter compatibility per §23/03 §5.
/// Wrong-scope filter (e.g. `predicate_id` with `Memory` scope)
/// returns `QueryParseFailed`.
pub fn validate_filters_for_scope(
    filters: &SemanticFilters,
    scope: SemanticScope,
) -> Result<(), SemanticError> {
    match scope {
        SemanticScope::Memory => {
            if filters.statement_kind.is_some() {
                return Err(SemanticError::QueryParseFailed(
                    "statement_kind filter applies only to Statement / Both".into(),
                ));
            }
            if filters.predicate_id.is_some() {
                return Err(SemanticError::QueryParseFailed(
                    "predicate_id filter applies only to Statement / Both".into(),
                ));
            }
            if filters.confidence_bucket.is_some() {
                return Err(SemanticError::QueryParseFailed(
                    "confidence_bucket filter applies only to Statement / Both".into(),
                ));
            }
            if filters.extracted_at_ms.is_some() {
                return Err(SemanticError::QueryParseFailed(
                    "extracted_at_ms filter applies only to Statement / Both".into(),
                ));
            }
        }
        SemanticScope::Statement => {
            if filters.agent_id.is_some() {
                return Err(SemanticError::QueryParseFailed(
                    "agent_id filter applies only to Memory / Both".into(),
                ));
            }
            if filters.memory_kind.is_some() {
                return Err(SemanticError::QueryParseFailed(
                    "memory_kind filter applies only to Memory / Both".into(),
                ));
            }
            if filters.created_at_ms.is_some() {
                return Err(SemanticError::QueryParseFailed(
                    "created_at_ms filter applies only to Memory / Both".into(),
                ));
            }
        }
        SemanticScope::Both => {
            // Filters apply per-scope inside Both — no
            // cross-scope rejection.
        }
    }
    Ok(())
}

/// Reusable: project HNSW `(MemoryId, similarity)` hits to
/// `RankedItem` with a similarity-threshold filter + dense
/// 1-based ranks. Used by the Memory impl in `brain-ops`.
#[must_use]
pub fn project_memory_hits(
    hits: Vec<(brain_core::MemoryId, f32)>,
    similarity_threshold: f32,
) -> Vec<RankedItem> {
    let mut out = Vec::with_capacity(hits.len());
    let mut rank: u32 = 0;
    for (id, score) in hits {
        if score < similarity_threshold {
            continue;
        }
        rank += 1;
        out.push(RankedItem {
            id: RankedItemId::Memory(id),
            rank,
            score,
            snippet: None,
        });
    }
    out
}

/// Same shape for statement scope.
#[must_use]
pub fn project_statement_hits(
    hits: Vec<(brain_core::StatementId, f32)>,
    similarity_threshold: f32,
) -> Vec<RankedItem> {
    let mut out = Vec::with_capacity(hits.len());
    let mut rank: u32 = 0;
    for (id, score) in hits {
        if score < similarity_threshold {
            continue;
        }
        rank += 1;
        out.push(RankedItem {
            id: RankedItemId::Statement(id),
            rank,
            score,
            snippet: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_wrong_scope_filter() {
        let mut f = SemanticFilters::default();
        f.statement_kind = Some(StatementKind::Fact);
        let err = validate_filters_for_scope(&f, SemanticScope::Memory).expect_err("rejects");
        assert!(matches!(err, SemanticError::QueryParseFailed(_)));
    }

    #[test]
    fn validate_allows_compatible_filter() {
        let mut f = SemanticFilters::default();
        f.memory_kind = Some(MemoryKind::Episodic);
        validate_filters_for_scope(&f, SemanticScope::Memory).expect("ok");
        validate_filters_for_scope(&f, SemanticScope::Both).expect("ok");
    }

    #[test]
    fn project_memory_hits_assigns_dense_ranks() {
        let hits = vec![
            (brain_core::MemoryId::pack(0, 1, 0), 0.95),
            (brain_core::MemoryId::pack(0, 2, 0), 0.80),
            (brain_core::MemoryId::pack(0, 3, 0), 0.50),
        ];
        let out = project_memory_hits(hits, 0.0);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].rank, 1);
        assert_eq!(out[1].rank, 2);
        assert_eq!(out[2].rank, 3);
    }

    #[test]
    fn project_memory_hits_drops_below_threshold() {
        let hits = vec![
            (brain_core::MemoryId::pack(0, 1, 0), 0.95),
            (brain_core::MemoryId::pack(0, 2, 0), 0.60),
        ];
        let out = project_memory_hits(hits, 0.7);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rank, 1);
    }
}
