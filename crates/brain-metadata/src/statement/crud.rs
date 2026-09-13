//! Statement creation + primary-row read + shared invariant helpers.
//!
//! Handles per-op index write paths plus evidence handling: the inline
//! cap, overflow spill, and reverse-index population.

use brain_core::{
    EntityId, EntityTypeId, EvidenceOverflowId, PredicateId, StatementId, StatementKind,
};
use brain_core::{EvidenceEntry, EvidenceRef, Predicate, Statement, StatementObject, SubjectRef};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::statement::{
    confidence_bucket, metadata_from_statement, statement_from_metadata, tombstone_reason,
    EvidenceOverflow, StatementMetadata, EVIDENCE_OVERFLOW_TABLE, STATEMENTS_BY_EVENT_TIME_TABLE,
    STATEMENTS_BY_EVIDENCE_TABLE, STATEMENTS_BY_OBJECT_ENTITY_TABLE,
    STATEMENTS_BY_PREDICATE_ID_TABLE, STATEMENTS_BY_PREDICATE_TABLE,
    STATEMENTS_BY_SUBJECT_ID_TABLE, STATEMENTS_BY_SUBJECT_TABLE, STATEMENTS_TABLE,
    STATEMENT_CHAIN_TABLE, STATEMENT_EMBED_QUEUE_TABLE,
};

use super::supersede::statement_supersede;
use super::StatementOpError;
use crate::tables::scope::RowScope;

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
/// proceeds to insert.
pub fn statement_create(
    wtxn: &WriteTransaction,
    scope: RowScope,
    session: brain_core::SessionId,
    s: &Statement,
    now_unix_nanos: u64,
) -> Result<StatementId, StatementOpError> {
    validate_statement_shape(s)?;

    // Subject is an entity, or the source memory itself (temporal
    // Events). Pending subjects are not yet supported here. Entity
    // subjects must exist; memory subjects carry no entity to check and
    // skip the entity-keyed existence / supersession / contradiction
    // paths below (temporal events are Event-kind, which is never
    // superseded or Fact-contradicted anyway).
    let subject_entity: Option<EntityId> = match s.subject {
        SubjectRef::Entity(e) => Some(e),
        SubjectRef::Memory(_) => None,
        SubjectRef::Pending(_) => {
            return Err(StatementOpError::InvalidArgument(
                "pending subjects deferred to phase 22 audits",
            ));
        }
    };
    if let Some(e) = subject_entity {
        let entity_rtxn_view = TxnAsRead::Write(wtxn);
        let exists = entity_get_via(entity_rtxn_view, e)?;
        if !exists {
            return Err(StatementOpError::UnknownSubject(e));
        }
    }

    // Predicate must be registered. Validate against its constraints.
    let pred = predicate_get_via(wtxn, s.predicate)?
        .ok_or(StatementOpError::UnknownPredicate(s.predicate.raw()))?;
    validate_against_predicate(wtxn, s, &pred)?;

    // ID uniqueness.
    {
        let t = wtxn.open_table(STATEMENTS_TABLE)?;
        if t.get(&s.id.to_bytes())?.is_some() {
            return Err(StatementOpError::AlreadyExists(s.id));
        }
    }

    // Auto-supersession is kind-derived: single-valued kinds (Attribute,
    // Directive, and user-declared `cardinality: single` kinds) supersede
    // their prior current value. A predicate explicitly declared
    // `stateful: true` in the schema is also honored as an override, so a
    // schema author can make any non-Event kind single-valued. Event is
    // never superseded — each event is its own row by design.
    let kind_single = crate::schema::kind::kind_supersedes_w(wtxn, s.kind)?;
    if (kind_single || pred.is_stateful) && s.kind != StatementKind::Event {
        if let Some(e) = subject_entity {
            if let Some(prior) = find_current_statement(wtxn, scope, e, s.predicate, s.kind)? {
                return statement_supersede(wtxn, scope, session, prior, s, now_unix_nanos);
            }
        }
    }

    // Content dedup for cumulative kinds. Single-valued kinds already
    // returned above via supersession; what falls through here is Fact /
    // Event / other cumulative kinds, where nothing collapses a
    // byte-identical repeat. A transient LLM failure re-runs the whole
    // extractor pipeline and re-applies already-committed pattern-tier
    // rows, so an identical `(subject, predicate, object, event_at)`
    // active row means this is a retry: no-op and return the existing id.
    // Distinct objects (multi-valued Facts) and distinct event times
    // (separate Events) never match, so legitimate cumulation is kept.
    if let Some(e) = subject_entity {
        if let Some(existing) = find_identical_active_statement(wtxn, scope, e, s)? {
            return Ok(existing);
        }
    }

    // Fact contradiction probe (read-only; insert proceeds). Only for
    // entity subjects — memory subjects don't participate in the
    // entity-keyed contradiction index (and temporal events are Events,
    // not Facts).
    if let (StatementKind::Fact, Some(subject_entity)) = (s.kind, subject_entity) {
        let active =
            load_active_facts_for_subject_predicate_wtxn(wtxn, scope, subject_entity, s.predicate)?;
        // A contradiction requires a *different* object AND *overlapping*
        // validity intervals: sequential facts (old.valid_to < new.valid_from)
        // describe different periods and are not contradictions.
        let conflicting: Vec<&Statement> = active
            .iter()
            .filter(|existing| existing.object != s.object && fact_intervals_overlap(existing, s))
            .collect();
        if !conflicting.is_empty() {
            tracing::warn!(
                subject = ?subject_entity,
                predicate = s.predicate.raw(),
                new_id = ?s.id,
                "statement_create: Fact contradicts active facts"
            );
            // Durable audit: the conflicting set is the overlapping,
            // disagreeing active Fact(s) plus the one being inserted. The
            // insert still proceeds (coexisting Facts are allowed);
            // operators reconcile via ADMIN_LIST_PENDING_CONTRADICTIONS.
            let mut contradicting: Vec<StatementId> = conflicting.iter().map(|a| a.id).collect();
            contradicting.push(s.id);
            super::contradiction::contradiction_audit_record(
                wtxn,
                subject_entity,
                s.predicate,
                &contradicting,
                now_unix_nanos,
            )?;
        }
    }

    // Confidence aggregation hookup. Recompute the statement's
    // confidence via noisy-OR iff inline evidence carries per-entry
    // metadata. Wire callers send EvidenceRef::Inline with
    // `confidence_milli = 0` (per-evidence metadata dropped on the
    // wire); in-process callers (extractors / unit tests) populate the
    // field.
    //
    // No predicate-bucket re-key here: this is a fresh insert, and
    // `insert_new_statement` below indexes it under the just-aggregated
    // confidence. Re-keying applies only to *later* confidence changes
    // on a live row (the confidence sweep and the FORGET cascade), which
    // call `rekey_predicate_index`.
    let mut to_insert = s.clone();
    recompute_confidence_from_evidence(wtxn, &mut to_insert, now_unix_nanos)?;

    insert_new_statement(wtxn, scope, session, &to_insert)?;
    Ok(to_insert.id)
}

// ---------------------------------------------------------------------------
// Internal helpers (shared with sibling modules via `pub(super)`).
// ---------------------------------------------------------------------------

/// Half-open validity interval `[from, to)` for a Fact. An unset
/// `valid_from` defaults to `extracted_at` (the fact became true no later
/// than when it was ingested); an unset `valid_to` is open-ended.
pub(super) fn fact_validity_interval(s: &Statement) -> (u64, u64) {
    let from = s.valid_from_unix_nanos.unwrap_or(s.extracted_at_unix_nanos);
    let to = s.valid_to_unix_nanos.unwrap_or(u64::MAX);
    (from, to)
}

/// Whether two Facts' validity intervals overlap. Uses half-open
/// `[from, to)` semantics, so sequential facts (`old.valid_to == new.valid_from`,
/// or `old.valid_to < new.valid_from`) do not overlap, while a still-open
/// `valid_to = None` fact overlaps any later fact.
pub(super) fn fact_intervals_overlap(a: &Statement, b: &Statement) -> bool {
    let (a_from, a_to) = fact_validity_interval(a);
    let (b_from, b_to) = fact_validity_interval(b);
    a_from < b_to && b_from < a_to
}

/// Per-kind invariants validated before any storage access.
pub(super) fn validate_statement_shape(s: &Statement) -> Result<(), StatementOpError> {
    if !(0.0..=1.0).contains(&s.confidence) || s.confidence.is_nan() {
        return Err(StatementOpError::InvalidArgument(
            "confidence must be in [0, 1] and not NaN",
        ));
    }
    match s.kind {
        // An Event MAY carry no distinct event_at: a dateless or same-day action
        // (whose only resolved date equals the memory's own message anchor) still
        // IS an event — it happened at a time we simply couldn't pin to a distinct
        // calendar date. Storing it as an Event with `event_at = None` keeps the
        // temporal role available so the read answers "when" from the evidence
        // memory's own `occurred_at`, instead of losing the fact by demoting it to
        // a timeless Fact. Only a NON-Event is forbidden a time.
        StatementKind::Event => {
            // Events are point-in-time, not validity ranges: they carry
            // event_at (or nothing), never valid_from / valid_to.
            if s.valid_from_unix_nanos.is_some() || s.valid_to_unix_nanos.is_some() {
                return Err(StatementOpError::InvalidArgument(
                    "Event may not set valid_from_unix_nanos / valid_to_unix_nanos",
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
fn validate_against_predicate(
    wtxn: &WriteTransaction,
    s: &Statement,
    p: &Predicate,
) -> Result<(), StatementOpError> {
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
    // A declared `object: Entity<Type>` range narrows the variant check
    // to one entity type. `0` means the declaration was a bare `Entity`
    // (or another variant entirely) and admits any type.
    if p.object_entity_type_id != 0 {
        if let StatementObject::Entity(eid) = s.object {
            let actual =
                entity_type_of(wtxn, eid)?.ok_or(StatementOpError::UnknownObjectEntity(eid))?;
            if actual.raw() != p.object_entity_type_id {
                return Err(StatementOpError::ObjectEntityTypeMismatch {
                    entity: eid,
                    expected: EntityTypeId::from(p.object_entity_type_id),
                    actual,
                });
            }
        }
    }
    Ok(())
}

/// Insert a fresh statement row + every secondary index in one call.
/// Used by both `statement_create` (root path) and `statement_supersede`
/// (new-statement path).
pub(super) fn insert_new_statement(
    wtxn: &WriteTransaction,
    scope: RowScope,
    session: brain_core::SessionId,
    s: &Statement,
) -> Result<(), StatementOpError> {
    let mut m = metadata_from_statement(s, scope);
    // Stamp the real per-utterance session onto the row (the brain-core
    // `Statement` has no session slot, so `metadata_from_statement`
    // defaulted it to 0). Session is a grouping column, never a key.
    m.session_id = session.raw();
    let ns = scope.namespace_id;
    let ag = scope.space_id_bytes;

    // 1. Primary row.
    {
        let mut t = wtxn.open_table(STATEMENTS_TABLE)?;
        t.insert(&m.statement_id_bytes, &m)?;
    }

    // 2. by_subject — for resolved-entity AND memory subjects (skip only
    // pending). Memory subjects must be indexed so "statements about
    // memory M" (e.g. the forget cascade) can find them; entity and
    // memory ids occupy disjoint byte spaces so they share the key.
    if m.subject_kind != 1 {
        let mut t = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
        t.insert(
            &(
                ns,
                ag,
                m.subject_entity_bytes,
                m.kind,
                m.predicate_id,
                m.is_current,
                m.statement_id_bytes,
            ),
            &m.statement_id_bytes,
        )?;
        // Immutable id-ordered twin used for keyset pagination resume. Its
        // key carries no mutable column, so a later supersession /
        // confidence recompute never relocates the row out from under a
        // paging cursor.
        let mut t = wtxn.open_table(STATEMENTS_BY_SUBJECT_ID_TABLE)?;
        t.insert(&(ns, ag, m.subject_entity_bytes, m.statement_id_bytes), &())?;
    }

    // 3. by_predicate.
    {
        let mut t = wtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE)?;
        t.insert(
            &(
                ns,
                ag,
                m.predicate_id,
                m.kind,
                confidence_bucket(m.confidence),
                m.statement_id_bytes,
            ),
            &m.statement_id_bytes,
        )?;
        // Immutable id-ordered twin (see the by_subject_id note above).
        let mut t = wtxn.open_table(STATEMENTS_BY_PREDICATE_ID_TABLE)?;
        t.insert(&(ns, ag, m.predicate_id, m.statement_id_bytes), &())?;
    }

    // 4. by_object_entity — only if object is Entity.
    if let StatementObject::Entity(eid) = &s.object {
        let mut t = wtxn.open_table(STATEMENTS_BY_OBJECT_ENTITY_TABLE)?;
        t.insert(
            &(ns, ag, eid.to_bytes(), m.kind, m.statement_id_bytes),
            &m.statement_id_bytes,
        )?;
    }

    // 5. by_event_time — only for Events.
    if s.kind == StatementKind::Event {
        if let Some(event_at) = s.event_at_unix_nanos {
            let mut t = wtxn.open_table(STATEMENTS_BY_EVENT_TIME_TABLE)?;
            t.insert(
                &(
                    ns,
                    ag,
                    event_at,
                    m.subject_entity_bytes,
                    m.statement_id_bytes,
                ),
                &m.statement_id_bytes,
            )?;
        }
    }

    // 6. by_evidence (one per memory).
    {
        let mut t = wtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE)?;
        match &s.evidence {
            EvidenceRef::Inline(entries) => {
                for e in entries.iter() {
                    t.insert(
                        &(ns, ag, e.memory_id.to_be_bytes(), m.statement_id_bytes),
                        &(),
                    )?;
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
                    t.insert(&(ns, ag, *mid, m.statement_id_bytes), &())?;
                }
            }
        }
    }

    // 7. chain.
    {
        let mut t = wtxn.open_table(STATEMENT_CHAIN_TABLE)?;
        t.insert(
            &(ns, ag, m.chain_root_bytes, m.version),
            &m.statement_id_bytes,
        )?;
    }

    // 8. embed queue. The per-shard StatementEmbedWorker drains this
    // table, embeds (subject + predicate + object) text, and writes
    // the vector into StatementHnswIndex. Enqueuing here (rather than
    // in an in-memory channel) means the worker survives crashes:
    // an extractor commit followed by an immediate restart still has
    // the queue row waiting. Tombstone removes the row so a doomed
    // statement never reaches the HNSW.
    {
        let mut t = wtxn.open_table(STATEMENT_EMBED_QUEUE_TABLE)?;
        t.insert(&m.statement_id_bytes, &s.extracted_at_unix_nanos)?;
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

// ---------------------------------------------------------------------------
// `statements_by_predicate` lifecycle helpers.
//
// Invariant: the predicate-bucket index holds an entry
// `(predicate_id, kind, confidence_bucket) -> statement_id` only for
// statements that are **current and not tombstoned**, bucketed by their
// current confidence. The index stores a single id per key
// (last-writer-wins when two live statements land in the same bucket),
// so every mutation here is ownership-guarded: we only touch a key that
// currently points at *this* statement, never evicting a bucket-sharing
// sibling.
//
// Obligations across the statement lifecycle:
//   - create / insert_new_statement: insert (live row)
//   - confidence recompute on a live row (confidence sweep, FORGET
//     cascade evidence-drop): `rekey_predicate_index`
//   - supersede of the old row / tombstone / retract (row leaves the
//     live set): `remove_from_predicate_index`
//   - physical reclamation: the row (and every index entry) is already
//     gone by tombstone time; reclaim strips defensively.
// ---------------------------------------------------------------------------

/// Remove a statement's `statements_by_predicate` entry when it leaves
/// the live set (tombstone, retract, or the old row of a supersede).
/// Ownership-guarded: a no-op unless the bucket currently points at this
/// statement, so a bucket-sharing sibling is never evicted.
pub fn remove_from_predicate_index(
    wtxn: &WriteTransaction,
    scope: RowScope,
    predicate_id: u32,
    kind: u8,
    confidence: f32,
    statement_id_bytes: &[u8; 16],
) -> Result<(), StatementOpError> {
    let mut t = wtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE)?;
    let key = (
        scope.namespace_id,
        scope.space_id_bytes,
        predicate_id,
        kind,
        confidence_bucket(confidence),
        *statement_id_bytes,
    );
    if t.get(&key)?.map(|g| g.value()).as_ref() == Some(statement_id_bytes) {
        t.remove(&key)?;
    }
    Ok(())
}

/// Count non-tombstoned statement rows in `namespace_id` that key on
/// `predicate_id`, capped at `limit` (pass `usize::MAX` for an exact
/// count). Walks the by-predicate index across every space in the
/// namespace and dereferences each hit to the primary row so
/// tombstoned rows don't count. Used by `SCHEMA_DROP`'s in-use safety
/// gate — dropping a predicate that still has live statements requires
/// the caller's explicit `force`.
pub fn statement_live_count_by_predicate(
    wtxn: &WriteTransaction,
    namespace_id: u32,
    predicate_id: PredicateId,
    limit: usize,
) -> Result<usize, StatementOpError> {
    if limit == 0 {
        return Ok(0);
    }
    let want = predicate_id.raw();
    let index = wtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE)?;
    let primary = wtxn.open_table(STATEMENTS_TABLE)?;
    let mut count = 0usize;
    for entry in index.iter()? {
        let (k, v) = entry?;
        let (k_ns, _space, k_pred, _kind, _bucket, _sid) = k.value();
        if k_ns != namespace_id || k_pred != want {
            continue;
        }
        let sid = v.value();
        if let Some(row) = primary.get(&sid)?.map(|g| g.value()) {
            if row.tombstoned == 0 {
                count += 1;
                if count >= limit {
                    break;
                }
            }
        }
    }
    Ok(count)
}

/// Re-key a still-current statement's predicate-bucket entry after its
/// confidence changed. No-op when the coarse bucket is unchanged — this
/// is the index-churn gate that mirrors the >0.05 confidence threshold
/// callers apply before recomputing (a >0.05 drift that stays in one
/// bucket rewrites the row but not the index). Ownership-guarded on the
/// old key like [`remove_from_predicate_index`].
pub fn rekey_predicate_index(
    wtxn: &WriteTransaction,
    scope: RowScope,
    predicate_id: u32,
    kind: u8,
    old_confidence: f32,
    new_confidence: f32,
    statement_id_bytes: &[u8; 16],
) -> Result<(), StatementOpError> {
    let old_bucket = confidence_bucket(old_confidence);
    let new_bucket = confidence_bucket(new_confidence);
    if old_bucket == new_bucket {
        return Ok(());
    }
    let mut t = wtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE)?;
    let old_key = (
        scope.namespace_id,
        scope.space_id_bytes,
        predicate_id,
        kind,
        old_bucket,
        *statement_id_bytes,
    );
    if t.get(&old_key)?.map(|g| g.value()).as_ref() == Some(statement_id_bytes) {
        t.remove(&old_key)?;
    }
    t.insert(
        &(
            scope.namespace_id,
            scope.space_id_bytes,
            predicate_id,
            kind,
            new_bucket,
            *statement_id_bytes,
        ),
        statement_id_bytes,
    )?;
    Ok(())
}

/// Insert a statement's `statements_by_predicate` entry when it (re-)joins
/// the live set — the FORGET-cascade revert path re-activating a row it
/// tombstoned. Bucketed by `confidence`. Mirrors the create-path insert in
/// [`insert_new_statement`] step 3. Idempotent: re-inserting the same key
/// is a redb overwrite of an identical value.
pub fn add_to_predicate_index(
    wtxn: &WriteTransaction,
    scope: RowScope,
    predicate_id: u32,
    kind: u8,
    confidence: f32,
    statement_id_bytes: &[u8; 16],
) -> Result<(), StatementOpError> {
    let mut t = wtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE)?;
    t.insert(
        &(
            scope.namespace_id,
            scope.space_id_bytes,
            predicate_id,
            kind,
            confidence_bucket(confidence),
            *statement_id_bytes,
        ),
        statement_id_bytes,
    )?;
    Ok(())
}

/// Flip a statement's `by_subject` index entry from non-current
/// (`is_current = 0`) back to current (`1`). The inverse of
/// [`flip_by_subject_to_noncurrent`], used by the FORGET-cascade revert
/// path when it un-tombstones a row.
pub fn flip_by_subject_to_current(
    wtxn: &WriteTransaction,
    scope: RowScope,
    subject_entity_bytes: [u8; 16],
    kind: u8,
    predicate_id: u32,
    statement_id_bytes: &[u8; 16],
) -> Result<(), StatementOpError> {
    let mut bys = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
    bys.remove(&(
        scope.namespace_id,
        scope.space_id_bytes,
        subject_entity_bytes,
        kind,
        predicate_id,
        0u8,
        *statement_id_bytes,
    ))?;
    bys.insert(
        &(
            scope.namespace_id,
            scope.space_id_bytes,
            subject_entity_bytes,
            kind,
            predicate_id,
            1u8,
            *statement_id_bytes,
        ),
        statement_id_bytes,
    )?;
    Ok(())
}

/// Recompute a statement's confidence via noisy-OR over its evidence,
/// **iff** the evidence carries per-entry metadata. Wire callers send
/// `EvidenceRef::Inline` with `confidence_milli = 0` (per-entry metadata
/// dropped on the wire), so this is a no-op for them; in-process callers
/// (extractors / tests) populate the field. Shared by `statement_create`
/// and `statement_supersede` so the aggregation hookup lives in one
/// place.
pub(super) fn recompute_confidence_from_evidence(
    wtxn: &WriteTransaction,
    stmt: &mut Statement,
    now_unix_nanos: u64,
) -> Result<(), StatementOpError> {
    if evidence_has_per_entry_metadata(wtxn, &stmt.evidence)? {
        let entries = resolve_evidence_entries(wtxn, &stmt.evidence)?;
        stmt.confidence = brain_core::aggregate_confidence(
            &entries,
            now_unix_nanos,
            stmt.kind,
            &brain_core::ConfidenceConfig::default_v1(),
        );
    }
    Ok(())
}

/// Flip a statement's `by_subject` index entry from current (`is_current
/// = 1`) to non-current (`0`). Shared by `statement_tombstone` and the
/// old-row path of `statement_supersede` — the deactivation flip is the
/// same in both.
pub(super) fn flip_by_subject_to_noncurrent(
    wtxn: &WriteTransaction,
    scope: RowScope,
    subject_entity_bytes: [u8; 16],
    kind: u8,
    predicate_id: u32,
    statement_id_bytes: &[u8; 16],
) -> Result<(), StatementOpError> {
    let mut bys = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
    bys.remove(&(
        scope.namespace_id,
        scope.space_id_bytes,
        subject_entity_bytes,
        kind,
        predicate_id,
        1u8,
        *statement_id_bytes,
    ))?;
    bys.insert(
        &(
            scope.namespace_id,
            scope.space_id_bytes,
            subject_entity_bytes,
            kind,
            predicate_id,
            0u8,
            *statement_id_bytes,
        ),
        statement_id_bytes,
    )?;
    Ok(())
}

/// Look up the **current** statement for a given (subject, predicate,
/// kind). Used by Preference auto-supersession.
fn find_current_statement(
    wtxn: &WriteTransaction,
    scope: RowScope,
    subject: EntityId,
    predicate: PredicateId,
    kind: StatementKind,
) -> Result<Option<StatementId>, StatementOpError> {
    let bys = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
    // The trailing statement id is part of the key now, so an exact get
    // can't address the single current row directly. Single-valued kinds
    // still have exactly one current row, so a range scan over the
    // (scope, subject, kind, predicate, is_current=1) prefix yields it as
    // the first (and only) value.
    let lo = (
        scope.namespace_id,
        scope.space_id_bytes,
        subject.to_bytes(),
        kind.as_u8(),
        predicate.raw(),
        1u8,
        [0u8; 16],
    );
    let hi = (
        scope.namespace_id,
        scope.space_id_bytes,
        subject.to_bytes(),
        kind.as_u8(),
        predicate.raw(),
        1u8,
        [0xffu8; 16],
    );
    // Single-valued kinds have exactly one current row; take the first.
    let mut it = bys.range(lo..=hi)?;
    let first = match it.next() {
        Some(entry) => Some(StatementId::from(entry?.1.value())),
        None => None,
    };
    Ok(first)
}

/// Find an active statement byte-identical to `s` on its semantic
/// identity — `(subject, predicate, kind, object, event_at)`.
///
/// Used to make cumulative-kind creation idempotent under extractor
/// retries. Matching includes the object and event time, so distinct
/// multi-values (different objects) and distinct events (different
/// `event_at`) never collapse — only an exact repeat does.
fn find_identical_active_statement(
    wtxn: &WriteTransaction,
    scope: RowScope,
    subject: EntityId,
    s: &Statement,
) -> Result<Option<StatementId>, StatementOpError> {
    let bys = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
    let lo = (
        scope.namespace_id,
        scope.space_id_bytes,
        subject.to_bytes(),
        s.kind.as_u8(),
        s.predicate.raw(),
        1u8,
        [0u8; 16],
    );
    let hi = (
        scope.namespace_id,
        scope.space_id_bytes,
        subject.to_bytes(),
        s.kind.as_u8(),
        s.predicate.raw(),
        1u8,
        [0xffu8; 16],
    );
    let mut ids: Vec<[u8; 16]> = Vec::new();
    for entry in bys.range(lo..=hi)? {
        let (_, v) = entry?;
        ids.push(v.value());
    }
    let st = wtxn.open_table(STATEMENTS_TABLE)?;
    let self_bytes = s.id.to_bytes();
    for id in ids {
        if id == self_bytes {
            continue;
        }
        let Some(m) = st.get(&id)?.map(|g| g.value()) else {
            continue;
        };
        let Some(existing) = statement_from_metadata(&m) else {
            continue;
        };
        if existing.object == s.object && existing.event_at_unix_nanos == s.event_at_unix_nanos {
            return Ok(Some(existing.id));
        }
    }
    Ok(None)
}

/// Write-txn variant used by the in-line contradiction probe so it
/// reads uncommitted state inside the same transaction.
fn load_active_facts_for_subject_predicate_wtxn(
    wtxn: &WriteTransaction,
    scope: RowScope,
    subject: EntityId,
    predicate: PredicateId,
) -> Result<Vec<Statement>, StatementOpError> {
    let bys = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
    // Cumulative Facts can have many current rows for one
    // (subject, predicate); collect every one via a range scan over the
    // is_current=1 prefix.
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
    let st = wtxn.open_table(STATEMENTS_TABLE)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let row: Option<StatementMetadata> = st.get(&id)?.map(|g| g.value());
        if let Some(m) = row {
            if let Some(s) = statement_from_metadata(&m) {
                out.push(s);
            }
        }
    }
    Ok(out)
}

/// True iff any inline evidence entry carries per-entry metadata
/// (`confidence_milli > 0`). Overflow rows are assumed to carry full
/// metadata (the four parallel vectors store confidence_milli per
/// entry). Noisy-OR aggregation gates on this signal — see the design
/// note in `statement_create`.
pub(super) fn evidence_has_per_entry_metadata(
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
pub(super) fn resolve_evidence_entries(
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
            use crate::tables::entity::{EntityMetadata, ENTITIES_TABLE};
            let t = wtxn.open_table(ENTITIES_TABLE)?;
            let row: Option<EntityMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
            Ok(row.is_some())
        }
    }
}

/// The registered entity type of `id`, or `None` when the entity row
/// doesn't exist. Used by the declared-`Entity<Type>` object check.
fn entity_type_of(
    wtxn: &WriteTransaction,
    id: EntityId,
) -> Result<Option<EntityTypeId>, StatementOpError> {
    use crate::tables::entity::{EntityMetadata, ENTITIES_TABLE};
    let t = wtxn.open_table(ENTITIES_TABLE)?;
    let row: Option<EntityMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
    Ok(row.map(|r| EntityTypeId::from(r.entity_type_id)))
}

fn predicate_get_via(
    wtxn: &WriteTransaction,
    id: PredicateId,
) -> Result<Option<Predicate>, StatementOpError> {
    use crate::tables::predicate::{PredicateDefinition, PREDICATES_TABLE};
    let t = wtxn.open_table(PREDICATES_TABLE)?;
    let row: Option<PredicateDefinition> = t.get(&id.raw())?.map(|g| g.value());
    Ok(row.as_ref().map(PredicateDefinition::to_predicate))
}

/// One-time construction of the immutable id-ordered pagination indexes
/// ([`STATEMENTS_BY_SUBJECT_ID_TABLE`] and
/// [`STATEMENTS_BY_PREDICATE_ID_TABLE`]) from the authoritative primary
/// rows, for a DB written before these indexes existed. Idempotent: a
/// no-op once the predicate-id index holds any row (every live statement
/// writes it, so a non-empty index is never stale-missing).
///
/// These indexes are derived data — like the in-RAM HNSW rebuilt from the
/// primary rows at boot — so reconstructing them is index construction,
/// not a format migration. Runs inside the caller's open-time write txn.
/// Uses redb-native errors to match [`crate::tables::materialize_all_tables`].
/// Returns the number of primary rows scanned into the indexes.
pub fn backfill_statement_id_indexes(wtxn: &WriteTransaction) -> Result<usize, redb::Error> {
    // Already populated → the common (already-migrated) boot. The
    // predicate-id index gets a row for every live statement, so it is the
    // authoritative sentinel (the subject-id index skips memory subjects).
    {
        let idx = wtxn.open_table(STATEMENTS_BY_PREDICATE_ID_TABLE)?;
        if idx.iter()?.next().is_some() {
            return Ok(0);
        }
    }
    // Collect keys first so the primary read iterator is dropped before the
    // indexes are opened for write (borrow discipline).
    struct Row {
        ns: u32,
        ag: [u8; 16],
        subject: [u8; 16],
        predicate_id: u32,
        id: [u8; 16],
        subject_kind: u8,
    }
    let rows: Vec<Row> = {
        let primary = wtxn.open_table(STATEMENTS_TABLE)?;
        let mut rs = Vec::new();
        for entry in primary.iter()? {
            let (_, v) = entry?;
            let m = v.value();
            rs.push(Row {
                ns: m.namespace_id,
                ag: m.space_id_bytes,
                subject: m.subject_entity_bytes,
                predicate_id: m.predicate_id,
                id: m.statement_id_bytes,
                subject_kind: m.subject_kind,
            });
        }
        rs
    };
    let n = rows.len();
    let mut bysi = wtxn.open_table(STATEMENTS_BY_SUBJECT_ID_TABLE)?;
    let mut bypi = wtxn.open_table(STATEMENTS_BY_PREDICATE_ID_TABLE)?;
    for r in rows {
        // Mirror the insert gate: memory subjects are by-subject-indexed
        // too, only `pending` (subject_kind == 1) is skipped.
        if r.subject_kind != 1 {
            bysi.insert(&(r.ns, r.ag, r.subject, r.id), &())?;
        }
        bypi.insert(&(r.ns, r.ag, r.predicate_id, r.id), &())?;
    }
    Ok(n)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::super::list::{statement_history, statement_list, StatementListFilter};
    use super::super::supersede::statement_supersede;
    use super::super::tombstone::statement_tombstone;
    use super::*;
    use crate::schema::predicate::predicate_intern;
    use brain_core::{Entity, EntityType, StatementValue, TombstoneReason, INLINE_EVIDENCE_CAP};
    use brain_core::{MemoryId, SessionId};
    use smallvec::SmallVec;

    const T0: u64 = 1_700_000_000_000_000_000;

    fn test_scope() -> RowScope {
        RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xAB; 16])
    }

    fn open_db() -> (tempfile::TempDir, crate::MetadataDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::MetadataDb::open(dir.path().join("md.redb")).unwrap();
        (dir, db)
    }

    /// Insert a Person entity in the open db; return its EntityId.
    fn make_entity(db: &mut crate::MetadataDb, name: &str) -> EntityId {
        let id = EntityId::new();
        let normalized = crate::entity::ops::normalize_name(name);
        let e = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.to_string(),
            normalized,
            1_700_000_000_000_000_000,
        );
        let wtxn = db.write_txn().unwrap();
        crate::entity::ops::entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e)
            .unwrap();
        wtxn.commit().unwrap();
        id
    }

    /// Intern a Fact-only Entity-object predicate; return its id.
    fn intern_fact_entity_pred(db: &mut crate::MetadataDb, name: &str) -> PredicateId {
        intern_fact_entity_pred_with_stateful(db, name, false)
    }

    fn intern_fact_entity_pred_with_stateful(
        db: &mut crate::MetadataDb,
        name: &str,
        is_stateful: bool,
    ) -> PredicateId {
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(StatementKind::Fact),
            /* object: Entity */ 1,
            /* schema_version */ 1,
            "",
            is_stateful,
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
            true,
            1_700_000_000_000_000_000,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    /// Intern a single-valued Attribute predicate with `stateful: false`,
    /// so any supersession observed is driven by the KIND, not the flag.
    fn intern_attribute_value_pred(db: &mut crate::MetadataDb, name: &str) -> PredicateId {
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(StatementKind::Attribute),
            /* object: Value */ 2,
            /* schema_version */ 1,
            "",
            /* is_stateful */ false,
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
            false,
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

    fn fresh_attr(subject: EntityId, predicate: PredicateId, value: &str) -> Statement {
        Statement::new_root(
            StatementId::new(),
            StatementKind::Attribute,
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
    fn live_count_by_predicate_counts_only_non_tombstoned() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "manager-role");
        let pred = intern_fact_entity_pred(&mut db, "reports_to");
        let ns = test_scope().namespace_id;

        // No statements yet.
        {
            let wtxn = db.write_txn().unwrap();
            assert_eq!(
                statement_live_count_by_predicate(&wtxn, ns, pred, usize::MAX).unwrap(),
                0
            );
        }

        // One live statement on the predicate.
        let sid = {
            let wtxn = db.write_txn().unwrap();
            let s = fresh_fact(subj, pred, obj);
            let id = statement_create(&wtxn, test_scope(), SessionId::DEFAULT, &s, T0).unwrap();
            wtxn.commit().unwrap();
            id
        };
        {
            let wtxn = db.write_txn().unwrap();
            assert_eq!(
                statement_live_count_by_predicate(&wtxn, ns, pred, usize::MAX).unwrap(),
                1
            );
            // A different predicate id sees nothing.
            assert_eq!(
                statement_live_count_by_predicate(&wtxn, ns, PredicateId::from(pred.raw() + 7), 1)
                    .unwrap(),
                0
            );
            // A different namespace sees nothing.
            assert_eq!(
                statement_live_count_by_predicate(&wtxn, ns + 1, pred, usize::MAX).unwrap(),
                0
            );
            // The cap short-circuits at the first hit.
            assert_eq!(
                statement_live_count_by_predicate(&wtxn, ns, pred, 1).unwrap(),
                1
            );
        }

        // Tombstoning removes it from the live count.
        {
            let wtxn = db.write_txn().unwrap();
            statement_tombstone(&wtxn, sid, TombstoneReason::UserRequest, T0 + 1).unwrap();
            wtxn.commit().unwrap();
        }
        {
            let wtxn = db.write_txn().unwrap();
            assert_eq!(
                statement_live_count_by_predicate(&wtxn, ns, pred, usize::MAX).unwrap(),
                0
            );
        }
    }

    #[test]
    fn create_fact_round_trips_via_get() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "manager-role");
        let pred = intern_fact_entity_pred(&mut db, "role");

        let s = fresh_fact(subj, pred, obj);

        let wtxn = db.write_txn().unwrap();
        let id = statement_create(
            &wtxn,
            test_scope(),
            brain_core::SessionId::DEFAULT,
            &s,
            1_700_000_000_000_000_001,
        )
        .unwrap();
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
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let sc = test_scope();
        // by_subject — range over the is_current=1 prefix; the trailing
        // statement id is now part of the key.
        let bys = rtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE).unwrap();
        let lo = (
            sc.namespace_id,
            sc.space_id_bytes,
            subj.to_bytes(),
            StatementKind::Fact.as_u8(),
            pred.raw(),
            1u8,
            [0u8; 16],
        );
        let hi = (
            sc.namespace_id,
            sc.space_id_bytes,
            subj.to_bytes(),
            StatementKind::Fact.as_u8(),
            pred.raw(),
            1u8,
            [0xffu8; 16],
        );
        assert!(bys.range(lo..=hi).unwrap().next().is_some());
        // by_predicate
        let byp = rtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE).unwrap();
        assert!(byp
            .get(&(
                sc.namespace_id,
                sc.space_id_bytes,
                pred.raw(),
                StatementKind::Fact.as_u8(),
                confidence_bucket(0.9),
                s.id.to_bytes(),
            ))
            .unwrap()
            .is_some());
        // by_object_entity
        let byo = rtxn.open_table(STATEMENTS_BY_OBJECT_ENTITY_TABLE).unwrap();
        assert!(byo
            .get(&(
                sc.namespace_id,
                sc.space_id_bytes,
                obj.to_bytes(),
                StatementKind::Fact.as_u8(),
                s.id.to_bytes(),
            ))
            .unwrap()
            .is_some());
        // chain
        let cht = rtxn.open_table(STATEMENT_CHAIN_TABLE).unwrap();
        assert!(cht
            .get(&(sc.namespace_id, sc.space_id_bytes, s.id.to_bytes(), 1u32))
            .unwrap()
            .is_some());
    }

    #[test]
    fn cumulative_fact_identical_reapply_is_noop() {
        // On a transient LLM failure the extractor worker re-runs the whole
        // pipeline and re-applies the already-committed cumulative-kind rows.
        // An identical `(subject, predicate, object)` Fact must not leak a
        // second active row: the create is a no-op returning the first id.
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "dup-subj");
        let obj = make_entity(&mut db, "dup-obj");
        let pred = intern_fact_entity_pred(&mut db, "role_dup");

        let s1 = fresh_fact(subj, pred, obj);
        let wtxn = db.write_txn().unwrap();
        let id1 =
            statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s1, 0).unwrap();
        wtxn.commit().unwrap();

        // The retry: identical content, fresh StatementId.
        let s2 = fresh_fact(subj, pred, obj);
        assert_ne!(s1.id, s2.id);
        let wtxn = db.write_txn().unwrap();
        let id2 =
            statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s2, 1).unwrap();
        wtxn.commit().unwrap();

        assert_eq!(id2, id1, "identical re-apply must return the existing id");
        let rtxn = db.read_txn().unwrap();
        assert!(statement_get(&rtxn, s2.id).unwrap().is_none());

        let filter = StatementListFilter {
            subject: Some(subj),
            predicate: Some(pred),
            kind: Some(StatementKind::Fact),
            current_only: true,
            ..Default::default()
        };
        let current = statement_list(&rtxn, test_scope(), &filter).unwrap();
        assert_eq!(current.len(), 1, "retry must not add a second Fact row");
        assert_eq!(current[0].id, s1.id);
    }

    #[test]
    fn cumulative_fact_distinct_objects_coexist() {
        // Dedup keys on the object, so two Facts with the same
        // (subject, predicate) but DIFFERENT objects are genuine
        // multi-values and both stay current.
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "multi-subj");
        let obj_a = make_entity(&mut db, "obj-a");
        let obj_b = make_entity(&mut db, "obj-b");
        let pred = intern_fact_entity_pred(&mut db, "linked_role");

        for obj in [obj_a, obj_b] {
            let s = fresh_fact(subj, pred, obj);
            let wtxn = db.write_txn().unwrap();
            statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s, 0).unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        let filter = StatementListFilter {
            subject: Some(subj),
            predicate: Some(pred),
            kind: Some(StatementKind::Fact),
            current_only: true,
            ..Default::default()
        };
        let current = statement_list(&rtxn, test_scope(), &filter).unwrap();
        assert_eq!(current.len(), 2);
    }

    #[test]
    fn event_distinct_times_coexist_identical_time_dedups() {
        // Events are cumulative but distinguished by event time: two events
        // with different `event_at` coexist, while re-applying an event with
        // the same object AND time is a no-op (extractor retry).
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "event-subj");
        let pred = intern_event_any_pred(&mut db, "scheduled_at");

        let e1 = fresh_event(subj, pred, 1_700_000_100_000_000_000);
        let e2 = fresh_event(subj, pred, 1_700_000_200_000_000_000);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e1, 0).unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e2, 1).unwrap();
        wtxn.commit().unwrap();

        // Retry of e1: same object, same event_at, fresh id → no-op.
        let e1_retry = fresh_event(subj, pred, 1_700_000_100_000_000_000);
        let wtxn = db.write_txn().unwrap();
        let dup_id = statement_create(
            &wtxn,
            test_scope(),
            brain_core::SessionId::DEFAULT,
            &e1_retry,
            2,
        )
        .unwrap();
        wtxn.commit().unwrap();

        assert_eq!(dup_id, e1.id, "identical-time event retry is a no-op");
        let rtxn = db.read_txn().unwrap();
        assert!(statement_get(&rtxn, e1_retry.id).unwrap().is_none());

        let filter = StatementListFilter {
            subject: Some(subj),
            predicate: Some(pred),
            kind: Some(StatementKind::Event),
            current_only: true,
            ..Default::default()
        };
        let current = statement_list(&rtxn, test_scope(), &filter).unwrap();
        assert_eq!(current.len(), 2, "two distinct-time events must coexist");
    }

    #[test]
    fn create_preference_auto_supersedes_prior() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pred = intern_pref_value_pred(&mut db, "prefers");

        // First Preference.
        let p1 = fresh_pref(subj, pred, "async");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &p1, 0).unwrap();
        wtxn.commit().unwrap();

        // Second Preference — should auto-supersede.
        let p2 = fresh_pref(subj, pred, "written-agendas");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &p2, 0).unwrap();
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
    fn create_stateful_fact_auto_supersedes_prior() {
        // A Fact predicate declared with `stateful: true` (e.g. works_at)
        // must auto-supersede the prior active row, the same way
        // Preference does — the gate is the flag, not the kind.
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let employer1 = make_entity(&mut db, "acme");
        let employer2 = make_entity(&mut db, "globex");
        let pred = intern_fact_entity_pred_with_stateful(&mut db, "works_at", true);

        let f1 = fresh_fact(subj, pred, employer1);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &f1, 0).unwrap();
        wtxn.commit().unwrap();

        let f2 = fresh_fact(subj, pred, employer2);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &f2, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let f1_back = statement_get(&rtxn, f1.id).unwrap().unwrap();
        let f2_back = statement_get(&rtxn, f2.id).unwrap().unwrap();
        assert_eq!(f1_back.superseded_by, Some(f2.id));
        assert_eq!(f2_back.supersedes, Some(f1.id));
        assert_eq!(f2_back.version, 2);
        assert_eq!(f2_back.chain_root, f1.id);
    }

    #[test]
    fn create_cumulative_fact_keeps_both_rows() {
        // Default Fact (is_stateful: false) accumulates — both rows
        // stay active. Regression: the supersession switch is gated by
        // the flag, not the kind.
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let target1 = make_entity(&mut db, "raj");
        let target2 = make_entity(&mut db, "sam");
        let pred = intern_fact_entity_pred(&mut db, "knows");

        let f1 = fresh_fact(subj, pred, target1);
        let f2 = fresh_fact(subj, pred, target2);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &f1, 0).unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &f2, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let f1_back = statement_get(&rtxn, f1.id).unwrap().unwrap();
        let f2_back = statement_get(&rtxn, f2.id).unwrap().unwrap();
        assert_eq!(f1_back.superseded_by, None);
        assert_eq!(f2_back.supersedes, None);
        assert!(f1_back.is_current(1_800_000_000_000_000_000));
        assert!(f2_back.is_current(1_800_000_000_000_000_000));
    }

    #[test]
    fn create_attribute_auto_supersedes_by_kind() {
        // An Attribute is single-valued (cardinality: single), so a new
        // value for the same (subject, predicate) supersedes the prior one —
        // "what is X's Y now". The predicate here is `stateful: false`, so
        // the supersession is driven by the KIND, not the flag.
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pred = intern_attribute_value_pred(&mut db, "city");

        let a1 = fresh_attr(subj, pred, "Paris");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &a1, 0).unwrap();
        wtxn.commit().unwrap();

        let a2 = fresh_attr(subj, pred, "Berlin");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &a2, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let a1_back = statement_get(&rtxn, a1.id).unwrap().unwrap();
        let a2_back = statement_get(&rtxn, a2.id).unwrap().unwrap();
        assert_eq!(a1_back.superseded_by, Some(a2.id));
        assert_eq!(a2_back.supersedes, Some(a1.id));
        assert_eq!(a2_back.version, 2);
        assert_eq!(a2_back.chain_root, a1.id);
        assert!(!a1_back.is_current(1_800_000_000_000_000_000));
        assert!(a2_back.is_current(1_800_000_000_000_000_000));
    }

    #[test]
    fn create_event_keeps_event_at_and_accumulates() {
        // Events are Set + append-only: each carries its own `event_at` and a
        // second Event for the same (subject, predicate) does NOT supersede —
        // both stay current so "when" and enumeration read off the same rows.
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pred = intern_event_any_pred(&mut db, "traveled_to");

        let e1 = fresh_event(subj, pred, 1_700_000_000);
        let e2 = fresh_event(subj, pred, 1_800_000_000);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e1, 0).unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e2, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let e1_back = statement_get(&rtxn, e1.id).unwrap().unwrap();
        let e2_back = statement_get(&rtxn, e2.id).unwrap().unwrap();
        // Time is owned by the fact.
        assert_eq!(e1_back.event_at_unix_nanos, Some(1_700_000_000));
        assert_eq!(e2_back.event_at_unix_nanos, Some(1_800_000_000));
        // No supersession — both current.
        assert_eq!(e1_back.superseded_by, None);
        assert_eq!(e2_back.supersedes, None);
        assert!(e1_back.is_current(1_900_000_000_000_000_000));
        assert!(e2_back.is_current(1_900_000_000_000_000_000));
    }

    #[test]
    fn create_event_allows_missing_event_at() {
        // A dateless/same-day Event persists as an Event with `event_at = None`
        // (no longer rejected, no longer demoted to Fact) so the read keeps its
        // temporal role and answers "when" from the memory's own occurred_at.
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pred = intern_event_any_pred(&mut db, "scheduled");

        let mut s = fresh_event(subj, pred, 1_700_000_000);
        s.event_at_unix_nanos = None;
        let wtxn = db.write_txn().unwrap();
        let id = statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s, 0)
            .expect("dateless Event should persist");
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let back = statement_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(back.kind, StatementKind::Event);
        assert_eq!(back.event_at_unix_nanos, None);
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
        let err = statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s, 0)
            .unwrap_err();
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
        let err = statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s, 0)
            .unwrap_err();
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
        let err = statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s, 0)
            .unwrap_err();
        matches!(err, StatementOpError::UnknownSubject(_))
            .then_some(())
            .expect("expected UnknownSubject");
    }

    // ----- Declared `object: Entity<Type>` range enforcement. -----

    const ORGANIZATION_ID: brain_core::EntityTypeId = brain_core::EntityTypeId(2);

    fn make_entity_typed(
        db: &mut crate::MetadataDb,
        name: &str,
        entity_type: brain_core::EntityTypeId,
    ) -> EntityId {
        let id = EntityId::new();
        let normalized = crate::entity::ops::normalize_name(name);
        let e = Entity::new_active(
            id,
            entity_type,
            name.to_string(),
            normalized,
            1_700_000_000_000_000_000,
        );
        let wtxn = db.write_txn().unwrap();
        crate::entity::ops::entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e)
            .unwrap();
        wtxn.commit().unwrap();
        id
    }

    /// Intern a Fact predicate declaring `object: Entity<entity_type>`.
    fn intern_fact_entity_typed_pred(
        db: &mut crate::MetadataDb,
        name: &str,
        entity_type: brain_core::EntityTypeId,
    ) -> PredicateId {
        use crate::schema::predicate::ObjectConstraint;
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(StatementKind::Fact),
            ObjectConstraint::entity(entity_type.raw()),
            /* schema_version */ 1,
            "",
            false,
            1_700_000_000_000_000_000,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    #[test]
    fn declared_object_entity_type_accepts_conforming_object() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "typed-subject");
        let org = make_entity_typed(&mut db, "typed-org", ORGANIZATION_ID);
        let pred = intern_fact_entity_typed_pred(&mut db, "works_at_typed", ORGANIZATION_ID);

        let s = fresh_fact(subj, pred, org);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        assert!(statement_get(&rtxn, s.id).unwrap().is_some());
    }

    #[test]
    fn declared_object_entity_type_rejects_wrong_type() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "typed-subject-2");
        let person_object = make_entity(&mut db, "not-an-org");
        let pred = intern_fact_entity_typed_pred(&mut db, "works_at_typed2", ORGANIZATION_ID);

        let s = fresh_fact(subj, pred, person_object);
        let wtxn = db.write_txn().unwrap();
        let err = statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s, 0)
            .unwrap_err();
        match err {
            StatementOpError::ObjectEntityTypeMismatch {
                entity,
                expected,
                actual,
            } => {
                assert_eq!(entity, person_object);
                assert_eq!(expected, ORGANIZATION_ID);
                assert_eq!(actual, EntityType::PERSON_ID);
            }
            other => panic!("expected ObjectEntityTypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn untyped_entity_object_declaration_permits_any_entity_type() {
        // `object: Entity<Any>` (and every implicit-from-write
        // predicate) constrains the variant only — the pre-existing
        // behaviour must not tighten.
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "bare-subject");
        let concept = make_entity_typed(&mut db, "bare-concept", brain_core::EntityTypeId(6));
        let pred = intern_fact_entity_pred(&mut db, "relates_bare");

        let s = fresh_fact(subj, pred, concept);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s, 0).unwrap();
        wtxn.commit().unwrap();
    }

    #[test]
    fn create_pending_subject_rejected_v1() {
        let (_dir, mut db) = open_db();
        let pred = intern_fact_entity_pred(&mut db, "role4");
        let obj = make_entity(&mut db, "x");
        let mut s = fresh_fact(EntityId::new(), pred, obj);
        s.subject = SubjectRef::Pending(brain_core::AuditId::new());
        let wtxn = db.write_txn().unwrap();
        let err = statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s, 0)
            .unwrap_err();
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
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &f1, 0).unwrap();
        // f2 contradicts f1 on object; both must store.
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &f2, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g1 = statement_get(&rtxn, f1.id).unwrap().unwrap();
        let g2 = statement_get(&rtxn, f2.id).unwrap().unwrap();
        assert!(!g1.tombstoned);
        assert!(!g2.tombstoned);

        let conflicts =
            super::super::list::statements_contradicting(&rtxn, test_scope(), subj, pred).unwrap();
        // The by_subject Fact index is multi-value now: appending the
        // statement id to the key means both contradicting Facts are
        // distinct rows, so the runtime probe enumerates both and
        // surfaces the disagreement.
        assert_eq!(conflicts.len(), 2);
        let ids: Vec<_> = conflicts.iter().map(|s| s.id).collect();
        assert!(ids.contains(&f1.id));
        assert!(ids.contains(&f2.id));
    }

    #[test]
    fn supersede_fact_chain_root_inherited_on_second_supersede() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "x");
        let pred = intern_fact_entity_pred(&mut db, "role5");

        let f1 = fresh_fact(subj, pred, obj);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &f1, 0).unwrap();
        wtxn.commit().unwrap();

        let f2 = fresh_fact(subj, pred, obj);
        let wtxn = db.write_txn().unwrap();
        statement_supersede(
            &wtxn,
            test_scope(),
            brain_core::SessionId::DEFAULT,
            f1.id,
            &f2,
            1,
        )
        .unwrap();
        wtxn.commit().unwrap();

        let f3 = fresh_fact(subj, pred, obj);
        let wtxn = db.write_txn().unwrap();
        statement_supersede(
            &wtxn,
            test_scope(),
            brain_core::SessionId::DEFAULT,
            f2.id,
            &f3,
            2,
        )
        .unwrap();
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
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &f1, 0).unwrap();
        wtxn.commit().unwrap();

        let f2 = fresh_fact(subj, pred, obj);
        let wtxn = db.write_txn().unwrap();
        statement_supersede(
            &wtxn,
            test_scope(),
            brain_core::SessionId::DEFAULT,
            f1.id,
            &f2,
            999_999_999_999,
        )
        .unwrap();
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
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e1, 0).unwrap();
        wtxn.commit().unwrap();

        let e2 = fresh_event(subj, pred, 2);
        let wtxn = db.write_txn().unwrap();
        let err = statement_supersede(
            &wtxn,
            test_scope(),
            brain_core::SessionId::DEFAULT,
            e1.id,
            &e2,
            0,
        )
        .unwrap_err();
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
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &f, 0).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.write_txn().unwrap();
        statement_tombstone(&wtxn, f.id, TombstoneReason::UserRequest, 42).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let sc = test_scope();
        let bys = rtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE).unwrap();
        let cur_lo = (
            sc.namespace_id,
            sc.space_id_bytes,
            subj.to_bytes(),
            StatementKind::Fact.as_u8(),
            pred.raw(),
            1u8,
            [0u8; 16],
        );
        let cur_hi = (
            sc.namespace_id,
            sc.space_id_bytes,
            subj.to_bytes(),
            StatementKind::Fact.as_u8(),
            pred.raw(),
            1u8,
            [0xffu8; 16],
        );
        assert!(
            bys.range(cur_lo..=cur_hi).unwrap().next().is_none(),
            "is_current=1 entry must be gone"
        );
        let stale_lo = (
            sc.namespace_id,
            sc.space_id_bytes,
            subj.to_bytes(),
            StatementKind::Fact.as_u8(),
            pred.raw(),
            0u8,
            [0u8; 16],
        );
        let stale_hi = (
            sc.namespace_id,
            sc.space_id_bytes,
            subj.to_bytes(),
            StatementKind::Fact.as_u8(),
            pred.raw(),
            0u8,
            [0xffu8; 16],
        );
        assert!(
            bys.range(stale_lo..=stale_hi).unwrap().next().is_some(),
            "is_current=0 entry must exist"
        );
    }

    #[test]
    fn tombstone_preserves_evidence_index() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "x");
        let pred = intern_fact_entity_pred(&mut db, "role8");
        let mem = MemoryId::pack(7, SessionId::DEFAULT.into(), 0);
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
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &f, 0).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.write_txn().unwrap();
        statement_tombstone(&wtxn, f.id, TombstoneReason::UserRequest, 1).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let sc = test_scope();
        let evi = rtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE).unwrap();
        assert!(evi
            .get(&(
                sc.namespace_id,
                sc.space_id_bytes,
                mem.to_be_bytes(),
                f.id.to_bytes()
            ))
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
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &p1, 0).unwrap();
        wtxn.commit().unwrap();

        let p2 = fresh_pref(subj, pred, "v2");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &p2, 0).unwrap();
        wtxn.commit().unwrap();

        let p3 = fresh_pref(subj, pred, "v3");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &p3, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let chain = statement_history(&rtxn, test_scope(), p1.id).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].id, p1.id);
        assert_eq!(chain[1].id, p2.id);
        assert_eq!(chain[2].id, p3.id);

        // Anchor from any member works.
        let chain2 = statement_history(&rtxn, test_scope(), p3.id).unwrap();
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
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &p1, 0).unwrap();
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &p2, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let filter = StatementListFilter {
            subject: Some(subj),
            predicate: Some(pred),
            kind: Some(StatementKind::Preference),
            current_only: true,
            ..Default::default()
        };
        let out = statement_list(&rtxn, test_scope(), &filter).unwrap();
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
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let filter = StatementListFilter {
            subject: Some(subj),
            min_confidence: Some(0.5),
            ..Default::default()
        };
        let out = statement_list(&rtxn, test_scope(), &filter).unwrap();
        assert!(out.is_empty());

        let filter2 = StatementListFilter {
            subject: Some(subj),
            min_confidence: Some(0.2),
            ..Default::default()
        };
        let out2 = statement_list(&rtxn, test_scope(), &filter2).unwrap();
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
                    MemoryId::pack(i as u16, SessionId::DEFAULT.into(), 0),
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
        statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s, 0).unwrap();
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
        let sc = test_scope();
        let evi = rtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE).unwrap();
        for e in &entries {
            assert!(evi
                .get(&(
                    sc.namespace_id,
                    sc.space_id_bytes,
                    e.memory_id.to_be_bytes(),
                    s.id.to_bytes(),
                ))
                .unwrap()
                .is_some());
        }
    }

    // ----- confidence aggregation hookup -----

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
                MemoryId::pack(1, SessionId::DEFAULT.into(), 0),
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
        statement_create(
            &wtxn,
            test_scope(),
            brain_core::SessionId::DEFAULT,
            &s,
            1_700_000_000_000_000_000,
        )
        .unwrap();
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
        // shape — the client decodes EvidenceRefWire::Inline into entries
        // with zero metadata).
        let mut s = fresh_fact(subj, pred, obj);
        s.confidence = 0.42;
        let entry_zero = EvidenceEntry {
            memory_id: MemoryId::pack(1, SessionId::DEFAULT.into(), 0),
            confidence_milli: 0,
            timestamp_unix_nanos: 0,
            extractor_id: brain_core::ExtractorId::from(0),
        };
        let mut sv = SmallVec::<[EvidenceEntry; INLINE_EVIDENCE_CAP]>::new();
        sv.push(entry_zero);
        s.evidence = EvidenceRef::Inline(Box::new(sv));

        let wtxn = db.write_txn().unwrap();
        statement_create(
            &wtxn,
            test_scope(),
            brain_core::SessionId::DEFAULT,
            &s,
            1_700_000_000_000_000_000,
        )
        .unwrap();
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

    #[test]
    fn embed_queue_seed_all_live_reenqueues_live_statements() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "Priya");
        let obj1 = make_entity(&mut db, "Stripe");
        let obj2 = make_entity(&mut db, "Plaid");
        let pred = intern_fact_entity_pred(&mut db, "works_at");
        let f1 = fresh_fact(subj, pred, obj1);
        let f2 = fresh_fact(subj, pred, obj2);

        let (id1, id2) = {
            let wtxn = db.write_txn().unwrap();
            let a = statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &f1, 0)
                .unwrap();
            let b = statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &f2, 0)
                .unwrap();
            wtxn.commit().unwrap();
            (a, b)
        };

        // `statement_create` auto-enqueues; simulate "embedded before a
        // crash" by draining the queue, then assert it's empty.
        {
            let wtxn = db.write_txn().unwrap();
            crate::statement::statement_embed_queue_remove_many(&wtxn, &[id1, id2]).unwrap();
            wtxn.commit().unwrap();
        }
        {
            let rtxn = db.read_txn().unwrap();
            assert_eq!(
                crate::statement::statement_embed_queue_len(&rtxn).unwrap(),
                0
            );
        }

        // Restart rebuild source: re-enqueue every live statement.
        let seeded = {
            let wtxn = db.write_txn().unwrap();
            let n = crate::statement::statement_embed_queue_seed_all_live(&wtxn).unwrap();
            wtxn.commit().unwrap();
            n
        };
        assert_eq!(seeded, 2);

        let rtxn = db.read_txn().unwrap();
        assert_eq!(
            crate::statement::statement_embed_queue_len(&rtxn).unwrap(),
            2
        );
    }

    // -- S1: Event validity invariants (validate_statement_shape) ---------

    #[test]
    fn event_with_valid_from_rejected() {
        let subj = EntityId::new();
        let pred = PredicateId::from(1);
        let mut s = fresh_event(subj, pred, T0 + 10);
        s.valid_from_unix_nanos = Some(T0);
        assert!(
            matches!(
                validate_statement_shape(&s),
                Err(StatementOpError::InvalidArgument(_))
            ),
            "an Event carrying valid_from must be rejected"
        );
    }

    #[test]
    fn event_with_valid_to_rejected() {
        let subj = EntityId::new();
        let pred = PredicateId::from(1);
        let mut s = fresh_event(subj, pred, T0 + 10);
        s.valid_to_unix_nanos = Some(T0 + 20);
        assert!(matches!(
            validate_statement_shape(&s),
            Err(StatementOpError::InvalidArgument(_))
        ));
    }

    #[test]
    fn event_with_only_event_at_accepted() {
        let subj = EntityId::new();
        let pred = PredicateId::from(1);
        let s = fresh_event(subj, pred, T0 + 10);
        assert!(validate_statement_shape(&s).is_ok());
    }

    #[test]
    fn fact_with_validity_accepted() {
        let subj = EntityId::new();
        let obj = EntityId::new();
        let pred = PredicateId::from(1);
        let mut s = fresh_fact(subj, pred, obj);
        s.valid_from_unix_nanos = Some(T0);
        s.valid_to_unix_nanos = Some(T0 + 100);
        assert!(validate_statement_shape(&s).is_ok());
    }

    // -- S5: extracted_at is record time, independent of valid_from -------

    #[test]
    fn create_preserves_arrival_extracted_at_with_historical_valid_from() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let obj = make_entity(&mut db, "manager-role");
        let pred = intern_fact_entity_pred(&mut db, "role");

        // Caller supplies a historical valid_from; extracted_at must be the
        // arrival time passed to statement_create, not the historical date.
        let historical = 1_600_000_000_000_000_000u64;
        let arrival = 1_700_000_000_000_000_500u64;
        let mut s = fresh_fact(subj, pred, obj);
        s.valid_from_unix_nanos = Some(historical);

        let wtxn = db.write_txn().unwrap();
        let id = statement_create(&wtxn, test_scope(), SessionId::DEFAULT, &s, arrival).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let stored = statement_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(stored.valid_from_unix_nanos, Some(historical));
        // extracted_at is set by Statement::new_root from the value the
        // caller passed as `extracted_at_unix_nanos` (fresh_fact uses a
        // fixed base); the point of S5 is the handler always passes arrival
        // there — this asserts the two fields stay independent in storage.
        assert_ne!(stored.extracted_at_unix_nanos, historical);
    }

    // -- S2: contradiction requires overlapping validity -----------------

    #[test]
    fn sequential_facts_do_not_contradict() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let engineer = make_entity(&mut db, "engineer-role");
        let manager = make_entity(&mut db, "manager-role");
        let pred = intern_fact_entity_pred(&mut db, "role");

        // F1 valid until T; F2 valid from T+1 -> non-overlapping.
        let mut f1 = fresh_fact(subj, pred, engineer);
        f1.valid_to_unix_nanos = Some(T0 + 100);
        let mut f2 = fresh_fact(subj, pred, manager);
        f2.valid_from_unix_nanos = Some(T0 + 200);

        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), SessionId::DEFAULT, &f1, T0).unwrap();
        statement_create(&wtxn, test_scope(), SessionId::DEFAULT, &f2, T0).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.write_txn().unwrap();
        let pending =
            crate::statement::contradiction::contradiction_audit_list_pending(&wtxn, 16, T0 + 1)
                .unwrap();
        wtxn.commit().unwrap();
        assert!(
            pending.is_empty(),
            "sequential facts describe different periods, not a contradiction"
        );
    }

    #[test]
    fn overlapping_distinct_facts_contradict() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let engineer = make_entity(&mut db, "engineer-role");
        let manager = make_entity(&mut db, "manager-role");
        let pred = intern_fact_entity_pred(&mut db, "role");

        // Overlapping intervals with distinct objects -> contradiction.
        let mut f1 = fresh_fact(subj, pred, engineer);
        f1.valid_from_unix_nanos = Some(T0);
        f1.valid_to_unix_nanos = Some(T0 + 300);
        let mut f2 = fresh_fact(subj, pred, manager);
        f2.valid_from_unix_nanos = Some(T0 + 100);
        f2.valid_to_unix_nanos = Some(T0 + 400);

        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, test_scope(), SessionId::DEFAULT, &f1, T0).unwrap();
        statement_create(&wtxn, test_scope(), SessionId::DEFAULT, &f2, T0).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.write_txn().unwrap();
        let pending =
            crate::statement::contradiction::contradiction_audit_list_pending(&wtxn, 16, T0 + 1)
                .unwrap();
        wtxn.commit().unwrap();
        assert_eq!(pending.len(), 1, "overlapping distinct facts contradict");
    }
}
