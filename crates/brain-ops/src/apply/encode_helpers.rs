//! Helpers around the ENCODE write path that don't fit in the
//! single-table apply modules.
//!
//! ## What lives here
//!
//! - [`fetch_extractor_context`] — assembles the bounded inferential
//!   context (top-m semantic neighbors + optional rolling summary)
//!   that the LLM extractor's prompt anchors on. The extractor worker
//!   calls this once per memory before invoking the LLM tier; without
//!   it the LLM only sees the memory currently being extracted and
//!   can't anchor predicates like "Alice mentioned earlier".
//!
//! ## Why a helper (not a module per concern)
//!
//! The fetch joins three subsystems — the per-shard
//! `SemanticRetriever`, the `MEMORIES_TABLE` (creation timestamp +
//! same-context filter), and `TEXTS_TABLE` (the neighbor text body).
//! Splitting that across `ops::index`, `apply::memory`, and a future
//! `apply::summary` would scatter what's logically one ENCODE-side
//! preflight. One helper, one redb read transaction, one async hop
//! into the embedder.

use std::time::Instant;

use brain_core::MemoryId;
use brain_extractors::{ExtractorContext, NeighborMemory};
use brain_index::{
    RankedItemId, SemanticFilters, SemanticFiltersConfigSlot, SemanticQuery,
    SemanticRetrieverConfig, SemanticScope,
};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::text::TEXTS_TABLE;

use crate::context::OpsContext;

/// Default top-m neighbors fed into the LLM prompt. Ten is the
/// sweet spot per the W2.3 spec: enough rows to surface relevant
/// prior context without exceeding the ~2k-token neighbor budget
/// (10 entries × ~200 chars ≈ 2k tokens).
pub const DEFAULT_EXTRACTOR_CONTEXT_TOP_M: usize = 10;

/// Per-neighbor text cap. Above this we truncate at the byte boundary
/// (UTF-8-safe) before adding to the prompt. Two hundred chars keeps
/// each row to roughly 50 tokens; 10 rows × 50 ≈ 500 tokens
/// well under the 2k-token neighbor budget.
const NEIGHBOR_TEXT_CHAR_CAP: usize = 200;

/// Knobs for [`fetch_extractor_context`]. Defaults come from
/// [`DEFAULT_EXTRACTOR_CONTEXT_TOP_M`] and `same_context_only = true`;
/// callers override per-deployment as needed.
#[derive(Debug, Clone, Copy)]
pub struct ExtractorContextFetchConfig {
    /// Maximum number of neighbor entries to return. The retriever
    /// is asked for `top_m + 1` rows so the self-match (the memory
    /// being extracted) can be dropped without sacrificing a slot.
    pub top_m: usize,
    /// When true, restrict neighbors to the same `context_id` as the
    /// memory being extracted. The LLM's "this user / this thread"
    /// signal is the high-value channel — cross-context noise dilutes
    /// the prompt.
    pub same_context_only: bool,
}

impl Default for ExtractorContextFetchConfig {
    fn default() -> Self {
        Self {
            top_m: DEFAULT_EXTRACTOR_CONTEXT_TOP_M,
            same_context_only: true,
        }
    }
}

/// Errors `fetch_extractor_context` can surface. Worker-side callers
/// treat any error as a soft fallback to context-free extraction; the
/// taxonomy is preserved so logs and metrics can distinguish a stale
/// HNSW from a missing memory row.
#[derive(Debug, thiserror::Error)]
pub enum ExtractorContextError {
    #[error("memory not found: {0:?}")]
    MemoryNotFound(MemoryId),
    #[error("metadata read failed: {0}")]
    Metadata(String),
    #[error("semantic retrieve failed: {0}")]
    Semantic(String),
}

/// Build the bounded LLM extractor context for `memory_id`.
///
/// Steps:
///   1. Look up the memory row to find its `context_id` (needed for
///      the `same_context_only` filter — `SemanticRetriever` doesn't
///      key on context, so we filter post-search).
///   2. Run the semantic retriever with `cue_text` against the memory
///      HNSW for `top_m + 1` hits.
///   3. Drop the self-match (the memory being extracted is often the
///      top result — including it as its own context is noise).
///   4. Drop hits from other contexts when `same_context_only`.
///   5. For each surviving hit, fetch the neighbor's text body from
///      `TEXTS_TABLE` and its `created_at_unix_nanos` from
///      `MEMORIES_TABLE`. Both reads happen in the same `read_txn`
///      so the snapshot is consistent.
///   6. Truncate each neighbor's text at `NEIGHBOR_TEXT_CHAR_CAP`
///      so the prompt budget can't blow up on long memories.
///   7. The rolling summary is left as `None` — the summarizer
///      worker isn't part of W2.3. The slot is preserved so a future
///      worker can populate it without changing this signature.
pub async fn fetch_extractor_context(
    ctx: &OpsContext,
    memory_id: MemoryId,
    cue_text: &str,
    config: ExtractorContextFetchConfig,
) -> Result<ExtractorContext, ExtractorContextError> {
    let started = Instant::now();
    let retriever = &ctx.semantic_retriever;

    // Step 1 + 5 prep: open one read txn against metadata. The shared
    // `Arc<MetadataDb>` lets every reader path open its own redb read
    // txn without serialising on a wrapping mutex.
    let metadata = ctx.executor.metadata.clone();
    let memory_context_id: u64 = {
        let rtxn = metadata
            .read_txn()
            .map_err(|e| ExtractorContextError::Metadata(format!("read_txn: {e}")))?;
        let table = rtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| ExtractorContextError::Metadata(format!("open MEMORIES_TABLE: {e}")))?;
        let key = memory_id.raw().to_be_bytes();
        let row = table
            .get(&key)
            .map_err(|e| ExtractorContextError::Metadata(format!("memory get: {e}")))?
            .ok_or(ExtractorContextError::MemoryNotFound(memory_id))?;
        row.value().context_id
    };

    // Step 2: top_m + 1 semantic hits over the memory HNSW. The "+1"
    // reserves room for the self-match that we drop in step 3 so a
    // top_m=10 fetch still yields up to 10 real neighbors.
    let cap = config.top_m.saturating_add(1).max(1);
    let cfg = SemanticRetrieverConfig {
        top_k: cap,
        // Retriever default is fine: 64 is already past the spec's
        // sweet spot for top-10 fetches. Lifting it doesn't help
        // recall at this scale.
        ..SemanticRetrieverConfig::default()
    };
    let filters = SemanticFilters::default();
    let cfg = SemanticRetrieverConfig {
        filters: SemanticFiltersConfigSlot(filters),
        ..cfg
    };
    let query = SemanticQuery::Text(cue_text.to_string());
    let hits = retriever
        .retrieve(&query, SemanticScope::Memory, &cfg)
        .map_err(|e| ExtractorContextError::Semantic(format!("{e}")))?;

    // Step 3-6: materialise neighbor entries inside a fresh read txn so
    // step-1's txn doesn't outlive the await above (Glommio shards
    // are single-threaded but we still keep txns short for redb's GC).
    let rtxn = metadata
        .read_txn()
        .map_err(|e| ExtractorContextError::Metadata(format!("read_txn (neighbors): {e}")))?;
    let memories_t = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| ExtractorContextError::Metadata(format!("open MEMORIES_TABLE: {e}")))?;
    let texts_t = rtxn
        .open_table(TEXTS_TABLE)
        .map_err(|e| ExtractorContextError::Metadata(format!("open TEXTS_TABLE: {e}")))?;

    let mut neighbors: Vec<NeighborMemory> = Vec::with_capacity(config.top_m);
    for hit in hits {
        if neighbors.len() >= config.top_m {
            break;
        }
        let RankedItemId::Memory(neighbor_id) = hit.id else {
            // Semantic scope is Memory, but if mixed scopes leak in
            // we silently skip rather than fail the whole fetch.
            continue;
        };
        if neighbor_id == memory_id {
            // Step 3: drop self-match.
            continue;
        }
        let key = neighbor_id.raw().to_be_bytes();
        let row = match memories_t
            .get(&key)
            .map_err(|e| ExtractorContextError::Metadata(format!("memory get: {e}")))?
        {
            Some(r) => r,
            None => continue,
        };
        let row = row.value();
        if config.same_context_only && row.context_id != memory_context_id {
            continue;
        }
        let text_bytes = match texts_t
            .get(&key)
            .map_err(|e| ExtractorContextError::Metadata(format!("text get: {e}")))?
        {
            Some(t) => t.value().to_vec(),
            None => continue,
        };
        let Ok(text) = String::from_utf8(text_bytes) else {
            continue;
        };
        neighbors.push(NeighborMemory {
            memory_id: neighbor_id,
            text: truncate_to_chars(&text, NEIGHBOR_TEXT_CHAR_CAP),
            similarity_score: hit.score,
            created_at_unix_nanos: row.created_at_unix_nanos,
        });
    }
    drop(rtxn);

    tracing::debug!(
        target: "brain_ops::apply::encode_helpers",
        memory_id = ?memory_id,
        neighbor_count = neighbors.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "fetched extractor context (summary not yet wired)",
    );

    Ok(ExtractorContext {
        neighbors,
        // Step 7: rolling summary stub. A future summarizer worker
        // can populate this; today the slot stays None and the LLM
        // prompt skips the summary section.
        summary: None,
    })
}

/// UTF-8-safe truncation at `max_chars` *characters* (not bytes).
/// Returns the full string when it already fits.
fn truncate_to_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out = String::with_capacity(max_chars + 1);
    for ch in s.chars().take(max_chars) {
        out.push(ch);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use brain_core::{AgentId, ContextId, MemoryId, MemoryKind, Salience};
    use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
    use brain_index::{IndexParams, RankedItem, SemanticError, SemanticRetriever, SharedHnsw};
    use brain_metadata::tables::memory::MemoryMetadata;
    use brain_planner::{ExecutorContext, WriterError as PlannerWriterError, WriterHandle};

    // ---------- mocks ----------

    struct NopDispatcher;
    impl Dispatcher for NopDispatcher {
        fn embed(&self, _: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            Ok([0.0; VECTOR_DIM])
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
            Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
        }
        fn fingerprint(&self) -> [u8; 16] {
            [0; 16]
        }
    }

    struct NopWriter;
    impl WriterHandle for NopWriter {
        fn reserve_memory_id<'a>(
            &'a self,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<MemoryId, PlannerWriterError>> + 'a>,
        > {
            Box::pin(async move { Err(PlannerWriterError::Internal("test stub".into())) })
        }
    }

    /// Deterministic stub retriever — returns a pre-baked hit list
    /// regardless of the query. Tests stuff in whatever they want
    /// the retriever to "see" via `with_hits`.
    struct StubRetriever {
        hits: Vec<RankedItem>,
    }

    impl SemanticRetriever for StubRetriever {
        fn retrieve(
            &self,
            _query: &SemanticQuery,
            _scope: SemanticScope,
            _config: &SemanticRetrieverConfig,
        ) -> Result<Vec<RankedItem>, SemanticError> {
            Ok(self.hits.clone())
        }
    }

    fn fresh_ctx() -> (tempfile::TempDir, OpsContext) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("metadata.redb");
        let metadata = Arc::new(brain_metadata::MetadataDb::open(&db_path).unwrap());
        let (shared, _writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let executor = ExecutorContext::new(
            Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
            shared,
            metadata,
            Arc::new(NopWriter) as Arc<dyn WriterHandle>,
        );
        let ops = crate::test_support::ops_context_for_tests_owning_tempdir(executor);
        (dir, ops)
    }

    fn insert_memory(
        ctx: &OpsContext,
        id: MemoryId,
        context_id: ContextId,
        text: &str,
        created_at_unix_nanos: u64,
    ) {
        let wtxn = ctx.executor.metadata.write_txn().unwrap();
        {
            let mut memories = wtxn.open_table(MEMORIES_TABLE).unwrap();
            let row = MemoryMetadata::new_active(
                id,
                AgentId::new(),
                context_id,
                0,
                id.version(),
                MemoryKind::Episodic,
                [0u8; 16],
                Salience::default().raw(),
                text.len() as u32,
                created_at_unix_nanos,
            );
            memories.insert(&id.raw().to_be_bytes(), &row).unwrap();
        }
        {
            let mut texts = wtxn.open_table(TEXTS_TABLE).unwrap();
            texts
                .insert(&id.raw().to_be_bytes(), text.as_bytes())
                .unwrap();
        }
        wtxn.commit().unwrap();
    }

    fn hit(id: MemoryId, score: f32) -> RankedItem {
        RankedItem {
            id: RankedItemId::Memory(id),
            rank: 1,
            score,
            snippet: None,
        }
    }

    #[test]
    fn fetch_extractor_context_returns_top_m_neighbors_excluding_self() {
        let (_dir, mut ops) = fresh_ctx();
        let self_id = MemoryId::pack(0, 1, 0);
        let n1 = MemoryId::pack(0, 2, 0);
        let n2 = MemoryId::pack(0, 3, 0);
        let n3 = MemoryId::pack(0, 4, 0);
        let cx = ContextId(7);
        insert_memory(&ops, self_id, cx, "self", 100);
        insert_memory(&ops, n1, cx, "neighbor one", 200);
        insert_memory(&ops, n2, cx, "neighbor two", 300);
        insert_memory(&ops, n3, cx, "neighbor three", 400);

        let stub = Arc::new(StubRetriever {
            hits: vec![
                hit(self_id, 0.99),
                hit(n1, 0.92),
                hit(n2, 0.84),
                hit(n3, 0.71),
            ],
        });
        ops.semantic_retriever = stub;

        let cfg = ExtractorContextFetchConfig {
            top_m: 10,
            same_context_only: true,
        };
        let ec = futures_lite::future::block_on(fetch_extractor_context(&ops, self_id, "cue", cfg))
            .expect("fetch succeeds");

        assert_eq!(ec.neighbors.len(), 3, "self dropped, three real neighbors");
        let ids: Vec<MemoryId> = ec.neighbors.iter().map(|n| n.memory_id).collect();
        assert!(!ids.contains(&self_id), "self-match must be excluded");
        assert!(ids.contains(&n1));
        assert!(ids.contains(&n2));
        assert!(ids.contains(&n3));
        // Neighbor texts round-trip from storage.
        let texts: Vec<&str> = ec.neighbors.iter().map(|n| n.text.as_str()).collect();
        assert!(texts.contains(&"neighbor one"));
        assert!(texts.contains(&"neighbor two"));
        assert!(texts.contains(&"neighbor three"));
        assert!(ec.summary.is_none(), "summary not yet wired for v1");
    }

    #[test]
    fn fetch_extractor_context_respects_same_context_only() {
        let (_dir, mut ops) = fresh_ctx();
        let self_id = MemoryId::pack(0, 1, 0);
        let near_ctx = MemoryId::pack(0, 2, 0);
        let far_ctx = MemoryId::pack(0, 3, 0);
        insert_memory(&ops, self_id, ContextId(7), "self", 100);
        insert_memory(&ops, near_ctx, ContextId(7), "same-context neighbor", 200);
        insert_memory(&ops, far_ctx, ContextId(8), "other-context neighbor", 300);

        let stub = Arc::new(StubRetriever {
            hits: vec![hit(near_ctx, 0.9), hit(far_ctx, 0.8)],
        });
        ops.semantic_retriever = stub;

        let cfg = ExtractorContextFetchConfig {
            top_m: 10,
            same_context_only: true,
        };
        let ec = futures_lite::future::block_on(fetch_extractor_context(&ops, self_id, "cue", cfg))
            .expect("fetch succeeds");
        assert_eq!(ec.neighbors.len(), 1, "cross-context neighbor dropped");
        assert_eq!(ec.neighbors[0].memory_id, near_ctx);

        // Same fixture, same_context_only=false → far-context neighbor lands too.
        let cfg = ExtractorContextFetchConfig {
            top_m: 10,
            same_context_only: false,
        };
        let ec = futures_lite::future::block_on(fetch_extractor_context(&ops, self_id, "cue", cfg))
            .expect("fetch succeeds");
        assert_eq!(ec.neighbors.len(), 2);
    }

    #[test]
    fn fetch_extractor_context_returns_empty_for_first_memory() {
        let (_dir, mut ops) = fresh_ctx();
        let self_id = MemoryId::pack(0, 1, 0);
        insert_memory(&ops, self_id, ContextId(7), "only memory", 100);

        // Retriever returns only the self-match — which we drop.
        let stub = Arc::new(StubRetriever {
            hits: vec![hit(self_id, 0.99)],
        });
        ops.semantic_retriever = stub;

        let cfg = ExtractorContextFetchConfig::default();
        let ec = futures_lite::future::block_on(fetch_extractor_context(&ops, self_id, "cue", cfg))
            .expect("fetch succeeds");
        assert!(
            ec.neighbors.is_empty(),
            "first memory has no priors to anchor on",
        );
        assert!(ec.summary.is_none());
        assert!(ec.is_empty());
    }

    #[test]
    fn fetch_extractor_context_caps_at_top_m() {
        let (_dir, mut ops) = fresh_ctx();
        let self_id = MemoryId::pack(0, 1, 0);
        insert_memory(&ops, self_id, ContextId(7), "self", 100);
        let mut hits = vec![hit(self_id, 0.99)];
        for slot in 2..=15u64 {
            let id = MemoryId::pack(0, slot, 0);
            insert_memory(&ops, id, ContextId(7), &format!("n{slot}"), 100 + slot);
            hits.push(hit(id, 0.9 - (slot as f32) * 0.01));
        }
        ops.semantic_retriever = Arc::new(StubRetriever { hits });

        let cfg = ExtractorContextFetchConfig {
            top_m: 5,
            same_context_only: true,
        };
        let ec = futures_lite::future::block_on(fetch_extractor_context(&ops, self_id, "cue", cfg))
            .expect("fetch succeeds");
        assert_eq!(ec.neighbors.len(), 5, "top_m is the hard cap");
    }

    #[test]
    fn truncate_to_chars_preserves_short_strings() {
        assert_eq!(truncate_to_chars("hello", 200), "hello");
    }

    #[test]
    fn truncate_to_chars_clips_long_strings_utf8_safe() {
        let s = "héllo🌍".repeat(50); // 6 chars repeated → 300 chars
        let out = truncate_to_chars(&s, 10);
        // 10 source chars + ellipsis.
        assert_eq!(out.chars().count(), 11);
        assert!(out.ends_with('…'));
    }
}
