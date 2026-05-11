//! `RecallPlan` and its step structs. Spec §08/03 §12 spells out
//! the full example; this module mirrors it.
//!
//! Single-shard for Phase 6 (orientation plan §4.7): `shards: Vec<_>`
//! is always length 1. The structure is preserved so cross-shard
//! fan-out lands in Phase 12 without re-spelling the plan.

use brain_core::MemoryId;

use super::common::{FilterRule, FilterStage, ShardId, SortKey};

#[derive(Debug, Clone)]
pub struct RecallPlan {
    pub embedding: EmbeddingStep,
    pub shards: Vec<ShardSearchStep>,
    pub merge: MergeStep,
    pub text_fetch: Option<TextFetchStep>,
    pub response: ResponseStep,
    /// Filled by 6.2's cost model when 6.3 builds the plan.
    pub estimated_cost_ms: f32,
}

/// Spec §08/03 §3 — the embedding step is shared across shards (we
/// embed the cue once, then reuse the vector).
#[derive(Debug, Clone)]
pub struct EmbeddingStep {
    pub text: String,
    /// Whether to consult the cue cache (spec §04/05). The planner
    /// sets this `true` by default; specialised paths (e.g. an admin
    /// `ADMIN_REINDEX`) may force-miss.
    pub cache_lookup: bool,
}

#[derive(Debug, Clone)]
pub struct ShardSearchStep {
    pub shard_id: ShardId,
    pub ann_search: AnnSearchStep,
    pub metadata_lookup: MetadataLookupStep,
    pub filter_apply: FilterStep,
}

#[derive(Debug, Clone)]
pub struct AnnSearchStep {
    /// Picked by 6.2's `pick_ef`.
    pub ef: usize,
    /// `k * over_factor`, capped at `PlannerConfig::max_candidates_per_search`.
    /// Spec §08/03 §5.
    pub candidates_to_request: usize,
    /// Cheap rules applied during HNSW's post-processing or before
    /// candidate gathering (spec §08/03 §6: PreFilter category).
    pub pre_filter: Vec<FilterRule>,
}

#[derive(Debug, Clone, Copy)]
pub struct MetadataLookupStep {
    pub include_extra: bool,
}

#[derive(Debug, Clone)]
pub struct FilterStep {
    pub stage: FilterStage,
    pub rules: Vec<FilterRule>,
}

#[derive(Debug, Clone, Copy)]
pub struct MergeStep {
    pub sort_by: SortKey,
    pub final_top: usize,
    pub confidence_min: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct TextFetchStep {
    pub memory_ids: Vec<MemoryId>,
    pub parallel: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ResponseStep {
    pub include_text: bool,
    pub include_metadata: bool,
}
