//! Listing, history walk, contradiction surface.
//!
//! Anchors a supersession chain from any member; surfaces Fact
//! contradictions without resolving them; dispatches to the narrowest
//! index for each query shape.

use brain_core::Statement;
use brain_core::{EntityId, MemoryId, PredicateId, StatementId, StatementKind};
use redb::{ReadTransaction, ReadableTable};

use crate::tables::statement::{
    statement_from_metadata, StatementMetadata, STATEMENTS_BY_PREDICATE_TABLE,
    STATEMENTS_BY_SUBJECT_TABLE, STATEMENTS_TABLE, STATEMENT_CHAIN_TABLE,
};

use super::crud::statement_get;
use super::StatementOpError;

/// Scan threshold above which `statements_citing_memory` logs a
/// `tracing::warn`. A full-scan of more than ~50K statements is the
/// signal that we should be standing up a `STATEMENT_EVIDENCE_INDEX`
/// secondary table instead.
const STATEMENTS_CITING_MEMORY_SLOW_SCAN_THRESHOLD: usize = 50_000;

// ---------------------------------------------------------------------------
// Filter struct.
// ---------------------------------------------------------------------------

/// Filter passed to [`statement_list`]. Empty fields mean "any".
#[derive(Clone, Debug, Default)]
pub struct StatementListFilter {
    pub subject: Option<EntityId>,
    pub predicate: Option<PredicateId>,
    pub kind: Option<StatementKind>,
    pub current_only: bool,
    pub min_confidence: Option<f32>,
    /// Hard cap on returned rows. `0` defaults to [`DEFAULT_LIST_LIMIT`].
    pub limit: usize,
}

/// Default cap when [`StatementListFilter::limit`] is `0`. Cursor
/// pagination may replace this later.
pub const DEFAULT_LIST_LIMIT: usize = 1_000;

// ---------------------------------------------------------------------------
// Read paths.
// ---------------------------------------------------------------------------

/// Walk a supersession chain in version ascending order. Anchor may
/// be the chain root id or any member of the chain.
pub fn statement_history(
    rtxn: &ReadTransaction,
    anchor: StatementId,
) -> Result<Vec<Statement>, StatementOpError> {
    // Probe: is anchor itself a chain_root? If yes the prefix scan
    // at (anchor, *) hits version=1.
    let chain_table = rtxn.open_table(STATEMENT_CHAIN_TABLE)?;
    let anchor_bytes = anchor.to_bytes();
    let is_chain_root = chain_table.get(&(anchor_bytes, 1u32))?.is_some();

    let chain_root_bytes = if is_chain_root {
        anchor_bytes
    } else {
        // Load anchor and follow `chain_root`.
        let s_table = rtxn.open_table(STATEMENTS_TABLE)?;
        let row: Option<StatementMetadata> = s_table.get(&anchor_bytes)?.map(|g| g.value());
        let Some(m) = row else {
            return Err(StatementOpError::NotFound(anchor));
        };
        m.chain_root_bytes
    };

    let lo = (chain_root_bytes, 0u32);
    let hi = (chain_root_bytes, u32::MAX);
    let s_table = rtxn.open_table(STATEMENTS_TABLE)?;
    let mut out = Vec::new();
    for entry in chain_table.range(lo..=hi)? {
        let (_, v) = entry?;
        let sid_bytes = v.value();
        let m_row: Option<StatementMetadata> = s_table.get(&sid_bytes)?.map(|g| g.value());
        if let Some(m) = m_row {
            if let Some(s) = statement_from_metadata(&m) {
                out.push(s);
            }
        }
    }
    Ok(out)
}

/// Surface contradicting active Facts for `(subject, predicate)`.
/// Returns `Vec::new()` when no contradiction (zero or one distinct
/// object value).
pub fn statements_contradicting(
    rtxn: &ReadTransaction,
    subject: EntityId,
    predicate: PredicateId,
) -> Result<Vec<Statement>, StatementOpError> {
    let candidates = load_active_facts_for_subject_predicate(rtxn, subject, predicate)?;
    if candidates.len() < 2 {
        return Ok(Vec::new());
    }
    let mut iter = candidates.iter();
    let first = iter.next().expect("len >= 2").object.clone();
    let any_disagree = iter.any(|s| s.object != first);
    if any_disagree {
        Ok(candidates)
    } else {
        Ok(Vec::new())
    }
}

/// List statements matching `filter`. Dispatches to the narrowest
/// applicable index.
pub fn statement_list(
    rtxn: &ReadTransaction,
    filter: &StatementListFilter,
) -> Result<Vec<Statement>, StatementOpError> {
    let cap = if filter.limit == 0 {
        DEFAULT_LIST_LIMIT
    } else {
        filter.limit.min(DEFAULT_LIST_LIMIT)
    };

    let ids: Vec<[u8; 16]> = match (filter.subject, filter.predicate, filter.kind) {
        (Some(subject), Some(predicate), Some(kind)) => {
            let by_subject = rtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
            let lo = (
                subject.to_bytes(),
                kind.as_u8(),
                predicate.raw(),
                0u8,
                [0u8; 16],
            );
            let hi = (
                subject.to_bytes(),
                kind.as_u8(),
                predicate.raw(),
                1u8,
                [0xffu8; 16],
            );
            let mut ids = Vec::new();
            for entry in by_subject.range(lo..=hi)? {
                let (k, v) = entry?;
                let (_, _, _, is_current_bit, _) = k.value();
                if filter.current_only && is_current_bit == 0 {
                    continue;
                }
                ids.push(v.value());
                if ids.len() >= cap {
                    break;
                }
            }
            ids
        }
        (Some(subject), _, _) => {
            let by_subject = rtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
            let lo = (subject.to_bytes(), 0u8, 0u32, 0u8, [0u8; 16]);
            let hi = (subject.to_bytes(), u8::MAX, u32::MAX, 1u8, [0xffu8; 16]);
            let mut ids = Vec::new();
            for entry in by_subject.range(lo..=hi)? {
                let (k, v) = entry?;
                let (_, k_kind, _, is_current_bit, _) = k.value();
                if filter.current_only && is_current_bit == 0 {
                    continue;
                }
                if let Some(want_kind) = filter.kind {
                    if k_kind != want_kind.as_u8() {
                        continue;
                    }
                }
                if let Some(want_pred) = filter.predicate {
                    let (_, _, k_pred, _, _) = k.value();
                    if k_pred != want_pred.raw() {
                        continue;
                    }
                }
                ids.push(v.value());
                if ids.len() >= cap {
                    break;
                }
            }
            ids
        }
        (None, Some(predicate), _) => {
            let by_predicate = rtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE)?;
            let lo = (predicate.raw(), 0u8, 0u8);
            let hi = (predicate.raw(), u8::MAX, u8::MAX);
            let mut ids = Vec::new();
            for entry in by_predicate.range(lo..=hi)? {
                let (k, v) = entry?;
                let (_, k_kind, _) = k.value();
                if let Some(want_kind) = filter.kind {
                    if k_kind != want_kind.as_u8() {
                        continue;
                    }
                }
                ids.push(v.value());
                if ids.len() >= cap {
                    break;
                }
            }
            ids
        }
        (None, None, _) => {
            let t = rtxn.open_table(STATEMENTS_TABLE)?;
            let mut ids = Vec::new();
            for entry in t.iter()? {
                let (k, _) = entry?;
                ids.push(k.value());
                if ids.len() >= cap {
                    break;
                }
            }
            ids
        }
    };

    let s_table = rtxn.open_table(STATEMENTS_TABLE)?;
    let mut out = Vec::with_capacity(ids.len());
    for sid in ids {
        let row: Option<StatementMetadata> = s_table.get(&sid)?.map(|g| g.value());
        if let Some(m) = row {
            if filter.current_only && (m.is_current == 0 || m.is_tombstoned()) {
                continue;
            }
            if let Some(min) = filter.min_confidence {
                if m.confidence < min {
                    continue;
                }
            }
            if let Some(want_kind) = filter.kind {
                if m.kind != want_kind.as_u8() {
                    continue;
                }
            }
            if let Some(s) = statement_from_metadata(&m) {
                out.push(s);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Internal helpers.
// ---------------------------------------------------------------------------

/// Find every active statement that cites `memory_id` in its
/// `evidence_inline` list. Used by the FORGET cascade worker to count
/// or audit dependents before re-derivation; the cascade engine itself
/// scans + mutates in a single pass via `cascade_forget_to_statements`.
///
/// Statements whose `evidence_inline` is empty (or which never carried
/// inline evidence) are skipped — they have nothing to cascade off.
/// Overflow-only evidence is also skipped in v1; widening that requires
/// the planned `STATEMENT_EVIDENCE_INDEX` table.
///
/// Performance: full table scan. For ≤100K rows this completes in ~ms.
/// Above `STATEMENTS_CITING_MEMORY_SLOW_SCAN_THRESHOLD` the call
/// emits a `tracing::warn` so the operator sees the cost growing and
/// can plan the secondary-index migration.
pub fn statements_citing_memory(
    rtxn: &ReadTransaction,
    memory_id: MemoryId,
) -> Result<Vec<StatementId>, StatementOpError> {
    let memory_bytes = memory_id.to_be_bytes();
    let table = rtxn.open_table(STATEMENTS_TABLE)?;
    let mut out = Vec::new();
    let mut scanned: usize = 0;
    for entry in table.iter()? {
        let (_, v) = entry?;
        let row: StatementMetadata = v.value();
        scanned += 1;
        if row.is_tombstoned() {
            continue;
        }
        if row
            .evidence_inline
            .iter()
            .any(|e| e.memory_id_bytes == memory_bytes)
        {
            out.push(row.statement_id());
        }
    }
    if scanned > STATEMENTS_CITING_MEMORY_SLOW_SCAN_THRESHOLD {
        tracing::warn!(
            target: "brain_metadata::statement",
            scanned,
            matched = out.len(),
            ?memory_id,
            "statements_citing_memory full-scanned above {threshold} rows — consider a STATEMENT_EVIDENCE_INDEX",
            threshold = STATEMENTS_CITING_MEMORY_SLOW_SCAN_THRESHOLD,
        );
    }
    Ok(out)
}

/// Load the active Facts for (subject, predicate) via a read txn.
fn load_active_facts_for_subject_predicate(
    rtxn: &ReadTransaction,
    subject: EntityId,
    predicate: PredicateId,
) -> Result<Vec<Statement>, StatementOpError> {
    let bys = rtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
    // Cumulative Facts can have many current rows; collect them all via a
    // range scan over the is_current=1 prefix (the trailing statement id
    // is part of the key now, so an exact get can't address one row).
    let lo = (
        subject.to_bytes(),
        StatementKind::Fact.as_u8(),
        predicate.raw(),
        1u8,
        [0u8; 16],
    );
    let hi = (
        subject.to_bytes(),
        StatementKind::Fact.as_u8(),
        predicate.raw(),
        1u8,
        [0xffu8; 16],
    );
    let mut ids: Vec<[u8; 16]> = Vec::new();
    for entry in bys.range(lo..=hi)? {
        let (_, v) = entry?;
        ids.push(v.value());
    }
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(s) = statement_get(rtxn, StatementId::from(id))? {
            out.push(s);
        }
    }
    Ok(out)
}
