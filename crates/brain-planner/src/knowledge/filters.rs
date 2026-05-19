//! Post-fusion filter chain (phase 23.5).
//!
//! Implements §24/00 §"Filter chain". Reads metadata from redb
//! to evaluate per-filter predicates against each fused item,
//! drops items that don't pass, applies the final limit, and
//! reports per-step survivor counts for EXPLAIN/TRACE (23.8).
//!
//! Filter order (§24/00 binding):
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
//!
//! Limit applied after all five.

use brain_core::knowledge::StatementKind;
use brain_core::{MemoryKind, PredicateId};
use brain_index::RankedItemId;
use brain_metadata::statement_ops::statement_get;
use brain_metadata::tables::knowledge::relation::RELATION_METADATA_TABLE;
use brain_metadata::tables::memory::{flags as memory_flags, MEMORIES_TABLE};
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
}

/// Per-step survivor counts. Surfaces in EXPLAIN/TRACE (23.8).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterChainStats {
    pub before: u32,
    pub after_type: u32,
    pub after_temporal: u32,
    pub after_confidence: u32,
    pub after_tombstone: u32,
    pub after_supersession: u32,
    pub after_limit: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum FilterError {
    #[error("metadata: {0}")]
    Metadata(String),
}

/// Apply the §24/00 filter chain in order, then truncate to
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

    let mut items = filter_supersession(items, chain, &rtxn)?;
    stats.after_supersession = items.len() as u32;

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
    // A redb table that's never been written to doesn't exist
    // yet. "No memory row at all" can't be tombstoned (no rows
    // exist), so keep the item — flipping these to drop would
    // make every query on a fresh shard return empty before
    // the first write.
    let memories_present = table_exists(rtxn, MEMORIES_TABLE)?;
    let relations_present = table_exists(rtxn, RELATION_METADATA_TABLE)?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let keep = match item.id {
            RankedItemId::Memory(id) => {
                !memories_present || memory_active(rtxn, id)?.unwrap_or(false)
            }
            RankedItemId::Statement(id) => {
                let Some(stmt) = statement_get(rtxn, id)
                    .map_err(|e| FilterError::Metadata(format!("statement_get: {e}")))?
                else {
                    continue;
                };
                !stmt.tombstoned
            }
            RankedItemId::Relation(id) => {
                !relations_present || relation_tombstoned(rtxn, id)?.is_some_and(|t| !t)
            }
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
    let relations_present = table_exists(rtxn, RELATION_METADATA_TABLE)?;
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
            RankedItemId::Relation(id) => {
                !relations_present || relation_superseded(rtxn, id)?.is_some_and(|x| !x)
            }
            // Memory / Entity have no supersession concept.
            RankedItemId::Memory(_) | RankedItemId::Entity(_) => true,
        };
        if keep {
            out.push(item);
        }
    }
    Ok(out)
}

fn table_exists<K, V>(
    rtxn: &ReadTransaction,
    def: redb::TableDefinition<'_, K, V>,
) -> Result<bool, FilterError>
where
    K: redb::Key + 'static,
    V: redb::Value + 'static,
{
    match rtxn.open_table(def) {
        Ok(_) => Ok(true),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
        Err(e) => Err(FilterError::Metadata(format!("open_table: {e}"))),
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
    // `TableDoesNotExist` = a fresh shard has never had a memory
    // written, so the redb table hasn't been materialised. Treat
    // as "no such row" — same defensive pattern audit_ops.rs and
    // sweeper_ops.rs use for read paths on empty DBs.
    let table = match rtxn.open_table(MEMORIES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(FilterError::Metadata(format!("open MEMORIES_TABLE: {e}"))),
    };
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
) -> Result<Option<brain_metadata::tables::knowledge::relation::RelationMetadata>, FilterError> {
    // Same defensive pattern as `open_memory_row`: a fresh shard
    // with no relations written has no on-disk RELATION_METADATA_TABLE;
    // treat as "no such row" rather than panicking.
    let table = match rtxn.open_table(RELATION_METADATA_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => {
            return Err(FilterError::Metadata(format!(
                "open RELATION_METADATA_TABLE: {e}"
            )))
        }
    };
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

fn statement_temporal_match(stmt: &brain_core::knowledge::Statement, range: &TimeRange) -> bool {
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
