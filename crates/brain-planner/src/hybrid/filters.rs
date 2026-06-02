//! Post-fusion filter chain.
//!
//! Reads metadata from redb to evaluate per-filter predicates against
//! each fused item, drops items that don't pass, applies the final
//! limit, and reports per-step survivor counts for EXPLAIN/TRACE.
//!
//! Filter order (binding):
//!
//! 1. Type — kind_filter (statement) + memory_kind_filter +
//!    predicate_filter.
//! 2. Temporal — time_filter against created_at (memory),
//!    event_at / valid_from..valid_to (statement, relation).
//! 3. Confidence — `confidence ≥ threshold` on Statement +
//!    Relation; `salience ≥ threshold` on Memory (the
//!    substrate's analog, documented inline).
//! 4. Tombstone — drop tombstoned rows unless
//!    `include_tombstoned = true`.
//! 5. Supersession — drop superseded statements / relations
//!    unless `include_superseded = true`.
//! 6. As-of — bi-temporal time-travel. When
//!    `as_of_record_time_unix_nanos = Some(t)`, keep only
//!    statements the substrate believed at `t`:
//!    `extracted_at <= t AND
//!     (record_invalidated_at IS NULL OR record_invalidated_at > t)`.
//!    Memory / Entity / Relation pass through (no record-axis
//!    timestamps stored on those rows yet).
//!
//! Limit applied after all six.

use brain_core::{MemoryKind, PredicateId};
use brain_core::{Statement, StatementKind};
use brain_index::RankedItemId;
use brain_metadata::statement::statement_get;
use brain_metadata::tables::memory::{flags as memory_flags, MEMORIES_TABLE};
use brain_metadata::tables::relation::RELATION_METADATA_TABLE;
use brain_metadata::MetadataDb;
use redb::ReadTransaction;

use super::fusion::FusedItem;
use super::router::TimeRange;

/// Per-filter configuration. All fields default to "pass
/// through"; an empty kind filter, `None` time range,
/// `None` confidence threshold etc. mean the corresponding
/// filter is a no-op.
#[derive(Debug, Clone, Default)]
pub struct FilterChain {
    /// Statement-kind filter (Fact / Preference / Event).
    /// Empty = pass all.
    pub kind_filter: Vec<StatementKind>,
    /// Memory-kind filter (Episodic / Semantic / Consolidated).
    /// Empty = pass all. v1 splits this from `kind_filter`
    /// because the two enums are distinct.
    pub memory_kind_filter: Vec<MemoryKind>,
    /// Statement predicate filter. Empty = pass all.
    pub predicate_filter: Vec<PredicateId>,
    pub time_filter: Option<TimeRange>,
    /// Min confidence (statement / relation) / min salience
    /// (memory).
    pub confidence_min: Option<f32>,
    /// `true` = keep tombstoned rows.
    pub include_tombstoned: bool,
    /// `true` = keep superseded rows.
    pub include_superseded: bool,
    /// Bi-temporal time-travel filter. When `Some(t)`, only return
    /// statements the substrate believed at record-time `t`:
    /// `extracted_at <= t AND
    ///  (record_invalidated_at IS NULL OR record_invalidated_at > t)`.
    /// `None` is the current-state default (every statement passes this
    /// step). Tombstoned-as-of-`t` statements pass the as-of step even
    /// when `include_tombstoned = false` — the tombstone filter runs
    /// against current state, while the as-of filter runs against
    /// historical state, and the historical answer must win when the
    /// caller asked for it. Callers may pair this with
    /// `include_tombstoned = true` if they want today-tombstoned rows
    /// that were alive at `t`.
    pub as_of_record_time_unix_nanos: Option<u64>,
}

/// Per-step survivor counts. Surfaces in EXPLAIN/TRACE.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterChainStats {
    pub before: u32,
    pub after_type: u32,
    pub after_temporal: u32,
    pub after_confidence: u32,
    pub after_tombstone: u32,
    pub after_supersession: u32,
    pub after_as_of: u32,
    pub after_limit: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum FilterError {
    #[error("metadata: {0}")]
    Metadata(String),
}

/// Apply the filter chain in order, then truncate to
/// `limit`. `limit == 0` means "no limit".
pub fn apply_filter_chain(
    items: Vec<FusedItem>,
    chain: &FilterChain,
    metadata: &MetadataDb,
    limit: u32,
) -> Result<(Vec<FusedItem>, FilterChainStats), FilterError> {
    let mut stats = FilterChainStats {
        before: items.len() as u32,
        ..Default::default()
    };

    let rtxn = metadata
        .read_txn()
        .map_err(|e| FilterError::Metadata(format!("read_txn: {e}")))?;

    let items = filter_type(items, chain, &rtxn)?;
    stats.after_type = items.len() as u32;

    let items = filter_temporal(items, chain, &rtxn)?;
    stats.after_temporal = items.len() as u32;

    let items = filter_confidence(items, chain, &rtxn)?;
    stats.after_confidence = items.len() as u32;

    let items = filter_tombstone(items, chain, &rtxn)?;
    stats.after_tombstone = items.len() as u32;

    let items = filter_supersession(items, chain, &rtxn)?;
    stats.after_supersession = items.len() as u32;

    let mut items = filter_as_of(items, chain, &rtxn)?;
    stats.after_as_of = items.len() as u32;

    if limit > 0 && items.len() > limit as usize {
        items.truncate(limit as usize);
    }
    stats.after_limit = items.len() as u32;

    Ok((items, stats))
}

// ---------------------------------------------------------------------------
// Per-filter helpers.
// ---------------------------------------------------------------------------

fn filter_type(
    items: Vec<FusedItem>,
    chain: &FilterChain,
    rtxn: &ReadTransaction,
) -> Result<Vec<FusedItem>, FilterError> {
    if chain.kind_filter.is_empty()
        && chain.memory_kind_filter.is_empty()
        && chain.predicate_filter.is_empty()
    {
        return Ok(items);
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let keep = match item.id {
            RankedItemId::Memory(id) => {
                if chain.memory_kind_filter.is_empty() {
                    true
                } else {
                    memory_kind(rtxn, id)?.is_some_and(|k| chain.memory_kind_filter.contains(&k))
                }
            }
            RankedItemId::Statement(id) => {
                let Some(stmt) = statement_get(rtxn, id)
                    .map_err(|e| FilterError::Metadata(format!("statement_get: {e}")))?
                else {
                    continue;
                };
                let kind_ok =
                    chain.kind_filter.is_empty() || chain.kind_filter.contains(&stmt.kind);
                let pred_ok = chain.predicate_filter.is_empty()
                    || chain.predicate_filter.contains(&stmt.predicate);
                kind_ok && pred_ok
            }
            RankedItemId::Entity(_) | RankedItemId::Relation(_) => true,
        };
        if keep {
            out.push(item);
        }
    }
    Ok(out)
}

fn filter_temporal(
    items: Vec<FusedItem>,
    chain: &FilterChain,
    rtxn: &ReadTransaction,
) -> Result<Vec<FusedItem>, FilterError> {
    let Some(range) = chain.time_filter else {
        return Ok(items);
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let keep = match item.id {
            RankedItemId::Memory(id) => {
                let Some(ms) = memory_created_at_ms(rtxn, id)? else {
                    continue;
                };
                in_range(&range, ms)
            }
            RankedItemId::Statement(id) => {
                let Some(stmt) = statement_get(rtxn, id)
                    .map_err(|e| FilterError::Metadata(format!("statement_get: {e}")))?
                else {
                    continue;
                };
                statement_temporal_match(&stmt, &range)
            }
            RankedItemId::Relation(id) => {
                let Some((vf, vt)) = relation_validity_ms(rtxn, id)? else {
                    continue;
                };
                window_overlaps(vf, vt, &range)
            }
            RankedItemId::Entity(_) => true,
        };
        if keep {
            out.push(item);
        }
    }
    Ok(out)
}

fn filter_confidence(
    items: Vec<FusedItem>,
    chain: &FilterChain,
    rtxn: &ReadTransaction,
) -> Result<Vec<FusedItem>, FilterError> {
    let Some(min) = chain.confidence_min else {
        return Ok(items);
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let keep = match item.id {
            RankedItemId::Memory(id) => {
                // Memory hits are filtered by salience by design — a strong but
                // decayed memory should drop out of a "give me what still
                // matters" recall. Cosine similarity has its own gate (the
                // surfaced `confidence` / similarity_score), and raw relevance
                // has `salience_floor`; this filter is the importance cut. Do
                // not "correct" this to similarity to match older spec text.
                let Some(salience) = memory_salience(rtxn, id)? else {
                    continue;
                };
                salience >= min
            }
            RankedItemId::Statement(id) => {
                let Some(stmt) = statement_get(rtxn, id)
                    .map_err(|e| FilterError::Metadata(format!("statement_get: {e}")))?
                else {
                    continue;
                };
                stmt.confidence >= min
            }
            RankedItemId::Relation(id) => {
                let Some(conf) = relation_confidence(rtxn, id)? else {
                    continue;
                };
                conf >= min
            }
            RankedItemId::Entity(_) => true,
        };
        if keep {
            out.push(item);
        }
    }
    Ok(out)
}

fn filter_tombstone(
    items: Vec<FusedItem>,
    chain: &FilterChain,
    rtxn: &ReadTransaction,
) -> Result<Vec<FusedItem>, FilterError> {
    if chain.include_tombstoned {
        return Ok(items);
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let keep = match item.id {
            RankedItemId::Memory(id) => memory_active(rtxn, id)?.unwrap_or(false),
            RankedItemId::Statement(id) => {
                let Some(stmt) = statement_get(rtxn, id)
                    .map_err(|e| FilterError::Metadata(format!("statement_get: {e}")))?
                else {
                    continue;
                };
                !stmt.tombstoned
            }
            RankedItemId::Relation(id) => relation_tombstoned(rtxn, id)?.is_some_and(|t| !t),
            RankedItemId::Entity(_) => true,
        };
        if keep {
            out.push(item);
        }
    }
    Ok(out)
}

fn filter_supersession(
    items: Vec<FusedItem>,
    chain: &FilterChain,
    rtxn: &ReadTransaction,
) -> Result<Vec<FusedItem>, FilterError> {
    if chain.include_superseded {
        return Ok(items);
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let keep = match item.id {
            RankedItemId::Statement(id) => {
                let Some(stmt) = statement_get(rtxn, id)
                    .map_err(|e| FilterError::Metadata(format!("statement_get: {e}")))?
                else {
                    continue;
                };
                stmt.superseded_by.is_none()
            }
            RankedItemId::Relation(id) => relation_superseded(rtxn, id)?.is_some_and(|x| !x),
            // Memory / Entity have no supersession concept.
            RankedItemId::Memory(_) | RankedItemId::Entity(_) => true,
        };
        if keep {
            out.push(item);
        }
    }
    Ok(out)
}

fn filter_as_of(
    items: Vec<FusedItem>,
    chain: &FilterChain,
    rtxn: &ReadTransaction,
) -> Result<Vec<FusedItem>, FilterError> {
    let Some(record_time) = chain.as_of_record_time_unix_nanos else {
        return Ok(items);
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let keep = match item.id {
            RankedItemId::Statement(id) => {
                let Some(stmt) = statement_get(rtxn, id)
                    .map_err(|e| FilterError::Metadata(format!("statement_get: {e}")))?
                else {
                    continue;
                };
                as_of_matches(&stmt, record_time)
            }
            // Memories / entities / relations do not yet track a
            // record-time invalidation timestamp; bi-temporal is a
            // statement-layer property today. Pass them through so the
            // filter is additive — operators who set as-of want it to
            // narrow statements without dropping the surrounding graph.
            RankedItemId::Memory(_) | RankedItemId::Entity(_) | RankedItemId::Relation(_) => true,
        };
        if keep {
            out.push(item);
        }
    }
    Ok(out)
}

/// `true` if the substrate believed `stmt` at record-time `record_time`.
/// A statement was active at `t` iff it had been extracted by `t` and
/// either still active today, or only invalidated after `t`.
#[must_use]
pub fn as_of_matches(stmt: &Statement, record_time_unix_nanos: u64) -> bool {
    if stmt.extracted_at_unix_nanos > record_time_unix_nanos {
        return false;
    }
    match stmt.record_invalidated_at_unix_nanos {
        None => true,
        Some(t) => t > record_time_unix_nanos,
    }
}

// ---------------------------------------------------------------------------
// Redb readers.
// ---------------------------------------------------------------------------

fn memory_kind(
    rtxn: &ReadTransaction,
    id: brain_core::MemoryId,
) -> Result<Option<MemoryKind>, FilterError> {
    let row = open_memory_row(rtxn, id)?;
    Ok(row.and_then(|m| match m.kind {
        0 => Some(MemoryKind::Episodic),
        1 => Some(MemoryKind::Semantic),
        2 => Some(MemoryKind::Consolidated),
        _ => None,
    }))
}

fn memory_created_at_ms(
    rtxn: &ReadTransaction,
    id: brain_core::MemoryId,
) -> Result<Option<u64>, FilterError> {
    let row = open_memory_row(rtxn, id)?;
    Ok(row.map(|m| m.created_at_unix_nanos / 1_000_000))
}

fn memory_salience(
    rtxn: &ReadTransaction,
    id: brain_core::MemoryId,
) -> Result<Option<f32>, FilterError> {
    let row = open_memory_row(rtxn, id)?;
    Ok(row.map(|m| m.salience))
}

fn memory_active(
    rtxn: &ReadTransaction,
    id: brain_core::MemoryId,
) -> Result<Option<bool>, FilterError> {
    let row = open_memory_row(rtxn, id)?;
    Ok(row.map(|m| (m.flags & memory_flags::ACTIVE) != 0))
}

fn open_memory_row(
    rtxn: &ReadTransaction,
    id: brain_core::MemoryId,
) -> Result<Option<brain_metadata::tables::memory::MemoryMetadata>, FilterError> {
    let table = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| FilterError::Metadata(format!("open MEMORIES_TABLE: {e}")))?;
    let key = id.raw().to_be_bytes();
    let row = table
        .get(&key)
        .map_err(|e| FilterError::Metadata(format!("memory get: {e}")))?
        .map(|g| g.value());
    Ok(row)
}

/// `(valid_from_ms, valid_to_ms)`, both endpoints open-ended when `None`.
type ValidityWindowMs = (Option<u64>, Option<u64>);

fn relation_validity_ms(
    rtxn: &ReadTransaction,
    id: brain_core::RelationId,
) -> Result<Option<ValidityWindowMs>, FilterError> {
    let row = open_relation_row(rtxn, id)?;
    Ok(row.map(|r| {
        (
            r.valid_from_unix_nanos.map(|n| n / 1_000_000),
            r.valid_to_unix_nanos.map(|n| n / 1_000_000),
        )
    }))
}

fn relation_confidence(
    rtxn: &ReadTransaction,
    id: brain_core::RelationId,
) -> Result<Option<f32>, FilterError> {
    let row = open_relation_row(rtxn, id)?;
    Ok(row.map(|r| r.confidence))
}

fn relation_tombstoned(
    rtxn: &ReadTransaction,
    id: brain_core::RelationId,
) -> Result<Option<bool>, FilterError> {
    let row = open_relation_row(rtxn, id)?;
    // `RelationMetadata.tombstoned` is u8-encoded on disk;
    // brain-core's `Relation.tombstoned` is bool. The
    // filter checks the metadata row directly (no Relation
    // projection needed), so we map non-zero → true here.
    Ok(row.map(|r| r.tombstoned != 0))
}

fn relation_superseded(
    rtxn: &ReadTransaction,
    id: brain_core::RelationId,
) -> Result<Option<bool>, FilterError> {
    let row = open_relation_row(rtxn, id)?;
    Ok(row.map(|r| r.superseded_by_bytes.is_some()))
}

fn open_relation_row(
    rtxn: &ReadTransaction,
    id: brain_core::RelationId,
) -> Result<Option<brain_metadata::tables::relation::RelationMetadata>, FilterError> {
    let table = rtxn
        .open_table(RELATION_METADATA_TABLE)
        .map_err(|e| FilterError::Metadata(format!("open RELATION_METADATA_TABLE: {e}")))?;
    let key = id.to_bytes();
    let row = table
        .get(&key)
        .map_err(|e| FilterError::Metadata(format!("relation get: {e}")))?
        .map(|g| g.value());
    Ok(row)
}

// ---------------------------------------------------------------------------
// Range helpers.
// ---------------------------------------------------------------------------

fn in_range(range: &TimeRange, ms: u64) -> bool {
    if let Some(lo) = range.from_unix_ms {
        if ms < lo {
            return false;
        }
    }
    if let Some(hi) = range.to_unix_ms {
        if ms > hi {
            return false;
        }
    }
    true
}

fn window_overlaps(vf: Option<u64>, vt: Option<u64>, range: &TimeRange) -> bool {
    let win_lo = vf.unwrap_or(0);
    let win_hi = vt.unwrap_or(u64::MAX);
    let q_lo = range.from_unix_ms.unwrap_or(0);
    let q_hi = range.to_unix_ms.unwrap_or(u64::MAX);
    win_lo <= q_hi && q_lo <= win_hi
}

fn statement_temporal_match(stmt: &brain_core::Statement, range: &TimeRange) -> bool {
    // Event kind: filter on event_at if present, else
    // extracted_at_unix_nanos as fallback (the row will have
    // one or the other).
    if stmt.kind == StatementKind::Event {
        let nanos = stmt
            .event_at_unix_nanos
            .unwrap_or(stmt.extracted_at_unix_nanos);
        return in_range(range, nanos / 1_000_000);
    }
    // Fact / Preference: validity window. Open-ended bounds
    // default to [0, u64::MAX).
    let vf = stmt.valid_from_unix_nanos.map(|n| n / 1_000_000);
    let vt = stmt.valid_to_unix_nanos.map(|n| n / 1_000_000);
    window_overlaps(vf, vt, range)
}

#[cfg(test)]
mod tests;
