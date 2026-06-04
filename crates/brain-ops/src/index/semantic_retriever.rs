//! Production `SemanticRetriever` impl.
//!
//! The trait + value types live in `brain-index::semantic_retriever`
//! (kept free of `brain-metadata` so brain-index stays
//! native-buildable on macOS). The impl ties together:
//!
//! - `brain-embed::Dispatcher` — for the `SemanticQuery::Text` path.
//! - `brain-index::SharedHnsw` — substrate memory HNSW
//!   reader handle.
//! - `brain-index::StatementHnswIndex` — statement HNSW
//!   (optional; `None` in v1 until the statement-embedding
//!   worker is wired).
//! - `brain-metadata::MetadataDb` — for HNSW filter push-down
//!   over `MemoryMetadata` rows.

use std::collections::HashSet;
use std::sync::Arc;

use brain_core::MemoryId;
use brain_embed::Dispatcher;
use brain_index::statement_hnsw::StatementHnswIndex;
use brain_index::{
    project_memory_hits, project_statement_hits, validate_semantic_filters, RankedItem,
    SemanticError, SemanticFilters, SemanticQuery, SemanticRetriever, SemanticRetrieverConfig,
    SemanticScope, SharedHnsw, SEMANTIC_EF_SEARCH_MAX, SEMANTIC_VECTOR_DIM,
};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::MetadataDb;
use parking_lot::RwLock;

/// Production `SemanticRetriever` impl.
///
/// Cheap to `Clone` — every field is `Arc`-like.
#[derive(Clone)]
pub struct BrainSemanticRetriever {
    embedder: Arc<dyn Dispatcher>,
    memory_index: SharedHnsw,
    statement_index: Option<Arc<RwLock<StatementHnswIndex>>>,
    metadata: Arc<MetadataDb>,
}

impl BrainSemanticRetriever {
    #[must_use]
    pub fn new(
        embedder: Arc<dyn Dispatcher>,
        memory_index: SharedHnsw,
        statement_index: Option<Arc<RwLock<StatementHnswIndex>>>,
        metadata: Arc<MetadataDb>,
    ) -> Self {
        Self {
            embedder,
            memory_index,
            statement_index,
            metadata,
        }
    }

    fn embed(
        &self,
        query: &SemanticQuery,
    ) -> Result<Box<[f32; SEMANTIC_VECTOR_DIM]>, SemanticError> {
        match query {
            SemanticQuery::Vector(v) => Ok(v.clone()),
            // BGE asymmetric retrieval: the retrieval
            // SemanticRetriever's query path applies the retrieval prefix.
            // The cache keys on input text so this doesn't collide with
            // any stored passage embedding for the same surface.
            SemanticQuery::Text(text) => self
                .embedder
                .embed_query(text)
                .map(Box::new)
                .map_err(|e| SemanticError::EmbedderFailure(e.to_string())),
        }
    }

    fn search_memory(
        &self,
        vector: &[f32; SEMANTIC_VECTOR_DIM],
        config: &SemanticRetrieverConfig,
        filters: &SemanticFilters,
    ) -> Result<Vec<RankedItem>, SemanticError> {
        let rtxn = self
            .metadata
            .read_txn()
            .map_err(|e| SemanticError::Internal(format!("read_txn: {e}")))?;
        let table = rtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| SemanticError::Internal(format!("open MEMORIES_TABLE: {e}")))?;

        let agent_filter: HashSet<[u8; 16]> =
            filters.agent_ids.iter().map(|a| (*a).into()).collect();
        let kind_filter = filters.memory_kind.map(memory_kind_to_u8);
        let created_range = filters.created_at_ms.clone();
        let context_filter = filters.context_ids.clone();

        let filter = |id: MemoryId| -> bool {
            let key = id.raw().to_be_bytes();
            let Some(row_guard) = table.get(&key).ok().flatten() else {
                return false;
            };
            let row = row_guard.value();
            if !agent_filter.is_empty() && !agent_filter.contains(&row.agent_id_bytes) {
                return false;
            }
            if let Some(kind) = kind_filter {
                if row.kind != kind {
                    return false;
                }
            }
            if let Some(range) = created_range.as_ref() {
                let ms = row.created_at_unix_nanos / 1_000_000;
                if !range.contains(&ms) {
                    return false;
                }
            }
            if !context_filter.is_empty() && !context_filter.contains(&row.context_id) {
                return false;
            }
            true
        };

        let hits = self
            .memory_index
            .search(vector, config.top_k, Some(config.ef_search), filter);
        drop(rtxn);

        Ok(project_memory_hits(hits, config.similarity_threshold))
    }

    fn search_statement(
        &self,
        vector: &[f32; SEMANTIC_VECTOR_DIM],
        config: &SemanticRetrieverConfig,
        _filters: &SemanticFilters,
    ) -> Result<Vec<RankedItem>, SemanticError> {
        let Some(handle) = self.statement_index.as_ref() else {
            // Statement HNSW corpus may be empty in
            // v1 until the embedding worker is wired. Silent
            // empty result, not an error.
            return Ok(Vec::new());
        };
        let guard = handle.read();
        let hits = guard
            .search_with_ef(vector, config.top_k, Some(config.ef_search))
            .map_err(|e| SemanticError::Internal(format!("statement search: {e}")))?;
        // v1 has no statement metadata-side filter push-down.
        // Post-search filters would land here if/when needed.
        Ok(project_statement_hits(hits, config.similarity_threshold))
    }

    fn merge_and_rerank(
        &self,
        memory: Vec<RankedItem>,
        statement: Vec<RankedItem>,
        config: &SemanticRetrieverConfig,
    ) -> Vec<RankedItem> {
        let mut combined: Vec<RankedItem> = memory.into_iter().chain(statement).collect();
        combined.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        combined.truncate(config.top_k);
        for (i, item) in combined.iter_mut().enumerate() {
            item.rank = (i as u32) + 1;
        }
        combined
    }
}

impl SemanticRetriever for BrainSemanticRetriever {
    fn retrieve(
        &self,
        query: &SemanticQuery,
        scope: SemanticScope,
        config: &SemanticRetrieverConfig,
    ) -> Result<Vec<RankedItem>, SemanticError> {
        validate_semantic_filters(&config.filters.0, scope)?;
        if config.ef_search > SEMANTIC_EF_SEARCH_MAX {
            return Err(SemanticError::QueryParseFailed(format!(
                "ef_search {} exceeds cap {SEMANTIC_EF_SEARCH_MAX}",
                config.ef_search
            )));
        }
        let t_embed = std::time::Instant::now();
        let vector = self.embed(query)?;
        let embed_us = t_embed.elapsed().as_micros();

        let t_search = std::time::Instant::now();
        let result = match scope {
            SemanticScope::Memory => self.search_memory(&vector, config, &config.filters.0),
            SemanticScope::Statement => self.search_statement(&vector, config, &config.filters.0),
            SemanticScope::Both => {
                let memory = self.search_memory(&vector, config, &config.filters.0)?;
                let statement = self.search_statement(&vector, config, &config.filters.0)?;
                Ok(self.merge_and_rerank(memory, statement, config))
            }
        };
        let search_us = t_search.elapsed().as_micros();

        // Surface the embed/search split. The 50→1000 ms budget bump
        // hides the embed cost from the WARN; this debug line lets an
        // operator confirm whether a slow recall is embedder-bound,
        // index-bound, or filter-bound.
        tracing::debug!(
            target: "brain_ops::semantic_retriever",
            ?scope,
            embed_us = embed_us as u64,
            search_us = search_us as u64,
            "semantic retrieve timing",
        );
        result
    }
}

fn memory_kind_to_u8(kind: brain_core::MemoryKind) -> u8 {
    // Mirror brain-metadata::tables::memory::memory_kind_to_u8
    // (which is `pub(crate)` so we duplicate the 3-arm match
    // here rather than expose it crate-wide).
    match kind {
        brain_core::MemoryKind::Episodic => 0,
        brain_core::MemoryKind::Semantic => 1,
        brain_core::MemoryKind::Consolidated => 2,
    }
}

#[cfg(test)]
mod tests;
