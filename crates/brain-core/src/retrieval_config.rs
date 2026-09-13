//! Process-global retrieval tuning, sourced from deploy config.
//!
//! A handful of deploy-time retrieval toggles (rank-fusion strategy, the HyPE
//! RRF join, occupancy-scaled `ef_search`, result autocut) are read deep in
//! the read path — inside the planner's fusion step, the semantic retriever,
//! and the RECALL handler — none of which carries the server's parsed config.
//! They used to be read there via bespoke `BRAIN_*` environment variables,
//! which violated the single-source config rule (structured TOML, with the
//! generic `BRAIN__SECTION__FIELD` parser as the only env override).
//!
//! These are genuinely process-global deploy knobs — they never vary per
//! request or per shard — so they live here as a read-only [`RetrievalTuning`]
//! installed once at server boot from the `[retrieval]` TOML section and read
//! at the call sites. Callers that never install it (unit tests, non-server
//! tools) transparently get [`RetrievalTuning::default`], which reproduces the
//! previous env-unset behaviour exactly (RRF fusion; every gate off).

use std::sync::OnceLock;

/// Deploy-time retrieval tuning. Read-only after [`install`](RetrievalTuning::install).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievalTuning {
    /// Rank-fusion strategy: `"rrf"` (default, score-scale-invariant), or the
    /// score-aware `"relative"` / `"zscore"` strategies. Parsed by the planner
    /// (the [`FusionMethod`](../../brain_planner) enum lives there); an
    /// unrecognized value falls back to RRF, matching the prior env behaviour.
    pub fusion_method: String,
    /// HyPE joins the semantic lane as its own RRF rank list rather than a
    /// non-displacing append. Default `false`.
    pub hype_rrf: bool,
    /// Scale `ef_search` by index occupancy on the memory probe. Default `false`.
    pub ef_occupancy_scaling: bool,
    /// Relative-drop autocut of the ranked result tail. Default `false`.
    pub autocut: bool,
}

impl Default for RetrievalTuning {
    fn default() -> Self {
        Self {
            fusion_method: "rrf".to_string(),
            hype_rrf: false,
            ef_occupancy_scaling: false,
            autocut: false,
        }
    }
}

static RETRIEVAL_TUNING: OnceLock<RetrievalTuning> = OnceLock::new();

impl RetrievalTuning {
    /// Install the process-wide tuning from parsed deploy config. First call
    /// wins (the server calls it once at boot); later calls are ignored, so a
    /// stray second install can never mutate live tuning. Returns whether this
    /// call was the one that installed the value.
    pub fn install(self) -> bool {
        RETRIEVAL_TUNING.set(self).is_ok()
    }

    /// The active tuning, or defaults if none was installed (tests / non-server
    /// callers). The default reproduces the historical env-unset behaviour.
    #[must_use]
    pub fn active() -> &'static RetrievalTuning {
        RETRIEVAL_TUNING.get_or_init(RetrievalTuning::default)
    }
}
