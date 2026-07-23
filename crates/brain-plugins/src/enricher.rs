//! Enricher plugins — mutate candidate items between extraction and
//! persistence.
//!
//! The pipeline runs every registered enricher in registration order
//! after the pattern + classifier + LLM tiers complete and before the
//! resolver / persistence stages. Enrichers may add, mutate, or drop
//! items; downstream stages see the union of the modifications.
//!
//! Enrichers are sync — they MUST NOT block on tokio tasks, spawn
//! threads, or perform synchronous IO that exceeds a few microseconds.
//! Use plugin-side caches and pre-built indexes for any non-trivial
//! enrichment work.

use crate::errors::PluginResult;
use crate::recall::RecallPlugin;
use brain_core::SpaceId;
use brain_extractors::framework::item::ExtractedItem;

/// Inputs handed to [`EnricherPlugin::enrich`].
///
/// The `items` vector is borrowed mutably so the plugin can edit in
/// place — push to add, retain to drop, mutate fields to enrich.
pub struct EnricherInput<'a> {
    /// Space that originated the source text. Plugins can use this for
    /// per-space policy (e.g. apply only to certain spaces) or for
    /// per-space state (per-space vocabularies).
    pub space_id: SpaceId,
    /// Candidate items produced by the upstream extractor tiers. The
    /// plugin may push new items, mutate existing ones, or remove
    /// items by retaining a filtered subset.
    pub items: &'a mut Vec<ExtractedItem>,
    /// Original text the items were extracted from. Plugins that need
    /// context (e.g. surrounding sentences) read it from here.
    pub source_text: &'a str,
    /// Wall-clock at the start of this enrichment phase, in unix nanos.
    /// Plugins should use this rather than re-reading the system clock
    /// so all enrichers in a single batch see a consistent timestamp.
    pub now_unix_nanos: u64,
}

/// Counters returned from [`EnricherPlugin::enrich`] for metrics and
/// audit logging.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EnricherOutput {
    pub items_added: u32,
    pub items_mutated: u32,
    pub items_dropped: u32,
}

impl EnricherOutput {
    /// Convenience: zero counters across the board.
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            items_added: 0,
            items_mutated: 0,
            items_dropped: 0,
        }
    }
}

/// Plugin trait for enrichers.
///
/// Implementors must also implement [`RecallPlugin`] for the lifecycle
/// hooks.
pub trait EnricherPlugin: RecallPlugin {
    /// Mutate, add, or drop items in the candidate batch.
    ///
    /// Errors are caught by the host registry and the failing plugin is
    /// skipped for this batch (fail-open). The pipeline continues with
    /// whatever the plugin had managed to write into `items` before
    /// returning — there is no rollback. Plugins that need
    /// transactional semantics should snapshot and restore themselves.
    fn enrich(&self, input: EnricherInput<'_>) -> PluginResult<EnricherOutput>;
}
