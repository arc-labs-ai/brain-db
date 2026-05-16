//! Statement wire-op handlers — `STATEMENT_CREATE` / `_GET` /
//! `_SUPERSEDE` / `_TOMBSTONE` / `_RETRACT` / `_HISTORY` / `_LIST`
//! (spec §28/06, phase 17.7).
//!
//! Each handler:
//!
//! 1. Validates the request at wire layer (caps, kind invariants,
//!    blob sizes).
//! 2. Acquires the per-shard `MetadataDb` lock.
//! 3. Resolves predicate qname → `PredicateId` via
//!    `brain_metadata::predicate_lookup_by_qname`.
//! 4. Projects wire → brain-core `Statement` (CREATE / SUPERSEDE).
//! 5. Opens a redb txn (read for GET / HISTORY / LIST, write
//!    otherwise) and calls into `brain_metadata::statement_ops::*`.
//! 6. Commits writes.
//! 7. Emits a post-commit subscription event (CREATE / SUPERSEDE /
//!    TOMBSTONE) per spec §28/02 §3.2.
//! 8. Projects storage `Statement` → wire `StatementView`.
//!
//! Phase 17.7 handlers do **not** touch the statement HNSW (17.5);
//! the embedding worker that populates it lives in phase 21.

use brain_core::{EntityId, PredicateId, StatementId, StatementKind};
use brain_core::knowledge::{Statement, TombstoneReason};
use brain_metadata::predicate_ops::{predicate_get, predicate_lookup_by_qname, PredicateOpError};
use redb::ReadableTable;
use brain_metadata::statement_ops::{
    evidence_overflow_load, statement_create, statement_get, statement_history, statement_list,
    statement_retract, statement_supersede, statement_tombstone, StatementListFilter,
    StatementOpError,
};
use brain_protocol::knowledge::{
    statement_kind_from_wire, StatementCreateRequest, StatementCreateResponse,
    StatementCreatedEvent, StatementGetRequest, StatementGetResponse,
    StatementHistoryRequest, StatementHistoryResponseFrame, StatementListRequest,
    StatementListResponseFrame, StatementRetractRequest, StatementRetractResponse,
    StatementSupersedeRequest, StatementSupersedeResponse, StatementSupersededEvent,
    StatementTombstoneRequest, StatementTombstoneResponse, StatementTombstonedEvent,
    StatementView, KnowledgeEventPayload,
};
use brain_protocol::response::EventType;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::ops::knowledge_entity::emit_knowledge_event;

// 30 days, per spec §19. Used by STATEMENT_RETRACT for the
// will_zero_at hint.
const RETRACT_GRACE_NANOS: u64 = 30 * 24 * 60 * 60 * 1_000_000_000;
const REASON_MESSAGE_MAX: usize = 4096;
const PREDICATE_QNAME_MAX: usize = 96;
const LIST_LIMIT_MAX: u32 = 1000;

// ---------------------------------------------------------------------------
// STATEMENT_CREATE
// ---------------------------------------------------------------------------

pub async fn handle_statement_create(
    req: StatementCreateRequest,
    ctx: &OpsContext,
) -> Result<StatementCreateResponse, OpError> {
    validate_predicate_qname(&req.predicate)?;
    if req.confidence.is_nan() || !(0.0..=1.0).contains(&req.confidence) {
        return Err(OpError::InvalidRequest(
            "confidence must be in [0, 1] and not NaN".into(),
        ));
    }
    let kind = statement_kind_from_wire(req.kind);

    let now = crate::txn::now_unix_nanos_pub();
    let (namespace, name) = split_qname(&req.predicate)?;

    let (created_id, chain_root, auto_superseded) = {
        let mut db_guard = ctx.executor.metadata.lock();
        let wtxn = db_guard
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;

        let predicate = predicate_lookup_by_qname_wtxn(&wtxn, namespace, name)?
            .ok_or_else(|| {
                OpError::InvalidRequest(format!(
                    "unknown predicate {namespace:?}:{name:?}; declare it via SCHEMA_UPLOAD first"
                ))
            })?;

        // Find a prior current Preference (informational — statement_create
        // will auto-supersede inside the same txn).
        let auto_superseded_id: Option<StatementId> = if kind == StatementKind::Preference {
            use brain_metadata::tables::knowledge::statement::STATEMENTS_BY_SUBJECT_TABLE;
            let bys = wtxn
                .open_table(STATEMENTS_BY_SUBJECT_TABLE)
                .map_err(|e| OpError::Internal(format!("open_table: {e}")))?;
            let key = (
                req.subject,
                StatementKind::Preference.as_u8(),
                predicate.id.raw(),
                1u8,
            );
            let raw: Option<[u8; 16]> = bys
                .get(&key)
                .map_err(|e| OpError::Internal(format!("by_subject lookup: {e}")))?
                .map(|g| g.value());
            raw.map(StatementId::from)
        } else {
            None
        };

        let statement_value = build_statement_from_create(&req, predicate.id, now, kind)?;
        let created = statement_create(&wtxn, &statement_value, now)
            .map_err(map_statement_op_error)?;
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;

        // For auto-superseded Preferences, `statement_create` delegates
        // to `statement_supersede` and returns the *new* id; the prior
        // current row is what we recorded above.
        let chain_root_id = if auto_superseded_id.is_some() {
            // Inherit chain_root from old (or old.id if old was the root).
            // Read it back from storage.
            let rtxn = db_guard
                .read_txn()
                .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
            let new = statement_get(&rtxn, created)
                .map_err(map_statement_op_error)?
                .ok_or_else(|| OpError::Internal("created statement missing post-commit".into()))?;
            new.chain_root
        } else {
            created
        };
        (created, chain_root_id, auto_superseded_id)
    };

    // Emit STATEMENT_CREATED event.
    emit_knowledge_event(
        ctx,
        EventType::StatementCreated,
        KnowledgeEventPayload::StatementCreated(StatementCreatedEvent {
            statement_id: created_id.to_bytes(),
            kind: req.kind as u8,
            subject: req.subject,
            predicate: req.predicate.clone(),
            confidence: req.confidence,
        }),
        now,
    );

    // If a Preference was auto-superseded, also emit STATEMENT_SUPERSEDED.
    if let Some(old) = auto_superseded {
        emit_knowledge_event(
            ctx,
            EventType::StatementSuperseded,
            KnowledgeEventPayload::StatementSuperseded(StatementSupersededEvent {
                old_statement_id: old.to_bytes(),
                new_statement_id: created_id.to_bytes(),
                chain_root: chain_root.to_bytes(),
            }),
            now,
        );
    }

    Ok(StatementCreateResponse {
        statement_id: created_id.to_bytes(),
        auto_superseded: auto_superseded.map(StatementId::to_bytes).unwrap_or([0u8; 16]),
        chain_root: chain_root.to_bytes(),
    })
}

// ---------------------------------------------------------------------------
// STATEMENT_GET
// ---------------------------------------------------------------------------

pub async fn handle_statement_get(
    req: StatementGetRequest,
    ctx: &OpsContext,
) -> Result<StatementGetResponse, OpError> {
    let id = StatementId::from(req.statement_id);

    let db_guard = ctx.executor.metadata.lock();
    let rtxn = db_guard
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    let mut current = statement_get(&rtxn, id)
        .map_err(map_statement_op_error)?
        .ok_or_else(|| OpError::NotFound {
            what: "statement",
            detail: format!("{id:?}"),
        })?;

    let mut returned_via_supersession = false;
    if req.follow_supersession {
        while let Some(succ) = current.superseded_by {
            returned_via_supersession = true;
            current = statement_get(&rtxn, succ)
                .map_err(map_statement_op_error)?
                .ok_or_else(|| {
                    OpError::Internal(format!("chain dangling at {succ:?} from {id:?}"))
                })?;
        }
    }

    let view = project_view(&rtxn, &current)?;
    Ok(StatementGetResponse {
        statement: view,
        returned_via_supersession,
    })
}

// ---------------------------------------------------------------------------
// STATEMENT_SUPERSEDE
// ---------------------------------------------------------------------------

pub async fn handle_statement_supersede(
    req: StatementSupersedeRequest,
    ctx: &OpsContext,
) -> Result<StatementSupersedeResponse, OpError> {
    validate_predicate_qname(&req.new_statement.predicate)?;
    if req.new_statement.confidence.is_nan()
        || !(0.0..=1.0).contains(&req.new_statement.confidence)
    {
        return Err(OpError::InvalidRequest(
            "confidence must be in [0, 1] and not NaN".into(),
        ));
    }
    let kind = statement_kind_from_wire(req.new_statement.kind);

    let old_id = StatementId::from(req.old_statement_id);
    let now = crate::txn::now_unix_nanos_pub();
    let (namespace, name) = split_qname(&req.new_statement.predicate)?;

    let (new_id, chain_root, version) = {
        let mut db_guard = ctx.executor.metadata.lock();
        let wtxn = db_guard
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;

        let predicate = predicate_lookup_by_qname_wtxn(&wtxn, namespace, name)?
            .ok_or_else(|| {
                OpError::InvalidRequest(format!(
                    "unknown predicate {namespace:?}:{name:?}"
                ))
            })?;

        let new_statement =
            build_statement_from_create(&req.new_statement, predicate.id, now, kind)?;
        let new_id = statement_supersede(&wtxn, old_id, &new_statement, now)
            .map_err(map_statement_op_error)?;
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;

        let rtxn = db_guard
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let new = statement_get(&rtxn, new_id)
            .map_err(map_statement_op_error)?
            .ok_or_else(|| OpError::Internal("new statement missing post-supersede".into()))?;
        (new_id, new.chain_root, new.version)
    };

    emit_knowledge_event(
        ctx,
        EventType::StatementSuperseded,
        KnowledgeEventPayload::StatementSuperseded(StatementSupersededEvent {
            old_statement_id: old_id.to_bytes(),
            new_statement_id: new_id.to_bytes(),
            chain_root: chain_root.to_bytes(),
        }),
        now,
    );

    Ok(StatementSupersedeResponse {
        new_statement_id: new_id.to_bytes(),
        chain_root: chain_root.to_bytes(),
        version,
    })
}

// ---------------------------------------------------------------------------
// STATEMENT_TOMBSTONE
// ---------------------------------------------------------------------------

pub async fn handle_statement_tombstone(
    req: StatementTombstoneRequest,
    ctx: &OpsContext,
) -> Result<StatementTombstoneResponse, OpError> {
    if req.reason_message.len() > REASON_MESSAGE_MAX {
        return Err(OpError::InvalidRequest("reason_message exceeds 4 KiB".into()));
    }
    let reason = decode_tombstone_reason(req.reason)?;
    let id = StatementId::from(req.statement_id);
    let now = crate::txn::now_unix_nanos_pub();

    {
        let mut db_guard = ctx.executor.metadata.lock();
        let wtxn = db_guard
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;
        statement_tombstone(&wtxn, id, reason, now).map_err(map_statement_op_error)?;
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;
    }

    emit_knowledge_event(
        ctx,
        EventType::StatementTombstoned,
        KnowledgeEventPayload::StatementTombstoned(StatementTombstonedEvent {
            statement_id: id.to_bytes(),
            reason: req.reason_message,
        }),
        now,
    );

    Ok(StatementTombstoneResponse {
        tombstoned_at_unix_nanos: now,
    })
}

// ---------------------------------------------------------------------------
// STATEMENT_RETRACT
// ---------------------------------------------------------------------------

pub async fn handle_statement_retract(
    req: StatementRetractRequest,
    ctx: &OpsContext,
) -> Result<StatementRetractResponse, OpError> {
    if req.reason_message.len() > REASON_MESSAGE_MAX {
        return Err(OpError::InvalidRequest("reason_message exceeds 4 KiB".into()));
    }
    let reason = decode_tombstone_reason(req.reason)?;
    let id = StatementId::from(req.statement_id);
    let now = crate::txn::now_unix_nanos_pub();

    {
        let mut db_guard = ctx.executor.metadata.lock();
        let wtxn = db_guard
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;
        statement_retract(&wtxn, id, reason, now).map_err(map_statement_op_error)?;
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;
    }

    // Retract emits StatementTombstoned in v1 (no discrete retract
    // event in v1.0; phase 22 may add one).
    emit_knowledge_event(
        ctx,
        EventType::StatementTombstoned,
        KnowledgeEventPayload::StatementTombstoned(StatementTombstonedEvent {
            statement_id: id.to_bytes(),
            reason: format!("retract: {}", req.reason_message),
        }),
        now,
    );

    Ok(StatementRetractResponse {
        retracted_at_unix_nanos: now,
        will_zero_at_unix_nanos: now.saturating_add(RETRACT_GRACE_NANOS),
    })
}

// ---------------------------------------------------------------------------
// STATEMENT_HISTORY
// ---------------------------------------------------------------------------

pub async fn handle_statement_history(
    req: StatementHistoryRequest,
    ctx: &OpsContext,
) -> Result<StatementHistoryResponseFrame, OpError> {
    let anchor = StatementId::from(req.anchor_id);

    let (items_storage, chain_root) = {
        let db_guard = ctx.executor.metadata.lock();
        let rtxn = db_guard
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let chain = statement_history(&rtxn, anchor).map_err(map_statement_op_error)?;
        if chain.is_empty() {
            return Err(OpError::NotFound {
                what: "statement",
                detail: format!("{anchor:?}"),
            });
        }
        let root = chain[0].chain_root;
        let mut items = Vec::with_capacity(chain.len());
        for s in chain {
            if !req.include_tombstoned && s.tombstoned {
                continue;
            }
            let view = project_view(&rtxn, &s)?;
            items.push(view);
        }
        (items, root)
    };

    Ok(StatementHistoryResponseFrame {
        total_versions: items_storage.len() as u32,
        items: items_storage,
        chain_root: chain_root.to_bytes(),
        is_final: true,
    })
}

// ---------------------------------------------------------------------------
// STATEMENT_LIST
// ---------------------------------------------------------------------------

pub async fn handle_statement_list(
    req: StatementListRequest,
    ctx: &OpsContext,
) -> Result<StatementListResponseFrame, OpError> {
    if req.limit == 0 || req.limit > LIST_LIMIT_MAX {
        return Err(OpError::InvalidRequest(
            "limit must be in 1..=1000".into(),
        ));
    }
    if !req.cursor.is_empty() {
        return Err(OpError::InvalidRequest(
            "STATEMENT_LIST cursor pagination lands in phase 23".into(),
        ));
    }
    let kind = match req.kind {
        0 => None,
        1 => Some(StatementKind::Fact),
        2 => Some(StatementKind::Preference),
        3 => Some(StatementKind::Event),
        other => {
            return Err(OpError::InvalidRequest(format!(
                "unknown kind byte {other}; expected 0..=3"
            )))
        }
    };
    let subject = if req.subject == [0u8; 16] {
        None
    } else {
        Some(EntityId::from(req.subject))
    };

    let (items_storage, count) = {
        let db_guard = ctx.executor.metadata.lock();
        let rtxn = db_guard
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

        // Resolve optional predicate qname → PredicateId.
        let predicate = if req.predicate.is_empty() {
            None
        } else {
            validate_predicate_qname(&req.predicate)?;
            let (ns, name) = split_qname(&req.predicate)?;
            let p = predicate_lookup_by_qname(&rtxn, ns, name)
                .map_err(map_predicate_op_error)?
                .ok_or_else(|| {
                    OpError::InvalidRequest(format!("unknown predicate {ns:?}:{name:?}"))
                })?;
            Some(p.id)
        };

        let filter = StatementListFilter {
            subject,
            predicate,
            kind,
            current_only: req.only_current,
            min_confidence: if req.min_confidence > 0.0 {
                Some(req.min_confidence)
            } else {
                None
            },
            limit: req.limit as usize,
        };
        let mut rows = statement_list(&rtxn, &filter).map_err(map_statement_op_error)?;

        // Wire-level filters not pushed into statement_list.
        if !req.include_tombstoned {
            rows.retain(|s| !s.tombstoned);
        }
        if req.time_range_start_unix_nanos != 0 || req.time_range_end_unix_nanos != 0 {
            let lo = req.time_range_start_unix_nanos;
            let hi = if req.time_range_end_unix_nanos == 0 {
                u64::MAX
            } else {
                req.time_range_end_unix_nanos
            };
            rows.retain(|s| match s.kind {
                StatementKind::Event => s
                    .event_at_unix_nanos
                    .map(|t| t >= lo && t <= hi)
                    .unwrap_or(false),
                _ => {
                    let from = s.valid_from_unix_nanos.unwrap_or(0);
                    let to = s.valid_to_unix_nanos.unwrap_or(u64::MAX);
                    from <= hi && to >= lo
                }
            });
        }

        let mut out = Vec::with_capacity(rows.len());
        for s in &rows {
            out.push(project_view(&rtxn, s)?);
        }
        let count = out.len() as u32;
        (out, count)
    };

    Ok(StatementListResponseFrame {
        items: items_storage,
        next_cursor: Vec::new(),
        cumulative_count: count,
        is_final: true,
    })
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn validate_predicate_qname(q: &str) -> Result<(), OpError> {
    if q.is_empty() {
        return Err(OpError::InvalidRequest("predicate must be non-empty".into()));
    }
    if q.len() > PREDICATE_QNAME_MAX {
        return Err(OpError::InvalidRequest(format!(
            "predicate qname exceeds {PREDICATE_QNAME_MAX} chars"
        )));
    }
    if !q.contains(':') {
        return Err(OpError::InvalidRequest(
            "predicate must use \"namespace:name\" form".into(),
        ));
    }
    Ok(())
}

fn split_qname(q: &str) -> Result<(&str, &str), OpError> {
    let (ns, name) = q
        .split_once(':')
        .ok_or_else(|| OpError::InvalidRequest("predicate missing ':' separator".into()))?;
    Ok((ns, name))
}

fn decode_tombstone_reason(byte: u8) -> Result<TombstoneReason, OpError> {
    TombstoneReason::from_u8(byte).ok_or_else(|| {
        OpError::InvalidRequest(format!(
            "unknown tombstone_reason byte {byte}; expected 1..=4"
        ))
    })
}

/// Build a brain-core `Statement` from a wire `StatementCreateRequest`
/// + the resolved `PredicateId`. Performs per-kind invariant checks
/// (Event requires event_at; Fact/Preference must not set event_at).
fn build_statement_from_create(
    req: &StatementCreateRequest,
    predicate: PredicateId,
    now: u64,
    kind: StatementKind,
) -> Result<Statement, OpError> {
    use brain_protocol::knowledge::{evidence_ref_from_wire, statement_object_from_wire};

    match kind {
        StatementKind::Event => {
            if req.event_at_unix_nanos == 0 {
                return Err(OpError::InvalidRequest(
                    "Event kind requires non-zero event_at_unix_nanos".into(),
                ));
            }
        }
        _ => {
            if req.event_at_unix_nanos != 0 {
                return Err(OpError::InvalidRequest(
                    "only Event kind may set event_at_unix_nanos".into(),
                ));
            }
        }
    }

    let evidence = evidence_ref_from_wire(&req.evidence).map_err(|e| match e {
        brain_protocol::knowledge::WireToStatementError::EvidenceInlineTooLarge { len, cap } => {
            OpError::InvalidRequest(format!(
                "inline evidence list exceeds cap of {cap}; got {len}"
            ))
        }
        other => OpError::InvalidRequest(format!("evidence decode: {other}")),
    })?;

    let object = statement_object_from_wire(&req.object);
    let subject = brain_core::knowledge::SubjectRef::Entity(EntityId::from(req.subject));

    let id = StatementId::new();
    let mut s = Statement::new_root(
        id,
        kind,
        subject,
        predicate,
        object,
        req.confidence,
        evidence,
        brain_core::ExtractorId::from(req.extractor_id),
        if req.valid_from_unix_nanos != 0 {
            req.valid_from_unix_nanos
        } else {
            now
        },
        if req.schema_version == 0 {
            1
        } else {
            req.schema_version
        },
    );
    // `new_root` uses `extracted_at_unix_nanos` for both extracted_at
    // and (implicitly) the chain start; expose explicit valid_from /
    // valid_to here.
    if req.valid_from_unix_nanos != 0 {
        s.valid_from_unix_nanos = Some(req.valid_from_unix_nanos);
    }
    if req.valid_to_unix_nanos != 0 {
        s.valid_to_unix_nanos = Some(req.valid_to_unix_nanos);
    }
    if req.event_at_unix_nanos != 0 {
        s.event_at_unix_nanos = Some(req.event_at_unix_nanos);
    }
    Ok(s)
}

/// Project a storage `Statement` to a wire `StatementView` by
/// resolving the `PredicateId` to its `"namespace:name"` canonical
/// string. Inline-evidence overflow is resolved to inline form when
/// possible (single-shot read).
fn project_view(
    rtxn: &redb::ReadTransaction,
    s: &Statement,
) -> Result<StatementView, OpError> {
    let predicate = predicate_get(rtxn, s.predicate)
        .map_err(map_predicate_op_error)?
        .ok_or_else(|| {
            OpError::Internal(format!(
                "statement {:?} references missing predicate {:?}",
                s.id, s.predicate
            ))
        })?;
    let qname = predicate.canonical();

    // If evidence is overflow, resolve to inline form so consumers
    // don't need a second op to read the memory ids. Phase 22's
    // STATEMENT_ADD_EVIDENCE will let callers fetch the per-entry
    // metadata separately if needed.
    let mut s = s.clone();
    if let brain_core::knowledge::EvidenceRef::Overflow(id) = s.evidence {
        let entries = evidence_overflow_load(rtxn, id)
            .map_err(map_statement_op_error)?
            .ok_or_else(|| {
                OpError::Internal(format!(
                    "statement {:?} references missing overflow row {:?}",
                    s.id, id
                ))
            })?;
        let mut sv = smallvec::SmallVec::<
            [brain_core::knowledge::EvidenceEntry; brain_core::knowledge::INLINE_EVIDENCE_CAP],
        >::new();
        for e in entries.into_iter().take(brain_core::knowledge::INLINE_EVIDENCE_CAP) {
            sv.push(e);
        }
        s.evidence = brain_core::knowledge::EvidenceRef::Inline(sv);
    }

    Ok(StatementView::from_statement(&s, qname))
}

fn map_predicate_op_error(err: PredicateOpError) -> OpError {
    match err {
        PredicateOpError::InvalidIdentifier { reason } => {
            OpError::InvalidRequest(format!("predicate identifier: {reason}"))
        }
        PredicateOpError::AlreadyExists { qname, existing_id } => OpError::Conflict(format!(
            "predicate {qname:?} already exists with id {existing_id:?}"
        )),
        PredicateOpError::Storage(e) => OpError::Internal(format!("redb storage: {e}")),
        PredicateOpError::Table(e) => OpError::Internal(format!("redb table: {e}")),
    }
}

/// Look up a predicate by qname using a write transaction. Mirrors
/// `predicate_lookup_by_qname` (which takes a `ReadTransaction`) but
/// works against an open `WriteTransaction` so handlers can probe
/// inside the same txn that runs the write.
fn predicate_lookup_by_qname_wtxn(
    wtxn: &redb::WriteTransaction,
    namespace: &str,
    name: &str,
) -> Result<Option<brain_core::knowledge::Predicate>, OpError> {
    use brain_metadata::tables::knowledge::predicate::{
        PredicateDefinition, PREDICATES_BY_QNAME_TABLE, PREDICATES_TABLE,
    };
    let q = format!("{namespace}:{name}");
    let idx = wtxn
        .open_table(PREDICATES_BY_QNAME_TABLE)
        .map_err(|e| OpError::Internal(format!("open by_qname: {e}")))?;
    let id_raw: Option<u32> = idx
        .get(q.as_str())
        .map_err(|e| OpError::Internal(format!("by_qname lookup: {e}")))?
        .map(|g| g.value());
    let Some(id_raw) = id_raw else {
        return Ok(None);
    };
    let t = wtxn
        .open_table(PREDICATES_TABLE)
        .map_err(|e| OpError::Internal(format!("open predicates: {e}")))?;
    let row: Option<PredicateDefinition> = t
        .get(&id_raw)
        .map_err(|e| OpError::Internal(format!("predicates lookup: {e}")))?
        .map(|g| g.value());
    Ok(row.as_ref().map(PredicateDefinition::to_predicate))
}

fn map_statement_op_error(err: StatementOpError) -> OpError {
    match err {
        StatementOpError::NotFound(id) => OpError::NotFound {
            what: "statement",
            detail: format!("{id:?}"),
        },
        StatementOpError::AlreadyExists(id) => {
            OpError::Conflict(format!("statement {id:?} already exists"))
        }
        StatementOpError::UnknownPredicate(p) => OpError::NotFound {
            what: "predicate",
            detail: format!("id={p}"),
        },
        StatementOpError::UnknownSubject(id) => OpError::NotFound {
            what: "subject entity",
            detail: format!("{id:?}"),
        },
        StatementOpError::InvalidArgument(s) => OpError::InvalidRequest(s.to_string()),
        StatementOpError::AlreadySuperseded(id, by) => OpError::Conflict(format!(
            "statement {id:?} already superseded by {by:?}"
        )),
        StatementOpError::AlreadyTombstoned(id) => {
            OpError::Conflict(format!("statement {id:?} is tombstoned"))
        }
        StatementOpError::EventCannotSupersede => {
            OpError::Conflict("events cannot be superseded".into())
        }
        StatementOpError::KindMismatch { old, new } => OpError::InvalidRequest(format!(
            "kind mismatch on supersede: old={old:?} new={new:?}"
        )),
        StatementOpError::SubjectMismatch => {
            OpError::InvalidRequest("subject must match on supersede".into())
        }
        StatementOpError::PredicateMismatch => {
            OpError::InvalidRequest("predicate must match on supersede".into())
        }
        StatementOpError::DecodeFailed => {
            OpError::Internal("statement row decode failed — possible corruption".into())
        }
        StatementOpError::Storage(e) => OpError::Internal(format!("redb storage: {e}")),
        StatementOpError::Table(e) => OpError::Internal(format!("redb table: {e}")),
        StatementOpError::PredicateOp(e) => map_predicate_op_error(e),
        StatementOpError::EntityOp(e) => {
            OpError::Internal(format!("entity op forwarded from statement_ops: {e}"))
        }
    }
}
