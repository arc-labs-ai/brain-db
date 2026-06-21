//! Event-time recency boost.
//!
//! A soft, additive ranking signal that nudges more-recent memories up
//! the fused list. It wires the reserved `temporal` per-retriever weight
//! as a fourth signal folded into `fused_score` *after* the filter chain
//! and *before* rerank, so a fresh memory both enters the rerank window
//! and carries its recency into the rerank blend.
//!
//! **Why additive, RRF-scale, and gated:**
//! - *Additive at RRF scale.* A single top-rank retriever contributes
//!   `w / (k + 1)` to `fused_score` (≈ 0.0164 at the default `k = 60`).
//!   The recency term is `temporal_weight · (1 / (k + 1)) · decay`, so a
//!   brand-new memory at the default `temporal_weight = 0.5` earns at
//!   most half of one top-rank vote — a tie-breaker, never a signal that
//!   overrides genuine relevance. Calibrated to the RRF unit; for the
//!   non-default `RelativeScore` fusion it is a minor nudge.
//! - *Exponential decay on event time.* `decay = 0.5^(age / half_life)`
//!   where `age = reference_time − event_time` and `event_time` is the
//!   client-supplied `occurred_at` (falling back to `created_at`). A
//!   memory one half-life old gets half the freshness boost. Future-dated
//!   events (event_time > reference) saturate at full freshness.
//! - *Gated on a temporal signal.* The caller applies this only when the
//!   query actually cares about time (a temporal expression, an explicit
//!   time filter, or an `as_of` anchor). Applying it unconditionally
//!   would quietly penalise timeless facts ("what's my wife's name").
//!
//! Memory hits only: statements/relations carry their own bi-temporal
//! validity, handled by the filter chain, and are left untouched here.

use brain_index::RankedItemId;
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::MetadataDb;

use crate::retrieval::fusion::{sort_by_fused_score, FusedItem};

/// Half-life of the recency boost, in days. A memory whose event time is
/// one half-life in the past receives half the freshness boost a
/// just-now memory would. Chosen for episodic agent memory where
/// "recent" means weeks-to-months, not seconds; large enough that the
/// decay is gentle within a conversation's lifetime, small enough that
/// year-old memories stop competing on recency alone.
pub const RECENCY_HALF_LIFE_DAYS: f64 = 90.0;

const NANOS_PER_DAY: f64 = 86_400.0 * 1_000_000_000.0;

#[derive(Debug, thiserror::Error)]
pub enum RecencyError {
    #[error("metadata: {0}")]
    Metadata(String),
}

/// Fold the event-time recency boost into `items` in place, then re-sort
/// by `fused_score`.
///
/// `reference_time_unix_nanos` is the "now" the decay is measured
/// against — the query's `as_of` anchor when set, otherwise wall-clock
/// now. `temporal_weight` is the reserved per-retriever weight from the
/// active [`crate::retrieval::router::PerRetrieverWeights`]; `k` is the
/// RRF smoothing constant (so the boost stays on the same scale as the
/// retriever contributions). A non-positive weight or empty list is a
/// no-op.
pub fn apply_recency_boost(
    items: &mut [FusedItem],
    metadata: &MetadataDb,
    reference_time_unix_nanos: u64,
    temporal_weight: f32,
    k: u32,
) -> Result<(), RecencyError> {
    if temporal_weight <= 0.0 || items.is_empty() {
        return Ok(());
    }

    let rtxn = metadata
        .read_txn()
        .map_err(|e| RecencyError::Metadata(format!("read_txn: {e}")))?;
    let table = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| RecencyError::Metadata(format!("open MEMORIES_TABLE: {e}")))?;

    // One top-rank retriever vote, the natural unit for an RRF-scale
    // additive term.
    let unit = 1.0_f64 / (f64::from(k) + 1.0);
    let weight = f64::from(temporal_weight);
    let half_life_nanos = RECENCY_HALF_LIFE_DAYS * NANOS_PER_DAY;

    for item in items.iter_mut() {
        let RankedItemId::Memory(id) = item.id else {
            continue;
        };
        let row = table
            .get(&id.raw().to_be_bytes())
            .map_err(|e| RecencyError::Metadata(format!("memory get: {e}")))?
            .map(|g| g.value());
        let Some(row) = row else { continue };

        // Client event time when supplied, else the server write time.
        let event_time = row
            .occurred_at_unix_nanos
            .unwrap_or(row.created_at_unix_nanos);
        // Future-dated events saturate at full freshness (age 0).
        let age_nanos = reference_time_unix_nanos.saturating_sub(event_time) as f64;
        let decay = 0.5_f64.powf(age_nanos / half_life_nanos);
        item.fused_score += weight * unit * decay;
    }

    sort_by_fused_score(items);
    Ok(())
}

#[cfg(test)]
#[path = "recency_tests.rs"]
mod tests;
