//! Production `SemanticRetriever` impl (phase 23.1).
//!
//! The trait + value types live in `brain-index::semantic_retriever`
//! (kept free of `brain-metadata` so brain-index stays
//! native-buildable on macOS). The impl ties together:
//!
//! - `brain-embed::Dispatcher` — for the `SemanticQuery::Text` path.
//! - `brain-index::SharedHnsw<384>` — substrate memory HNSW
//!   reader handle.
//! - `brain-index::StatementHnswIndex` — statement HNSW
//!   (optional; `None` in v1 until the statement-embedding
//!   worker is wired).
//! - `brain-metadata::MetadataDb` — for HNSW filter push-down
//!   over `MemoryMetadata` rows.

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
use parking_lot::{Mutex, RwLock};
use redb::TableError;

/// Production `SemanticRetriever` impl.
///
/// Cheap to `Clone` — every field is `Arc`-like.
#[derive(Clone)]
pub struct BrainSemanticRetriever {
    embedder: Arc<dyn Dispatcher>,
    memory_index: SharedHnsw<SEMANTIC_VECTOR_DIM>,
    statement_index: Option<Arc<RwLock<StatementHnswIndex>>>,
    metadata: Arc<Mutex<MetadataDb>>,
}

impl BrainSemanticRetriever {
    #[must_use]
    pub fn new(
        embedder: Arc<dyn Dispatcher>,
        memory_index: SharedHnsw<SEMANTIC_VECTOR_DIM>,
        statement_index: Option<Arc<RwLock<StatementHnswIndex>>>,
        metadata: Arc<Mutex<MetadataDb>>,
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
            SemanticQuery::Text(text) => self
                .embedder
                .embed(text)
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
        let metadata = self.metadata.clone();
        let db_guard = metadata.lock();
        let rtxn = db_guard
            .read_txn()
            .map_err(|e| SemanticError::Internal(format!("read_txn: {e}")))?;
        // MEMORIES_TABLE is created lazily on first ENCODE. A query
        // against a freshly opened shard with no memories yet
        // should silently return an empty result, not surface an
        // internal error.
        let table = match rtxn.open_table(MEMORIES_TABLE) {
            Ok(t) => t,
            Err(TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(SemanticError::Internal(format!("open MEMORIES_TABLE: {e}"))),
        };

        let agent_filter = filters.agent_id.map(|a| -> [u8; 16] { a.into() });
        let kind_filter = filters.memory_kind.map(memory_kind_to_u8);
        let created_range = filters.created_at_ms.clone();

        let filter = |id: MemoryId| -> bool {
            let key = id.raw().to_be_bytes();
            let Some(row_guard) = table.get(&key).ok().flatten() else {
                return false;
            };
            let row = row_guard.value();
            if let Some(agent) = agent_filter.as_ref() {
                if row.agent_id_bytes != *agent {
                    return false;
                }
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
            true
        };

        let hits = self
            .memory_index
            .search(vector, config.top_k, Some(config.ef_search), filter);
        drop(rtxn);
        drop(db_guard);

        Ok(project_memory_hits(hits, config.similarity_threshold))
    }

    fn search_statement(
        &self,
        vector: &[f32; SEMANTIC_VECTOR_DIM],
        config: &SemanticRetrieverConfig,
        _filters: &SemanticFilters,
    ) -> Result<Vec<RankedItem>, SemanticError> {
        let Some(handle) = self.statement_index.as_ref() else {
            // §23/03 §9: statement HNSW corpus may be empty in
            // v1 until the embedding worker is wired. Silent
            // empty result, not an error.
            return Ok(Vec::new());
        };
        let guard = handle.read();
        let hits = guard
            .search_with_ef(vector, config.top_k, Some(config.ef_search))
            .map_err(|e| SemanticError::Internal(format!("statement search: {e}")))?;
        // v1 has no statement metadata-side filter push-down
        // (§23/03 §5 fallback). Post-search filters would land
        // here if/when needed.
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
        let vector = self.embed(query)?;
        match scope {
            SemanticScope::Memory => self.search_memory(&vector, config, &config.filters.0),
            SemanticScope::Statement => self.search_statement(&vector, config, &config.filters.0),
            SemanticScope::Both => {
                let memory = self.search_memory(&vector, config, &config.filters.0)?;
                let statement = self.search_statement(&vector, config, &config.filters.0)?;
                Ok(self.merge_and_rerank(memory, statement, config))
            }
        }
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
