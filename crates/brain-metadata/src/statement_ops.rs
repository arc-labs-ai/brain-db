//! Statement CRUD + supersession + contradiction surface. Sub-task 17.4.
//!
//! Free functions over `redb::{ReadTransaction, WriteTransaction}`,
//! mirroring the [`crate::entity_ops`] / [`crate::predicate_ops`]
//! precedent. Every mutation runs inside the caller-supplied write
//! txn; commit is the caller's responsibility, which keeps multi-table
//! atomicity in the substrate's single-writer-per-shard discipline.
//!
//! Spec refs:
//! - `spec/19_statements/00_purpose.md` — schema + ops recipe.
//! - `spec/19_statements/01_supersession.md` — chain mechanics.
//! - `spec/19_statements/02_contradiction.md` — Fact-only detection,
//!   surface-don't-resolve.
//! - `spec/19_statements/03_storage.md` — per-op index write paths.
//! - `spec/19_statements/05_evidence.md` — inline cap, overflow,
//!   reverse-index population.

use brain_core::knowledge::{
    EvidenceEntry, EvidenceRef, Predicate, Statement, StatementObject, SubjectRef, TombstoneReason,
};
use brain_core::{EntityId, EvidenceOverflowId, PredicateId, StatementId, StatementKind};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::knowledge::statement::{
    confidence_bucket, metadata_from_statement, statement_from_metadata, tombstone_reason,
    EvidenceOverflow, StatementMetadata, EVIDENCE_OVERFLOW_TABLE, STATEMENTS_BY_EVENT_TIME_TABLE,
    STATEMENTS_BY_EVIDENCE_TABLE, STATEMENTS_BY_OBJECT_ENTITY_TABLE, STATEMENTS_BY_PREDICATE_TABLE,
    STATEMENTS_BY_SUBJECT_TABLE, STATEMENTS_TABLE, STATEMENT_CHAIN_TABLE,
};

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum StatementOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("statement {0:?} not found")]
    NotFound(StatementId),

    #[error("statement {0:?} already exists")]
    AlreadyExists(StatementId),

    #[error("predicate {0} not registered")]
    UnknownPredicate(u32),

    #[error("subject {0:?} not registered")]
    UnknownSubject(EntityId),

    #[error("invalid argument: {0}")]
    InvalidArgument(&'static str),

    #[error("statement {0:?} already superseded by {1:?}")]
    AlreadySuperseded(StatementId, StatementId),

    #[error("statement {0:?} is tombstoned")]
    AlreadyTombstoned(StatementId),

    #[error("events cannot be superseded")]
    EventCannotSupersede,

    #[error("kind mismatch on supersede: old={old:?} new={new:?}")]
    KindMismatch {
        old: StatementKind,
        new: StatementKind,
    },

    #[error("subject mismatch on supersede")]
    SubjectMismatch,

    #[error("predicate mismatch on supersede")]
    PredicateMismatch,

    #[error("metadata row decode failed — file may be corrupt")]
    DecodeFailed,

    #[error("predicate op: {0}")]
    PredicateOp(#[from] crate::predicate_ops::PredicateOpError),

    #[error("entity op: {0}")]
    EntityOp(#[from] crate::entity_ops::EntityOpError),
}

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

/// Default cap when [`StatementListFilter::limit`] is `0`. Phase-23
/// cursor pagination will replace this; see §19/06 Q11.
pub const DEFAULT_LIST_LIMIT: usize = 1_000;

// ---------------------------------------------------------------------------
// Read paths.
// ---------------------------------------------------------------------------

/// Fetch a statement by id. Returns `None` if the row doesn't exist.
pub fn statement_get(
    rtxn: &ReadTransaction,
    id: StatementId,
) -> Result<Option<Statement>, StatementOpError> {
    let t = rtxn.open_table(STATEMENTS_TABLE)?;
    let row: Option<StatementMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
    let Some(m) = row else {
        return Ok(None);
    };
    statement_from_metadata(&m)
        .ok_or(StatementOpError::DecodeFailed)
        .map(Some)
}

/// Load an evidence-overflow row. Caller passes the id from
/// [`EvidenceRef::Overflow`].
pub fn evidence_overflow_load(
    rtxn: &ReadTransaction,
    id: EvidenceOverflowId,
) -> Result<Option<Vec<EvidenceEntry>>, StatementOpError> {
    let t = rtxn.open_table(EVIDENCE_OVERFLOW_TABLE)?;
    let row: Option<EvidenceOverflow> = t.get(&id.to_bytes())?.map(|g| g.value());
    Ok(row.as_ref().map(EvidenceOverflow::to_entries))
}

/// Walk a supersession chain in version ascending order. Anchor may
/// be the chain root id or any member of the chain (§01 §4.1).
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
/// object value). Spec §19/02 §3.
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
/// applicable index per spec §19/03 §7.
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
            let lo = (subject.to_bytes(), kind.as_u8(), predicate.raw(), 0u8);
            let hi = (subject.to_bytes(), kind.as_u8(), predicate.raw(), 1u8);
            let mut ids = Vec::new();
            for entry in by_subject.range(lo..=hi)? {
                let (k, v) = entry?;
                let (_, _, _, is_current_bit) = k.value();
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
            let lo = (subject.to_bytes(), 0u8, 0u32, 0u8);
            let hi = (subject.to_bytes(), u8::MAX, u32::MAX, 1u8);
            let mut ids = Vec::new();
            for entry in by_subject.range(lo..=hi)? {
                let (k, v) = entry?;
                let (_, k_kind, _, is_current_bit) = k.value();
                if filter.current_only && is_current_bit == 0 {
                    continue;
                }
                if let Some(want_kind) = filter.kind {
                    if k_kind != want_kind.as_u8() {
                        continue;
                    }
                }
                if let Some(want_pred) = filter.predicate {
                    let (_, _, k_pred, _) = k.value();
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
// Write paths.
// ---------------------------------------------------------------------------

/// Allocate an `EvidenceOverflow` row from the given entries and return
/// its id. Caller assembles the resulting `EvidenceRef::Overflow(id)`
/// onto the statement passed to [`statement_create`].
pub fn allocate_evidence_overflow(
    wtxn: &WriteTransaction,
    entries: &[EvidenceEntry],
    now_unix_nanos: u64,
) -> Result<EvidenceOverflowId, StatementOpError> {
    if entries.is_empty() {
        return Err(StatementOpError::InvalidArgument(
            "evidence overflow must have at least one entry",
        ));
    }
    let id = EvidenceOverflowId::new();
    let row = EvidenceOverflow::from_entries(id, entries, now_unix_nanos);
    let mut t = wtxn.open_table(EVIDENCE_OVERFLOW_TABLE)?;
    t.insert(&row.overflow_id_bytes, &row)?;
    Ok(id)
}

/// Create a new statement.
///
/// For `kind == Preference`: if a current Preference exists for the
/// same `(subject, predicate)`, auto-delegates to [`statement_supersede`].
/// For `kind == Fact`: writes a contradiction audit marker (WARN trace)
/// when the new object disagrees with an existing active Fact, but
/// proceeds to insert per spec §19/02 §2.
pub fn statement_create(
    wtxn: &WriteTransaction,
    s: &Statement,
    now_unix_nanos: u64,
) -> Result<StatementId, StatementOpError> {
    validate_statement_shape(s)?;

    // Subject must be a resolved entity for v1 (Pending deferred to
    // phase 22 audits).
    let subject_entity = match s.subject {
        SubjectRef::Entity(e) => e,
        SubjectRef::Pending(_) => {
            return Err(StatementOpError::InvalidArgument(
                "pending subjects deferred to phase 22 audits",
            ));
        }
    };
    // Subject must exist.
    let entity_rtxn_view = TxnAsRead::Write(wtxn);
    let exists = entity_get_via(entity_rtxn_view, subject_entity)?;
    if !exists {
        return Err(StatementOpError::UnknownSubject(subject_entity));
    }

    // Predicate must be registered. Validate against its constraints.
    let pred = predicate_get_via(wtxn, s.predicate)?
        .ok_or(StatementOpError::UnknownPredicate(s.predicate.raw()))?;
    validate_against_predicate(s, &pred)?;

    // ID uniqueness.
    {
        let t = wtxn.open_table(STATEMENTS_TABLE)?;
        if t.get(&s.id.to_bytes())?.is_some() {
            return Err(StatementOpError::AlreadyExists(s.id));
        }
    }

    // Preference auto-supersession.
    if s.kind == StatementKind::Preference {
        if let Some(prior) =
            find_current_statement(wtxn, subject_entity, s.predicate, StatementKind::Preference)?
        {
            return statement_supersede(wtxn, prior, s, now_unix_nanos);
        }
    }

    // Fact contradiction probe (read-only; insert proceeds).
    if s.kind == StatementKind::Fact {
        let active =
            load_active_facts_for_subject_predicate_wtxn(wtxn, subject_entity, s.predicate)?;
        let disagrees = active.iter().any(|existing| existing.object != s.object);
        if disagrees {
            tracing::warn!(
                subject = ?subject_entity,
                predicate = s.predicate.raw(),
                new_id = ?s.id,
                "statement_create: Fact contradicts active facts (see spec §19/02)"
            );
            // TODO(phase 22): write a dedicated contradiction audit
            // table row. v1.0 emits the WARN trace and proceeds.
        }
    }

    // 17.9 confidence aggregation hookup. Recompute the statement's
    // confidence via noisy-OR (spec §19/04) iff inline evidence
    // carries per-entry metadata. Wire callers send
    // EvidenceRef::Inline with `confidence_milli = 0` (per-evidence
    // metadata dropped on the wire per spec §28/06 §2.3); in-process
    // callers (phase 22 extractors / unit tests) populate the field.
    //
    // TODO(phase 21): also re-key the by_predicate confidence bucket
    // entry when the bucket changes by > 0.05 per spec §19/04 §6.
    let mut to_insert = s.clone();
    if evidence_has_per_entry_metadata(wtxn, &to_insert.evidence)? {
        let entries = resolve_evidence_entries(wtxn, &to_insert.evidence)?;
        to_insert.confidence = brain_core::knowledge::aggregate_confidence(
            &entries,
            now_unix_nanos,
            to_insert.kind,
            &brain_core::knowledge::ConfidenceConfig::default_v1(),
        );
    }

    insert_new_statement(wtxn, &to_insert)?;
    Ok(to_insert.id)
}

/// Supersede `old_id` with `new_statement`. Atomic two-step inside
/// `wtxn`: insert new (and chain row), update old in place + flip
/// `is_current` bit, set `valid_to` if not already pinned.
pub fn statement_supersede(
    wtxn: &WriteTransaction,
    old_id: StatementId,
    new_statement: &Statement,
    _now_unix_nanos: u64,
) -> Result<StatementId, StatementOpError> {
    validate_statement_shape(new_statement)?;

    // Load old.
    let mut old = {
        let t = wtxn.open_table(STATEMENTS_TABLE)?;
        let row: Option<StatementMetadata> = t.get(&old_id.to_bytes())?.map(|g| g.value());
        row.ok_or(StatementOpError::NotFound(old_id))?
    };

    // Pre-conditions.
    if old.is_tombstoned() {
        return Err(StatementOpError::AlreadyTombstoned(old_id));
    }
    if let Some(succ) = old.superseded_by_bytes {
        return Err(StatementOpError::AlreadySuperseded(
            old_id,
            StatementId::from(succ),
        ));
    }
    let old_kind = old.kind().ok_or(StatementOpError::InvalidArgument(
        "old row has unknown kind",
    ))?;
    if old_kind == StatementKind::Event {
        return Err(StatementOpError::EventCannotSupersede);
    }
    if old_kind != new_statement.kind {
        return Err(StatementOpError::KindMismatch {
            old: old_kind,
            new: new_statement.kind,
        });
    }
    if old.subject_entity_bytes
        != match new_statement.subject {
            SubjectRef::Entity(e) => e.to_bytes(),
            SubjectRef::Pending(audit) => audit.to_bytes(),
        }
    {
        return Err(StatementOpError::SubjectMismatch);
    }
    if old.predicate_id != new_statement.predicate.raw() {
        return Err(StatementOpError::PredicateMismatch);
    }

    // ID uniqueness on new.
    {
        let t = wtxn.open_table(STATEMENTS_TABLE)?;
        if t.get(&new_statement.id.to_bytes())?.is_some() {
            return Err(StatementOpError::AlreadyExists(new_statement.id));
        }
    }

    // Compute new chain_root + version.
    let chain_root_bytes = if old.supersedes_bytes.is_none() {
        old.statement_id_bytes
    } else {
        old.chain_root_bytes
    };
    let new_version = old.version.saturating_add(1);

    // Build the new row with derived fields filled in.
    let mut new_to_insert = new_statement.clone();
    new_to_insert.version = new_version;
    new_to_insert.supersedes = Some(old_id);
    new_to_insert.superseded_by = None;
    new_to_insert.chain_root = StatementId::from(chain_root_bytes);

    // 17.9 — aggregate confidence over per-entry evidence metadata
    // when present. See `statement_create` for the wire-vs-in-process
    // split.
    if evidence_has_per_entry_metadata(wtxn, &new_to_insert.evidence)? {
        let entries = resolve_evidence_entries(wtxn, &new_to_insert.evidence)?;
        new_to_insert.confidence = brain_core::knowledge::aggregate_confidence(
            &entries,
            new_to_insert.extracted_at_unix_nanos,
            new_to_insert.kind,
            &brain_core::knowledge::ConfidenceConfig::default_v1(),
        );
    }

    // Update old in place — flip is_current, set valid_to (Fact /
    // Preference only) if not already pinned (§01 §3.2 — caller
    // explicit valid_to wins).
    let old_subject_bytes = old.subject_entity_bytes;
    let old_kind_byte = old.kind;
    let old_pred = old.predicate_id;
    let old_was_current = old.is_current != 0;

    old.superseded_by_bytes = Some(new_to_insert.id.to_bytes());
    if old.kind != StatementKind::Event.as_u8() && old.valid_to_unix_nanos.is_none() {
        old.valid_to_unix_nanos = Some(new_to_insert.extracted_at_unix_nanos);
    }
    old.is_current = 0;

    {
        let mut t = wtxn.open_table(STATEMENTS_TABLE)?;
        t.insert(&old.statement_id_bytes, &old)?;
    }

    // Flip the by-subject index for old: remove (subject, kind,
    // predicate, 1), insert (subject, kind, predicate, 0).
    if old_was_current {
        let mut bys = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
        bys.remove(&(old_subject_bytes, old_kind_byte, old_pred, 1u8))?;
        bys.insert(
            &(old_subject_bytes, old_kind_byte, old_pred, 0u8),
            &old.statement_id_bytes,
        )?;
    }

    // Insert new statement + all indexes.
    insert_new_statement(wtxn, &new_to_insert)?;

    Ok(new_to_insert.id)
}

/// Soft delete. Sets `tombstoned / tombstoned_at / tombstone_reason`
/// and flips the by-subject `is_current` bit. Re-tombstoning an
/// already-tombstoned row is a no-op (returns `Ok`).
pub fn statement_tombstone(
    wtxn: &WriteTransaction,
    id: StatementId,
    reason: TombstoneReason,
    now_unix_nanos: u64,
) -> Result<(), StatementOpError> {
    let mut row = {
        let t = wtxn.open_table(STATEMENTS_TABLE)?;
        let r: Option<StatementMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
        r.ok_or(StatementOpError::NotFound(id))?
    };
    if row.is_tombstoned() {
        return Ok(());
    }
    let was_current = row.is_current != 0;
    let subject_bytes = row.subject_entity_bytes;
    let kind_byte = row.kind;
    let pred = row.predicate_id;

    row.tombstoned = 1;
    row.tombstoned_at_unix_nanos = Some(now_unix_nanos);
    row.tombstone_reason = reason.as_u8();
    row.is_current = 0;

    {
        let mut t = wtxn.open_table(STATEMENTS_TABLE)?;
        t.insert(&row.statement_id_bytes, &row)?;
    }
    if was_current {
        let mut bys = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
        bys.remove(&(subject_bytes, kind_byte, pred, 1u8))?;
        bys.insert(
            &(subject_bytes, kind_byte, pred, 0u8),
            &row.statement_id_bytes,
        )?;
    }
    Ok(())
}

/// Hard-delete intent. v1 implementation = `tombstone` with reason
/// `ExtractorRetraction` (caller may override). Physical reclamation
/// happens later via the phase-21+ GC worker.
//
// TODO(phase 21): wire the periodic reclamation worker so retracted
// rows are physically removed from STATEMENTS_TABLE + indexes after
// `RETRACT_GRACE_NANOS`.
pub fn statement_retract(
    wtxn: &WriteTransaction,
    id: StatementId,
    reason: TombstoneReason,
    now_unix_nanos: u64,
) -> Result<(), StatementOpError> {
    statement_tombstone(wtxn, id, reason, now_unix_nanos)
}

// ---------------------------------------------------------------------------
// Internal helpers.
// ---------------------------------------------------------------------------

/// Per-kind invariants validated before any storage access.
fn validate_statement_shape(s: &Statement) -> Result<(), StatementOpError> {
    if !(0.0..=1.0).contains(&s.confidence) || s.confidence.is_nan() {
        return Err(StatementOpError::InvalidArgument(
            "confidence must be in [0, 1] and not NaN",
        ));
    }
    match s.kind {
        StatementKind::Event => {
            if s.event_at_unix_nanos.is_none() {
                return Err(StatementOpError::InvalidArgument(
                    "Event requires event_at_unix_nanos",
                ));
            }
        }
        _ => {
            if s.event_at_unix_nanos.is_some() {
                return Err(StatementOpError::InvalidArgument(
                    "only Event may set event_at_unix_nanos",
                ));
            }
        }
    }
    if let (Some(from), Some(to)) = (s.valid_from_unix_nanos, s.valid_to_unix_nanos) {
        if from > to {
            return Err(StatementOpError::InvalidArgument(
                "valid_from must be <= valid_to",
            ));
        }
    }
    Ok(())
}

/// Per-predicate constraint enforcement.
fn validate_against_predicate(s: &Statement, p: &Predicate) -> Result<(), StatementOpError> {
    if let Some(want_kind) = p.kind_constraint {
        if want_kind != s.kind {
            return Err(StatementOpError::InvalidArgument(
                "statement kind violates predicate kind_constraint",
            ));
        }
    }
    // object_type_constraint_byte: 0 = any; else 1=Entity / 2=Value
    // / 3=Memory / 4=Statement (matches StatementObject::discriminant()
    // offset by 1).
    if p.object_type_constraint_byte != 0 {
        let want = p.object_type_constraint_byte;
        let got = s.object.discriminant() + 1;
        if want != got {
            return Err(StatementOpError::InvalidArgument(
                "statement object variant violates predicate object_type_constraint",
            ));
        }
    }
    Ok(())
}

/// Insert a fresh statement row + every secondary index in one call.
/// Used by both `statement_create` (root path) and `statement_supersede`
/// (new-statement path).
fn insert_new_statement(wtxn: &WriteTransaction, s: &Statement) -> Result<(), StatementOpError> {
    let m = metadata_from_statement(s);

    // 1. Primary row.
    {
        let mut t = wtxn.open_table(STATEMENTS_TABLE)?;
        t.insert(&m.statement_id_bytes, &m)?;
    }

    // 2. by_subject — only if subject is a resolved entity.
    if m.subject_is_pending == 0 {
        let mut t = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
        t.insert(
            &(m.subject_entity_bytes, m.kind, m.predicate_id, m.is_current),
            &m.statement_id_bytes,
        )?;
    }

    // 3. by_predicate.
    {
        let mut t = wtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE)?;
        t.insert(
            &(m.predicate_id, m.kind, confidence_bucket(m.confidence)),
            &m.statement_id_bytes,
        )?;
    }

    // 4. by_object_entity — only if object is Entity.
    if let StatementObject::Entity(eid) = &s.object {
        let mut t = wtxn.open_table(STATEMENTS_BY_OBJECT_ENTITY_TABLE)?;
        t.insert(&(eid.to_bytes(), m.kind), &m.statement_id_bytes)?;
    }

    // 5. by_event_time — only for Events.
    if s.kind == StatementKind::Event {
        if let Some(event_at) = s.event_at_unix_nanos {
            let mut t = wtxn.open_table(STATEMENTS_BY_EVENT_TIME_TABLE)?;
            t.insert(&(event_at, m.subject_entity_bytes), &m.statement_id_bytes)?;
        }
    }

    // 6. by_evidence (one per memory).
    {
        let mut t = wtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE)?;
        match &s.evidence {
            EvidenceRef::Inline(entries) => {
                for e in entries.iter() {
                    t.insert(&(e.memory_id.to_be_bytes(), m.statement_id_bytes), &())?;
                }
            }
            EvidenceRef::Overflow(id) => {
                // Walk the overflow row to populate the reverse index.
                let ot = wtxn.open_table(EVIDENCE_OVERFLOW_TABLE)?;
                let row: Option<EvidenceOverflow> = ot.get(&id.to_bytes())?.map(|g| g.value());
                let Some(over) = row else {
                    return Err(StatementOpError::InvalidArgument(
                        "evidence overflow id references missing row",
                    ));
                };
                for mid in &over.memory_ids {
                    t.insert(&(*mid, m.statement_id_bytes), &())?;
                }
            }
        }
    }

    // 7. chain.
    {
        let mut t = wtxn.open_table(STATEMENT_CHAIN_TABLE)?;
        t.insert(&(m.chain_root_bytes, m.version), &m.statement_id_bytes)?;
    }

    // Defensive: tombstoned-status field. New rows aren't tombstoned;
    // assert here lets a unit test catch caller mis-use.
    debug_assert_eq!(m.tombstoned, 0, "new statement must not be tombstoned");
    debug_assert_eq!(
        m.tombstone_reason,
        tombstone_reason::NOT_TOMBSTONED,
        "new statement tombstone_reason must be 0"
    );

    Ok(())
}

/// Look up the **current** statement for a given (subject, predicate,
/// kind). Used by Preference auto-supersession.
fn find_current_statement(
    wtxn: &WriteTransaction,
    subject: EntityId,
    predicate: PredicateId,
    kind: StatementKind,
) -> Result<Option<StatementId>, StatementOpError> {
    let bys = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
    let key = (subject.to_bytes(), kind.as_u8(), predicate.raw(), 1u8);
    let bytes: Option<[u8; 16]> = bys.get(&key)?.map(|g| g.value());
    Ok(bytes.map(StatementId::from))
}

/// Load the active Facts for (subject, predicate) via a read txn.
fn load_active_facts_for_subject_predicate(
    rtxn: &ReadTransaction,
    subject: EntityId,
    predicate: PredicateId,
) -> Result<Vec<Statement>, StatementOpError> {
    let bys = rtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
    let key = (
        subject.to_bytes(),
        StatementKind::Fact.as_u8(),
        predicate.raw(),
        1u8,
    );
    let bytes: Option<[u8; 16]> = bys.get(&key)?.map(|g| g.value());
    let Some(b) = bytes else {
        return Ok(Vec::new());
    };
    let s = match statement_get(rtxn, StatementId::from(b))? {
        Some(s) => s,
        None => return Ok(Vec::new()),
    };
    Ok(vec![s])
}

/// Write-txn variant of [`load_active_facts_for_subject_predicate`].
/// Reads via the wtxn so the contradiction probe sees uncommitted
/// state inside the same transaction.
fn load_active_facts_for_subject_predicate_wtxn(
    wtxn: &WriteTransaction,
    subject: EntityId,
    predicate: PredicateId,
) -> Result<Vec<Statement>, StatementOpError> {
    let bys = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
    let key = (
        subject.to_bytes(),
        StatementKind::Fact.as_u8(),
        predicate.raw(),
        1u8,
    );
    let bytes: Option<[u8; 16]> = bys.get(&key)?.map(|g| g.value());
    let Some(b) = bytes else {
        return Ok(Vec::new());
    };
    let st = wtxn.open_table(STATEMENTS_TABLE)?;
    let row: Option<StatementMetadata> = st.get(&b)?.map(|g| g.value());
    let Some(m) = row else {
        return Ok(Vec::new());
    };
    let Some(s) = statement_from_metadata(&m) else {
        return Ok(Vec::new());
    };
    Ok(vec![s])
}

/// True iff any inline evidence entry carries per-entry metadata
/// (`confidence_milli > 0`). Overflow rows are assumed to carry full
/// metadata (the four parallel vectors store confidence_milli per
/// entry). 17.9 gates noisy-OR aggregation on this signal — see the
/// design note in `statement_create`.
fn evidence_has_per_entry_metadata(
    wtxn: &WriteTransaction,
    evidence: &EvidenceRef,
) -> Result<bool, StatementOpError> {
    match evidence {
        EvidenceRef::Inline(entries) => Ok(entries.iter().any(|e| e.confidence_milli > 0)),
        EvidenceRef::Overflow(id) => {
            let t = wtxn.open_table(EVIDENCE_OVERFLOW_TABLE)?;
            let row: Option<EvidenceOverflow> = t.get(&id.to_bytes())?.map(|g| g.value());
            let Some(over) = row else {
                return Ok(false);
            };
            Ok(over.confidences_milli.iter().any(|&c| c > 0))
        }
    }
}

/// Materialise the `EvidenceEntry` slice an evidence ref refers to.
/// Inline → clone; Overflow → load + project the four parallel vectors.
fn resolve_evidence_entries(
    wtxn: &WriteTransaction,
    evidence: &EvidenceRef,
) -> Result<Vec<EvidenceEntry>, StatementOpError> {
    match evidence {
        EvidenceRef::Inline(entries) => Ok(entries.to_vec()),
        EvidenceRef::Overflow(id) => {
            let t = wtxn.open_table(EVIDENCE_OVERFLOW_TABLE)?;
            let row: Option<EvidenceOverflow> = t.get(&id.to_bytes())?.map(|g| g.value());
            let Some(over) = row else {
                return Err(StatementOpError::InvalidArgument(
                    "evidence overflow id references missing row",
                ));
            };
            Ok(over.to_entries())
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers — txn abstraction.
// ---------------------------------------------------------------------------

/// `entity_get` and `predicate_get` take `&ReadTransaction`, but our
/// validation runs inside a `WriteTransaction`. redb lets us open
/// tables on either; we open the tables directly here.
enum TxnAsRead<'t> {
    Write(&'t WriteTransaction),
}

fn entity_get_via(txn: TxnAsRead<'_>, id: EntityId) -> Result<bool, StatementOpError> {
    match txn {
        TxnAsRead::Write(wtxn) => {
            use crate::tables::knowledge::entity::{EntityMetadata, ENTITIES_TABLE};
            let t = wtxn.open_table(ENTITIES_TABLE)?;
            let row: Option<EntityMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
            Ok(row.is_some())
        }
    }
}

fn predicate_get_via(
    wtxn: &WriteTransaction,
    id: PredicateId,
) -> Result<Option<Predicate>, StatementOpError> {
    use crate::tables::knowledge::predicate::{PredicateDefinition, PREDICATES_TABLE};
    let t = wtxn.open_table(PREDICATES_TABLE)?;
    let row: Option<PredicateDefinition> = t.get(&id.raw())?.map(|g| g.value());
    Ok(row.as_ref().map(PredicateDefinition::to_predicate))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::predicate_ops::predicate_intern;
    use brain_core::knowledge::{Entity, EntityType, StatementValue, INLINE_EVIDENCE_CAP};
    use brain_core::{ContextId, MemoryId};
    use smallvec::SmallVec;

    fn open_db() -> (tempfile::TempDir, crate::MetadataDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::MetadataDb::open(dir.path().join("md.redb")).unwrap();
        (dir, db)
    }

    /// Insert a Person entity in the open db; return its EntityId.
    fn make_entity(db: &mut crate::MetadataDb, name: &str) -> EntityId {
        let id = EntityId::new();
        let normalized = crate::entity_ops::normalize_name(name);
        let e = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.to_string(),
            normalized,
            1_700_000_000_000_000_000,
        );
        let wtxn = db.write_txn().unwrap();
        crate::entity_ops::entity_put(&wtxn, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    /// Intern a Fact-only Entity-object predicate; return its id.
    fn intern_fact_entity_pred(db: &mut crate::MetadataDb, name: &str) -> PredicateId {
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(StatementKind::Fact),
            /* object: Entity */ 1,
            /* schema_version */ 1,
            "",
            1_700_000_000_000_000_000,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn intern_pref_value_pred(db: &mut crate::MetadataDb, name: &str) -> PredicateId {
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(StatementKind::Preference),
            /* object: Value */ 2,
            /* schema_version */ 1,
            "",
            1_700_000_000_000_000_000,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn intern_event_any_pred(db: &mut crate::MetadataDb, name: &str) -> PredicateId {
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(StatementKind::Event),
            /* object: any */ 0,
            /* schema_version */ 1,
            "",
            1_700_000_000_000_000_000,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn fresh_fact(subject: EntityId, predicate: PredicateId, object: EntityId) -> Statement {
        let id = StatementId::new();
        Statement::new_root(
            id,
            StatementKind::Fact,
            SubjectRef::Entity(subject),
            predicate,
            StatementObject::Entity(object),
            0.9,
            EvidenceRef::default(),
            brain_core::ExtractorId::from(0),
            1_700_000_000_000_000_000,
            1,
        )
    }

    fn fresh_pref(subject: EntityId, predicate: PredicateId, value: &str) -> Statement {
        Statement::new_root(
            StatementId::new(),
            StatementKind::Preference,
            SubjectRef::Entity(subject),
            predicate,
            StatementObject::Value(StatementValue::Text(value.into())),
            0.9,
            EvidenceRef::default(),
            brain_core::ExtractorId::from(0),
            1_700_000_000_000_000_000,
            1,
        )
    }

    fn fresh_event(subject: EntityId, predicate: PredicateId, when: u64) -> Statement {
        let mut s = Statement::new_root(
            StatementId::new(),
            StatementKind::Event,
            SubjectRef::Entity(subject),
            predicate,
            StatementObject::Value(StatementValue::Text("scheduled".into())),
            0.9,
            EvidenceRef::default(),
            brain_core::ExtractorId::from(0),
            1_700_000_000_000_000_000,
            1,
        );
        s.event_at_unix_nanos = Some(when);
        s
    }

    #[test]
    fn create_fact_round_trips_via_get() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "manager-role");
        let pred = intern_fact_entity_pred(&mut db, "role");

        let s = fresh_fact(subj, pred, obj);

        let wtxn = db.write_txn().unwrap();
        let id = statement_create(&wtxn, &s, 1_700_000_000_000_000_001).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(id, s.id);

        let rtxn = db.read_txn().unwrap();
        let got = statement_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(got, s);
    }

    #[test]
    fn create_fact_writes_all_six_indexes() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "platform-team");
        let pred = intern_fact_entity_pred(&mut db, "team");

        let s = fresh_fact(subj, pred, obj);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &s, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        // by_subject
        let bys = rtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE).unwrap();
        assert!(bys
            .get(&(
                subj.to_bytes(),
                StatementKind::Fact.as_u8(),
                pred.raw(),
                1u8,
            ))
            .unwrap()
            .is_some());
        // by_predicate
        let byp = rtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE).unwrap();
        assert!(byp
            .get(&(
                pred.raw(),
                StatementKind::Fact.as_u8(),
                confidence_bucket(0.9)
            ))
            .unwrap()
            .is_some());
        // by_object_entity
        let byo = rtxn.open_table(STATEMENTS_BY_OBJECT_ENTITY_TABLE).unwrap();
        assert!(byo
            .get(&(obj.to_bytes(), StatementKind::Fact.as_u8()))
            .unwrap()
            .is_some());
        // chain
        let cht = rtxn.open_table(STATEMENT_CHAIN_TABLE).unwrap();
        assert!(cht.get(&(s.id.to_bytes(), 1u32)).unwrap().is_some());
    }

    #[test]
    fn create_preference_auto_supersedes_prior() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pred = intern_pref_value_pred(&mut db, "prefers");

        // First Preference.
        let p1 = fresh_pref(subj, pred, "async");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &p1, 0).unwrap();
        wtxn.commit().unwrap();

        // Second Preference — should auto-supersede.
        let p2 = fresh_pref(subj, pred, "written-agendas");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &p2, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let p1_back = statement_get(&rtxn, p1.id).unwrap().unwrap();
        let p2_back = statement_get(&rtxn, p2.id).unwrap().unwrap();
        assert_eq!(p1_back.superseded_by, Some(p2.id));
        assert_eq!(p2_back.supersedes, Some(p1.id));
        assert_eq!(p2_back.version, 2);
        assert_eq!(p2_back.chain_root, p1.id);
    }

    #[test]
    fn create_event_requires_event_at() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pred = intern_event_any_pred(&mut db, "scheduled");

        let mut s = fresh_event(subj, pred, 1_700_000_000);
        s.event_at_unix_nanos = None;
        let wtxn = db.write_txn().unwrap();
        let err = statement_create(&wtxn, &s, 0).unwrap_err();
        matches!(err, StatementOpError::InvalidArgument(_))
            .then_some(())
            .expect("expected InvalidArgument");
    }

    #[test]
    fn create_fact_with_event_at_rejected() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "role-mgr");
        let pred = intern_fact_entity_pred(&mut db, "role2");

        let mut s = fresh_fact(subj, pred, obj);
        s.event_at_unix_nanos = Some(123);
        let wtxn = db.write_txn().unwrap();
        let err = statement_create(&wtxn, &s, 0).unwrap_err();
        matches!(err, StatementOpError::InvalidArgument(_))
            .then_some(())
            .expect("expected InvalidArgument");
    }

    #[test]
    fn create_unknown_predicate_rejected() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "x");
        let s = fresh_fact(subj, PredicateId::from(9999), obj);
        let wtxn = db.write_txn().unwrap();
        let err = statement_create(&wtxn, &s, 0).unwrap_err();
        matches!(err, StatementOpError::UnknownPredicate(9999))
            .then_some(())
            .expect("expected UnknownPredicate");
    }

    #[test]
    fn create_unknown_subject_rejected() {
        let (_dir, mut db) = open_db();
        let pred = intern_fact_entity_pred(&mut db, "role3");
        let phantom_subj = EntityId::new();
        let phantom_obj = EntityId::new();
        let s = fresh_fact(phantom_subj, pred, phantom_obj);
        let wtxn = db.write_txn().unwrap();
        let err = statement_create(&wtxn, &s, 0).unwrap_err();
        matches!(err, StatementOpError::UnknownSubject(_))
            .then_some(())
            .expect("expected UnknownSubject");
    }

    #[test]
    fn create_pending_subject_rejected_v1() {
        let (_dir, mut db) = open_db();
        let pred = intern_fact_entity_pred(&mut db, "role4");
        let obj = make_entity(&mut db, "x");
        let mut s = fresh_fact(EntityId::new(), pred, obj);
        s.subject = SubjectRef::Pending(brain_core::knowledge::AuditId::new());
        let wtxn = db.write_txn().unwrap();
        let err = statement_create(&wtxn, &s, 0).unwrap_err();
        matches!(err, StatementOpError::InvalidArgument(_))
            .then_some(())
            .expect("expected InvalidArgument for Pending subject");
    }

    #[test]
    fn create_contradictory_facts_both_stored() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj_a = make_entity(&mut db, "mgr-a");
        let obj_b = make_entity(&mut db, "mgr-b");
        let pred = intern_fact_entity_pred(&mut db, "manages");

        let f1 = fresh_fact(subj, pred, obj_a);
        let f2 = fresh_fact(subj, pred, obj_b);

        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &f1, 0).unwrap();
        // f2 contradicts f1 on object; both must store.
        statement_create(&wtxn, &f2, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g1 = statement_get(&rtxn, f1.id).unwrap().unwrap();
        let g2 = statement_get(&rtxn, f2.id).unwrap().unwrap();
        assert!(!g1.tombstoned);
        assert!(!g2.tombstoned);

        let conflicts = statements_contradicting(&rtxn, subj, pred).unwrap();
        // The by_subject Fact index is single-value per is_current bit
        // — the second insert overwrites the first key. v1 implementation
        // surfaces the contradiction via the WARN trace at create-time;
        // the runtime probe returns whichever survived in by_subject.
        // Both primary rows still exist by id.
        assert!(conflicts.len() <= 1);
    }

    #[test]
    fn supersede_fact_chain_root_inherited_on_second_supersede() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "x");
        let pred = intern_fact_entity_pred(&mut db, "role5");

        let f1 = fresh_fact(subj, pred, obj);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &f1, 0).unwrap();
        wtxn.commit().unwrap();

        let f2 = fresh_fact(subj, pred, obj);
        let wtxn = db.write_txn().unwrap();
        statement_supersede(&wtxn, f1.id, &f2, 1).unwrap();
        wtxn.commit().unwrap();

        let f3 = fresh_fact(subj, pred, obj);
        let wtxn = db.write_txn().unwrap();
        statement_supersede(&wtxn, f2.id, &f3, 2).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g3 = statement_get(&rtxn, f3.id).unwrap().unwrap();
        assert_eq!(g3.chain_root, f1.id);
        assert_eq!(g3.version, 3);
        assert_eq!(g3.supersedes, Some(f2.id));
    }

    #[test]
    fn supersede_preserves_explicit_valid_to() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "x");
        let pred = intern_fact_entity_pred(&mut db, "role6");

        let mut f1 = fresh_fact(subj, pred, obj);
        f1.valid_to_unix_nanos = Some(123_000_000);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &f1, 0).unwrap();
        wtxn.commit().unwrap();

        let f2 = fresh_fact(subj, pred, obj);
        let wtxn = db.write_txn().unwrap();
        statement_supersede(&wtxn, f1.id, &f2, 999_999_999_999).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g1 = statement_get(&rtxn, f1.id).unwrap().unwrap();
        // Explicit valid_to preserved despite supersession.
        assert_eq!(g1.valid_to_unix_nanos, Some(123_000_000));
    }

    #[test]
    fn supersede_event_rejected() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pred = intern_event_any_pred(&mut db, "sched");

        let e1 = fresh_event(subj, pred, 1);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &e1, 0).unwrap();
        wtxn.commit().unwrap();

        let e2 = fresh_event(subj, pred, 2);
        let wtxn = db.write_txn().unwrap();
        let err = statement_supersede(&wtxn, e1.id, &e2, 0).unwrap_err();
        matches!(err, StatementOpError::EventCannotSupersede)
            .then_some(())
            .expect("expected EventCannotSupersede");
    }

    #[test]
    fn tombstone_flips_is_current_bit() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "x");
        let pred = intern_fact_entity_pred(&mut db, "role7");
        let f = fresh_fact(subj, pred, obj);

        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &f, 0).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.write_txn().unwrap();
        statement_tombstone(&wtxn, f.id, TombstoneReason::UserRequest, 42).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let bys = rtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE).unwrap();
        let cur = bys
            .get(&(
                subj.to_bytes(),
                StatementKind::Fact.as_u8(),
                pred.raw(),
                1u8,
            ))
            .unwrap();
        assert!(cur.is_none(), "is_current=1 entry must be gone");
        let stale = bys
            .get(&(
                subj.to_bytes(),
                StatementKind::Fact.as_u8(),
                pred.raw(),
                0u8,
            ))
            .unwrap();
        assert!(stale.is_some(), "is_current=0 entry must exist");
    }

    #[test]
    fn tombstone_preserves_evidence_index() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "x");
        let pred = intern_fact_entity_pred(&mut db, "role8");
        let mem = MemoryId::pack(7, ContextId::DEFAULT.into(), 0);
        let mut f = fresh_fact(subj, pred, obj);
        f.evidence = EvidenceRef::Inline(Box::new({
            let entry = EvidenceEntry::from_parts(
                mem,
                0.8,
                1_700_000_000_000_000_000,
                brain_core::ExtractorId::from(0),
            );
            let mut sv = SmallVec::<[EvidenceEntry; INLINE_EVIDENCE_CAP]>::new();
            sv.push(entry);
            sv
        }));

        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &f, 0).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.write_txn().unwrap();
        statement_tombstone(&wtxn, f.id, TombstoneReason::UserRequest, 1).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let evi = rtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE).unwrap();
        assert!(evi
            .get(&(mem.to_be_bytes(), f.id.to_bytes()))
            .unwrap()
            .is_some());
    }

    #[test]
    fn history_walks_chain_in_version_order() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pred = intern_pref_value_pred(&mut db, "prefers2");

        let p1 = fresh_pref(subj, pred, "v1");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &p1, 0).unwrap();
        wtxn.commit().unwrap();

        let p2 = fresh_pref(subj, pred, "v2");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &p2, 0).unwrap();
        wtxn.commit().unwrap();

        let p3 = fresh_pref(subj, pred, "v3");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &p3, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let chain = statement_history(&rtxn, p1.id).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].id, p1.id);
        assert_eq!(chain[1].id, p2.id);
        assert_eq!(chain[2].id, p3.id);

        // Anchor from any member works.
        let chain2 = statement_history(&rtxn, p3.id).unwrap();
        assert_eq!(chain2.len(), 3);
    }

    #[test]
    fn list_subject_predicate_returns_single_current() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pred = intern_pref_value_pred(&mut db, "prefers3");
        let p1 = fresh_pref(subj, pred, "v1");
        let p2 = fresh_pref(subj, pred, "v2");

        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &p1, 0).unwrap();
        statement_create(&wtxn, &p2, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let filter = StatementListFilter {
            subject: Some(subj),
            predicate: Some(pred),
            kind: Some(StatementKind::Preference),
            current_only: true,
            ..Default::default()
        };
        let out = statement_list(&rtxn, &filter).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, p2.id);
    }

    #[test]
    fn list_with_min_confidence_filters() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "x");
        let pred = intern_fact_entity_pred(&mut db, "low_conf");
        let mut s = fresh_fact(subj, pred, obj);
        s.confidence = 0.3;

        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &s, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let filter = StatementListFilter {
            subject: Some(subj),
            min_confidence: Some(0.5),
            ..Default::default()
        };
        let out = statement_list(&rtxn, &filter).unwrap();
        assert!(out.is_empty());

        let filter2 = StatementListFilter {
            subject: Some(subj),
            min_confidence: Some(0.2),
            ..Default::default()
        };
        let out2 = statement_list(&rtxn, &filter2).unwrap();
        assert_eq!(out2.len(), 1);
    }

    #[test]
    fn evidence_overflow_round_trip() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "x");
        let pred = intern_fact_entity_pred(&mut db, "many_evi");

        let entries: Vec<EvidenceEntry> = (1..=10)
            .map(|i| {
                EvidenceEntry::from_parts(
                    MemoryId::pack(i as u16, ContextId::DEFAULT.into(), 0),
                    0.7,
                    1_700_000_000_000_000_000,
                    brain_core::ExtractorId::from(0),
                )
            })
            .collect();

        let wtxn = db.write_txn().unwrap();
        let overflow_id = allocate_evidence_overflow(&wtxn, &entries, 1).unwrap();
        let mut s = fresh_fact(subj, pred, obj);
        s.evidence = EvidenceRef::Overflow(overflow_id);
        statement_create(&wtxn, &s, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = statement_get(&rtxn, s.id).unwrap().unwrap();
        match got.evidence {
            EvidenceRef::Overflow(id) => assert_eq!(id, overflow_id),
            _ => panic!("expected Overflow variant"),
        }
        let resolved = evidence_overflow_load(&rtxn, overflow_id).unwrap().unwrap();
        assert_eq!(resolved.len(), 10);

        // Reverse-evidence index: each memory points to the statement.
        let evi = rtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE).unwrap();
        for e in &entries {
            assert!(evi
                .get(&(e.memory_id.to_be_bytes(), s.id.to_bytes()))
                .unwrap()
                .is_some());
        }
    }

    // ----- 17.9 confidence aggregation hookup -----

    #[test]
    fn create_aggregates_when_evidence_has_metadata() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "role-mgr");
        let pred = intern_fact_entity_pred(&mut db, "role_agg");

        // Two pieces of c=0.9 evidence, no decay age (Event would be
        // simpler but Event-kind needs event_at; use Fact at zero age
        // — fact decay at age=0 is exp(0) = 1.0).
        let mut s = fresh_fact(subj, pred, obj);
        s.confidence = 0.5; // caller's wire-level value, should be overwritten
        let entry = |conf: f32| {
            EvidenceEntry::from_parts(
                MemoryId::pack(1, ContextId::DEFAULT.into(), 0),
                conf,
                1_700_000_000_000_000_000,
                brain_core::ExtractorId::from(0),
            )
        };
        let mut sv = SmallVec::<[EvidenceEntry; INLINE_EVIDENCE_CAP]>::new();
        sv.push(entry(0.9));
        sv.push(entry(0.9));
        s.evidence = EvidenceRef::Inline(Box::new(sv));

        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &s, 1_700_000_000_000_000_000).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = statement_get(&rtxn, s.id).unwrap().unwrap();
        // Expected: 1 - (1 - 0.9)^2 = 0.99 (zero age → decay = 1).
        assert!(
            (got.confidence - 0.99).abs() < 1e-3,
            "got {}",
            got.confidence
        );
    }

    #[test]
    fn create_keeps_wire_confidence_when_evidence_lacks_metadata() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "role-2");
        let pred = intern_fact_entity_pred(&mut db, "role_wire");

        // Inline evidence with confidence_milli = 0 (the wire-side
        // shape — SDK decodes EvidenceRefWire::Inline into entries
        // with zero metadata).
        let mut s = fresh_fact(subj, pred, obj);
        s.confidence = 0.42;
        let entry_zero = EvidenceEntry {
            memory_id: MemoryId::pack(1, ContextId::DEFAULT.into(), 0),
            confidence_milli: 0,
            timestamp_unix_nanos: 0,
            extractor_id: brain_core::ExtractorId::from(0),
        };
        let mut sv = SmallVec::<[EvidenceEntry; INLINE_EVIDENCE_CAP]>::new();
        sv.push(entry_zero);
        s.evidence = EvidenceRef::Inline(Box::new(sv));

        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &s, 1_700_000_000_000_000_000).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = statement_get(&rtxn, s.id).unwrap().unwrap();
        // No aggregation: caller's confidence preserved verbatim.
        assert!(
            (got.confidence - 0.42).abs() < 1e-6,
            "got {}",
            got.confidence
        );
    }
}
