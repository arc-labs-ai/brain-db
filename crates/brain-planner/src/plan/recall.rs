//! `RecallPlan` and its step structs.
//!
//! Single-shard for now: `shards: Vec<_>` is always length 1. The
//! structure is preserved so cross-shard fan-out lands later without
//! re-spelling the plan.

use brain_core::MemoryId;

use super::common::{FilterRule, FilterStage, ShardId, SortKey};

#[derive(Debug, Clone)]
pub struct RecallPlan {
    pub embedding: EmbeddingStep,
    pub shards: Vec<ShardSearchStep>,
    pub merge: MergeStep,
    pub text_fetch: Option<TextFetchStep>,
    pub response: ResponseStep,
    /// Filled by the cost model when the plan is built.
    pub estimated_cost_ms: f32,
}

/// The embedding step is shared across shards (we embed the cue once,
/// then reuse the vector).
#[derive(Debug, Clone)]
pub struct EmbeddingStep {
    pub text: String,
    /// Whether to consult the cue cache. The planner
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
    /// Picked by the cost model's `pick_ef`.
    pub ef: usize,
    /// `k * over_factor`, capped at `PlannerConfig::max_candidates_per_search`.
    pub candidates_to_request: usize,
    /// Cheap rules applied during HNSW's post-processing or before
    /// candidate gathering (PreFilter category).
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
