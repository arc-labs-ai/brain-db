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
    SemanticRetrieverConfig, SemanticScope, SharedHnsw, SEMANTIC_EF_SEARCH_MAX,
    SEMANTIC_VECTOR_DIM,
};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
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

        let id_passes = |id: MemoryId| -> bool {
            let key = id.raw().to_be_bytes();
            let Some(row_guard) = table.get(&key).ok().flatten() else {
                return false;
            };
            memory_row_passes(
                &row_guard.value(),
                &agent_filter,
                kind_filter,
                created_range.as_ref(),
                &context_filter,
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
                                    &agent_filter,
                                    kind_filter,
                                    created_range.as_ref(),
                                    &context_filter,
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
            let raw = bridge.read().search(vector, config.top_k).unwrap_or_default();
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

    fn vector_for(&self, id: brain_core::MemoryId) -> Option<[f32; SEMANTIC_VECTOR_DIM]> {
        // hnsw_rs exposes no by-id vector reconstruct, so recover the memory's
        // embedding from its stored text — the same passage the index was built
        // from. Bounded cost: this is called only for the entity-graph walk's
        // candidates (a small, capped set), to cue-condition them by cosine to
        // the query. A missing text row yields `None` → the candidate keeps its
        // structural graph score (the caller decides how to treat that).
        let rtxn = self.metadata.read_txn().ok()?;
        let table = rtxn
            .open_table(brain_metadata::tables::text::TEXTS_TABLE)
            .ok()?;
        let row = table.get(&id.to_be_bytes()).ok()??;
        let text = String::from_utf8_lossy(row.value());
        self.embedder.embed(&text).ok()
    }
}

/// Whether a memory row clears the active semantic filters. Shared by the
/// direct HNSW visit closure and the HyPE-hit post-filter so both lanes
/// apply identical agent / kind / created-range / context scoping.
fn memory_row_passes(
    row: &MemoryMetadata,
    agent_filter: &HashSet<[u8; 16]>,
    kind_filter: Option<u8>,
    created_range: Option<&std::ops::RangeInclusive<u64>>,
    context_filter: &[u64],
) -> bool {
    if !agent_filter.is_empty() && !agent_filter.contains(&row.agent_id_bytes) {
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
    if !context_filter.is_empty() && !context_filter.contains(&row.context_id) {
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
