//! Listing, history walk, contradiction surface.
//!
//! Anchors a supersession chain from any member; surfaces Fact
//! contradictions without resolving them; dispatches to the narrowest
//! index for each query shape.

use brain_core::Statement;
use brain_core::{EntityId, MemoryId, PredicateId, StatementId, StatementKind};
use redb::{ReadTransaction, ReadableTable};

use crate::tables::statement::{
    confidence_bucket, statement_from_metadata, StatementMetadata, STATEMENTS_BY_PREDICATE_TABLE,
    STATEMENTS_BY_SUBJECT_TABLE, STATEMENTS_TABLE, STATEMENT_CHAIN_TABLE,
};

use super::crud::statement_get;
use super::StatementOpError;
use crate::tables::scope::RowScope;

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
    scope: RowScope,
    anchor: StatementId,
) -> Result<Vec<Statement>, StatementOpError> {
    // Probe: is anchor itself a chain_root? If yes the prefix scan
    // at (scope, anchor, *) hits version=1.
    let chain_table = rtxn.open_table(STATEMENT_CHAIN_TABLE)?;
    let anchor_bytes = anchor.to_bytes();
    let is_chain_root = chain_table
        .get(&(scope.namespace_id, scope.space_id_bytes, anchor_bytes, 1u32))?
        .is_some();

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

    let lo = (
        scope.namespace_id,
        scope.space_id_bytes,
        chain_root_bytes,
        0u32,
    );
    let hi = (
        scope.namespace_id,
        scope.space_id_bytes,
        chain_root_bytes,
        u32::MAX,
    );
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
    scope: RowScope,
    subject: EntityId,
    predicate: PredicateId,
) -> Result<Vec<Statement>, StatementOpError> {
    let candidates = load_active_facts_for_subject_predicate(rtxn, scope, subject, predicate)?;
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
    scope: RowScope,
    filter: &StatementListFilter,
) -> Result<Vec<Statement>, StatementOpError> {
    let cap = if filter.limit == 0 {
        DEFAULT_LIST_LIMIT
    } else {
        filter.limit.min(DEFAULT_LIST_LIMIT)
    };
    let ns = scope.namespace_id;
    let ag = scope.space_id_bytes;

    let ids: Vec<[u8; 16]> = match (filter.subject, filter.predicate, filter.kind) {
        (Some(subject), Some(predicate), Some(kind)) => {
            let by_subject = rtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
            let lo = (
                ns,
                ag,
                subject.to_bytes(),
                kind.as_u8(),
                predicate.raw(),
                0u8,
                [0u8; 16],
            );
            let hi = (
                ns,
                ag,
                subject.to_bytes(),
                kind.as_u8(),
                predicate.raw(),
                1u8,
                [0xffu8; 16],
            );
            let mut ids = Vec::new();
            for entry in by_subject.range(lo..=hi)? {
                let (k, v) = entry?;
                let (_, _, _, _, _, is_current_bit, _) = k.value();
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
            let lo = (ns, ag, subject.to_bytes(), 0u8, 0u32, 0u8, [0u8; 16]);
            let hi = (
                ns,
                ag,
                subject.to_bytes(),
                u8::MAX,
                u32::MAX,
                1u8,
                [0xffu8; 16],
            );
            let mut ids = Vec::new();
            for entry in by_subject.range(lo..=hi)? {
                let (k, v) = entry?;
                let (_, _, _, k_kind, k_pred, is_current_bit, _) = k.value();
                if filter.current_only && is_current_bit == 0 {
                    continue;
                }
                if let Some(want_kind) = filter.kind {
                    if k_kind != want_kind.as_u8() {
                        continue;
                    }
                }
                if let Some(want_pred) = filter.predicate {
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
            let lo = (ns, ag, predicate.raw(), 0u8, 0u8, [0u8; 16]);
            let hi = (ns, ag, predicate.raw(), u8::MAX, u8::MAX, [0xffu8; 16]);
            let mut ids = Vec::new();
            for entry in by_predicate.range(lo..=hi)? {
                let (k, v) = entry?;
                let (_, _, _, k_kind, _, _) = k.value();
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
            // No anchoring index — full scan of the primary table,
            // filtered to the caller's scope on the row itself (the
            // primary key is scope-agnostic).
            let t = rtxn.open_table(STATEMENTS_TABLE)?;
            let mut ids = Vec::new();
            for entry in t.iter()? {
                let (k, v) = entry?;
                let m: StatementMetadata = v.value();
                if m.namespace_id != ns || m.space_id_bytes != ag {
                    continue;
                }
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
            // Unconditional scope wall — a row whose scope differs from
            // the caller's is never returned, on any index path.
            if m.namespace_id != ns || m.space_id_bytes != ag {
                continue;
            }
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
// Keyset (seek) pagination.
// ---------------------------------------------------------------------------

/// The exact position of one statement inside whichever secondary index
/// the current filter selects. It is the resume point for keyset
/// pagination: the trailing `id` plus the discriminant key columns
/// (`kind`, `predicate_id`, `is_current`, `confidence_bucket`) let the
/// next page rebuild the row's full index key and seek strictly past it
/// — without a point lookup back into the primary table, so a boundary
/// row hard-forgotten between two page fetches can never strand the walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatementListCursor {
    pub id: [u8; 16],
    pub kind: u8,
    pub predicate_id: u32,
    pub is_current: u8,
    pub confidence_bucket: u8,
}

impl StatementListCursor {
    fn from_row(m: &StatementMetadata) -> Self {
        Self {
            id: m.statement_id_bytes,
            kind: m.kind,
            predicate_id: m.predicate_id,
            is_current: m.is_current,
            confidence_bucket: confidence_bucket(m.confidence),
        }
    }
}

/// One page of a keyset-paginated statement listing.
pub struct StatementPage {
    /// Rows on this page, in the selected index's scan order.
    pub rows: Vec<Statement>,
    /// `true` when at least one more wire-visible row exists past the
    /// last row on this page.
    pub has_more: bool,
    /// Position of the last emitted row — the opaque resume point for the
    /// next page. `None` on an empty page.
    pub last: Option<StatementListCursor>,
}

/// Wire-visible predicates applied *inside* the page walk (they are not
/// index columns, so they must be evaluated per row before a row counts
/// toward `limit` — otherwise a page could come back short while a full
/// next page still exists).
#[derive(Clone, Copy, Debug, Default)]
pub struct StatementPageExtra {
    /// When `false`, tombstoned rows are excluded (matches the wire
    /// `include_tombstoned = false` default).
    pub include_tombstoned: bool,
    /// Optional object-time window `(lo, hi)` inclusive. Events match on
    /// `event_at`; other kinds on their `[valid_from, valid_to]` span
    /// (an unset `valid_from` defaults to `extracted_at`, an unset
    /// `valid_to` is open-ended).
    pub time_range: Option<(u64, u64)>,
}

fn statement_time_admits(m: &StatementMetadata, lo: u64, hi: u64) -> bool {
    if m.kind == StatementKind::Event.as_u8() {
        m.event_at_unix_nanos
            .map(|t| t >= lo && t <= hi)
            .unwrap_or(false)
    } else {
        let from = m.valid_from_unix_nanos.unwrap_or(m.extracted_at_unix_nanos);
        let to = m.valid_to_unix_nanos.unwrap_or(u64::MAX);
        from <= hi && to >= lo
    }
}

/// Decide whether a fully-loaded metadata row is visible on the wire for
/// the given filter (scope wall + current / tombstone / confidence /
/// kind / time). Shared by every index shape below.
fn statement_row_admits(
    m: &StatementMetadata,
    ns: u32,
    ag: [u8; 16],
    filter: &StatementListFilter,
    extra: &StatementPageExtra,
) -> bool {
    if m.namespace_id != ns || m.space_id_bytes != ag {
        return false;
    }
    // Tombstoned rows are hidden unless explicitly requested, and always
    // hidden under current_only.
    if m.is_tombstoned() && (!extra.include_tombstoned || filter.current_only) {
        return false;
    }
    if filter.current_only && m.is_current == 0 {
        return false;
    }
    if let Some(min) = filter.min_confidence {
        if m.confidence < min {
            return false;
        }
    }
    if let Some(want_kind) = filter.kind {
        if m.kind != want_kind.as_u8() {
            return false;
        }
    }
    if let Some((lo, hi)) = extra.time_range {
        if !statement_time_admits(m, lo, hi) {
            return false;
        }
    }
    true
}

/// Keyset (seek) page over the statements matching `filter`, resuming
/// strictly past `after` when present.
///
/// Dispatches to the same secondary index as [`statement_list`], but
/// pages directly from the store: it seeks past the cursor's exact index
/// key (rebuilt from `after`'s discriminant columns) instead of
/// materializing a fixed window and slicing it in memory. So rows beyond
/// the old 1000-row ceiling are reachable and each page costs one page's
/// worth of scan, not (pages × window).
///
/// Rows come back in the selected index's native key order (stable
/// across pages as long as the underlying rows do not change). Every
/// wire-visible predicate — including tombstone and time filters that
/// are not index columns — is applied in-walk so `has_more` and the
/// returned `last` are exact.
pub fn statement_list_page(
    rtxn: &ReadTransaction,
    scope: RowScope,
    filter: &StatementListFilter,
    extra: &StatementPageExtra,
    after: Option<StatementListCursor>,
    limit: usize,
) -> Result<StatementPage, StatementOpError> {
    use std::ops::Bound;

    let ns = scope.namespace_id;
    let ag = scope.space_id_bytes;

    let mut rows: Vec<Statement> = Vec::new();
    let mut has_more = false;
    let mut last: Option<StatementListCursor> = None;

    // A page is limit=0 → nothing to return (callers validate limit >= 1,
    // but be defensive so an empty walk doesn't loop).
    if limit == 0 {
        return Ok(StatementPage {
            rows,
            has_more: false,
            last: None,
        });
    }

    let s_table = rtxn.open_table(STATEMENTS_TABLE)?;

    // Push one admitted row; returns true when the page is now full and a
    // further admitted row was seen (so the caller should stop).
    macro_rules! try_push {
        ($m:expr) => {{
            let m: StatementMetadata = $m;
            if statement_row_admits(&m, ns, ag, filter, extra) {
                if rows.len() == limit {
                    has_more = true;
                    true
                } else {
                    last = Some(StatementListCursor::from_row(&m));
                    if let Some(s) = statement_from_metadata(&m) {
                        rows.push(s);
                    }
                    false
                }
            } else {
                false
            }
        }};
    }

    match (filter.subject, filter.predicate) {
        (Some(subject), _) => {
            // Subject-anchored index: (ns, ag, subject, kind, pred,
            // is_current, id). Resume strictly past the cursor's rebuilt
            // key; kind / predicate narrowing stays an in-loop filter so
            // one bound shape covers every subject query.
            let by_subject = rtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
            let subj = subject.to_bytes();
            let lo_key = (ns, ag, subj, 0u8, 0u32, 0u8, [0u8; 16]);
            let hi_key = (ns, ag, subj, u8::MAX, u32::MAX, 1u8, [0xffu8; 16]);
            let lo_bound = match after {
                Some(c) => {
                    Bound::Excluded((ns, ag, subj, c.kind, c.predicate_id, c.is_current, c.id))
                }
                None => Bound::Included(lo_key),
            };
            for entry in by_subject.range((lo_bound, Bound::Included(hi_key)))? {
                let (k, v) = entry?;
                let (_, _, _, k_kind, k_pred, is_current_bit, _) = k.value();
                if filter.current_only && is_current_bit == 0 {
                    continue;
                }
                if let Some(want) = filter.kind {
                    if k_kind != want.as_u8() {
                        continue;
                    }
                }
                if let Some(want) = filter.predicate {
                    if k_pred != want.raw() {
                        continue;
                    }
                }
                let Some(m) = s_table.get(&v.value())?.map(|g| g.value()) else {
                    continue;
                };
                if try_push!(m) {
                    break;
                }
            }
        }
        (None, Some(predicate)) => {
            // Predicate-anchored index: (ns, ag, pred, kind,
            // confidence_bucket, id).
            let by_predicate = rtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE)?;
            let pred = predicate.raw();
            let lo_key = (ns, ag, pred, 0u8, 0u8, [0u8; 16]);
            let hi_key = (ns, ag, pred, u8::MAX, u8::MAX, [0xffu8; 16]);
            let lo_bound = match after {
                Some(c) => Bound::Excluded((ns, ag, pred, c.kind, c.confidence_bucket, c.id)),
                None => Bound::Included(lo_key),
            };
            for entry in by_predicate.range((lo_bound, Bound::Included(hi_key)))? {
                let (k, v) = entry?;
                let (_, _, _, k_kind, _, _) = k.value();
                if let Some(want) = filter.kind {
                    if k_kind != want.as_u8() {
                        continue;
                    }
                }
                let Some(m) = s_table.get(&v.value())?.map(|g| g.value()) else {
                    continue;
                };
                if try_push!(m) {
                    break;
                }
            }
        }
        (None, None) => {
            // No anchoring index — page the primary table, which is keyed
            // by statement id, seeking strictly past the cursor id.
            let lo_bound: Bound<[u8; 16]> = match after {
                Some(c) => Bound::Excluded(c.id),
                None => Bound::Unbounded,
            };
            for entry in s_table.range::<[u8; 16]>((lo_bound, Bound::Unbounded))? {
                let (_, v) = entry?;
                if try_push!(v.value()) {
                    break;
                }
            }
        }
    }

    Ok(StatementPage {
        rows,
        has_more,
        last,
    })
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
    scope: RowScope,
    subject: EntityId,
    predicate: PredicateId,
) -> Result<Vec<Statement>, StatementOpError> {
    let bys = rtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
    // Cumulative Facts can have many current rows; collect them all via a
    // range scan over the is_current=1 prefix (the trailing statement id
    // is part of the key now, so an exact get can't address one row).
    let lo = (
        scope.namespace_id,
        scope.space_id_bytes,
        subject.to_bytes(),
        StatementKind::Fact.as_u8(),
        predicate.raw(),
        1u8,
        [0u8; 16],
    );
    let hi = (
        scope.namespace_id,
        scope.space_id_bytes,
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
