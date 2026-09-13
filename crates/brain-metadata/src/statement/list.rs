//! Listing, history walk, contradiction surface.
//!
//! Anchors a supersession chain from any member; surfaces Fact
//! contradictions without resolving them; dispatches to the narrowest
//! index for each query shape.

use brain_core::Statement;
use brain_core::{EntityId, MemoryId, PredicateId, StatementId, StatementKind};
use redb::{ReadTransaction, ReadableTable};

use crate::tables::statement::{
    statement_from_metadata, StatementMetadata, STATEMENTS_BY_PREDICATE_ID_TABLE,
    STATEMENTS_BY_PREDICATE_TABLE, STATEMENTS_BY_SUBJECT_ID_TABLE, STATEMENTS_BY_SUBJECT_TABLE,
    STATEMENTS_TABLE, STATEMENT_CHAIN_TABLE,
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
            // A PRESENT chain row that fails to decode is corruption, not
            // absence (genuine absence is the `None` from the get above). Fail-
            // stop rather than silently drop it from the history (invariant #7).
            let s = statement_from_metadata(&m).ok_or(StatementOpError::DecodeFailed)?;
            out.push(s);
        }
    }
    Ok(out)
}

/// One page of a keyset-paginated supersession-chain history walk.
#[derive(Debug)]
pub struct StatementHistoryPage {
    /// This page's chain entries, `version` ascending.
    pub rows: Vec<Statement>,
    /// True iff a further present entry exists past this page — exact, so a
    /// short page never hides a full next one.
    pub has_more: bool,
    /// The `version` of the last row on this page — the keyset resume point
    /// for the next request. `None` when the page is empty.
    pub last_version: Option<u32>,
    /// The full chain length (count of present entries across every version),
    /// independent of the page window.
    pub total: u32,
    /// The resolved chain root — lets the caller mint a cursor bound to this
    /// chain and reject one replayed against a different anchor.
    pub chain_root: [u8; 16],
}

/// Paginated variant of [`statement_history`]: walk the supersession chain
/// keyset-style, resuming strictly past `after_version` and returning at most
/// `limit` entries.
///
/// The keyset is the chain's **immutable `version`** number (the 4th component
/// of the `STATEMENT_CHAIN_TABLE` key), so the exhaustive-tiling contract holds
/// under concurrent appends: following the cursor to exhaustion yields every
/// present version exactly once. `has_more` is exact — it peeks one entry past
/// the page. A present-but-undecodable chain row is corruption, not absence, and
/// fails stop (invariant #7), exactly as [`statement_history`] does.
pub fn statement_history_page(
    rtxn: &ReadTransaction,
    scope: RowScope,
    anchor: StatementId,
    after_version: Option<u32>,
    limit: usize,
) -> Result<StatementHistoryPage, StatementOpError> {
    let chain_table = rtxn.open_table(STATEMENT_CHAIN_TABLE)?;
    let anchor_bytes = anchor.to_bytes();
    let is_chain_root = chain_table
        .get(&(scope.namespace_id, scope.space_id_bytes, anchor_bytes, 1u32))?
        .is_some();
    let chain_root_bytes = if is_chain_root {
        anchor_bytes
    } else {
        let s_table = rtxn.open_table(STATEMENTS_TABLE)?;
        let row: Option<StatementMetadata> = s_table.get(&anchor_bytes)?.map(|g| g.value());
        let Some(m) = row else {
            return Err(StatementOpError::NotFound(anchor));
        };
        m.chain_root_bytes
    };

    let s_table = rtxn.open_table(STATEMENTS_TABLE)?;

    // Full chain length: count present entries across every version. Cheap for
    // a typical short chain; independent of the page window.
    let mut total: u32 = 0;
    {
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
        for entry in chain_table.range(lo..=hi)? {
            let (_, v) = entry?;
            if s_table.get(&v.value())?.is_some() {
                total = total.saturating_add(1);
            }
        }
    }

    // Page walk: seek strictly past `after_version`.
    let start = after_version.map_or(0u32, |v| v.saturating_add(1));
    let lo = (
        scope.namespace_id,
        scope.space_id_bytes,
        chain_root_bytes,
        start,
    );
    let hi = (
        scope.namespace_id,
        scope.space_id_bytes,
        chain_root_bytes,
        u32::MAX,
    );

    let mut rows: Vec<Statement> = Vec::new();
    let mut last_version: Option<u32> = None;
    let mut has_more = false;
    for entry in chain_table.range(lo..=hi)? {
        let (k, v) = entry?;
        let version = k.value().3;
        let sid_bytes = v.value();
        let Some(m) = s_table.get(&sid_bytes)?.map(|g| g.value()) else {
            // Genuine absence (retracted / never materialized): skip.
            continue;
        };
        if rows.len() == limit {
            // A further present entry exists → the page is full and more remain.
            has_more = true;
            break;
        }
        // A PRESENT-but-undecodable row is corruption — fail stop (invariant #7).
        let s = statement_from_metadata(&m).ok_or(StatementOpError::DecodeFailed)?;
        rows.push(s);
        last_version = Some(version);
    }

    Ok(StatementHistoryPage {
        rows,
        has_more,
        last_version,
        total,
        chain_root: chain_root_bytes,
    })
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
            // A PRESENT row that survives the filters but fails to decode is
            // corruption, not absence (genuine absence is the `None` from the
            // get above). Fail-stop rather than silently drop it (invariant #7).
            let s = statement_from_metadata(&m).ok_or(StatementOpError::DecodeFailed)?;
            out.push(s);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Keyset (seek) pagination.
// ---------------------------------------------------------------------------

/// The resume point for keyset pagination: the immutable statement id of
/// the last emitted row.
///
/// Every anchored page walk (subject, predicate, or unanchored) orders by
/// the statement id — a UUIDv7 that never changes for the life of the row.
/// Seeking strictly past this id can therefore neither gap a row that was
/// superseded / retracted / had its confidence recomputed between two page
/// fetches (its ordering position does not move) nor re-emit one. Mutable
/// attributes (`is_current`, `confidence`, `kind`, `predicate`, tombstone,
/// time) are applied as in-walk filters against the primary row, never as
/// cursor key columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatementListCursor {
    pub id: [u8; 16],
}

impl StatementListCursor {
    fn from_row(m: &StatementMetadata) -> Self {
        Self {
            id: m.statement_id_bytes,
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
    // Predicate is no longer a resume-key column on the paged walk (it
    // pages by immutable id), so narrow on the row here.
    if let Some(want_pred) = filter.predicate {
        if m.predicate_id != want_pred.raw() {
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
/// Pages directly from the store over an **immutable** id-ordered index
/// (subject → [`STATEMENTS_BY_SUBJECT_ID_TABLE`], predicate →
/// [`STATEMENTS_BY_PREDICATE_ID_TABLE`], unanchored → the id-keyed primary
/// table), seeking strictly past the cursor's statement id instead of
/// materializing a fixed window and slicing it in memory. So rows beyond
/// the old 1000-row ceiling are reachable and each page costs one page's
/// worth of scan, not (pages × window).
///
/// Because the resume ordering is the statement id — which never changes
/// for the life of a row — a row that is superseded, retracted, or has its
/// confidence recomputed *between* two page fetches keeps its ordering
/// position: it can neither be gapped (relocated behind the cursor) nor
/// re-emitted. Every mutable, wire-visible predicate (`is_current`,
/// confidence, kind, predicate, tombstone, time) is applied in-walk
/// against the primary row via [`statement_row_admits`], so `has_more`
/// and the returned `last` are exact.
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

    // Push one admitted row; evaluates to `Ok(true)` when the page is now
    // full and a further admitted row was seen (so the caller should stop).
    //
    // Every `$m` handed here is a PRESENT primary row (the id-index entry it
    // came from was already resolved through `s_table.get`; a genuinely
    // absent row is skipped at that call site before reaching this macro). A
    // present row that fails to decode is corruption, not absence — so we
    // fail-stop with `DecodeFailed` rather than silently skipping it and
    // advancing the cursor past it, which would hide the corruption and drop
    // the row from every future page (invariant #7: no silent corruption).
    macro_rules! try_push {
        ($m:expr) => {{
            let m: StatementMetadata = $m;
            if statement_row_admits(&m, ns, ag, filter, extra) {
                if rows.len() == limit {
                    has_more = true;
                    Ok::<bool, StatementOpError>(true)
                } else {
                    let s = statement_from_metadata(&m).ok_or(StatementOpError::DecodeFailed)?;
                    last = Some(StatementListCursor::from_row(&m));
                    rows.push(s);
                    Ok::<bool, StatementOpError>(false)
                }
            } else {
                Ok::<bool, StatementOpError>(false)
            }
        }};
    }

    match (filter.subject, filter.predicate) {
        (Some(subject), _) => {
            // Subject-anchored immutable index: (ns, ag, subject, id).
            // Its only varying column is the immutable statement id, so a
            // supersession / confidence recompute between pages cannot
            // relocate a row out from under the cursor. Resume strictly
            // past the cursor id; every discriminant (kind / predicate /
            // is_current / tombstone / confidence / time) is an in-walk
            // filter on the primary row via `statement_row_admits`.
            let by_subject_id = rtxn.open_table(STATEMENTS_BY_SUBJECT_ID_TABLE)?;
            let subj = subject.to_bytes();
            let lo_key = (ns, ag, subj, [0u8; 16]);
            let hi_key = (ns, ag, subj, [0xffu8; 16]);
            let lo_bound = match after {
                Some(c) => Bound::Excluded((ns, ag, subj, c.id)),
                None => Bound::Included(lo_key),
            };
            for entry in by_subject_id.range((lo_bound, Bound::Included(hi_key)))? {
                let (k, _) = entry?;
                let (_, _, _, id) = k.value();
                let Some(m) = s_table.get(&id)?.map(|g| g.value()) else {
                    continue;
                };
                if try_push!(m)? {
                    break;
                }
            }
        }
        (None, Some(predicate)) => {
            // Predicate-anchored immutable index: (ns, ag, pred, id). Same
            // id-ordered resume discipline as the subject path — the
            // mutable confidence bucket is a per-row filter, not a key
            // column, so a recompute between pages never gaps or dups.
            let by_predicate_id = rtxn.open_table(STATEMENTS_BY_PREDICATE_ID_TABLE)?;
            let pred = predicate.raw();
            let lo_key = (ns, ag, pred, [0u8; 16]);
            let hi_key = (ns, ag, pred, [0xffu8; 16]);
            let lo_bound = match after {
                Some(c) => Bound::Excluded((ns, ag, pred, c.id)),
                None => Bound::Included(lo_key),
            };
            for entry in by_predicate_id.range((lo_bound, Bound::Included(hi_key)))? {
                let (k, _) = entry?;
                let (_, _, _, id) = k.value();
                let Some(m) = s_table.get(&id)?.map(|g| g.value()) else {
                    continue;
                };
                if try_push!(m)? {
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
                if try_push!(v.value())? {
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

// ---------------------------------------------------------------------------
// Tests — keyset pagination under mid-page mutation.
//
// These exercise the exact defect the immutable-id resume closes: a row
// that changes a formerly-key column (is_current on the by-subject index,
// confidence bucket on the by-predicate index) between two page fetches.
// Under the old mutable-column cursor such a row was silently gapped or
// duplicated; under id-order resume it appears exactly once.
// ---------------------------------------------------------------------------
#[cfg(all(test, not(miri)))]
mod tests {
    use super::super::crud::{rekey_predicate_index, statement_create};
    use super::super::supersede::statement_supersede;
    use super::*;
    use crate::schema::predicate::predicate_intern;
    use crate::tables::statement::STATEMENTS_TABLE;
    use brain_core::{
        Entity, EntityType, EvidenceRef, ExtractorId, SessionId, StatementObject, SubjectRef,
    };

    const T0: u64 = 1_700_000_000_000_000_000;

    fn test_scope() -> RowScope {
        RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xAB; 16])
    }

    fn open_db() -> (tempfile::TempDir, crate::MetadataDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::MetadataDb::open(dir.path().join("md.redb")).unwrap();
        (dir, db)
    }

    fn make_entity(db: &crate::MetadataDb, name: &str) -> EntityId {
        let id = EntityId::new();
        let normalized = crate::entity::ops::normalize_name(name);
        let e = Entity::new_active(id, EntityType::PERSON_ID, name.to_string(), normalized, T0);
        let wtxn = db.write_txn().unwrap();
        crate::entity::ops::entity_put(&wtxn, test_scope(), SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    /// Fact / Entity-object predicate. `is_stateful=false` keeps it
    /// cumulative, so many rows for one (subject, predicate) stay current.
    fn intern_cumulative_pred(db: &crate::MetadataDb, name: &str) -> PredicateId {
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(StatementKind::Fact),
            /* object: Entity */ 1,
            /* schema_version */ 1,
            "",
            /* is_stateful */ false,
            T0,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn fact(
        subject: EntityId,
        predicate: PredicateId,
        object: EntityId,
        confidence: f32,
    ) -> Statement {
        Statement::new_root(
            StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(subject),
            predicate,
            StatementObject::Entity(object),
            confidence,
            EvidenceRef::default(),
            ExtractorId::from(0),
            T0,
            1,
        )
    }

    fn create(db: &crate::MetadataDb, s: &Statement) -> StatementId {
        let wtxn = db.write_txn().unwrap();
        let id = statement_create(&wtxn, test_scope(), SessionId::DEFAULT, s, T0).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn history_filter(
        subject: Option<EntityId>,
        predicate: Option<PredicateId>,
    ) -> StatementListFilter {
        StatementListFilter {
            subject,
            predicate,
            kind: None,
            current_only: false,
            min_confidence: None,
            limit: 0,
        }
    }

    /// Walk every remaining page from `after`, appending each row id.
    fn drain_pages(
        db: &crate::MetadataDb,
        filter: &StatementListFilter,
        mut after: Option<StatementListCursor>,
        page_size: usize,
        out: &mut Vec<[u8; 16]>,
    ) {
        let extra = StatementPageExtra::default();
        loop {
            let rtxn = db.read_txn().unwrap();
            let page =
                statement_list_page(&rtxn, test_scope(), filter, &extra, after, page_size).unwrap();
            for s in &page.rows {
                out.push(s.id.to_bytes());
            }
            if !page.has_more {
                break;
            }
            after = page.last;
        }
    }

    fn assert_each_once(seen: &[[u8; 16]], expected: &[[u8; 16]]) {
        use std::collections::BTreeSet;
        let unique: BTreeSet<[u8; 16]> = seen.iter().copied().collect();
        assert_eq!(
            unique.len(),
            seen.len(),
            "a row appeared on two pages (dup)"
        );
        let want: BTreeSet<[u8; 16]> = expected.iter().copied().collect();
        assert_eq!(&unique, &want, "pages did not tile the full set (gap)");
    }

    /// by_subject path: superseding an *uncollected* still-current
    /// cumulative row between pages flips its `is_current` 1→0. On the old
    /// cursor (which resumed including `is_current`) that relocated the row
    /// behind the cursor and it was silently gapped. Id-order resume keeps
    /// it in place.
    #[test]
    fn statement_history_page_tiles_the_chain_exactly() {
        let (_dir, db) = open_db();
        let subj = make_entity(&db, "subject");
        let pred = intern_cumulative_pred(&db, "role");

        // A 5-version supersession chain (one chain_root, versions 1..=5).
        let root = create(&db, &fact(subj, pred, make_entity(&db, "obj0"), 0.9));
        let mut latest = root;
        for i in 1..5u64 {
            let next = fact(subj, pred, make_entity(&db, &format!("obj{i}")), 0.9);
            let wtxn = db.write_txn().unwrap();
            latest = statement_supersede(
                &wtxn,
                test_scope(),
                SessionId::DEFAULT,
                latest,
                &next,
                T0 + i,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }

        // Page at limit 2, following next_cursor (last_version) to exhaustion.
        let mut versions: Vec<u32> = Vec::new();
        let mut ids: Vec<[u8; 16]> = Vec::new();
        let mut after: Option<u32> = None;
        let mut pages = 0;
        loop {
            let rtxn = db.read_txn().unwrap();
            let page = statement_history_page(&rtxn, test_scope(), root, after, 2).unwrap();
            assert_eq!(
                page.total, 5,
                "total is the full chain length on every page"
            );
            assert_eq!(page.chain_root, root.to_bytes());
            for s in &page.rows {
                versions.push(s.version);
                ids.push(s.id.to_bytes());
            }
            pages += 1;
            assert!(pages <= 10, "cursor must terminate");
            match (page.has_more, page.last_version) {
                (true, Some(v)) => after = Some(v),
                _ => break,
            }
        }

        // Exhaustive tiling: versions 1..=5, ascending, each exactly once.
        assert_eq!(versions, vec![1, 2, 3, 4, 5]);
        let uniq: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(uniq.len(), ids.len(), "no duplicate across pages");
        assert_eq!(pages, 3, "5 rows at limit 2 → 3 pages (2,2,1)");
    }

    #[test]
    fn statement_history_page_resume_past_exhaustion_is_empty_not_error() {
        let (_dir, db) = open_db();
        let subj = make_entity(&db, "s");
        let pred = intern_cumulative_pred(&db, "p");
        let root = create(&db, &fact(subj, pred, make_entity(&db, "o"), 0.9));
        // after_version past the only version → empty page, has_more false.
        let rtxn = db.read_txn().unwrap();
        let page = statement_history_page(&rtxn, test_scope(), root, Some(99), 10).unwrap();
        assert!(page.rows.is_empty());
        assert!(!page.has_more);
        assert_eq!(page.last_version, None);
        assert_eq!(page.total, 1);
    }

    #[test]
    fn subject_history_page_survives_mid_page_supersession() {
        let (_dir, db) = open_db();
        let subj = make_entity(&db, "subject");
        let pred = intern_cumulative_pred(&db, "knows");

        // Five cumulative (distinct-object) Facts, all current.
        let mut created: Vec<StatementId> = Vec::new();
        for i in 0..5 {
            let obj = make_entity(&db, &format!("obj{i}"));
            created.push(create(&db, &fact(subj, pred, obj, 0.9)));
        }

        let filter = history_filter(Some(subj), None);
        let mut seen: Vec<[u8; 16]> = Vec::new();

        // Page 1 (limit 2).
        let page1 = {
            let rtxn = db.read_txn().unwrap();
            statement_list_page(
                &rtxn,
                test_scope(),
                &filter,
                &StatementPageExtra::default(),
                None,
                2,
            )
            .unwrap()
        };
        assert_eq!(page1.rows.len(), 2);
        assert!(page1.has_more);
        for s in &page1.rows {
            seen.push(s.id.to_bytes());
        }
        let collected: std::collections::BTreeSet<[u8; 16]> =
            page1.rows.iter().map(|s| s.id.to_bytes()).collect();

        // Supersede an uncollected, still-current row.
        let victim = *created
            .iter()
            .find(|id| !collected.contains(&id.to_bytes()))
            .expect("an uncollected row exists");
        let replacement_obj = make_entity(&db, "replacement");
        let replacement = fact(subj, pred, replacement_obj, 0.9);
        let replacement_id = {
            let wtxn = db.write_txn().unwrap();
            let id = statement_supersede(
                &wtxn,
                test_scope(),
                SessionId::DEFAULT,
                victim,
                &replacement,
                T0,
            )
            .unwrap();
            wtxn.commit().unwrap();
            id
        };

        // Continue paging from page 1's cursor.
        drain_pages(&db, &filter, page1.last, 2, &mut seen);

        // Every original row (the superseded one included, as history) plus
        // the replacement must appear exactly once.
        let mut expected: Vec<[u8; 16]> = created.iter().map(|id| id.to_bytes()).collect();
        expected.push(replacement_id.to_bytes());
        assert_each_once(&seen, &expected);
        assert!(
            seen.contains(&victim.to_bytes()),
            "superseded uncollected row was gapped"
        );
    }

    /// by_predicate path: recomputing an already-collected row's confidence
    /// upward between pages moved it across a bucket boundary. On the old
    /// cursor (which resumed including `confidence_bucket`) that relocated
    /// the row ahead of the cursor and it was emitted a second time. Id-order
    /// resume keeps it in place.
    #[test]
    fn predicate_history_page_survives_mid_page_confidence_recompute() {
        let (_dir, db) = open_db();
        let pred = intern_cumulative_pred(&db, "linked");

        // Five rows sharing the predicate, distinct subjects (so all stay
        // current), across distinct confidence buckets.
        let confidences = [0.1_f32, 0.3, 0.5, 0.7, 0.9];
        let mut created: Vec<StatementId> = Vec::new();
        for (i, c) in confidences.iter().enumerate() {
            let subj = make_entity(&db, &format!("s{i}"));
            let obj = make_entity(&db, &format!("o{i}"));
            created.push(create(&db, &fact(subj, pred, obj, *c)));
        }

        let filter = history_filter(None, Some(pred));
        let mut seen: Vec<[u8; 16]> = Vec::new();

        // Page 1 (limit 2) — id order, so the two earliest-created rows.
        let page1 = {
            let rtxn = db.read_txn().unwrap();
            statement_list_page(
                &rtxn,
                test_scope(),
                &filter,
                &StatementPageExtra::default(),
                None,
                2,
            )
            .unwrap()
        };
        assert_eq!(page1.rows.len(), 2);
        assert!(page1.has_more);
        for s in &page1.rows {
            seen.push(s.id.to_bytes());
        }

        // Recompute a page-1 row's confidence into a higher bucket, exactly
        // as the confidence sweep does (primary row + predicate index).
        let victim = page1.rows[0].id;
        {
            let wtxn = db.write_txn().unwrap();
            let old_conf = {
                let mut t = wtxn.open_table(STATEMENTS_TABLE).unwrap();
                let mut m = t.get(&victim.to_bytes()).unwrap().unwrap().value();
                let old = m.confidence;
                m.confidence = 0.99;
                t.insert(&victim.to_bytes(), &m).unwrap();
                old
            };
            rekey_predicate_index(
                &wtxn,
                test_scope(),
                pred.raw(),
                StatementKind::Fact.as_u8(),
                old_conf,
                0.99,
                &victim.to_bytes(),
            )
            .unwrap();
            wtxn.commit().unwrap();
        }

        drain_pages(&db, &filter, page1.last, 2, &mut seen);

        let expected: Vec<[u8; 16]> = created.iter().map(|id| id.to_bytes()).collect();
        assert_each_once(&seen, &expected);
    }

    /// A PRESENT primary row whose object blob no longer decodes is
    /// corruption, not absence. Pagination must fail-stop (invariant #7:
    /// no silent corruption) rather than silently skip the row and advance
    /// the cursor past it — which would hide the corruption and drop the
    /// row from every future page.
    #[test]
    fn paginate_fails_stop_on_undecodable_present_row() {
        let (_dir, db) = open_db();
        let subj = make_entity(&db, "subject");
        let pred = intern_cumulative_pred(&db, "knows");

        let mut created: Vec<StatementId> = Vec::new();
        for i in 0..3 {
            let obj = make_entity(&db, &format!("obj{i}"));
            created.push(create(&db, &fact(subj, pred, obj, 0.9)));
        }

        // Corrupt one row's object blob in place, leaving the row PRESENT:
        // `decode_object` now fails, so `statement_from_metadata` returns
        // `None` on a present row.
        let victim = created[1];
        {
            let wtxn = db.write_txn().unwrap();
            {
                let mut t = wtxn.open_table(STATEMENTS_TABLE).unwrap();
                let mut m = t.get(&victim.to_bytes()).unwrap().unwrap().value();
                m.object_blob = vec![0xFF, 0xFF, 0xFF, 0xFF];
                t.insert(&victim.to_bytes(), &m).unwrap();
            }
            wtxn.commit().unwrap();
        }

        let filter = history_filter(Some(subj), None);
        let rtxn = db.read_txn().unwrap();
        let result = statement_list_page(
            &rtxn,
            test_scope(),
            &filter,
            &StatementPageExtra::default(),
            None,
            10,
        );
        match result {
            Err(StatementOpError::DecodeFailed) => {}
            Err(other) => panic!("expected DecodeFailed, got {other:?}"),
            Ok(_) => panic!("undecodable present row must fail-stop, not skip"),
        }
    }

    /// A genuinely ABSENT primary row (an id-index entry that outlived its
    /// primary row — the legitimately-swept / removed shape) is a benign
    /// skip: pagination returns the remaining rows with no error and never
    /// surfaces the absent one.
    #[test]
    fn paginate_skips_genuinely_absent_row() {
        let (_dir, db) = open_db();
        let subj = make_entity(&db, "subject");
        let pred = intern_cumulative_pred(&db, "knows");

        let mut created: Vec<StatementId> = Vec::new();
        for i in 0..3 {
            let obj = make_entity(&db, &format!("obj{i}"));
            created.push(create(&db, &fact(subj, pred, obj, 0.9)));
        }

        // Remove one primary row, leaving its id-index entries dangling.
        let absent = created[1];
        {
            let wtxn = db.write_txn().unwrap();
            {
                let mut t = wtxn.open_table(STATEMENTS_TABLE).unwrap();
                t.remove(&absent.to_bytes()).unwrap();
            }
            wtxn.commit().unwrap();
        }

        let filter = history_filter(Some(subj), None);
        let mut seen: Vec<[u8; 16]> = Vec::new();
        drain_pages(&db, &filter, None, 2, &mut seen);

        let expected: Vec<[u8; 16]> = created
            .iter()
            .filter(|id| **id != absent)
            .map(|id| id.to_bytes())
            .collect();
        assert_each_once(&seen, &expected);
        assert!(
            !seen.contains(&absent.to_bytes()),
            "genuinely absent row must be skipped"
        );
    }

    /// Non-paged live path (`statement_list`, the one graph/grounded RECALL
    /// walks). A PRESENT primary row whose object blob no longer decodes is
    /// corruption, not absence: `statement_list` must fail-stop with
    /// `DecodeFailed` (invariant #7) rather than silently omit the row — the
    /// same discipline the paged walk already enforces.
    #[test]
    fn statement_list_fails_stop_on_undecodable_present_row() {
        let (_dir, db) = open_db();
        let subj = make_entity(&db, "subject");
        let pred = intern_cumulative_pred(&db, "knows");

        let mut created: Vec<StatementId> = Vec::new();
        for i in 0..3 {
            let obj = make_entity(&db, &format!("obj{i}"));
            created.push(create(&db, &fact(subj, pred, obj, 0.9)));
        }

        // Corrupt one row's object blob in place, leaving the row PRESENT so
        // `statement_from_metadata` returns `None` on a present row.
        let victim = created[1];
        {
            let wtxn = db.write_txn().unwrap();
            {
                let mut t = wtxn.open_table(STATEMENTS_TABLE).unwrap();
                let mut m = t.get(&victim.to_bytes()).unwrap().unwrap().value();
                m.object_blob = vec![0xFF, 0xFF, 0xFF, 0xFF];
                t.insert(&victim.to_bytes(), &m).unwrap();
            }
            wtxn.commit().unwrap();
        }

        let filter = history_filter(Some(subj), None);
        let rtxn = db.read_txn().unwrap();
        match statement_list(&rtxn, test_scope(), &filter) {
            Err(StatementOpError::DecodeFailed) => {}
            Err(other) => panic!("expected DecodeFailed, got {other:?}"),
            Ok(_) => panic!("undecodable present row must fail-stop, not skip"),
        }
    }
}
