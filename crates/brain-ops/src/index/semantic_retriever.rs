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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use brain_core::MemoryId;
use brain_embed::Dispatcher;
use brain_index::hype_hnsw::HypeHnswIndex;
use brain_index::statement_hnsw::StatementHnswIndex;
use brain_index::statement_question_hnsw::StatementQuestionHnswIndex;
use brain_index::{
    project_memory_hits, project_statement_hits, validate_semantic_filters, RankedItem,
    RankedItemId, SemanticError, SemanticFilters, SemanticQuery, SemanticRetriever,
    SemanticRetrieverConfig, SemanticScope, SharedHnsw, SpaceVectorSource, SEMANTIC_EF_SEARCH_MAX,
    SEMANTIC_VECTOR_DIM,
};
use brain_metadata::tables::memory::{
    space_timeline_prefix_space, MemoryMetadata, MEMORIES_BY_SPACE_TIMELINE_TABLE, MEMORIES_TABLE,
    SPACE_TIMELINE_KEY_LEN,
};
use brain_metadata::MetadataDb;
use parking_lot::RwLock;

/// Cap on a space's live memory count for the exact brute-force lane. At
/// or below this, a single-space memory query does an exact cosine scan of
/// only that space's arena vectors (recall = 1.0, the filtered shared-HNSW
/// walk misses a sparse tenant at high selectivity). Above it, the query
/// falls through to the shared HNSW path — a lazy per-space HNSW for large
/// spaces is Phase 2. Phase-1 constant (the `[index]` TOML surface does not
/// yet carry an index-tuning block); calibrated by the recall probe.
pub const SPACE_BRUTEFORCE_MAX: u64 = 20_000;

/// Production `SemanticRetriever` impl.
///
/// Cheap to `Clone` — every field is `Arc`-like.
#[derive(Clone)]
pub struct BrainSemanticRetriever {
    embedder: Arc<dyn Dispatcher>,
    memory_index: SharedHnsw,
    statement_index: Option<Arc<RwLock<StatementHnswIndex>>>,
    /// Optional HyPE pool. When wired, a memory-scope search also probes
    /// the hypothetical-question embeddings with the same query vector and
    /// unions the best-per-memory hits into the direct cosine hits —
    /// surfacing memories the user's phrasing matches only via a generated
    /// question. `None` means the shard has no HyPE index (disabled, or no
    /// LLM tier to generate questions).
    hype_index: Option<Arc<RwLock<HypeHnswIndex>>>,
    /// Optional per-statement question-bridge pool. When wired, a
    /// statement-scope search also probes the templated-question embeddings
    /// and unions the best-per-statement hits into the direct statement
    /// cosine hits — the per-statement analogue of `hype_index`. A hit is a
    /// `StatementId`, which the RECALL projector maps back to its evidence
    /// memory. `None` when the bridge capability is off.
    statement_question_index: Option<Arc<RwLock<StatementQuestionHnswIndex>>>,
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
            hype_index: None,
            statement_question_index: None,
            metadata,
        }
    }

    /// Wire the per-statement question-bridge pool. Without this a statement
    /// search uses only direct statement cosine; with it, question-bridge
    /// hits are unioned in (recall-additive). Set by production shards when
    /// the bridge capability is enabled.
    #[must_use]
    pub fn with_statement_question_index(
        mut self,
        index: Arc<RwLock<StatementQuestionHnswIndex>>,
    ) -> Self {
        self.statement_question_index = Some(index);
        self
    }

    /// Wire the HyPE question-vector pool. Without this call a memory
    /// search uses only direct passage cosine; with it, HyPE hits are
    /// unioned in (recall-additive). Production shards set it when HyPE is
    /// enabled; tests and substrate-only deployments leave it unset.
    #[must_use]
    pub fn with_hype_index(mut self, hype_index: Arc<RwLock<HypeHnswIndex>>) -> Self {
        self.hype_index = Some(hype_index);
        self
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
        arena: Option<&dyn SpaceVectorSource>,
    ) -> Result<Vec<RankedItem>, SemanticError> {
        // Single-space brute-force lane. When the query is scoped to exactly
        // one space, the shard read path is active (`arena.is_some()` — the
        // arena handle is the read-path marker, not the vector source; see
        // below), and that space is small, exact-scan only that space's own
        // vectors instead of walking the shared HNSW graph (which misses a
        // sparse tenant at high selectivity). Returns `Some` with the final
        // hits (HyPE union already applied); `None` means "space missing, too
        // large, or no live vectors resolved — use the shared HNSW path".
        //
        // The arena is only the routing signal: its mmap is populated solely
        // by WAL recovery on restart, so a memory encoded in the current run
        // is absent from it. The live by-id vectors live in the redb artifact
        // store (written on the ENCODE ack path), which `brute_force_memory`
        // reads directly — resolving from the arena would drop every same-run
        // memory and return a degraded set.
        if filters.space_ids.len() == 1 && arena.is_some() {
            if let Some(hits) = self.brute_force_memory(vector, config, filters)? {
                return Ok(hits);
            }
        }

        let rtxn = self
            .metadata
            .read_txn()
            .map_err(|e| SemanticError::Internal(format!("read_txn: {e}")))?;
        let table = rtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| SemanticError::Internal(format!("open MEMORIES_TABLE: {e}")))?;

        let namespace_id = filters.namespace_id;
        let space_filter: HashSet<[u8; 16]> =
            filters.space_ids.iter().map(|a| (*a).into()).collect();
        let kind_filter = filters.memory_kind.map(memory_kind_to_u8);
        let created_range = filters.created_at_ms.clone();
        let session_filter = filters.session_ids.clone();
        let include_tombstoned = filters.include_tombstoned;

        let id_passes = |id: MemoryId| -> bool {
            let key = id.raw().to_be_bytes();
            let Some(row_guard) = table.get(&key).ok().flatten() else {
                return false;
            };
            memory_row_passes(
                &row_guard.value(),
                namespace_id,
                &space_filter,
                kind_filter,
                created_range.as_ref(),
                &session_filter,
                include_tombstoned,
            )
        };

        let ef = occupancy_scaled_ef(config.ef_search, config.top_k, self.memory_index.len());
        let hits = self
            .memory_index
            .search(vector, config.top_k, Some(ef), id_passes);
        let mut direct = project_memory_hits(hits, config.similarity_threshold);

        // HyPE union: probe the question-vector pool with the same query
        // vector, keep only hits that pass the same metadata filters as
        // the direct lane, and merge best-per-memory. Recall-additive — a
        // direct hit is never dropped; a memory found only via HyPE joins
        // the lane. Done while the read txn + table are still open so the
        // filter reuses one transaction.
        if let Some(hype) = self.hype_index.as_ref() {
            let raw = hype.read().search(vector, config.top_k).unwrap_or_default();
            let filtered: Vec<(MemoryId, f32)> = raw
                .into_iter()
                .filter(|(id, score)| {
                    *score >= config.similarity_threshold && {
                        table
                            .get(&id.raw().to_be_bytes())
                            .ok()
                            .flatten()
                            .map(|g| {
                                memory_row_passes(
                                    &g.value(),
                                    namespace_id,
                                    &space_filter,
                                    kind_filter,
                                    created_range.as_ref(),
                                    &session_filter,
                                    include_tombstoned,
                                )
                            })
                            .unwrap_or(false)
                    }
                })
                .collect();
            merge_memory_hits(&mut direct, filtered, config.top_k);
        }
        drop(rtxn);

        Ok(direct)
    }

    /// Exact-cosine scan of one space's own vectors.
    ///
    /// Precondition: `filters.space_ids.len() == 1`. Reads the space's
    /// live `memory_count` from the SPACES registry: absent or
    /// `> SPACE_BRUTEFORCE_MAX` returns `Ok(None)` (the caller falls
    /// through to the shared HNSW path — per-space HNSW for large spaces
    /// is Phase 2). Otherwise it range-scans that space's
    /// `MEMORIES_BY_SPACE_TIMELINE_TABLE` keyspace, resolves each candidate's
    /// vector from the redb artifact store (the live by-id store — the arena
    /// mmap holds only WAL-recovered vectors and is empty for same-run
    /// encodes), drops tombstoned rows unless `include_tombstoned`, scores
    /// exact cosine, keeps hits clearing the threshold, and then applies the
    /// **same HyPE union** the shared path applies — the entire reason this
    /// lane lives in the retriever, so single-space paraphrase recall is
    /// never dropped.
    ///
    /// Returns `Ok(None)` (fall through to the shared HNSW path) when the
    /// space is missing, too large, or when it had live candidates but none
    /// resolved to a vector — the latter guards against a degraded empty
    /// result when the artifact store is momentarily behind the timeline
    /// index; the shared HNSW (which carries the live vectors in its pending
    /// buffer) then serves the query.
    fn brute_force_memory(
        &self,
        vector: &[f32; SEMANTIC_VECTOR_DIM],
        config: &SemanticRetrieverConfig,
        filters: &SemanticFilters,
    ) -> Result<Option<Vec<RankedItem>>, SemanticError> {
        let namespace_id = filters.namespace_id;
        let space_bytes: [u8; 16] = Into::<[u8; 16]>::into(filters.space_ids[0]);

        let rtxn = self
            .metadata
            .read_txn()
            .map_err(|e| SemanticError::Internal(format!("read_txn: {e}")))?;

        // Routing gate: only brute-force a space small enough to scan
        // exactly. A missing row or an oversized count falls through to
        // the shared HNSW path.
        let count = match brain_metadata::space_get(&rtxn, namespace_id, space_bytes)
            .map_err(|e| SemanticError::Internal(format!("space_get: {e}")))?
        {
            Some(meta) => meta.memory_count,
            None => {
                tracing::debug!(
                    target: "brain_ops::semantic_retriever",
                    namespace_id,
                    "brute-force fallthrough: space registry row absent",
                );
                return Ok(None);
            }
        };
        if count > SPACE_BRUTEFORCE_MAX {
            tracing::debug!(
                target: "brain_ops::semantic_retriever",
                namespace_id,
                memory_count = count,
                max = SPACE_BRUTEFORCE_MAX,
                "brute-force fallthrough: space too large (per-space HNSW is Phase 2)",
            );
            return Ok(None);
        }

        let timeline_t = rtxn
            .open_table(MEMORIES_BY_SPACE_TIMELINE_TABLE)
            .map_err(|e| SemanticError::Internal(format!("open timeline table: {e}")))?;

        let prefix = space_timeline_prefix_space(namespace_id, space_bytes);
        let upper = prefix_successor(&prefix);
        let lower_bound: std::ops::Bound<&[u8]> = std::ops::Bound::Included(&prefix[..]);
        let upper_bound: std::ops::Bound<&[u8]> = match &upper {
            Some(u) => std::ops::Bound::Excluded(u.as_slice()),
            None => std::ops::Bound::Unbounded,
        };

        let range = timeline_t
            .range::<&[u8]>((lower_bound, upper_bound))
            .map_err(|e| SemanticError::Internal(format!("timeline range: {e}")))?;

        // The tombstone gate needs the row's ACTIVE flag; the timeline key
        // carries only scope, not liveness. Open the memories table once and
        // look each candidate up (cheap point gets, bounded by the small
        // space's candidate count).
        let memories_t = rtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| SemanticError::Internal(format!("open MEMORIES_TABLE: {e}")))?;

        let query_norm = l2_norm(vector);
        let mut hits: Vec<(MemoryId, f32)> = Vec::new();
        // Track whether any live candidate resolved a vector. If the space
        // had candidates but none resolved, the artifact store is behind the
        // timeline — fall through to the shared HNSW rather than return an
        // empty set (the bug this lane previously hit: same-run encodes are
        // absent from the arena, so every candidate was skipped).
        let mut candidates: usize = 0;
        let mut resolved: usize = 0;
        for entry in range {
            let (k, _v) =
                entry.map_err(|e| SemanticError::Internal(format!("timeline entry: {e}")))?;
            let key = k.value();
            if key.len() != SPACE_TIMELINE_KEY_LEN {
                continue;
            }
            // Session scope is carried inline in the key (bytes 28..36) —
            // no extra row lookup needed to honour `session_ids`.
            if !filters.session_ids.is_empty() {
                let mut sid = [0u8; 8];
                sid.copy_from_slice(&key[28..36]);
                if !filters.session_ids.contains(&u64::from_be_bytes(sid)) {
                    continue;
                }
            }
            let mut idb = [0u8; 16];
            idb.copy_from_slice(&key[36..52]);
            let id = MemoryId::from_raw(u128::from_be_bytes(idb));

            // Tombstone gate: exclude soft-forgotten rows unless requested,
            // matching the shared-HNSW lane. A missing row means the memory
            // is gone (hard forget) — skip it.
            if !filters.include_tombstoned {
                match memories_t.get(&id.raw().to_be_bytes()) {
                    Ok(Some(g)) => {
                        if !g.value().is_active() {
                            continue;
                        }
                    }
                    Ok(None) => continue,
                    Err(e) => return Err(SemanticError::Internal(format!("memory row read: {e}"))),
                }
            }
            candidates += 1;

            // Live by-id vector store (redb artifact), NOT the arena mmap:
            // the arena is empty for anything encoded this run.
            let Some(v) = crate::memory_artifact::get_artifact_vector(&rtxn, id.to_be_bytes())
            else {
                continue;
            };
            resolved += 1;
            let cos = cosine_prenorm(vector, query_norm, &v);
            if cos >= config.similarity_threshold {
                hits.push((id, cos));
            }
        }

        // Fall through to the shared HNSW when live candidates existed but
        // none had a resolvable vector — the shared graph holds the live
        // vectors in its pending buffer and will serve them.
        if candidates > 0 && resolved == 0 {
            tracing::debug!(
                target: "brain_ops::semantic_retriever",
                namespace_id,
                candidates,
                "brute-force fallthrough: no candidate vector resolved from artifact store",
            );
            return Ok(None);
        }

        // Exact top-k: sort descending, break ties deterministically on id.
        hits.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.raw().cmp(&b.0.raw()))
        });
        hits.truncate(config.top_k);
        let mut direct = project_memory_hits(hits, config.similarity_threshold);

        // HyPE union — identical in effect to the shared-HNSW path: probe
        // the question-vector pool with the same query, keep only hits
        // passing the same metadata filters, and merge best-per-memory
        // (recall-additive, non-displacing). Dropping this for single-space
        // queries would regress paraphrase recall — the keystone reason the
        // brute-force lane lives inside the retriever.
        if let Some(hype) = self.hype_index.as_ref() {
            // Reuse the already-open memories table from the direct scan.
            let space_filter: HashSet<[u8; 16]> =
                filters.space_ids.iter().map(|a| (*a).into()).collect();
            let kind_filter = filters.memory_kind.map(memory_kind_to_u8);
            let created_range = filters.created_at_ms.clone();
            let session_filter = filters.session_ids.clone();
            let raw = hype.read().search(vector, config.top_k).unwrap_or_default();
            let filtered: Vec<(MemoryId, f32)> = raw
                .into_iter()
                .filter(|(id, score)| {
                    *score >= config.similarity_threshold && {
                        memories_t
                            .get(&id.raw().to_be_bytes())
                            .ok()
                            .flatten()
                            .map(|g| {
                                memory_row_passes(
                                    &g.value(),
                                    namespace_id,
                                    &space_filter,
                                    kind_filter,
                                    created_range.as_ref(),
                                    &session_filter,
                                    filters.include_tombstoned,
                                )
                            })
                            .unwrap_or(false)
                    }
                })
                .collect();
            merge_memory_hits(&mut direct, filtered, config.top_k);
        }
        drop(rtxn);

        Ok(Some(direct))
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
        let mut direct = project_statement_hits(hits, config.similarity_threshold);

        // Question-bridge union: probe the templated-question pool with the
        // same query vector, keep hits clearing the threshold, and union
        // best-per-statement. A bridge-only statement (no direct cosine hit)
        // joins the lane — that is the phrasing-gap recall the bridge exists
        // for. Each hit's `StatementId` is mapped back to its evidence memory
        // by the RECALL projector.
        if let Some(bridge) = self.statement_question_index.as_ref() {
            let raw = bridge
                .read()
                .search(vector, config.top_k)
                .unwrap_or_default();
            // Retrieval boosting only needs "which statement is relevant", not
            // which slot — the slot is consumed by the separate slot-projection
            // grounded path (`statement_slot_hits_for_query`), not this
            // recall-additive statement union. Drop the slot here.
            let raw: Vec<(brain_core::StatementId, f32)> = raw
                .into_iter()
                .map(|(id, _slot, score)| (id, score))
                .collect();
            merge_statement_hits(&mut direct, raw, config.similarity_threshold, config.top_k);
        }
        Ok(direct)
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
        arena: Option<&dyn SpaceVectorSource>,
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
            // Brute-force is Memory-only: `Both` (typed-graph QUERY /
            // entity-anchored) keeps the shared-HNSW path, so it passes
            // `None` for the arena.
            SemanticScope::Memory => self.search_memory(&vector, config, &config.filters.0, arena),
            SemanticScope::Statement => self.search_statement(&vector, config, &config.filters.0),
            SemanticScope::Both => {
                let memory = self.search_memory(&vector, config, &config.filters.0, None)?;
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

    fn vector_for(&self, id: brain_core::MemoryId) -> Option<[f32; SEMANTIC_VECTOR_DIM]> {
        // The exact embedding was persisted by-id at ENCODE, so resolve it from
        // the artifact store first — a single point lookup, no model forward
        // pass — exactly as the brute-force lane does. This is called for each
        // entity-graph walk candidate (a small, capped set) to cue-condition it
        // by cosine to the query; re-embedding every one on the read hot path
        // burns the BGE model needlessly.
        let rtxn = self.metadata.read_txn().ok()?;
        if let Some(v) = crate::memory_artifact::get_artifact_vector(&rtxn, id.to_be_bytes()) {
            return Some(v);
        }
        // Fallback — a fresh-this-run miss (the artifact row for a memory
        // encoded this process hasn't been point-persisted yet): reconstruct
        // from the stored text, the same passage the index was built from. A
        // missing text row yields `None` → the candidate keeps its structural
        // graph score (the caller decides how to treat that).
        let table = rtxn
            .open_table(brain_metadata::tables::text::TEXTS_TABLE)
            .ok()?;
        let row = table.get(&id.to_be_bytes()).ok()??;
        let text = String::from_utf8_lossy(row.value());
        self.embedder.embed(&text).ok()
    }

    fn hype_scores_for_query(
        &self,
        query: &[f32; SEMANTIC_VECTOR_DIM],
        k: usize,
    ) -> Vec<(MemoryId, f32)> {
        // One HNSW probe of the hypothetical-question pool, collapsed to the
        // best cosine per memory. `None` (no HyPE index wired: disabled, or no
        // LLM tier) yields no answer-lead signal — the read path then keeps its
        // existing order untouched.
        let Some(hype) = self.hype_index.as_ref() else {
            return Vec::new();
        };
        hype.read().search(query, k).unwrap_or_default()
    }

    fn statement_slot_hits_for_query(
        &self,
        query: &[f32; SEMANTIC_VECTOR_DIM],
        k: usize,
    ) -> Vec<(brain_core::StatementId, brain_core::Slot, f32)> {
        // One probe of the per-statement question-bridge pool, collapsed to the
        // best cosine per (statement, slot). `None` (no bridge index wired)
        // yields no slot-projection signal, so the grounded read keeps its
        // existing predicate-embedding path.
        let Some(bridge) = self.statement_question_index.as_ref() else {
            return Vec::new();
        };
        bridge.read().search(query, k).unwrap_or_default()
    }
}

/// Whether a memory row clears the active semantic filters. Shared by the
/// direct HNSW visit closure and the HyPE-hit post-filter so both lanes
/// apply identical namespace / space / kind / created-range / context scoping.
fn memory_row_passes(
    row: &MemoryMetadata,
    namespace_id: u32,
    space_filter: &HashSet<[u8; 16]>,
    kind_filter: Option<u8>,
    created_range: Option<&std::ops::RangeInclusive<u64>>,
    session_filter: &[u64],
    include_tombstoned: bool,
) -> bool {
    // Tenant wall: unconditional. A row from a different namespace never
    // surfaces in the vector lane, regardless of any other filter.
    if row.namespace_id != namespace_id {
        return false;
    }
    // Tombstone gate: a soft-forgotten row's HNSW node lingers until the
    // next rebuild, so exclude it here (unless explicitly requested) to keep
    // the semantic lane consistent with the graph/lexical lanes and to stop
    // tombstoned candidates from crowding live matches out of the ef window.
    if !include_tombstoned && !row.is_active() {
        return false;
    }
    if !space_filter.is_empty() && !space_filter.contains(&row.space_id_bytes) {
        return false;
    }
    if let Some(kind) = kind_filter {
        if row.kind != kind {
            return false;
        }
    }
    if let Some(range) = created_range {
        let ms = row.created_at_unix_nanos / 1_000_000;
        if !range.contains(&ms) {
            return false;
        }
    }
    if !session_filter.is_empty() && !session_filter.contains(&row.session_id) {
        return false;
    }
    true
}

/// Union HyPE memory hits into the direct cosine hits, recall-additively
/// and **non-displacingly**.
///
/// Every direct (passage-cosine) hit keeps its position ahead of any
/// HyPE-only hit: a memory found only through the hypothetical-question
/// pool is appended *after* all direct hits (in HyPE-score order), never
/// promoted above one. This is the precision guard — and it is what makes
/// the HyPE lane query-adaptive without any keyword classification or
/// tunable threshold:
///   - a narrow factual query has strong direct hits that fill the head,
///     so its cue is never pushed out of the window by a HyPE bridge;
///   - a broad / vocabulary-gap query has few direct hits, so HyPE-only
///     memories naturally fill the remaining slots and the recall gain is
///     preserved.
///
/// On a collision (a memory surfaced by both lanes) the direct hit keeps
/// its head position but its score is lifted to the stronger of the two so
/// downstream fusion sees the better signal. `direct` arrives sorted
/// descending by cosine; we preserve that order in the head.
/// When set, HyPE joins the semantic lane as its OWN rank list, fused with
/// the direct-cosine list by Reciprocal Rank Fusion rather than appended
/// behind it. RRF is rank-based, so it neutralizes the systematic scale
/// mismatch between HyPE (question↔query) and direct (passage↔query) cosines
/// — a HyPE-strong memory can earn a head position on rank agreement instead
/// of being pinned to the tail. Default OFF: the non-displacing append is the
/// proven-non-regressive baseline; this lane is measured before defaulting.
fn hype_rrf_enabled() -> bool {
    matches!(
        std::env::var("BRAIN_HYPE_RRF").ok().as_deref(),
        Some("1" | "true" | "TRUE" | "on" | "ON")
    )
}

/// Weight of the HyPE-agreement term in the bounded additive boost. Small by
/// design: a memory both lanes surface is lifted by at most this fraction of a
/// full-confidence HyPE hit, so HyPE refines the direct order without letting
/// a loose bridge match leapfrog a much stronger direct hit.
const HYPE_BOOST_WEIGHT: f32 = 0.15;

fn merge_memory_hits(direct: &mut Vec<RankedItem>, hype: Vec<(MemoryId, f32)>, top_k: usize) {
    if hype.is_empty() {
        direct.truncate(top_k);
        return;
    }
    if hype_rrf_enabled() {
        merge_memory_hits_rrf(direct, hype, top_k);
        return;
    }
    // Best HyPE score per memory.
    let mut hype_by_id: HashMap<MemoryId, f32> = HashMap::new();
    for (id, score) in hype {
        hype_by_id
            .entry(id)
            .and_modify(|cur| {
                if score > *cur {
                    *cur = score;
                }
            })
            .or_insert(score);
    }
    // Collision: lift a direct hit's score to max(direct, hype) without
    // moving it, and note which ids are already in the direct head.
    let mut direct_ids: HashSet<MemoryId> = HashSet::new();
    for item in direct.iter_mut() {
        if let RankedItemId::Memory(m) = item.id {
            direct_ids.insert(m);
            if let Some(hs) = hype_by_id.get(&m) {
                if *hs > item.score {
                    item.score = *hs;
                }
            }
        }
    }
    // HyPE-only memories (not already in the direct head), descending by
    // HyPE score, appended after the direct hits.
    let mut hype_only: Vec<(MemoryId, f32)> = hype_by_id
        .into_iter()
        .filter(|(id, _)| !direct_ids.contains(id))
        .collect();
    hype_only.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.raw().cmp(&b.0.raw()))
    });
    for (id, score) in hype_only {
        direct.push(RankedItem {
            id: RankedItemId::Memory(id),
            rank: 0,
            score,
            snippet: None,
        });
    }
    direct.truncate(top_k);
    // Dense 1-based ranks in the new direct-first order.
    for (i, item) in direct.iter_mut().enumerate() {
        item.rank = (i as u32) + 1;
    }
}

/// Env gate for occupancy-scaled `ef_search` (`BRAIN_EF_OCCUPANCY`). Default
/// OFF — the planner's configured ef is used verbatim.
fn ef_occupancy_enabled() -> bool {
    matches!(
        std::env::var("BRAIN_EF_OCCUPANCY").ok().as_deref(),
        Some("1" | "true" | "TRUE" | "on" | "ON")
    )
}

/// Scale `ef_search` to the index occupancy for size-invariant recall.
/// Default-off returns the configured ef unchanged. When on: ef rises toward
/// the live occupancy (so a small index is searched exhaustively — exact
/// recall at tiny N, no phantom neighbours), is never below `top_k` (the
/// ef ≥ k invariant) nor below the planner's configured ef, and is capped at
/// `SEMANTIC_EF_SEARCH_MAX` so a huge index pays a bounded search cost.
fn occupancy_scaled_ef(configured_ef: usize, top_k: usize, occupancy: usize) -> usize {
    if !ef_occupancy_enabled() {
        return configured_ef;
    }
    occupancy
        .min(SEMANTIC_EF_SEARCH_MAX)
        .max(configured_ef)
        .max(top_k)
        .min(SEMANTIC_EF_SEARCH_MAX)
}

/// Bounded additive HyPE boost (replaces the old rank-based RRF reorder).
///
/// The old RRF variant fused HyPE and direct as equal rank lists (k=60),
/// which let a topically-loose question-bridge hit outrank a precise direct
/// hit and displace it from the head — proven to halve accuracy on
/// conversational queries (the synthesizer weighs the head most). This is the
/// corrected, **boost-only** form:
///
///   - Direct hits are re-scored `cosine + HYPE_BOOST_WEIGHT * hype_agreement`
///     and re-sorted *among themselves*. A small weight means HyPE can lift a
///     memory both lanes agree on by a bounded amount, never leapfrog a much
///     stronger direct hit.
///   - HyPE-only memories (no direct hit) are appended **after every direct
///     hit**, in HyPE-score order — recall-additive, but structurally
///     incapable of outranking any direct hit.
///
/// The emitted `score` stays the representative cosine so `confidence` and the
/// cross-lane fusion that consumes it keep their meaning.
fn merge_memory_hits_rrf(direct: &mut Vec<RankedItem>, hype: Vec<(MemoryId, f32)>, top_k: usize) {
    // Best HyPE score per memory.
    let mut hype_by_id: HashMap<MemoryId, f32> = HashMap::new();
    for (id, score) in hype {
        hype_by_id
            .entry(id)
            .and_modify(|cur| {
                if score > *cur {
                    *cur = score;
                }
            })
            .or_insert(score);
    }

    // Phase 1: re-order the direct hits by their bounded-boosted score.
    let direct_ids: HashSet<MemoryId> = direct
        .iter()
        .filter_map(|it| match it.id {
            RankedItemId::Memory(m) => Some(m),
            _ => None,
        })
        .collect();
    direct.sort_by(|a, b| {
        let boost = |it: &RankedItem| -> f32 {
            match it.id {
                RankedItemId::Memory(m) => {
                    it.score + HYPE_BOOST_WEIGHT * hype_by_id.get(&m).copied().unwrap_or(0.0)
                }
                _ => it.score,
            }
        };
        boost(b)
            .partial_cmp(&boost(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Phase 2: append HyPE-only memories after ALL direct hits (recall, never
    // displacing), in descending HyPE score.
    let mut hype_only: Vec<(MemoryId, f32)> = hype_by_id
        .into_iter()
        .filter(|(id, _)| !direct_ids.contains(id))
        .collect();
    hype_only.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.raw().cmp(&b.0.raw()))
    });
    for (id, score) in hype_only {
        direct.push(RankedItem {
            id: RankedItemId::Memory(id),
            rank: 0,
            score,
            snippet: None,
        });
    }

    direct.truncate(top_k);
    for (i, item) in direct.iter_mut().enumerate() {
        item.rank = (i as u32) + 1;
    }
}

/// Union question-bridge statement hits into the direct statement-cosine
/// hits, recall-additively and non-displacingly — the statement analogue of
/// [`merge_memory_hits`]. Direct hits keep their head positions (so a
/// statement found by direct cosine is never demoted by a bridge-only hit);
/// statements found only via the question bridge are appended after, in
/// bridge-score order. A collision lifts the direct hit's score to the
/// stronger of the two. Filters bridge hits below `threshold`.
fn merge_statement_hits(
    direct: &mut Vec<RankedItem>,
    bridge: Vec<(brain_core::StatementId, f32)>,
    threshold: f32,
    top_k: usize,
) {
    use brain_core::StatementId;
    let mut bridge_by_id: HashMap<StatementId, f32> = HashMap::new();
    for (id, score) in bridge {
        if score < threshold {
            continue;
        }
        bridge_by_id
            .entry(id)
            .and_modify(|cur| {
                if score > *cur {
                    *cur = score;
                }
            })
            .or_insert(score);
    }
    if bridge_by_id.is_empty() {
        direct.truncate(top_k);
        return;
    }
    let mut direct_ids: HashSet<StatementId> = HashSet::new();
    for item in direct.iter_mut() {
        if let RankedItemId::Statement(s) = item.id {
            direct_ids.insert(s);
            if let Some(bs) = bridge_by_id.get(&s) {
                if *bs > item.score {
                    item.score = *bs;
                }
            }
        }
    }
    let mut bridge_only: Vec<(StatementId, f32)> = bridge_by_id
        .into_iter()
        .filter(|(id, _)| !direct_ids.contains(id))
        .collect();
    bridge_only.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.to_bytes().cmp(&b.0.to_bytes()))
    });
    for (id, score) in bridge_only {
        direct.push(RankedItem {
            id: RankedItemId::Statement(id),
            rank: 0,
            score,
            snippet: None,
        });
    }
    direct.truncate(top_k);
    for (i, item) in direct.iter_mut().enumerate() {
        item.rank = (i as u32) + 1;
    }
}

/// Lexicographic successor of `prefix`: the smallest byte string strictly
/// greater than every string starting with `prefix`. `None` when `prefix`
/// is all `0xFF` (unbounded above). Bounds the space-prefix timeline scan
/// without a trailing sentinel.
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.last_mut() {
        if *last != 0xFF {
            *last += 1;
            return Some(out);
        }
        out.pop();
    }
    None
}

/// L2 norm of a 384-d vector, SIMD-accelerated over 8-wide lanes.
fn l2_norm(v: &[f32; SEMANTIC_VECTOR_DIM]) -> f32 {
    use wide::f32x8;
    let mut acc = f32x8::ZERO;
    // 384 == 48 * 8, so the whole vector is covered by full lanes.
    for chunk in v.as_chunks::<8>().0.iter() {
        let x = f32x8::from([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        acc += x * x;
    }
    acc.reduce_add().sqrt()
}

/// Cosine similarity between the query (with precomputed norm `a_norm`)
/// and candidate `b`, SIMD-accelerated over 8-wide lanes. Returns 0.0 when
/// either vector has zero magnitude (degenerate — never a real match).
fn cosine_prenorm(
    a: &[f32; SEMANTIC_VECTOR_DIM],
    a_norm: f32,
    b: &[f32; SEMANTIC_VECTOR_DIM],
) -> f32 {
    use wide::f32x8;
    let mut dot = f32x8::ZERO;
    let mut bsq = f32x8::ZERO;
    for (ca, cb) in a.as_chunks::<8>().0.iter().zip(b.as_chunks::<8>().0.iter()) {
        let va = f32x8::from([ca[0], ca[1], ca[2], ca[3], ca[4], ca[5], ca[6], ca[7]]);
        let vb = f32x8::from([cb[0], cb[1], cb[2], cb[3], cb[4], cb[5], cb[6], cb[7]]);
        dot += va * vb;
        bsq += vb * vb;
    }
    let b_norm = bsq.reduce_add().sqrt();
    let denom = a_norm * b_norm;
    if denom <= f32::EPSILON {
        return 0.0;
    }
    dot.reduce_add() / denom
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
