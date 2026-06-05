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

use crate::context::OpsContext;
use crate::error::OpError;
use crate::handlers::entity::emit_graph_event;
use crate::handlers::link::downcast_writer_pub;
use crate::index::text_indexer::{statement::upsert_op_from_statement, StatementTextOp};
use crate::write::{
    EvidenceRefPhase, Phase, PhaseAck, SupersedeReplacement, SupersedeReplacementId,
    SupersedeTarget, TombstoneTarget, Write, WriteId,
};
use brain_core::{EntityId, PredicateId, RequestId, StatementId, StatementKind};
use brain_core::{EvidenceEntry, Statement, TombstoneReason};
use brain_metadata::schema::predicate::{
    predicate_get, predicate_intern_or_get, predicate_lookup_by_qname,
    predicates_active_for_schema,
};
use brain_metadata::schema::store::schema_active;
use brain_metadata::statement::{
    evidence_overflow_load, statement_get, statement_history, statement_list, StatementListFilter,
};
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
    // collapsing what used to be three commits (intern, statement,
    // flag stamp) into one.
    let (predicate_id_opt, schemaless) = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let active_version = schema_active(&rtxn, namespace)
            .map_err(|e| OpError::Internal(format!("schema_active: {e}")))?;
        let predicate_id_opt: Option<PredicateId> = match active_version {
            Some(version) => {
                let pred = predicate_lookup_by_qname(&rtxn, namespace, name)
                    .map_err(OpError::from)?
                    .ok_or_else(|| OpError::PredicateNotInSchema {
                        predicate: req.predicate.clone(),
                        namespace: namespace.to_string(),
                        version,
                    })?;
                let active = predicates_active_for_schema(&rtxn, namespace, version)
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
            None => predicate_lookup_by_qname(&rtxn, namespace, name)
                .map_err(OpError::from)?
                .map(|p| p.id),
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
    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_statement_create_request(&req);

    let statement_value = build_statement_from_create(&req, predicate_id, now, kind)?;
    let phase = build_upsert_statement_phase(&statement_value, intern_hint);
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
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
            return Err(OpError::Internal(format!(
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
            .ok_or_else(|| OpError::Internal("created statement missing post-commit".into()))?;
        (new.chain_root, new.supersedes)
    };

    // Emit STATEMENT_CREATED event.
    emit_graph_event(
        ctx,
        EventType::StatementCreated,
        GraphEventPayload::StatementCreated(StatementCreatedEvent {
            statement_id: created_id.to_bytes(),
            kind: req.kind as u8,
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

    let mut current = statement_get(&rtxn, id)
        .map_err(OpError::from)?
        .ok_or_else(|| OpError::NotFound {
            what: "statement",
            detail: format!("{id:?}"),
        })?;

    let mut returned_via_supersession = false;
    if req.follow_supersession {
        while let Some(succ) = current.superseded_by {
            returned_via_supersession = true;
            current = statement_get(&rtxn, succ)
                .map_err(OpError::from)?
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
    if req.new_statement.confidence.is_nan() || !(0.0..=1.0).contains(&req.new_statement.confidence)
    {
        return Err(OpError::InvalidRequest(
            "confidence must be in [0, 1] and not NaN".into(),
        ));
    }
    let kind = statement_kind_from_wire(req.new_statement.kind);

    let old_id = StatementId::from(req.old_statement_id);
    let now = crate::txn::now_unix_nanos_pub();
    let (namespace, name) = split_qname(&req.new_statement.predicate)?;

    // Step A — pre-submit predicate resolution mirror of CREATE.
    let (predicate_id_opt, schemaless) = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let active_version = schema_active(&rtxn, namespace)
            .map_err(|e| OpError::Internal(format!("schema_active: {e}")))?;
        let pid = match active_version {
            Some(version) => {
                let pred = predicate_lookup_by_qname(&rtxn, namespace, name)
                    .map_err(OpError::from)?
                    .ok_or_else(|| OpError::PredicateNotInSchema {
                        predicate: req.new_statement.predicate.clone(),
                        namespace: namespace.to_string(),
                        version,
                    })?;
                let active = predicates_active_for_schema(&rtxn, namespace, version)
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
            None => predicate_lookup_by_qname(&rtxn, namespace, name)
                .map_err(OpError::from)?
                .map(|p| p.id),
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
            let pid = predicate_intern_or_get(&wtxn, namespace, name, 0, now)
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

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_statement_supersede_request(&req);

    let phase = Phase::Supersede {
        target: SupersedeTarget::Statement(old_id),
        replacement: SupersedeReplacement::Statement(Box::new(new_statement)),
        at_unix_nanos: now,
    };
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    // Recover the new statement id from the ack — handles replays
    // (cached ack's id is the original, may differ from the freshly-
    // minted one in `new_statement.id`).
    let new_id = match ack.single_phase() {
        PhaseAck::Superseded(_, SupersedeReplacementId::Statement(id)) => *id,
        other => {
            return Err(OpError::Internal(format!(
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
            .ok_or_else(|| OpError::Internal("new statement missing post-supersede".into()))?;
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
    if req.reason_message.len() > REASON_MESSAGE_MAX {
        return Err(OpError::InvalidRequest(
            "reason_message exceeds 4 KiB".into(),
        ));
    }
    let reason = decode_tombstone_reason(req.reason)?;
    let id = StatementId::from(req.statement_id);
    let now = crate::txn::now_unix_nanos_pub();

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_statement_tombstone_request(&req);

    let phase = Phase::Tombstone {
        target: TombstoneTarget::Statement(id),
        reason: reason.as_u8(),
        at_unix_nanos: now,
    };
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    // Pull the tombstone timestamp from the ack so an idempotency replay
    // returns the originally-stored value rather than today's `now`.
    let tombstoned_at_unix_nanos = match ack.single_phase() {
        PhaseAck::Tombstoned {
            tombstoned_at_unix_nanos,
            ..
        } => *tombstoned_at_unix_nanos,
        other => {
            return Err(OpError::Internal(format!(
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
    if req.reason_message.len() > REASON_MESSAGE_MAX {
        return Err(OpError::InvalidRequest(
            "reason_message exceeds 4 KiB".into(),
        ));
    }
    // Validate the caller's audit reason byte (kept in the event /
    // message) but stamp the row with `Retract` regardless: the
    // reclamation GC worker keys on this byte to physically remove only
    // rows the caller asked to retract, never plain tombstones or
    // superseded rows.
    decode_tombstone_reason(req.reason)?;
    let id = StatementId::from(req.statement_id);
    let now = crate::txn::now_unix_nanos_pub();

    // RETRACT shares the tombstone apply path; the wire distinction is
    // the post-commit behavior (drops the row from the lexical index
    // immediately, hidden from STATEMENT_HISTORY) plus the durable
    // `Retract` reason byte that lets the GC worker reclaim the row
    // after the grace period.
    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_statement_retract_request(&req);

    let phase = Phase::Tombstone {
        target: TombstoneTarget::Statement(id),
        reason: TombstoneReason::Retract.as_u8(),
        at_unix_nanos: now,
    };
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    // Retract reuses the tombstone apply path; pull the stamped timestamp
    // from the ack so idempotency replays don't drift to today's clock.
    let retracted_at_unix_nanos = match ack.single_phase() {
        PhaseAck::Tombstoned {
            tombstoned_at_unix_nanos,
            ..
        } => *tombstoned_at_unix_nanos,
        other => {
            return Err(OpError::Internal(format!(
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
    let anchor = StatementId::from(req.anchor_id);

    let (items_storage, chain_root) = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let chain = statement_history(&rtxn, anchor).map_err(OpError::from)?;
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
        return Err(OpError::InvalidRequest("limit must be in 1..=1000".into()));
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
            let active_version = schema_active(&rtxn, ns)
                .map_err(|e| OpError::Internal(format!("schema_active: {e}")))?;
            match predicate_lookup_by_qname(&rtxn, ns, name).map_err(OpError::from)? {
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
            limit: req.limit as usize,
        };
        let mut rows = statement_list(&rtxn, &filter).map_err(OpError::from)?;

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
        return Err(OpError::InvalidRequest(
            "predicate must be non-empty".into(),
        ));
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
/// and the resolved `PredicateId`. Performs per-kind invariant checks
/// (Event requires event_at; Fact/Preference must not set event_at).
fn build_statement_from_create(
    req: &StatementCreateRequest,
    predicate: PredicateId,
    now: u64,
    kind: StatementKind,
) -> Result<Statement, OpError> {
    use brain_protocol::{evidence_ref_from_wire, statement_object_from_wire};

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
fn project_view(rtxn: &redb::ReadTransaction, s: &Statement) -> Result<StatementView, OpError> {
    let predicate = predicate_get(rtxn, s.predicate)
        .map_err(OpError::from)?
        .ok_or_else(|| {
            OpError::Internal(format!(
                "statement {:?} references missing predicate {:?}",
                s.id, s.predicate
            ))
        })?;
    let qname = predicate.canonical();

    // If evidence is overflow, resolve to inline form so consumers
    // don't need a second op to read the memory ids. A future
    // STATEMENT_ADD_EVIDENCE will let callers fetch the per-entry
    // metadata separately if needed.
    let mut s = s.clone();
    if let brain_core::EvidenceRef::Overflow(id) = s.evidence {
        let entries = evidence_overflow_load(rtxn, id)
            .map_err(OpError::from)?
            .ok_or_else(|| {
                OpError::Internal(format!(
                    "statement {:?} references missing overflow row {:?}",
                    s.id, id
                ))
            })?;
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
    h.update(&[req.kind as u8]);
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

/// Wire-level writer-error projection shared across all migrated
/// statement handlers.
fn map_writer_err(err: WriterError) -> OpError {
    match &err {
        WriterError::Internal(msg) if msg.contains("UnknownPredicate") => OpError::NotFound {
            what: "predicate",
            detail: msg.clone(),
        },
        WriterError::Internal(msg) if msg.contains("UnknownSubject") => OpError::NotFound {
            what: "subject entity",
            detail: msg.clone(),
        },
        WriterError::Internal(msg) if msg.contains("AlreadyTombstoned") => {
            OpError::Conflict(msg.clone())
        }
        WriterError::Internal(msg) if msg.contains("AlreadySuperseded") => {
            OpError::Conflict(msg.clone())
        }
        WriterError::Internal(msg) if msg.contains("EventCannotSupersede") => {
            OpError::Conflict(msg.clone())
        }
        _ => OpError::ExecError(brain_planner::ExecError::WriterFailed(err)),
    }
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
    let upsert_op = {
        let rtxn = match ctx.executor.metadata.read_txn() {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(
                    target: "brain_ops::text_indexer",
                    error = %err,
                    "statement text indexer dispatch: read_txn failed",
                );
                return;
            }
        };
        let statement = match statement_get(&rtxn, id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                tracing::warn!(
                    target: "brain_ops::text_indexer",
                    ?id,
                    "statement vanished between commit and indexer dispatch",
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    target: "brain_ops::text_indexer",
                    error = %err,
                    "statement_get during text-indexer dispatch failed",
                );
                return;
            }
        };
        drop(rtxn);
        upsert_op_from_statement(&statement, ctx.executor.metadata.as_ref())
    };

    if let Some(op) = upsert_op {
        dispatcher.dispatch(op).await;
    } else {
        tracing::debug!(
            target: "brain_ops::text_indexer",
            ?id,
            "statement text indexer skip — Pending subject or missing metadata",
        );
    }
}
