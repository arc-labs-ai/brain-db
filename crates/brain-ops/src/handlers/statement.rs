//! Statement wire-op handlers — `STATEMENT_CREATE` / `_GET` /
//! `_SUPERSEDE` / `_TOMBSTONE` / `_RETRACT` / `_HISTORY` / `_LIST`.
//!
//! Write handlers go through the unified writer's `submit(Write)` path:
//! pre-submit reads validate inputs against the active schema (rtxn-
//! only); for schemaless writes the predicate intern is carried on the
//! `Phase::UpsertStatement.predicate_intern_hint` and runs inside the
//! main submit wtxn (no separate fsync); a post-submit rtxn recovers
//! the wire-shape fields (chain_root, version) that the `WriteAck`
//! doesn't surface.
//!
//! Read handlers (GET / HISTORY / LIST) stay direct-rtxn.
//!
//! Subscription events (CREATE / SUPERSEDE / TOMBSTONE) and the
//! statement text-indexer dispatch remain on the handler path; a
//! later slice unifies them with the writer's post-commit fan-out.
//!
//! These handlers do **not** touch the statement HNSW; the embedding
//! worker that populates it lives elsewhere.

use crate::clock::now_unix_nanos;
use crate::context::OpsContext;
use crate::error::{caller_tombstone_reason, OpError};
use crate::handlers::events::emit_graph_event;
use crate::handlers::limits::{
    check_byte_len, QNAME_MAX_BYTES, REASON_MESSAGE_MAX_BYTES, STATEMENT_VALUE_MAX_BYTES,
};
use crate::handlers::writer_handle::real_writer_handle;
use crate::index::text_indexer::StatementTextOp;
use crate::metrics::read_degrade::{global as read_degrade_metrics, ReadDegradeReason};
use crate::write::{
    EvidenceRefPhase, Phase, PhaseAck, SupersedeReplacement, SupersedeReplacementId,
    SupersedeTarget, TombstoneTarget, Write, WriteId,
};
use brain_core::{EntityId, PredicateId, RequestId, StatementId, StatementKind};
use brain_core::{EvidenceEntry, Statement, TombstoneReason};
use brain_metadata::schema::predicate::{
    predicate_intern_or_get, predicate_lookup_by_qname, predicates_active_for_schema,
};
use brain_metadata::schema::qnames::PredicateQnames;
use brain_metadata::schema::store::schema_active;
use brain_metadata::statement::{
    evidence_overflow_load, statement_get, statement_history_page, statement_list_page,
    StatementListCursor, StatementListFilter, StatementPageExtra,
};
use brain_metadata::DeclScope;
use brain_planner::WriterError;
use brain_protocol::envelope::response::EventType;
use brain_protocol::{
    statement_kind_from_wire, GraphEventPayload, StatementCreateRequest, StatementCreateResponse,
    StatementCreatedEvent, StatementGetRequest, StatementGetResponse, StatementHistoryRequest,
    StatementHistoryResponseFrame, StatementListRequest, StatementListResponseFrame,
    StatementRetractRequest, StatementRetractResponse, StatementSupersedeRequest,
    StatementSupersedeResponse, StatementSupersededEvent, StatementTombstoneRequest,
    StatementTombstoneResponse, StatementTombstonedEvent, StatementView,
};

// 30 days. Used by STATEMENT_RETRACT for the
// will_zero_at hint.
const RETRACT_GRACE_NANOS: u64 = 30 * 24 * 60 * 60 * 1_000_000_000;
const LIST_LIMIT_MAX: u32 = 1000;

/// Whether statement `id`'s row belongs to the caller's `(namespace,
/// space)` scope. `statement_get` returns a brain-core `Statement` with
/// the scope dropped, so the tenant wall is enforced here by re-reading
/// the row's `namespace_id` / `space_id_bytes` from `STATEMENTS_TABLE`.
/// Fail-closed: a missing row or read error denies.
fn statement_id_in_caller_scope(ctx: &OpsContext, id: StatementId) -> bool {
    use brain_metadata::tables::statement::{StatementMetadata, STATEMENTS_TABLE};
    let Ok(rtxn) = ctx.executor.metadata.read_txn() else {
        return false;
    };
    let Ok(t) = rtxn.open_table(STATEMENTS_TABLE) else {
        return false;
    };
    let row: Option<StatementMetadata> = t.get(&id.to_bytes()).ok().flatten().map(|g| g.value());
    match row {
        Some(m) => {
            m.namespace_id == ctx.executor.caller_namespace.raw()
                && m.space_id_bytes == <[u8; 16]>::from(ctx.executor.caller_space)
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// STATEMENT_CREATE
// ---------------------------------------------------------------------------

pub async fn handle_statement_create(
    req: StatementCreateRequest,
    ctx: &OpsContext,
) -> Result<StatementCreateResponse, OpError> {
    validate_statement_create_request(&req)?;
    let kind = statement_kind_from_wire(req.kind);

    let now = now_unix_nanos();
    let (namespace, name) = split_qname(&req.predicate)?;

    // Pre-submit validation in rtxn. Schema vocabulary check and
    // existing-predicate lookup. We do NOT pre-read the prior current
    // Preference here: that lookup is repeated each call and would
    // drift across replays. Instead the post-submit reconstruction
    // recovers `auto_superseded` from the committed row's `supersedes`
    // field, which is stable.
    //
    // For schemaless mode with a missing predicate, we don't run a
    // separate intern wtxn — the Phase carries the qname as an intern
    // hint and apply does the work inside the main submit wtxn,
    // collapsing three separate commits (intern, statement, flag
    // stamp) into one.
    // Vocabulary resolves in the scope the caller writes into: the space's
    // own declarations when it has them, otherwise the namespace's. Asking
    // the namespace base instead would reject a predicate the caller's own
    // space declares.
    let caller_space: [u8; 16] = ctx.executor.caller_space.into();
    let (predicate_id_opt, schemaless) = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let active_version = schema_active(&rtxn, DeclScope::space(namespace, caller_space))
            .map_err(|e| OpError::Internal(format!("schema_active: {e}")))?;
        let predicate_id_opt: Option<PredicateId> = match active_version {
            Some(version) => {
                let pred = predicate_lookup_by_qname(
                    &rtxn,
                    DeclScope::space(namespace, caller_space),
                    name,
                )
                .map_err(OpError::from)?
                .ok_or_else(|| OpError::PredicateNotInSchema {
                    predicate: req.predicate.clone(),
                    namespace: namespace.to_string(),
                    version,
                })?;
                let active = predicates_active_for_schema(
                    &rtxn,
                    DeclScope::space(namespace, caller_space),
                    version,
                )
                .map_err(OpError::from)?;
                if !active.contains(&pred.id) {
                    return Err(OpError::PredicateNotInSchema {
                        predicate: req.predicate.clone(),
                        namespace: namespace.to_string(),
                        version,
                    });
                }
                Some(pred.id)
            }
            None => {
                predicate_lookup_by_qname(&rtxn, DeclScope::space(namespace, caller_space), name)
                    .map_err(OpError::from)?
                    .map(|p| p.id)
            }
        };
        let schemaless = active_version.is_none();
        (predicate_id_opt, schemaless)
    };

    // Resolve to either an existing PredicateId (strict mode, or
    // schemaless where a prior write already interned the qname) or an
    // intern hint that apply will run inside the main wtxn.
    let (predicate_id, intern_hint) = match predicate_id_opt {
        Some(pid) => (pid, None),
        None => {
            if !schemaless {
                return Err(OpError::Internal(
                    "predicate resolution inconsistency: strict-mode none after vocab check".into(),
                ));
            }
            // Sentinel predicate id; apply will replace it with the
            // result of `predicate_intern_or_get` against the hint.
            (
                PredicateId::from(0u32),
                Some((namespace.to_string(), name.to_string())),
            )
        }
    };

    // Submit the UpsertStatement phase through the unified writer. The
    // apply function runs (optional) predicate intern + statement_create
    // + (optional) IMPLICIT_PREDICATE flag stamp in one wtxn.
    let real_writer = real_writer_handle(ctx)?;
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_statement_create_request(&req);

    let statement_value = build_statement_from_create(&req, predicate_id, now, kind)?;
    let phase = build_upsert_statement_phase(
        &statement_value,
        intern_hint,
        brain_core::SessionId::from(req.session_id),
    );
    let write = Write::single(write_id, ctx.executor.caller_space, phase)
        .with_namespace(ctx.executor.caller_namespace)
        .with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    // On a replay-hit the cached `WriteAck` is returned; the id we
    // recover from it is the one the *original* call wrote (which may
    // differ from `statement_value.id` because that's freshly minted
    // each call). Reading downstream state by the ack's id is what
    // makes replay safe. The original predicate intern also stays
    // stable across replays — apply doesn't re-run on a cache hit, so
    // even if a later schema declared the same qname differently, the
    // replay surfaces the originally-stored PredicateId.
    let created_id = match ack.single_phase() {
        PhaseAck::UpsertedStatement(id, _) => *id,
        other => {
            return Err(OpError::PermanentFault(format!(
                "unexpected phase ack for STATEMENT_CREATE: {other:?}"
            )))
        }
    };

    // Recover chain_root + auto_superseded from storage. The
    // committed row's `supersedes` field carries the prior current
    // Preference's id (when statement_create delegated to supersede)
    // or `None` (fresh row). `chain_root` is self-referential for a
    // fresh root; the supersede path inherits the prior chain's root.
    let (chain_root, auto_superseded) = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let new = statement_get(&rtxn, created_id)
            .map_err(OpError::from)?
            .ok_or_else(|| {
                OpError::PermanentFault("created statement missing post-commit".into())
            })?;
        (new.chain_root, new.supersedes)
    };

    // Emit STATEMENT_CREATED event.
    emit_graph_event(
        ctx,
        EventType::StatementCreated,
        GraphEventPayload::StatementCreated(StatementCreatedEvent {
            statement_id: created_id.to_bytes(),
            kind: req.kind.as_storage_byte(),
            subject: req.subject,
            predicate: req.predicate.clone(),
            confidence: req.confidence,
        }),
        now,
    )
    .await;

    // If a Preference was auto-superseded, also emit STATEMENT_SUPERSEDED.
    if let Some(old) = auto_superseded {
        emit_graph_event(
            ctx,
            EventType::StatementSuperseded,
            GraphEventPayload::StatementSuperseded(StatementSupersededEvent {
                old_statement_id: old.to_bytes(),
                new_statement_id: created_id.to_bytes(),
                chain_root: chain_root.to_bytes(),
            }),
            now,
        )
        .await;
    }

    // Statement text indexer dispatch.
    // For auto-superseded Preferences we emit a Delete for the
    // old id before the Upsert for the new one (mirroring the
    // supersede = Delete + Upsert pattern).
    if let Some(dispatcher) = ctx.statement_text_dispatcher.as_ref() {
        if let Some(old_id) = auto_superseded {
            dispatcher
                .dispatch(StatementTextOp::Delete { id: old_id })
                .await;
        }
        dispatch_upsert_for(ctx, created_id, dispatcher).await;
    }

    Ok(StatementCreateResponse {
        statement_id: created_id.to_bytes(),
        auto_superseded: auto_superseded
            .map(StatementId::to_bytes)
            .unwrap_or([0u8; 16]),
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

    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    let current = statement_get(&rtxn, id)
        .map_err(OpError::from)?
        .ok_or_else(|| OpError::NotFound {
            what: "statement",
            detail: format!("{id:?}"),
        })?;

    // Tenant wall (unconditional): a statement named by a foreign
    // `(namespace, space)`'s id reads as NotFound, indistinguishable
    // from a genuinely absent row.
    if !statement_id_in_caller_scope(ctx, id) {
        return Err(OpError::NotFound {
            what: "statement",
            detail: format!("{id:?}"),
        });
    }

    let (current, returned_via_supersession) = if req.follow_supersession {
        resolve_supersession_head(&rtxn, id, current)?
    } else {
        (current, false)
    };

    let view = project_view(&rtxn, &mut PredicateQnames::new(), &current)?;
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
    validate_statement_create_request(&req.new_statement)?;
    let kind = statement_kind_from_wire(req.new_statement.kind);

    let old_id = StatementId::from(req.old_statement_id);
    let now = now_unix_nanos();
    // Tenant wall (early): a caller may only supersede a statement it
    // owns. A foreign / absent id reads as NotFound before any predicate
    // intern work. The apply-layer wall re-checks atomically.
    if !statement_id_in_caller_scope(ctx, old_id) {
        return Err(OpError::NotFound {
            what: "statement",
            detail: format!("{old_id:?}"),
        });
    }
    let (namespace, name) = split_qname(&req.new_statement.predicate)?;

    // Step A — pre-submit predicate resolution mirror of CREATE, resolved in
    // the same scope.
    let caller_space: [u8; 16] = ctx.executor.caller_space.into();
    let (predicate_id_opt, schemaless) = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let active_version = schema_active(&rtxn, DeclScope::space(namespace, caller_space))
            .map_err(|e| OpError::Internal(format!("schema_active: {e}")))?;
        let pid = match active_version {
            Some(version) => {
                let pred = predicate_lookup_by_qname(
                    &rtxn,
                    DeclScope::space(namespace, caller_space),
                    name,
                )
                .map_err(OpError::from)?
                .ok_or_else(|| OpError::PredicateNotInSchema {
                    predicate: req.new_statement.predicate.clone(),
                    namespace: namespace.to_string(),
                    version,
                })?;
                let active = predicates_active_for_schema(
                    &rtxn,
                    DeclScope::space(namespace, caller_space),
                    version,
                )
                .map_err(OpError::from)?;
                if !active.contains(&pred.id) {
                    return Err(OpError::PredicateNotInSchema {
                        predicate: req.new_statement.predicate.clone(),
                        namespace: namespace.to_string(),
                        version,
                    });
                }
                Some(pred.id)
            }
            None => {
                predicate_lookup_by_qname(&rtxn, DeclScope::space(namespace, caller_space), name)
                    .map_err(OpError::from)?
                    .map(|p| p.id)
            }
        };
        (pid, active_version.is_none())
    };

    let predicate_id = match predicate_id_opt {
        Some(pid) => pid,
        None => {
            if !schemaless {
                return Err(OpError::Internal(
                    "predicate resolution inconsistency: strict-mode none after vocab check".into(),
                ));
            }
            let wtxn = ctx
                .executor
                .metadata
                .write_txn()
                .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;
            let pid = predicate_intern_or_get(&wtxn, DeclScope::base(namespace), name, 0, now)
                .map_err(OpError::from)?;
            wtxn.commit()
                .map_err(|e| OpError::Internal(format!("commit: {e}")))?;
            pid
        }
    };

    // Step B — submit the Supersede phase. SupersedeReplacement
    // carries the full new statement so apply_supersede_statement can
    // call statement_supersede inside one wtxn.
    let new_statement = build_statement_from_create(&req.new_statement, predicate_id, now, kind)?;

    let real_writer = real_writer_handle(ctx)?;
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_statement_supersede_request(&req);

    let phase = Phase::Supersede {
        target: SupersedeTarget::Statement(old_id),
        replacement: SupersedeReplacement::Statement(Box::new(new_statement)),
        at_unix_nanos: now,
    };
    let write = Write::single(write_id, ctx.executor.caller_space, phase)
        .with_namespace(ctx.executor.caller_namespace)
        .with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    // Recover the new statement id from the ack — handles replays
    // (cached ack's id is the original, may differ from the freshly-
    // minted one in `new_statement.id`).
    let new_id = match ack.single_phase() {
        PhaseAck::Superseded(_, SupersedeReplacementId::Statement(id)) => *id,
        other => {
            return Err(OpError::PermanentFault(format!(
                "unexpected phase ack for STATEMENT_SUPERSEDE: {other:?}"
            )))
        }
    };

    // Step C — recover chain_root + version from storage. The
    // PhaseAck doesn't surface them; the wire ack needs both.
    let (chain_root, version) = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let new = statement_get(&rtxn, new_id)
            .map_err(OpError::from)?
            .ok_or_else(|| {
                OpError::PermanentFault("new statement missing post-supersede".into())
            })?;
        (new.chain_root, new.version)
    };

    emit_graph_event(
        ctx,
        EventType::StatementSuperseded,
        GraphEventPayload::StatementSuperseded(StatementSupersededEvent {
            old_statement_id: old_id.to_bytes(),
            new_statement_id: new_id.to_bytes(),
            chain_root: chain_root.to_bytes(),
        }),
        now,
    )
    .await;

    // Lexical index: Delete old + Upsert new.
    if let Some(dispatcher) = ctx.statement_text_dispatcher.as_ref() {
        dispatcher
            .dispatch(StatementTextOp::Delete { id: old_id })
            .await;
        dispatch_upsert_for(ctx, new_id, dispatcher).await;
    }

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
    check_byte_len(
        "reason_message",
        req.reason_message.len(),
        REASON_MESSAGE_MAX_BYTES,
    )?;
    let reason = caller_tombstone_reason(req.reason)?;
    let id = StatementId::from(req.statement_id);
    let now = now_unix_nanos();

    // Tenant wall (early): a foreign / absent id reads as NotFound before
    // the write is built. The apply-layer wall re-checks atomically.
    if !statement_id_in_caller_scope(ctx, id) {
        return Err(OpError::NotFound {
            what: "statement",
            detail: format!("{id:?}"),
        });
    }

    let real_writer = real_writer_handle(ctx)?;
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_statement_tombstone_request(&req);

    let phase = Phase::Tombstone {
        target: TombstoneTarget::Statement(id),
        reason: reason.as_u8(),
        at_unix_nanos: now,
    };
    let write = Write::single(write_id, ctx.executor.caller_space, phase)
        .with_namespace(ctx.executor.caller_namespace)
        .with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    // Pull the tombstone timestamp from the ack so an idempotency replay
    // returns the originally-stored value rather than today's `now`.
    let tombstoned_at_unix_nanos = match ack.single_phase() {
        PhaseAck::Tombstoned {
            tombstoned_at_unix_nanos,
            ..
        } => *tombstoned_at_unix_nanos,
        other => {
            return Err(OpError::PermanentFault(format!(
                "unexpected phase ack for STATEMENT_TOMBSTONE: {other:?}"
            )));
        }
    };

    emit_graph_event(
        ctx,
        EventType::StatementTombstoned,
        GraphEventPayload::StatementTombstoned(StatementTombstonedEvent {
            statement_id: id.to_bytes(),
            reason: req.reason_message,
        }),
        now,
    )
    .await;

    if let Some(dispatcher) = ctx.statement_text_dispatcher.as_ref() {
        dispatcher.dispatch(StatementTextOp::Delete { id }).await;
    }

    Ok(StatementTombstoneResponse {
        tombstoned_at_unix_nanos,
    })
}

// ---------------------------------------------------------------------------
// STATEMENT_RETRACT
// ---------------------------------------------------------------------------

pub async fn handle_statement_retract(
    req: StatementRetractRequest,
    ctx: &OpsContext,
) -> Result<StatementRetractResponse, OpError> {
    check_byte_len(
        "reason_message",
        req.reason_message.len(),
        REASON_MESSAGE_MAX_BYTES,
    )?;
    // Validate the caller's audit reason byte (kept in the event /
    // message) but stamp the row with `Retract` regardless: the
    // reclamation GC worker keys on this byte to physically remove only
    // rows the caller asked to retract, never plain tombstones or
    // superseded rows.
    caller_tombstone_reason(req.reason)?;
    let id = StatementId::from(req.statement_id);
    let now = now_unix_nanos();

    // Tenant wall (early): a foreign / absent id reads as NotFound before
    // the write is built. The apply-layer wall re-checks atomically.
    if !statement_id_in_caller_scope(ctx, id) {
        return Err(OpError::NotFound {
            what: "statement",
            detail: format!("{id:?}"),
        });
    }

    // RETRACT shares the tombstone apply path; the wire distinction is
    // the post-commit behavior (drops the row from the lexical index
    // immediately, hidden from STATEMENT_HISTORY) plus the durable
    // `Retract` reason byte that lets the GC worker reclaim the row
    // after the grace period.
    let real_writer = real_writer_handle(ctx)?;
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_statement_retract_request(&req);

    let phase = Phase::Tombstone {
        target: TombstoneTarget::Statement(id),
        reason: TombstoneReason::Retract.as_u8(),
        at_unix_nanos: now,
    };
    let write = Write::single(write_id, ctx.executor.caller_space, phase)
        .with_namespace(ctx.executor.caller_namespace)
        .with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    // Retract reuses the tombstone apply path; pull the stamped timestamp
    // from the ack so idempotency replays don't drift to today's clock.
    let retracted_at_unix_nanos = match ack.single_phase() {
        PhaseAck::Tombstoned {
            tombstoned_at_unix_nanos,
            ..
        } => *tombstoned_at_unix_nanos,
        other => {
            return Err(OpError::PermanentFault(format!(
                "unexpected phase ack for STATEMENT_RETRACT: {other:?}"
            )));
        }
    };

    // Retract emits StatementTombstoned in v1 (no discrete retract
    // event in v1.0; one may be added later).
    emit_graph_event(
        ctx,
        EventType::StatementTombstoned,
        GraphEventPayload::StatementTombstoned(StatementTombstonedEvent {
            statement_id: id.to_bytes(),
            reason: format!("retract: {}", req.reason_message),
        }),
        now,
    )
    .await;

    // Retract drops the row from the lexical index (the grace
    // period only affects when the metadata is zeroed; the
    // statement is invisible to retrieval immediately).
    if let Some(dispatcher) = ctx.statement_text_dispatcher.as_ref() {
        dispatcher.dispatch(StatementTextOp::Delete { id }).await;
    }

    Ok(StatementRetractResponse {
        retracted_at_unix_nanos,
        will_zero_at_unix_nanos: retracted_at_unix_nanos.saturating_add(RETRACT_GRACE_NANOS),
    })
}

// ---------------------------------------------------------------------------
// STATEMENT_HISTORY
// ---------------------------------------------------------------------------

pub async fn handle_statement_history(
    req: StatementHistoryRequest,
    ctx: &OpsContext,
) -> Result<StatementHistoryResponseFrame, OpError> {
    if req.limit == 0 || req.limit > LIST_LIMIT_MAX {
        return Err(OpError::InvalidRequest("limit must be in 1..=1000".into()));
    }
    let anchor = StatementId::from(req.anchor_id);
    let scope =
        brain_metadata::RowScope::new(ctx.executor.caller_namespace, ctx.executor.caller_space);
    // Decode the resume point. The cursor binds the `include_tombstoned` toggle,
    // so echoing one back with a different toggle is rejected (`stale_cursor`).
    let resume = decode_history_cursor(&req.cursor, scope, req.include_tombstoned)?;
    let first_page = resume.is_none();
    let after_version = resume.map(|(_, v)| v);

    let (items_storage, chain_root, total, next_cursor) = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

        // Tenant wall (unconditional): never surface another tenant's
        // supersession chain via a foreign anchor id.
        if !statement_id_in_caller_scope(ctx, anchor) {
            return Err(OpError::NotFound {
                what: "statement",
                detail: format!("{anchor:?}"),
            });
        }

        let page = statement_history_page(&rtxn, scope, anchor, after_version, req.limit as usize)
            .map_err(OpError::from)?;

        // A resumed cursor must name the same chain the anchor resolves to — a
        // cursor minted for one anchor cannot be replayed against another chain.
        if let Some((cursor_root, _)) = resume {
            if cursor_root != page.chain_root {
                return Err(OpError::InvalidRequest(
                    "stale_cursor: anchor changed between pages".into(),
                ));
            }
        }

        // First-page-only absence: an anchor that resolves but whose whole chain
        // has no present rows is NotFound, preserving the prior contract. A
        // resumed page past exhaustion is a valid empty final page, not an error.
        if first_page && page.total == 0 {
            return Err(OpError::NotFound {
                what: "statement",
                detail: format!("{anchor:?}"),
            });
        }

        let mut qnames = PredicateQnames::new();
        let mut items = Vec::with_capacity(page.rows.len());
        for s in &page.rows {
            if !req.include_tombstoned && s.tombstoned {
                continue;
            }
            items.push(project_view(&rtxn, &mut qnames, s)?);
        }
        let next = match (page.has_more, page.last_version) {
            (true, Some(v)) => {
                encode_history_cursor(scope, req.include_tombstoned, page.chain_root, v)
            }
            _ => Vec::new(),
        };
        (items, page.chain_root, page.total, next)
    };

    Ok(StatementHistoryResponseFrame {
        total_versions: total,
        items: items_storage,
        chain_root,
        next_cursor,
        is_final: true,
    })
}

// History pagination cursor: `[ver | ns(4) | space(16) | include_tombstoned(1) |
// chain_root(16) | last_version(4)]`. Keyset is the immutable chain `version`;
// the toggle byte is the stale-cursor guard (a toggled request can't resume a
// filtered tiling without gap/dup). Bound to the caller's tenant scope.
const HISTORY_CURSOR_VERSION: u8 = 1;
const HISTORY_CURSOR_LEN: usize = 1 + 4 + 16 + 1 + 16 + 4;

fn encode_history_cursor(
    scope: brain_metadata::RowScope,
    include_tombstoned: bool,
    chain_root: [u8; 16],
    last_version: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(HISTORY_CURSOR_LEN);
    out.push(HISTORY_CURSOR_VERSION);
    out.extend_from_slice(&scope.namespace_id.to_le_bytes());
    out.extend_from_slice(&scope.space_id_bytes);
    out.push(u8::from(include_tombstoned));
    out.extend_from_slice(&chain_root);
    out.extend_from_slice(&last_version.to_le_bytes());
    out
}

/// Decode a history cursor to `(chain_root, last_version)`. Empty ⇒ first page
/// (`None`). Verifies tenant scope and the `include_tombstoned` toggle.
fn decode_history_cursor(
    cursor: &[u8],
    scope: brain_metadata::RowScope,
    include_tombstoned: bool,
) -> Result<Option<([u8; 16], u32)>, OpError> {
    if cursor.is_empty() {
        return Ok(None);
    }
    if cursor.len() != HISTORY_CURSOR_LEN || cursor[0] != HISTORY_CURSOR_VERSION {
        return Err(OpError::InvalidRequest("malformed cursor".into()));
    }
    let mut ns = [0u8; 4];
    ns.copy_from_slice(&cursor[1..5]);
    if u32::from_le_bytes(ns) != scope.namespace_id || cursor[5..21] != scope.space_id_bytes {
        return Err(OpError::InvalidRequest(
            "cursor does not belong to the caller's tenant".into(),
        ));
    }
    if cursor[21] != u8::from(include_tombstoned) {
        return Err(OpError::InvalidRequest(
            "stale_cursor: include_tombstoned changed between pages".into(),
        ));
    }
    let mut root = [0u8; 16];
    root.copy_from_slice(&cursor[22..38]);
    let mut ver = [0u8; 4];
    ver.copy_from_slice(&cursor[38..42]);
    Ok(Some((root, u32::from_le_bytes(ver))))
}

// ---------------------------------------------------------------------------
// STATEMENT_LIST
// ---------------------------------------------------------------------------

pub async fn handle_statement_list(
    req: StatementListRequest,
    ctx: &OpsContext,
) -> Result<StatementListResponseFrame, OpError> {
    if req.limit == 0 || req.limit > LIST_LIMIT_MAX {
        return Err(OpError::InvalidRequest("limit must be in 1..=1000".into()));
    }
    let scope =
        brain_metadata::RowScope::new(ctx.executor.caller_namespace, ctx.executor.caller_space);
    let filter_sig = statement_list_filter_signature(&req);
    let resume_after = decode_statement_cursor(&req.cursor, scope, &filter_sig)?;
    // The predicate filter resolves in the scope the caller reads in.
    let caller_space: [u8; 16] = ctx.executor.caller_space.into();
    // Wire filter byte: `0` = no filter; any non-zero byte is the
    // `brain_core` kind byte + 1 (so `1=Fact … 6=Directive`, `7+ = Custom`).
    let kind = match req.kind {
        0 => None,
        b => Some(StatementKind::from_u8(b - 1)),
    };
    let subject = if req.subject == [0u8; 16] {
        None
    } else {
        Some(EntityId::from(req.subject))
    };

    let (items_storage, count, next_cursor) = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

        // Resolve optional predicate qname → PredicateId.
        //
        // Schemaless mode: an unknown qname must not be an error —
        // it just yields an empty result set. Schema-strict mode:
        // it must be a `PredicateNotInSchema` so clients can tell
        // their vocabulary from a typo.
        let predicate = if req.predicate.is_empty() {
            None
        } else {
            validate_predicate_qname(&req.predicate)?;
            let (ns, name) = split_qname(&req.predicate)?;
            let active_version = schema_active(&rtxn, DeclScope::space(ns, caller_space))
                .map_err(|e| OpError::Internal(format!("schema_active: {e}")))?;
            match predicate_lookup_by_qname(&rtxn, DeclScope::space(ns, caller_space), name)
                .map_err(OpError::from)?
            {
                Some(p) => Some(p.id),
                None => {
                    if let Some(version) = active_version {
                        return Err(OpError::PredicateNotInSchema {
                            predicate: req.predicate.clone(),
                            namespace: ns.to_string(),
                            version,
                        });
                    }
                    // Schemaless: no rows could match, so short-circuit.
                    return Ok(StatementListResponseFrame {
                        items: Vec::new(),
                        next_cursor: Vec::new(),
                        cumulative_count: 0,
                        is_final: true,
                    });
                }
            }
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
            // `statement_list_page` takes its page size as an explicit
            // argument; the struct field is unused on this path.
            limit: 0,
        };
        // Tombstone and time-range are not index columns; push them into
        // the page walk so a page is exactly the wire-visible rows and
        // `has_more` is exact (never a short page hiding a full next one).
        let time_range =
            if req.time_range_start_unix_nanos != 0 || req.time_range_end_unix_nanos != 0 {
                let lo = req.time_range_start_unix_nanos;
                let hi = if req.time_range_end_unix_nanos == 0 {
                    u64::MAX
                } else {
                    req.time_range_end_unix_nanos
                };
                Some((lo, hi))
            } else {
                None
            };
        let extra = StatementPageExtra {
            include_tombstoned: req.include_tombstoned,
            time_range,
        };

        // Page directly from the store: seek strictly past the cursor,
        // apply every predicate in the walk, and return one page plus
        // whether more remain. No 1000-row window, so rows past 1000 are
        // reachable and each page costs one page's worth of scan.
        let page = statement_list_page(
            &rtxn,
            scope,
            &filter,
            &extra,
            resume_after,
            req.limit as usize,
        )
        .map_err(OpError::from)?;

        let mut qnames = PredicateQnames::new();
        let mut out = Vec::with_capacity(page.rows.len());
        for s in &page.rows {
            out.push(project_view(&rtxn, &mut qnames, s)?);
        }
        let count = out.len() as u32;
        let next_cursor = match (page.has_more, page.last) {
            (true, Some(c)) => encode_statement_cursor(scope, &filter_sig, &c),
            _ => Vec::new(),
        };
        (out, count, next_cursor)
    };

    Ok(StatementListResponseFrame {
        items: items_storage,
        next_cursor,
        cumulative_count: count,
        is_final: true,
    })
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Hop cap for the `STATEMENT_GET` supersession walk.
///
/// A supersession chain grows by one row each time the same claim is
/// corrected, so it is legitimately far longer than a merge redirect —
/// nothing in the substrate caps it, the chain's `version` column is a
/// `u32`, and `statement_history` pages a chain rather than truncating
/// it. The cap therefore sits far above any chain a correcting writer
/// could plausibly build, while still turning a corrupt cycle into a
/// bounded sequence of point reads inside one read transaction instead
/// of a loop that never ends.
const STATEMENT_SUPERSESSION_MAX_HOPS: usize = 1024;

/// Walk `superseded_by` from `start` to the head of its supersession
/// chain, returning the head row and whether any hop was taken.
///
/// Returns [`OpError::SupersessionChainCycle`] if the chain is longer
/// than [`STATEMENT_SUPERSESSION_MAX_HOPS`] — a cycle, which no write
/// path can create and which would otherwise spin forever inside a read
/// that never yields to the executor.
fn resolve_supersession_head(
    rtxn: &redb::ReadTransaction,
    anchor: StatementId,
    start: Statement,
) -> Result<(Statement, bool), OpError> {
    let mut current = start;
    let mut followed = false;
    for _ in 0..=STATEMENT_SUPERSESSION_MAX_HOPS {
        let Some(succ) = current.superseded_by else {
            return Ok((current, followed));
        };
        followed = true;
        current = statement_get(rtxn, succ)
            .map_err(OpError::from)?
            .ok_or_else(|| {
                OpError::PermanentFault(format!("chain dangling at {succ:?} from {anchor:?}"))
            })?;
    }
    Err(OpError::SupersessionChainCycle {
        what: "statement",
        detail: format!("{anchor:?}"),
    })
}

fn validate_predicate_qname(q: &str) -> Result<(), OpError> {
    if q.is_empty() {
        return Err(OpError::InvalidRequest(
            "predicate must be non-empty".into(),
        ));
    }
    check_byte_len("predicate qname", q.len(), QNAME_MAX_BYTES)?;
    if !q.contains(':') {
        return Err(OpError::InvalidRequest(
            "predicate must use \"namespace:name\" form".into(),
        ));
    }
    Ok(())
}

fn validate_statement_create_request(req: &StatementCreateRequest) -> Result<(), OpError> {
    validate_predicate_qname(&req.predicate)?;
    if req.confidence.is_nan() || !(0.0..=1.0).contains(&req.confidence) {
        return Err(OpError::InvalidRequest(
            "confidence must be in [0, 1] and not NaN".into(),
        ));
    }
    if let brain_protocol::StatementObjectWire::Value(value) = &req.object {
        let (field, len) = match value {
            brain_protocol::StatementValueWire::Text(text) => ("statement text value", text.len()),
            brain_protocol::StatementValueWire::Blob(blob) => ("statement blob value", blob.len()),
            _ => return Ok(()),
        };
        check_byte_len(field, len, STATEMENT_VALUE_MAX_BYTES)?;
    }
    Ok(())
}

fn split_qname(q: &str) -> Result<(&str, &str), OpError> {
    let (ns, name) = q
        .split_once(':')
        .ok_or_else(|| OpError::InvalidRequest("predicate missing ':' separator".into()))?;
    Ok((ns, name))
}

// ---------------------------------------------------------------------------
// STATEMENT_LIST keyset-pagination cursor.
//
// The cursor is opaque bytes on the wire (a `bytes` field — the manifest
// is unchanged and no SDK parses it). It carries the owning scope so a
// token minted for one tenant is rejected against another, a signature of
// the query filters so a mid-pagination filter change fails closed rather
// than mis-resuming, and the last row's exact index position so the next
// page seeks strictly past it in-store (no in-memory window, rows past
// 1000 reachable).
// ---------------------------------------------------------------------------

// v3 drops the mutable discriminant columns (kind / predicate_id /
// is_current / confidence_bucket) that v2 carried: the page walk now
// resumes on the immutable statement id alone, so those columns are dead
// weight and, worse, encoded a resume position that could move. Bumping
// the internal discriminator makes any v2 cursor still in flight decode as
// malformed rather than mis-resume. This is the opaque `bytes` cursor's
// own layout version, not a wire/protocol version.
const STATEMENT_CURSOR_VERSION: u8 = 3;
/// `version(1) + namespace_id(4) + space_id(16) + filter_sig(8) + id(16)`.
const STATEMENT_CURSOR_LEN: usize = 1 + 4 + 16 + 8 + 16;

fn statement_list_filter_signature(req: &StatementListRequest) -> [u8; 8] {
    let mut h = blake3::Hasher::new();
    h.update(&req.subject);
    h.update(&(req.predicate.len() as u32).to_le_bytes());
    h.update(req.predicate.as_bytes());
    h.update(&[
        req.kind,
        u8::from(req.only_current),
        u8::from(req.include_tombstoned),
    ]);
    h.update(&req.min_confidence.to_le_bytes());
    h.update(&req.time_range_start_unix_nanos.to_le_bytes());
    h.update(&req.time_range_end_unix_nanos.to_le_bytes());
    let full = h.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&full.as_bytes()[..8]);
    out
}

fn encode_statement_cursor(
    scope: brain_metadata::RowScope,
    sig: &[u8; 8],
    c: &StatementListCursor,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(STATEMENT_CURSOR_LEN);
    out.push(STATEMENT_CURSOR_VERSION);
    out.extend_from_slice(&scope.namespace_id.to_le_bytes());
    out.extend_from_slice(&scope.space_id_bytes);
    out.extend_from_slice(sig);
    out.extend_from_slice(&c.id);
    out
}

fn decode_statement_cursor(
    cursor: &[u8],
    scope: brain_metadata::RowScope,
    sig: &[u8; 8],
) -> Result<Option<StatementListCursor>, OpError> {
    if cursor.is_empty() {
        return Ok(None);
    }
    if cursor.len() != STATEMENT_CURSOR_LEN || cursor[0] != STATEMENT_CURSOR_VERSION {
        return Err(OpError::InvalidRequest("malformed cursor".into()));
    }
    let mut ns = [0u8; 4];
    ns.copy_from_slice(&cursor[1..5]);
    if u32::from_le_bytes(ns) != scope.namespace_id || cursor[5..21] != scope.space_id_bytes {
        return Err(OpError::InvalidRequest(
            "cursor does not belong to the caller's tenant".into(),
        ));
    }
    if cursor[21..29] != *sig {
        return Err(OpError::InvalidRequest(
            "stale_cursor: filters changed between pages".into(),
        ));
    }
    let mut id = [0u8; 16];
    id.copy_from_slice(&cursor[29..45]);
    Ok(Some(StatementListCursor { id }))
}

/// Build a brain-core `Statement` from a wire `StatementCreateRequest`
/// and the resolved `PredicateId`. Performs per-kind invariant checks:
/// `event_at` is Event-exclusive (only an Event may set it), but a dateless
/// Event is valid — the read answers "when" from the evidence memory's own
/// `occurred_at`.
fn build_statement_from_create(
    req: &StatementCreateRequest,
    predicate: PredicateId,
    now: u64,
    kind: StatementKind,
) -> Result<Statement, OpError> {
    use brain_protocol::{evidence_ref_from_wire, statement_object_from_wire};

    // A dateless Event (`event_at_unix_nanos == 0` → `None`) is valid: a
    // same-day/undated action still IS an event, and the read answers "when"
    // from the evidence memory's own `occurred_at`. Only a non-Event kind
    // carrying an event date is rejected — that field is Event-exclusive.
    if kind != StatementKind::Event && req.event_at_unix_nanos != 0 {
        return Err(OpError::InvalidRequest(
            "only Event kind may set event_at_unix_nanos".into(),
        ));
    }

    // Events are point-in-time, not validity ranges: an Event must not
    // carry valid_from / valid_to. Reject loudly, mirroring the
    // event_at check above (the apply-layer `validate_statement_shape`
    // enforces the same invariant authoritatively for in-process callers).
    if kind == StatementKind::Event
        && (req.valid_from_unix_nanos != 0 || req.valid_to_unix_nanos != 0)
    {
        return Err(OpError::InvalidRequest(
            "Event kind must not set valid_from_unix_nanos / valid_to_unix_nanos".into(),
        ));
    }

    let evidence = evidence_ref_from_wire(&req.evidence).map_err(|e| match e {
        brain_protocol::WireToStatementError::EvidenceInlineTooLarge { len, cap } => {
            OpError::InvalidRequest(format!(
                "inline evidence list exceeds cap of {cap}; got {len}"
            ))
        }
        other => OpError::InvalidRequest(format!("evidence decode: {other}")),
    })?;

    let object = statement_object_from_wire(&req.object);
    let subject = brain_core::SubjectRef::Entity(EntityId::from(req.subject));

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
        // `extracted_at` is record time — when the substrate ingested the
        // claim — and must always be the true arrival time. A caller-supplied
        // historical `valid_from` is object-time and is applied separately
        // below; conflating the two would mis-pin a later supersede's
        // `old.valid_to = new.extracted_at` to a historical date.
        now,
        if req.schema_version == 0 {
            1
        } else {
            req.schema_version
        },
    );
    // Object-time validity bounds, distinct from `extracted_at` (record
    // time). Events are point-in-time and carry neither (rejected above),
    // so only set validity for non-Event kinds.
    if kind != StatementKind::Event {
        if req.valid_from_unix_nanos != 0 {
            s.valid_from_unix_nanos = Some(req.valid_from_unix_nanos);
        }
        if req.valid_to_unix_nanos != 0 {
            s.valid_to_unix_nanos = Some(req.valid_to_unix_nanos);
        }
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
///
/// `qnames` is the caller's resolver, shared across a page. It memoises
/// the registry, so a projection loop costs one lookup per DISTINCT
/// predicate rather than one per row, and it names an unresolvable
/// predicate with the empty sentinel instead of failing. One row whose
/// predicate row has gone would otherwise deny the caller every healthy
/// row beside it.
///
/// An overflow row the store no longer has degrades the same way: the
/// statement projects with an empty evidence list. That empty list is
/// indistinguishable from a statement that genuinely cites nothing, because
/// `StatementView` has no field for "evidence incomplete" and adding one
/// would change the protocol. Both degrades are counted into the degraded-row
/// family, so the condition has a rate an operator can alert on even where
/// the projected value looks healthy.
fn project_view(
    rtxn: &redb::ReadTransaction,
    qnames: &mut PredicateQnames,
    s: &Statement,
) -> Result<StatementView, OpError> {
    let qname = qnames.resolve(rtxn, s.predicate);
    // The resolver blanks a predicate it cannot name and memoises the
    // answer, so the row-level count lives here: an operator needs the rate
    // of claims reaching clients unnamed, not the number of registry
    // entries that went missing.
    if qname.is_empty() {
        read_degrade_metrics().inc(ReadDegradeReason::PredicateQnameMissing);
    }

    // If evidence is overflow, resolve to inline form so consumers
    // don't need a second op to read the memory ids. A future
    // STATEMENT_ADD_EVIDENCE will let callers fetch the per-entry
    // metadata separately if needed.
    let mut s = s.clone();
    if let brain_core::EvidenceRef::Overflow(id) = s.evidence {
        let entries = evidence_overflow_load(rtxn, id)
            .map_err(OpError::from)?
            .unwrap_or_else(|| {
                // A live statement pointing at an overflow row the store no
                // longer has is an inconsistency inside one read transaction,
                // not a client error. The claim, its subject, its predicate,
                // and its history are still true and worth returning, so
                // project the row without its citations rather than denying
                // every healthy row on the same page.
                tracing::warn!(
                    statement_id = ?s.id,
                    overflow_id = ?id,
                    "statement references an evidence-overflow row that is gone; projecting with no evidence"
                );
                read_degrade_metrics().inc(ReadDegradeReason::EvidenceOverflowMissing);
                Vec::new()
            });
        let mut sv = smallvec::SmallVec::<
            [brain_core::EvidenceEntry; brain_core::INLINE_EVIDENCE_CAP],
        >::new();
        for e in entries.into_iter().take(brain_core::INLINE_EVIDENCE_CAP) {
            sv.push(e);
        }
        s.evidence = brain_core::EvidenceRef::inline(sv);
    }

    Ok(StatementView::from_statement(&s, qname))
}

/// Project a brain-core `Statement` into the `Phase::UpsertStatement`
/// shape consumed by `apply_upsert_statement`. The fields mirror
/// `Statement::new_root` so the apply function reproduces an
/// equivalent row.
///
/// `predicate_intern_hint` is `Some((namespace, name))` for schemaless
/// writes whose predicate isn't yet in the registry; apply will run
/// `predicate_intern_or_get` inside the main wtxn and stamp the row
/// `IMPLICIT_PREDICATE`. `None` for the strict path or schemaless
/// writes where the predicate is already interned.
fn build_upsert_statement_phase(
    s: &Statement,
    predicate_intern_hint: Option<(String, String)>,
    session: brain_core::SessionId,
) -> Phase {
    let evidence = match &s.evidence {
        brain_core::EvidenceRef::Inline(entries) => {
            let v: Vec<EvidenceEntry> = entries.iter().copied().collect();
            EvidenceRefPhase::Inline(v)
        }
        brain_core::EvidenceRef::Overflow(id) => EvidenceRefPhase::Overflow(*id),
    };
    Phase::UpsertStatement {
        id: s.id,
        kind: s.kind,
        session,
        subject: s.subject,
        predicate: s.predicate,
        object: s.object.clone(),
        confidence: s.confidence,
        evidence,
        valid_from_unix_nanos: s.valid_from_unix_nanos,
        extractor: s.extractor_id,
        extracted_at_unix_nanos: s.extracted_at_unix_nanos,
        schema_version: s.schema_version,
        predicate_intern_hint,
    }
}

/// BLAKE3 over the canonical STATEMENT_CREATE fields. Excludes the
/// request_id (which is the cache key) and the freshly-minted
/// statement id (the writer's idempotency cache resolves replays via
/// request_id, not by output id).
fn hash_statement_create_request(req: &StatementCreateRequest) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"statement_create:");
    h.update(&[req.kind.as_storage_byte()]);
    h.update(b"\0");
    h.update(&req.subject);
    h.update(b"\0");
    h.update(req.predicate.as_bytes());
    h.update(b"\0");
    hash_statement_object(&mut h, &req.object);
    h.update(b"\0");
    h.update(&req.confidence.to_le_bytes());
    h.update(b"\0");
    h.update(&req.extractor_id.to_le_bytes());
    h.update(b"\0");
    h.update(&req.valid_from_unix_nanos.to_le_bytes());
    h.update(&req.valid_to_unix_nanos.to_le_bytes());
    h.update(&req.event_at_unix_nanos.to_le_bytes());
    h.update(&req.schema_version.to_le_bytes());
    h.update(&req.session_id.to_le_bytes());
    h.update(b"\0");
    hash_evidence_ref(&mut h, &req.evidence);
    *h.finalize().as_bytes()
}

fn hash_statement_supersede_request(req: &StatementSupersedeRequest) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"statement_supersede:");
    h.update(&req.old_statement_id);
    h.update(b"\0");
    let new_hash = hash_statement_create_request(&req.new_statement);
    h.update(&new_hash);
    *h.finalize().as_bytes()
}

fn hash_statement_tombstone_request(req: &StatementTombstoneRequest) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"statement_tombstone:");
    h.update(&req.statement_id);
    h.update(b"\0");
    h.update(&[req.reason]);
    h.update(b"\0");
    h.update(req.reason_message.as_bytes());
    *h.finalize().as_bytes()
}

fn hash_statement_retract_request(req: &StatementRetractRequest) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"statement_retract:");
    h.update(&req.statement_id);
    h.update(b"\0");
    h.update(&[req.reason]);
    h.update(b"\0");
    h.update(req.reason_message.as_bytes());
    *h.finalize().as_bytes()
}

fn hash_statement_object(h: &mut blake3::Hasher, obj: &brain_protocol::StatementObjectWire) {
    use brain_protocol::{StatementObjectWire, StatementValueWire};
    h.update(&[obj.discriminant()]);
    match obj {
        StatementObjectWire::EntityRef(id) => {
            h.update(b"e");
            h.update(id);
        }
        StatementObjectWire::Value(v) => match v {
            StatementValueWire::Text(s) => {
                h.update(b"vT");
                h.update(s.as_bytes());
            }
            StatementValueWire::Integer(i) => {
                h.update(b"vI");
                h.update(&i.to_le_bytes());
            }
            StatementValueWire::Float(f) => {
                h.update(b"vF");
                h.update(&f.to_le_bytes());
            }
            StatementValueWire::Bool(b) => {
                h.update(b"vB");
                h.update(&[u8::from(*b)]);
            }
            StatementValueWire::UnixNanos(t) => {
                h.update(b"vN");
                h.update(&t.to_le_bytes());
            }
            StatementValueWire::Blob(bytes) => {
                h.update(b"vL");
                h.update(&(bytes.len() as u32).to_le_bytes());
                h.update(bytes);
            }
        },
        StatementObjectWire::MemoryRef(m) => {
            h.update(b"m");
            h.update(m);
        }
        StatementObjectWire::StatementRef(s) => {
            h.update(b"s");
            h.update(s);
        }
    }
}

fn hash_evidence_ref(h: &mut blake3::Hasher, ev: &brain_protocol::EvidenceRefWire) {
    use brain_protocol::EvidenceRefWire;
    match ev {
        EvidenceRefWire::Inline(ids) => {
            h.update(b"I");
            h.update(&(ids.len() as u32).to_le_bytes());
            for id in ids {
                h.update(id);
            }
        }
        EvidenceRefWire::Overflow(id) => {
            h.update(b"O");
            h.update(id);
        }
    }
}

/// Project a writer fault into the wire error taxonomy. The writer's own
/// variants already carry the classification, so this forwards them intact.
fn map_writer_err(err: WriterError) -> OpError {
    OpError::ExecError(brain_planner::ExecError::WriterFailed(err))
}

// Statement / predicate error classification lives in
// `OpError`'s `From` impls (crate::error) — handlers use `OpError::from`.

// ---------------------------------------------------------------------------
// Text-indexer dispatch helpers.
// ---------------------------------------------------------------------------

/// Look up the just-committed statement by id, project it to a
/// `StatementTextOp::Upsert`, and dispatch. Returns silently on
/// any error — text-indexer drift is reported via shard metrics,
/// not as a statement-op failure.
async fn dispatch_upsert_for(
    ctx: &OpsContext,
    id: StatementId,
    dispatcher: &crate::index::text_indexer::StatementTextDispatcher,
) {
    crate::index::text_indexer::statement::dispatch_statement_text_upsert(
        ctx.executor.metadata.as_ref(),
        dispatcher,
        id,
    )
    .await;
}

#[cfg(test)]
mod history_cursor_tests {
    use super::{decode_history_cursor, encode_history_cursor};
    use brain_metadata::RowScope;

    fn scope() -> RowScope {
        RowScope::from_bytes(1, [7u8; 16])
    }

    #[test]
    fn round_trips_chain_root_and_version() {
        let root = [0xAB; 16];
        let c = encode_history_cursor(scope(), false, root, 42);
        let got = decode_history_cursor(&c, scope(), false).unwrap();
        assert_eq!(got, Some((root, 42)));
    }

    #[test]
    fn empty_cursor_is_first_page() {
        assert_eq!(decode_history_cursor(&[], scope(), false).unwrap(), None);
    }

    #[test]
    fn toggled_include_tombstoned_is_stale() {
        let c = encode_history_cursor(scope(), false, [1; 16], 3);
        assert!(decode_history_cursor(&c, scope(), true).is_err());
    }

    #[test]
    fn foreign_tenant_cursor_rejected() {
        let c = encode_history_cursor(scope(), false, [1; 16], 3);
        let other = RowScope::from_bytes(2, [7u8; 16]);
        assert!(decode_history_cursor(&c, other, false).is_err());
        let other_space = RowScope::from_bytes(1, [9u8; 16]);
        assert!(decode_history_cursor(&c, other_space, false).is_err());
    }

    #[test]
    fn malformed_cursor_rejected() {
        assert!(decode_history_cursor(&[1, 2, 3], scope(), false).is_err());
        let mut c = encode_history_cursor(scope(), false, [1; 16], 3);
        c[0] = 0xFF; // wrong version byte
        assert!(decode_history_cursor(&c, scope(), false).is_err());
    }
}

#[cfg(test)]
mod build_statement_tests {
    use super::{build_statement_from_create, validate_statement_create_request};
    use brain_core::{PredicateId, StatementKind};
    use brain_protocol::{
        EvidenceRefWire, StatementCreateRequest, StatementKindWire, StatementObjectWire,
        StatementValueWire,
    };

    const NOW: u64 = 1_700_000_000_000_000_500;
    const HISTORICAL: u64 = 1_600_000_000_000_000_000;

    fn req(kind: StatementKindWire) -> StatementCreateRequest {
        StatementCreateRequest {
            kind,
            subject: [0u8; 16],
            predicate: "test:role".into(),
            object: StatementObjectWire::Value(StatementValueWire::Text("x".into())),
            confidence: 0.9,
            evidence: EvidenceRefWire::Inline(Vec::new()),
            extractor_id: 0,
            valid_from_unix_nanos: 0,
            valid_to_unix_nanos: 0,
            event_at_unix_nanos: 0,
            schema_version: 0,
            session_id: 0,
            request_id: [0u8; 16],
            act_as: None,
        }
    }

    // S1: an Event carrying validity is rejected at the wire layer.
    #[test]
    fn event_with_valid_from_rejected() {
        let mut r = req(StatementKindWire::Event);
        r.event_at_unix_nanos = NOW;
        r.valid_from_unix_nanos = HISTORICAL;
        let out = build_statement_from_create(&r, PredicateId::from(1), NOW, StatementKind::Event);
        assert!(out.is_err(), "Event with valid_from must be rejected");
    }

    #[test]
    fn oversize_value_is_invalid_request() {
        let mut r = req(StatementKindWire::Fact);
        r.object = StatementObjectWire::Value(StatementValueWire::Text(
            "x".repeat(super::STATEMENT_VALUE_MAX_BYTES + 1),
        ));
        let err =
            validate_statement_create_request(&r).expect_err("over-cap value must be refused");
        let crate::error::OpError::InvalidField { field, .. } = err else {
            panic!("an over-cap value must name the offending field");
        };
        assert_eq!(field, "statement text value");
    }

    #[test]
    fn event_with_valid_to_rejected() {
        let mut r = req(StatementKindWire::Event);
        r.event_at_unix_nanos = NOW;
        r.valid_to_unix_nanos = NOW + 10;
        let out = build_statement_from_create(&r, PredicateId::from(1), NOW, StatementKind::Event);
        assert!(out.is_err());
    }

    // S1: an Event with only event_at is accepted and carries no validity.
    #[test]
    fn event_with_only_event_at_accepted() {
        let mut r = req(StatementKindWire::Event);
        r.event_at_unix_nanos = NOW;
        let s = build_statement_from_create(&r, PredicateId::from(1), NOW, StatementKind::Event)
            .expect("event accepted");
        assert_eq!(s.event_at_unix_nanos, Some(NOW));
        assert_eq!(s.valid_from_unix_nanos, None);
        assert_eq!(s.valid_to_unix_nanos, None);
    }

    // S5: extracted_at is arrival time; a historical valid_from is preserved
    // independently.
    #[test]
    fn fact_extracted_at_is_arrival_not_historical_valid_from() {
        let mut r = req(StatementKindWire::Fact);
        r.valid_from_unix_nanos = HISTORICAL;
        let s = build_statement_from_create(&r, PredicateId::from(1), NOW, StatementKind::Fact)
            .expect("fact accepted");
        assert_eq!(s.extracted_at_unix_nanos, NOW);
        assert_eq!(s.valid_from_unix_nanos, Some(HISTORICAL));
    }
}

#[cfg(all(test, not(miri)))]
mod project_view_tests {
    use super::project_view;
    use crate::metrics::read_degrade::{global as read_degrade_metrics, ReadDegradeReason};
    use brain_core::{
        EntityId, EvidenceEntry, EvidenceOverflowId, EvidenceRef, ExtractorId, MemoryId,
        PredicateId, Statement, StatementId, StatementKind, StatementObject, SubjectRef,
    };
    use brain_metadata::schema::predicate::{predicate_intern, ObjectConstraint};
    use brain_metadata::schema::qnames::PredicateQnames;
    use brain_metadata::statement::allocate_evidence_overflow;
    use brain_metadata::{DeclScope, MetadataDb};
    use brain_protocol::ops::statement::EvidenceRefWire;

    /// The degraded-row counters are process-wide, so a test reading them as a
    /// delta has to be the only one projecting while it does.
    static DEGRADE_COUNTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const NOW: u64 = 1_700_000_000_000_000_000;

    /// `MetadataDb::open` seeds the system schema, so every catalog table
    /// the projection reads already exists.
    fn fresh_db(dir: &tempfile::TempDir) -> MetadataDb {
        MetadataDb::open(dir.path().join("meta.redb")).expect("open metadata")
    }

    fn intern(db: &MetadataDb, name: &str) -> PredicateId {
        let wtxn = db.write_txn().expect("write txn");
        let id = predicate_intern(
            &wtxn,
            DeclScope::base("acme"),
            name,
            None,
            ObjectConstraint::ANY,
            1,
            "",
            false,
            NOW,
        )
        .expect("intern predicate");
        wtxn.commit().expect("commit");
        id
    }

    fn statement(predicate: PredicateId) -> Statement {
        Statement::new_root(
            StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(EntityId::new()),
            predicate,
            StatementObject::Entity(EntityId::new()),
            1.0,
            EvidenceRef::default(),
            ExtractorId::from(0u32),
            NOW,
            1,
        )
    }

    fn statement_with_evidence(predicate: PredicateId, evidence: EvidenceRef) -> Statement {
        let mut s = statement(predicate);
        s.evidence = evidence;
        s
    }

    /// The healthy case, which the degraded one must leave alone.
    #[test]
    fn a_live_predicate_reaches_the_view_as_its_qname() {
        let _counts = DEGRADE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let p = intern(&db, "employs");
        let rtxn = db.read_txn().expect("read txn");
        let view = project_view(&rtxn, &mut PredicateQnames::new(), &statement(p))
            .expect("healthy row projects");
        assert_eq!(view.predicate, "acme:employs");
    }

    /// A degraded row has to leave a trace an operator can alert on, because
    /// the projected row itself looks ordinary.
    #[test]
    fn a_statement_whose_predicate_row_is_gone_counts_a_degraded_row() {
        let _counts = DEGRADE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let rtxn = db.read_txn().expect("read txn");
        let before = read_degrade_metrics()
            .snapshot()
            .rows(ReadDegradeReason::PredicateQnameMissing);
        let view = project_view(
            &rtxn,
            &mut PredicateQnames::new(),
            &statement(PredicateId::from(4242)),
        )
        .expect("an orphaned predicate must not fail the projection");
        assert_eq!(view.predicate, "");
        let after = read_degrade_metrics()
            .snapshot()
            .rows(ReadDegradeReason::PredicateQnameMissing);
        assert!(
            after > before,
            "an unnamed predicate must raise the degraded-row count"
        );
    }

    /// A healthy row must not raise the alert, or the signal is worthless.
    #[test]
    fn a_live_predicate_counts_no_degraded_row() {
        let _counts = DEGRADE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let p = intern(&db, "employs");
        let rtxn = db.read_txn().expect("read txn");
        let before = read_degrade_metrics()
            .snapshot()
            .rows(ReadDegradeReason::PredicateQnameMissing);
        let view = project_view(&rtxn, &mut PredicateQnames::new(), &statement(p))
            .expect("healthy row projects");
        assert_eq!(view.predicate, "acme:employs");
        let after = read_degrade_metrics()
            .snapshot()
            .rows(ReadDegradeReason::PredicateQnameMissing);
        assert_eq!(
            after, before,
            "a nameable predicate must not raise the degraded-row count"
        );
    }

    /// A statement pointing at a predicate the registry has no row for is a
    /// broken row, not a broken request. It projects with the empty qname so
    /// the caller sees the statement and can act on it.
    #[test]
    fn a_statement_whose_predicate_row_is_gone_still_projects() {
        let _counts = DEGRADE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let rtxn = db.read_txn().expect("read txn");
        let orphan = statement(PredicateId::from(4242));
        let view = project_view(&rtxn, &mut PredicateQnames::new(), &orphan)
            .expect("an orphaned predicate must not fail the projection");
        assert_eq!(view.predicate, "");
        assert_eq!(view.statement_id, orphan.id.to_bytes());
    }

    /// What STATEMENT_LIST and STATEMENT_HISTORY do: one resolver over a
    /// page. A row whose predicate row is missing projects with an empty
    /// qname, so a single orphan does not deny the caller the healthy rows
    /// beside it.
    #[test]
    fn one_orphan_does_not_deny_the_rest_of_the_page() {
        let _counts = DEGRADE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let p = intern(&db, "employs");
        let page = [
            statement(p),
            statement(PredicateId::from(4242)),
            statement(p),
        ];
        let rtxn = db.read_txn().expect("read txn");
        let mut qnames = PredicateQnames::new();
        let names: Vec<String> = page
            .iter()
            .map(|s| {
                project_view(&rtxn, &mut qnames, s)
                    .expect("no row may fail the page")
                    .predicate
            })
            .collect();
        assert_eq!(names, vec!["acme:employs", "", "acme:employs"]);
    }

    /// A broken overflow row is a broken row, not a broken request. The
    /// statement still projects with an empty evidence list.
    #[test]
    fn a_statement_whose_evidence_overflow_row_is_gone_still_projects() {
        let _counts = DEGRADE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let p = intern(&db, "employs");
        let orphan = statement_with_evidence(p, EvidenceRef::Overflow(EvidenceOverflowId::new()));
        let rtxn = db.read_txn().expect("read txn");
        let view = project_view(&rtxn, &mut PredicateQnames::new(), &orphan)
            .expect("a missing overflow row must not fail the projection");
        assert_eq!(view.statement_id, orphan.id.to_bytes());
        assert_eq!(view.predicate, "acme:employs");
        assert!(matches!(
            view.evidence,
            EvidenceRefWire::Inline(ref entries) if entries.is_empty()
        ));
    }

    /// An unreachable citation list is counted under its own reason: the
    /// remedy is evidence storage, not the schema catalog.
    #[test]
    fn a_statement_whose_evidence_overflow_row_is_gone_counts_a_degraded_row() {
        let _counts = DEGRADE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let p = intern(&db, "employs");
        let orphan = statement_with_evidence(p, EvidenceRef::Overflow(EvidenceOverflowId::new()));
        let rtxn = db.read_txn().expect("read txn");
        let before = read_degrade_metrics()
            .snapshot()
            .rows(ReadDegradeReason::EvidenceOverflowMissing);
        project_view(&rtxn, &mut PredicateQnames::new(), &orphan)
            .expect("a missing overflow row must not fail the projection");
        let after = read_degrade_metrics()
            .snapshot()
            .rows(ReadDegradeReason::EvidenceOverflowMissing);
        assert!(
            after > before,
            "an unreachable overflow row must raise the degraded-row count"
        );
    }

    /// An empty evidence list is an ordinary healthy value, so a statement
    /// that genuinely cites nothing must not look like corruption.
    #[test]
    fn a_statement_citing_nothing_counts_no_degraded_row() {
        let _counts = DEGRADE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let p = intern(&db, "employs");
        let rtxn = db.read_txn().expect("read txn");
        let before = read_degrade_metrics()
            .snapshot()
            .rows(ReadDegradeReason::EvidenceOverflowMissing);
        project_view(&rtxn, &mut PredicateQnames::new(), &statement(p))
            .expect("healthy row projects");
        let after = read_degrade_metrics()
            .snapshot()
            .rows(ReadDegradeReason::EvidenceOverflowMissing);
        assert_eq!(
            after, before,
            "a statement with no citations is healthy and must not be counted"
        );
    }

    /// The page shape STATEMENT_LIST and STATEMENT_HISTORY produce: one
    /// statement whose citations are unreachable costs the caller that row's
    /// evidence and nothing else — the rows beside it keep theirs.
    #[test]
    fn one_orphan_overflow_does_not_deny_the_rest_of_the_page() {
        let _counts = DEGRADE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let p = intern(&db, "employs");
        let first_id = MemoryId::pack(1, 10, 1);
        let third_id = MemoryId::pack(1, 11, 1);
        let page = [
            statement_with_evidence(
                p,
                EvidenceRef::inline_from_slice(&[EvidenceEntry::from_parts(
                    first_id,
                    1.0,
                    NOW,
                    ExtractorId::from(0),
                )]),
            ),
            statement_with_evidence(p, EvidenceRef::Overflow(EvidenceOverflowId::new())),
            statement_with_evidence(
                p,
                EvidenceRef::inline_from_slice(&[EvidenceEntry::from_parts(
                    third_id,
                    1.0,
                    NOW,
                    ExtractorId::from(0),
                )]),
            ),
        ];
        let rtxn = db.read_txn().expect("read txn");
        let mut qnames = PredicateQnames::new();
        let views: Vec<_> = page
            .iter()
            .map(|s| project_view(&rtxn, &mut qnames, s).expect("no row may fail the page"))
            .collect();
        assert_eq!(views.len(), 3);
        assert!(matches!(
            views[0].evidence,
            EvidenceRefWire::Inline(ref entries) if entries == &vec![first_id.to_be_bytes()]
        ));
        assert!(matches!(
            views[1].evidence,
            EvidenceRefWire::Inline(ref entries) if entries.is_empty()
        ));
        assert!(matches!(
            views[2].evidence,
            EvidenceRefWire::Inline(ref entries) if entries == &vec![third_id.to_be_bytes()]
        ));
    }

    /// The healthy case, which the degraded one must leave alone: a reachable
    /// overflow row is flattened into the inline form the caller reads without
    /// a second op.
    #[test]
    fn a_live_overflow_row_reaches_the_view_inline() {
        let _counts = DEGRADE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let p = intern(&db, "employs");
        let entries = vec![
            EvidenceEntry::from_parts(MemoryId::pack(1, 20, 1), 1.0, NOW, ExtractorId::from(0)),
            EvidenceEntry::from_parts(MemoryId::pack(1, 21, 1), 0.8, NOW, ExtractorId::from(1)),
        ];
        let overflow_id = {
            let wtxn = db.write_txn().expect("write txn");
            let id = allocate_evidence_overflow(&wtxn, &entries, NOW).expect("allocate overflow");
            wtxn.commit().expect("commit overflow");
            id
        };
        let s = statement_with_evidence(p, EvidenceRef::Overflow(overflow_id));
        let rtxn = db.read_txn().expect("read txn");
        let view = project_view(&rtxn, &mut PredicateQnames::new(), &s)
            .expect("a live overflow row must project");
        assert!(matches!(
            view.evidence,
            EvidenceRefWire::Inline(ref ids)
                if ids == &vec![
                    MemoryId::pack(1, 20, 1).to_be_bytes(),
                    MemoryId::pack(1, 21, 1).to_be_bytes(),
                ]
        ));
    }
}

#[cfg(all(test, not(miri)))]
mod supersession_walk_tests {
    use super::{resolve_supersession_head, STATEMENT_SUPERSESSION_MAX_HOPS};
    use crate::error::OpError;
    use brain_core::{
        EntityId, EvidenceRef, ExtractorId, PredicateId, Statement, StatementId, StatementKind,
        StatementObject, SubjectRef,
    };
    use brain_metadata::statement::statement_get;
    use brain_metadata::tables::scope::RowScope;
    use brain_metadata::tables::statement::{metadata_from_statement, STATEMENTS_TABLE};
    use brain_metadata::MetadataDb;

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn statement() -> Statement {
        Statement::new_root(
            StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(EntityId::new()),
            PredicateId::from(1),
            StatementObject::Entity(EntityId::new()),
            1.0,
            EvidenceRef::default(),
            ExtractorId::from(0u32),
            NOW,
            1,
        )
    }

    fn insert_rows(db: &MetadataDb, rows: &[(Statement, Option<StatementId>)]) {
        let scope = RowScope::from_bytes(0, [0u8; 16]);
        let wtxn = db.write_txn().expect("write txn");
        {
            let mut table = wtxn.open_table(STATEMENTS_TABLE).expect("statements table");
            for (s, next) in rows {
                let mut metadata = metadata_from_statement(s, scope);
                metadata.superseded_by_bytes = next.map(|id| id.to_bytes());
                table
                    .insert(&s.id.to_bytes(), &metadata)
                    .expect("insert statement row");
            }
        }
        wtxn.commit().expect("commit statement rows");
    }

    /// A malformed cycle terminates with a typed substrate error instead of
    /// monopolising the shard in a read loop that never yields.
    #[test]
    fn a_cyclic_chain_stops_instead_of_spinning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = MetadataDb::open(dir.path().join("meta.redb")).expect("open metadata");
        let first = statement();
        let second = statement();
        insert_rows(
            &db,
            &[
                (first.clone(), Some(second.id)),
                (second.clone(), Some(first.id)),
            ],
        );

        let rtxn = db.read_txn().expect("read txn");
        let start = statement_get(&rtxn, first.id)
            .expect("statement lookup")
            .expect("starting row");
        let err = resolve_supersession_head(&rtxn, first.id, start)
            .expect_err("a cyclic chain must stop with an error");
        assert!(matches!(
            err,
            OpError::SupersessionChainCycle {
                what: "statement",
                ..
            }
        ));
    }

    /// A long valid correction history reaches its terminal row at the hop
    /// cap, while an independent current row remains a zero-hop resolution.
    #[test]
    fn a_long_but_legal_chain_still_resolves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = MetadataDb::open(dir.path().join("meta.redb")).expect("open metadata");
        let rows: Vec<Statement> = (0..=STATEMENT_SUPERSESSION_MAX_HOPS)
            .map(|_| statement())
            .collect();
        let links: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), rows.get(i + 1).map(|next| next.id)))
            .collect();
        insert_rows(&db, &links);
        let standalone = statement();
        insert_rows(&db, &[(standalone.clone(), None)]);

        let rtxn = db.read_txn().expect("read txn");
        let start = statement_get(&rtxn, rows[0].id)
            .expect("statement lookup")
            .expect("starting row");
        let (head, followed) =
            resolve_supersession_head(&rtxn, rows[0].id, start).expect("legal chain resolves");
        assert_eq!(head.id, rows[STATEMENT_SUPERSESSION_MAX_HOPS].id);
        assert!(followed);

        let current = statement_get(&rtxn, standalone.id)
            .expect("statement lookup")
            .expect("standalone row");
        let (same, followed) =
            resolve_supersession_head(&rtxn, standalone.id, current).expect("current row resolves");
        assert_eq!(same.id, standalone.id);
        assert!(!followed);
    }
}
