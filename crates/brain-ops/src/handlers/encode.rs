//! ENCODE handler.
//!
//! Without `txn_id`: validate + embed + reserve id + dedup check +
//! build a multi-phase Write (UpsertMemory + N × Link) and submit.
//! With `txn_id`: validate + embed + reserve a `MemoryId`, push to
//! the buffer, return a preview response. Writes happen at TXN_COMMIT.

use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::{ContextId, EdgeKind, EdgeKindRef, MemoryId, MemoryKind, NodeRef, Salience};
use brain_metadata::tables::memory::MemoryMetadata;
use brain_planner::{plan_encode_inner, EdgeOutcome};
use brain_protocol::envelope::request::EncodeRequest;
use brain_protocol::envelope::response::EncodeResponse;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::handlers::link::downcast_writer_pub;
use crate::state::idempotency::hash_encode_request;
use crate::txn::{BufferedEdgeSpec, BufferedEncode, BufferedReplay};
use crate::write::{Phase, PhaseAck, Write, WriteId};

pub async fn handle_encode(
    req: EncodeRequest,
    ctx: &OpsContext,
) -> Result<EncodeResponse, OpError> {
    if let Some(txn_id) = req.txn_id {
        return handle_encode_in_txn(req, txn_id, ctx).await;
    }

    // 1. Input validation via the existing planner check.
    let plan = plan_encode_inner(&req, &ctx.planner_ctx)?;
    let salience = plan.wal_append.salience_initial;

    // 1b. Idempotency replay short-circuit. The same
    // request_id arriving twice must return the original response. The
    // writer's idempotency cache is keyed by WriteId, but it lives
    // behind submit() — by then we've already burned embedding work
    // and a slot reservation. Peek it up-front so a replay is free.
    // A mismatched request hash on the same WriteId is a Conflict.
    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(brain_core::RequestId::from(req.request_id));
    let context_id = ContextId::from(req.context_id);
    let kind = MemoryKind::from(req.kind);
    let embedding_model_fp = ctx.executor.embedder.fingerprint();
    let request_hash = encode_request_hash(&req, embedding_model_fp, ctx.executor.caller_agent);
    match real_writer.idempotency_lookup(write_id, Some(request_hash)) {
        crate::writer::submit::CacheLookup::Hit(cached) => {
            return reconstruct_encode_response(ctx, &req, &cached, salience, embedding_model_fp);
        }
        crate::writer::submit::CacheLookup::Conflict => {
            return Err(OpError::Conflict(format!(
                "encode request_id replay with different params: request_id={}",
                hex_short(&req.request_id),
            )));
        }
        crate::writer::submit::CacheLookup::Miss => {}
    }

    // 2. Embed text → vector.
    let vector = ctx
        .executor
        .embedder
        .embed(&req.text)
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::EmbedFailed(e)))?;
    let content_hash = *blake3::hash(req.text.as_bytes()).as_bytes();

    // 3. Dedup check — when opted in, look up
    // (agent, context, content_hash). On hit, return the existing
    // memory id without submitting a Write.
    if req.deduplicate {
        if let Some(existing) = lookup_fingerprint(ctx, content_hash, context_id)? {
            return Ok(EncodeResponse {
                memory_id: existing.raw(),
                was_deduplicated: true,
                salience,
                auto_edges_added: 0,
                lsn: 0,
                agent_id: ctx.executor.caller_agent.into(),
                context_id: req.context_id,
                kind: req.kind,
                created_at_unix_nanos: 0,
                edges_out_count: 0,
                embedding_model_fp,
                // Dedup hit — no fresh write, so no background stages
                // were queued. The client has nothing to wait for.
                pending_stages: Vec::new(),
                has_active_schema: ctx.schema_gate.is_declared(),
                has_llm_extractor: ctx.extractor_registry.read().has_enabled_llm_extractor(),
            });
        }
    }

    // 4. Reserve a fresh MemoryId via the writer's slot allocator.
    let memory_id = ctx
        .executor
        .writer
        .reserve_memory_id()
        .await
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::WriterFailed(e)))?;
    let created_at = now_unix_nanos();

    // 5. Compute per-edge outcomes: an edge whose target doesn't
    // exist is `TargetMissing` (skipped, not an error). Read once
    // through a single rtxn.
    let edge_outcomes = compute_edge_outcomes(ctx, &req)?;
    let auto_edges_added = edge_outcomes
        .iter()
        .filter(|o| matches!(o, EdgeOutcome::Inserted))
        .count() as u32;

    // 6. Build the multi-phase Write: UpsertMemory + N × Link.
    let mut phases: Vec<Phase> = Vec::with_capacity(1 + req.edges.len());
    phases.push(Phase::UpsertMemory {
        id: memory_id,
        text: req.text.clone(),
        vector: Box::new(vector),
        kind,
        salience: Salience::new(salience),
        context: context_id,
        created_at_unix_nanos: created_at,
        arena_slot: memory_id.slot(),
        embedding_model_fp,
        content_hash: if req.deduplicate {
            Some(content_hash)
        } else {
            None
        },
        deduplicate: req.deduplicate,
    });
    for (edge, outcome) in req.edges.iter().zip(edge_outcomes.iter()) {
        if !matches!(outcome, EdgeOutcome::Inserted) {
            continue;
        }
        phases.push(Phase::Link {
            from: NodeRef::Memory(memory_id),
            to: NodeRef::Memory(MemoryId::from(edge.target)),
            kind: EdgeKindRef::Builtin(EdgeKind::from(edge.kind)),
            weight: edge.weight,
            origin: brain_metadata::tables::edge::origin::EXPLICIT,
            derived_by: brain_metadata::tables::edge::derived_by::CLIENT,
            disambiguator: brain_metadata::tables::edge::zero_disambiguator(),
            created_at_unix_nanos: created_at,
        });
    }

    // 7. Submit.
    let write = Write::from_phases(write_id, ctx.executor.caller_agent, phases)
        .with_request_hash(request_hash);
    let ack = real_writer
        .submit(write)
        .await
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::WriterFailed(e)))?;
    debug_assert!(matches!(ack.phase_acks[0], PhaseAck::UpsertedMemory(_)));

    // Project the write's pending background stages onto the wire
    // response. Clients waiting via `--wait` decrement this list as
    // `StageCompleted` events arrive on the subscribe stream.
    let pending_stages = ack
        .pending_stages
        .iter()
        .filter(|s| s.memory_id == memory_id)
        .map(|s| s.stage_kind)
        .collect();

    Ok(EncodeResponse {
        memory_id: memory_id.into(),
        was_deduplicated: false,
        salience,
        auto_edges_added,
        lsn: ack.lsn_first.raw(),
        agent_id: ctx.executor.caller_agent.into(),
        context_id: req.context_id,
        kind: req.kind,
        created_at_unix_nanos: created_at,
        edges_out_count: auto_edges_added,
        embedding_model_fp,
        pending_stages,
        has_active_schema: ctx.schema_gate.is_declared(),
        has_llm_extractor: ctx.extractor_registry.read().has_enabled_llm_extractor(),
    })
}

/// Look up a content-hash fingerprint to deduplicate against an
/// existing memory. Returns `Some(MemoryId)` if a
/// row exists for `(caller_agent, context, content_hash)`.
fn lookup_fingerprint(
    ctx: &OpsContext,
    content_hash: [u8; 32],
    context_id: ContextId,
) -> Result<Option<MemoryId>, OpError> {
    let db_guard = ctx.executor.metadata.lock();
    let rtxn = db_guard.read_txn().map_err(|e| {
        OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
    })?;
    // `TableDoesNotExist` on a fresh shard means no prior encode opted
    // into dedup yet — the lookup is a miss, not an error. Same pattern
    // as the knowledge/filters and recall read paths.
    let t = match rtxn.open_table(brain_metadata::tables::fingerprint::FINGERPRINTS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => {
            return Err(OpError::ExecError(
                brain_planner::ExecError::MetadataReadFailed(e.to_string()),
            ))
        }
    };
    let key = brain_metadata::tables::fingerprint::fingerprint_key(
        ctx.executor.caller_agent,
        context_id,
        &content_hash,
    );
    Ok(t.get(&key).ok().flatten().map(|g| g.value().memory_id()))
}

/// For each edge in the request, classify the outcome:
/// `Inserted` when the target memory exists, `TargetMissing` otherwise.
/// Runs in one rtxn so all edge targets see a consistent metadata
/// view; the corresponding Link phases skip TargetMissing edges.
fn compute_edge_outcomes(
    ctx: &OpsContext,
    req: &EncodeRequest,
) -> Result<Vec<EdgeOutcome>, OpError> {
    if req.edges.is_empty() {
        return Ok(Vec::new());
    }
    let db_guard = ctx.executor.metadata.lock();
    let rtxn = db_guard.read_txn().map_err(|e| {
        OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
    })?;
    // Fresh shard with no memories yet → every edge target is missing
    // by definition; degrade gracefully rather than 500-ing.
    let mems_table = match rtxn.open_table(brain_metadata::tables::memory::MEMORIES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Ok(vec![EdgeOutcome::TargetMissing; req.edges.len()]);
        }
        Err(e) => {
            return Err(OpError::ExecError(
                brain_planner::ExecError::MetadataReadFailed(e.to_string()),
            ))
        }
    };
    Ok(req
        .edges
        .iter()
        .map(|edge| {
            let target = MemoryId::from(edge.target);
            if mems_table
                .get(target.to_be_bytes())
                .ok()
                .flatten()
                .is_some()
            {
                EdgeOutcome::Inserted
            } else {
                EdgeOutcome::TargetMissing
            }
        })
        .collect())
}

/// Hash of the encode request used for idempotency conflict
/// detection. Mirrors [`crate::state::idempotency::hash_encode_request`]
/// but operates directly on [`EncodeRequest`] so the non-TXN path can
/// stamp the writer's cache without first building an EncodeOp.
fn encode_request_hash(
    req: &EncodeRequest,
    embedding_model_fp: [u8; 16],
    agent: brain_core::AgentId,
) -> [u8; 32] {
    let op = brain_planner::EncodeOp {
        request_id: brain_core::RequestId::from(req.request_id),
        context_id: ContextId::from(req.context_id),
        kind: MemoryKind::from(req.kind),
        text: req.text.clone(),
        vector: [0.0; brain_embed::VECTOR_DIM],
        salience_initial: req.salience_hint,
        fingerprint: embedding_model_fp,
        edges: req
            .edges
            .iter()
            .map(|e| brain_planner::EncodeOpEdge {
                target: MemoryId::from(e.target),
                kind: EdgeKind::from(e.kind),
                weight: e.weight,
            })
            .collect(),
        deduplicate: req.deduplicate,
        content_hash: *blake3::hash(req.text.as_bytes()).as_bytes(),
        agent_id: agent,
    };
    crate::state::idempotency::hash_encode_request(&op)
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Build the response for an idempotency-replay hit. The cached
/// `WriteAck` carries the original memory_id (in phase_acks[0]) and
/// LSN; the `created_at` field is recovered by reading the row from
/// `MEMORIES_TABLE` since the apply stamped it there. Everything else
/// is deterministic from the request.
fn reconstruct_encode_response(
    ctx: &OpsContext,
    req: &EncodeRequest,
    cached: &crate::write::WriteAck,
    salience: f32,
    embedding_model_fp: [u8; 16],
) -> Result<EncodeResponse, OpError> {
    let memory_id = match cached.phase_acks.first() {
        Some(PhaseAck::UpsertedMemory(id)) => *id,
        _ => {
            return Err(OpError::Internal(
                "idempotency cache hit but phase_acks[0] is not UpsertedMemory".into(),
            ));
        }
    };
    let auto_edges_added = cached
        .phase_acks
        .iter()
        .filter(|a| matches!(a, PhaseAck::Linked))
        .count() as u32;

    // Recover the original created_at by reading the row. Cache hits
    // are rare; the extra read is cheaper than carrying created_at on
    // every PhaseAck for this one case.
    let created_at = {
        let db_guard = ctx.executor.metadata.lock();
        let rtxn = db_guard.read_txn().map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;
        let t = rtxn
            .open_table(brain_metadata::tables::memory::MEMORIES_TABLE)
            .map_err(|e| {
                OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
            })?;
        t.get(memory_id.to_be_bytes())
            .ok()
            .flatten()
            .map(|g| g.value().created_at_unix_nanos)
            .unwrap_or(0)
    };

    let pending_stages = cached
        .pending_stages
        .iter()
        .filter(|s| s.memory_id == memory_id)
        .map(|s| s.stage_kind)
        .collect();

    Ok(EncodeResponse {
        memory_id: memory_id.into(),
        was_deduplicated: false,
        salience,
        auto_edges_added,
        lsn: cached.lsn_first.raw(),
        agent_id: ctx.executor.caller_agent.into(),
        context_id: req.context_id,
        kind: req.kind,
        created_at_unix_nanos: created_at,
        edges_out_count: auto_edges_added,
        embedding_model_fp,
        pending_stages,
        has_active_schema: ctx.schema_gate.is_declared(),
        has_llm_extractor: ctx.extractor_registry.read().has_enabled_llm_extractor(),
    })
}

async fn handle_encode_in_txn(
    req: EncodeRequest,
    txn_id: [u8; 16],
    ctx: &OpsContext,
) -> Result<EncodeResponse, OpError> {
    // 1. Validate via the planner first — same input checks the non-
    //    txn path runs (text length, salience range, edge cap, kind).
    let plan = plan_encode_inner(&req, &ctx.planner_ctx)?;
    let salience = plan.wal_append.salience_initial;
    let _ = plan;

    // 2. Validate the txn is Active.
    let _ = ctx.txn_store.validate_active(txn_id)?;

    // 3. Build an EncodeOp shape for hashing (matches the non-txn
    //    idempotency hash so a cross-txn replay surfaces conflicts).
    let request_hash = {
        let op = brain_planner::EncodeOp {
            request_id: brain_core::RequestId::from(req.request_id),
            context_id: ContextId::from(req.context_id),
            kind: MemoryKind::from(req.kind),
            text: req.text.clone(),
            vector: [0.0; brain_embed::VECTOR_DIM],
            salience_initial: req.salience_hint,
            fingerprint: ctx.executor.embedder.fingerprint(),
            edges: req
                .edges
                .iter()
                .map(|e| brain_planner::EncodeOpEdge {
                    target: MemoryId::from(e.target),
                    kind: EdgeKind::from(e.kind),
                    weight: e.weight,
                })
                .collect(),
            deduplicate: req.deduplicate,
            content_hash: *blake3::hash(req.text.as_bytes()).as_bytes(),
            agent_id: ctx.executor.caller_agent,
        };
        hash_encode_request(&op)
    };

    // 4. Intra-txn replay check.
    let replay = ctx.txn_store.with_buffer(txn_id, |buf| {
        if let Some(prior_hash) = buf.request_hashes.get(&req.request_id) {
            if prior_hash != &request_hash {
                return Err(OpError::Conflict(format!(
                    "encode in-txn request_id replay with different params: txn={}",
                    hex_short(&txn_id)
                )));
            }
            // Same request → return cached preview.
            if let Some(BufferedReplay::Encode {
                memory_id,
                edge_outcomes,
            }) = buf.request_id_cache.get(&req.request_id)
            {
                let auto = edge_outcomes
                    .iter()
                    .filter(|o| matches!(o, EdgeOutcome::Inserted))
                    .count() as u32;
                return Ok(Some((*memory_id, auto)));
            }
        }
        Ok(None)
    })?;
    if let Some((memory_id, auto_edges_added)) = replay {
        return Ok(EncodeResponse {
            memory_id: memory_id.into(),
            // Intra-txn request_id replay is idempotency, not
            // dedup. Per, idempotency replay is
            // transparent to the caller — surface whatever the
            // original response would have carried. The original
            // was a buffered encode (no dedup hit possible during
            // a txn in v1; in-txn dedup would require cross-encode
            // coordination), so `false` is correct.
            was_deduplicated: false,
            salience,
            auto_edges_added,
            // Buffered ops aren't WAL'd until TXN_COMMIT; LSN is
            // unknown at this point — the COMMIT-time ack carries
            // it. Clients chaining subscribe-from-encode inside a
            // txn must subscribe after COMMIT instead.
            lsn: 0,
            agent_id: ctx.executor.caller_agent.into(),
            context_id: req.context_id,
            kind: req.kind,
            created_at_unix_nanos: 0,
            edges_out_count: auto_edges_added,
            embedding_model_fp: ctx.executor.embedder.fingerprint(),
            // Buffered inside a txn — no background work has been
            // queued yet (workers fire post-commit). The COMMIT
            // ack carries the aggregated stages for the whole txn.
            pending_stages: Vec::new(),
            has_active_schema: ctx.schema_gate.is_declared(),
            has_llm_extractor: ctx.extractor_registry.read().has_enabled_llm_extractor(),
        });
    }

    // 5. Embed.
    let vector = ctx
        .executor
        .embedder
        .embed(&req.text)
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::EmbedFailed(e)))?;

    // 6. Reserve a MemoryId from the writer.
    let memory_id = ctx
        .executor
        .writer
        .reserve_memory_id()
        .await
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::WriterFailed(e)))?;
    let created_at = crate::txn::now_unix_nanos_pub();

    // 7. Compute edge outcomes against committed + in-buffer memories.
    //    The metadata read uses a fresh redb read txn; pending
    //    memories are checked against the buffer.
    let edge_outcomes: Vec<EdgeOutcome> = {
        let db_guard = ctx.executor.metadata.lock();
        let rtxn = db_guard.read_txn().map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;
        let mems_table = rtxn
            .open_table(brain_metadata::tables::memory::MEMORIES_TABLE)
            .map_err(|e| {
                OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
            })?;

        let pending_ids: std::collections::HashSet<MemoryId> =
            ctx.txn_store.with_buffer(txn_id, |buf| {
                Ok(buf.encodes.iter().map(|e| e.memory_id).collect())
            })?;

        req.edges
            .iter()
            .map(|edge| {
                let target = MemoryId::from(edge.target);
                let committed = mems_table
                    .get(target.to_be_bytes())
                    .map(|opt| opt.is_some())
                    .unwrap_or(false);
                if committed || pending_ids.contains(&target) {
                    EdgeOutcome::Inserted
                } else {
                    EdgeOutcome::TargetMissing
                }
            })
            .collect()
    };

    let auto_edges_added = edge_outcomes
        .iter()
        .filter(|o| matches!(o, EdgeOutcome::Inserted))
        .count() as u32;

    // 8. Build the BufferedEncode and push. Slot version comes from
    //    the reserved id (`reserve_memory_id` consults the
    //    slot-version table) so reclaimed-then-reused slots get the
    //    bumped version, not a hardcoded 1.
    let metadata = MemoryMetadata::new_active(
        memory_id,
        brain_core::AgentId(uuid::Uuid::nil()),
        ContextId::from(req.context_id),
        memory_id.slot(),
        memory_id.version(),
        MemoryKind::from(req.kind),
        ctx.executor.embedder.fingerprint(),
        salience,
        req.text.len() as u32,
        created_at,
    );

    let buffered = BufferedEncode {
        memory_id,
        metadata,
        text: req.text.clone(),
        vector,
        edges: req
            .edges
            .iter()
            .zip(edge_outcomes.iter())
            .filter_map(|(e, o)| {
                if matches!(o, EdgeOutcome::Inserted) {
                    Some(BufferedEdgeSpec {
                        target: MemoryId::from(e.target),
                        kind: EdgeKind::from(e.kind),
                        weight: e.weight,
                    })
                } else {
                    None
                }
            })
            .collect(),
        kind: MemoryKind::from(req.kind),
        context_id: ContextId::from(req.context_id),
        salience_initial: salience,
        fingerprint: ctx.executor.embedder.fingerprint(),
        request_id: req.request_id,
        request_hash,
        created_at_unix_nanos: created_at,
        agent_id: ctx.executor.caller_agent,
    };

    ctx.txn_store.with_buffer(txn_id, |buf| {
        buf.encodes.push(buffered);
        buf.request_hashes.insert(req.request_id, request_hash);
        buf.request_id_cache.insert(
            req.request_id,
            BufferedReplay::Encode {
                memory_id,
                edge_outcomes: edge_outcomes.clone(),
            },
        );
        Ok(())
    })?;

    Ok(EncodeResponse {
        memory_id: memory_id.into(),
        was_deduplicated: false,
        salience,
        auto_edges_added,
        // Buffered op — durable LSN lands at TXN_COMMIT.
        lsn: 0,
        agent_id: ctx.executor.caller_agent.into(),
        context_id: req.context_id,
        kind: req.kind,
        created_at_unix_nanos: created_at,
        edges_out_count: auto_edges_added,
        embedding_model_fp: ctx.executor.embedder.fingerprint(),
        // Workers fire post-commit; the COMMIT ack carries the
        // aggregated stages for the whole txn.
        pending_stages: Vec::new(),
        has_active_schema: ctx.schema_gate.is_declared(),
        has_llm_extractor: ctx.extractor_registry.read().has_enabled_llm_extractor(),
    })
}

fn hex_short(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(8);
    for b in &bytes[..4] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
