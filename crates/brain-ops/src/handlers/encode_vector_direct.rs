//! `ENCODE_VECTOR_DIRECT` handler.
//!
//! Power-user encode: the client supplies its own embedding vector
//! (typically because it runs a domain-specific or multi-modal model
//! that Brain doesn't host) along with a fingerprint identifying the
//! model. Brain skips its own embed step but still runs every
//! downstream validation, dedup, slot reservation, edge wiring, and
//! write submission. The wire response shape matches `ENCODE_RESP`
//! exactly — only the opcode differs.
//!
//! Up-front validation that's specific to this path:
//!
//! 1. `vector.len()` matches `brain_embed::VECTOR_DIM` (384 in v1).
//! 2. The vector is finite (no NaN / Inf) and L2-normalised within
//!    `+/- 1e-3` of unit norm. RECALL's cosine math assumes unit norm;
//!    a non-normalised vector would silently mis-rank.
//! 3. `model_fingerprint` matches the shard's currently-loaded
//!    embedder. Mismatched fingerprints produce memories that are
//!    unreachable from future text-cued recalls — refuse the write.
//!
//! TXN buffering is not supported on this path in v1: power users
//! drive the vector path from offline pipelines where transactions
//! aren't useful. A non-None `txn_id` is rejected so we don't
//! silently degrade the user's intent.

use brain_core::{ContextId, EdgeKind, EdgeKindRef, MemoryId, MemoryKind, NodeRef, Salience};
use brain_embed::VECTOR_DIM;
use brain_planner::{EdgeOutcome, EncodeOp, EncodeOpEdge};
use brain_protocol::envelope::request::EncodeVectorDirectRequest;
use brain_protocol::envelope::response::EncodeResponse;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::handlers::link::downcast_writer_pub;
use crate::state::idempotency::hash_encode_request;
use crate::write::{Phase, PhaseAck, Write, WriteId};

/// L2-norm tolerance window of `+/- 1e-3`, so client embedders that
/// round to f32 don't trip the unit-norm check.
const L2_NORM_TOLERANCE: f32 = 1.0e-3;

pub async fn handle_encode_vector_direct(
    req: EncodeVectorDirectRequest,
    ctx: &OpsContext,
) -> Result<EncodeResponse, OpError> {
    // 0. TXN path not supported on this opcode in v1.
    if req.txn_id.is_some() {
        return Err(OpError::InvalidRequest(
            "ENCODE_VECTOR_DIRECT does not support txn_id in v1; \
             submit the encode outside the transaction"
                .into(),
        ));
    }

    // 1. Shape validation borrowed from the text-encode path. We
    //    rebuild an EncodeRequest-shaped value for the planner so the
    //    same text-length / kind / salience / edge checks fire. The
    //    planner is pure (no I/O), so this is free.
    let salience = validate_common(&req, ctx)?;

    // 2. Vector-specific validation. Order matters: dim first (cheapest),
    //    then finite, then norm — each step assumes the prior held.
    if req.vector.len() != VECTOR_DIM {
        return Err(OpError::InvalidRequest(format!(
            "vector dimension {} does not match server dimension {VECTOR_DIM}",
            req.vector.len()
        )));
    }
    let norm_sq: f64 = req
        .vector
        .iter()
        .map(|x| {
            if !x.is_finite() {
                return f64::NAN;
            }
            f64::from(*x) * f64::from(*x)
        })
        .sum();
    if !norm_sq.is_finite() {
        return Err(OpError::InvalidRequest(
            "vector contains NaN or Inf elements".into(),
        ));
    }
    let norm = norm_sq.sqrt() as f32;
    if (norm - 1.0).abs() > L2_NORM_TOLERANCE {
        return Err(OpError::InvalidRequest(format!(
            "vector L2 norm {norm:.6} is outside the unit-norm \
             tolerance window [{lo:.6}, {hi:.6}]",
            lo = 1.0 - L2_NORM_TOLERANCE,
            hi = 1.0 + L2_NORM_TOLERANCE,
        )));
    }

    // 3. Fingerprint must match the shard's loaded model — otherwise
    //    the vector lands in an index keyed to a different fingerprint
    //    and future text-cued recalls cannot reach it.
    let server_fp = ctx.executor.embedder.fingerprint();
    if req.model_fingerprint != server_fp {
        return Err(OpError::InvalidRequest(format!(
            "model_fingerprint mismatch: client {:02x?}, server {:02x?}",
            &req.model_fingerprint[..4],
            &server_fp[..4],
        )));
    }

    // 4. Idempotency replay short-circuit. Same hash schema as
    //    text-encode so a request_id that hops between the two paths
    //    is detected as a conflict.
    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(brain_core::RequestId::from(req.request_id));
    let context_id = ContextId::from(req.context_id);
    let kind = MemoryKind::from(req.kind);
    // For dedup we need a stable content hash. When `text` is present
    // (the common case), hash it — that matches the text-encode path
    // and lets two clients dedup the same memory whether they sent
    // text or text+vector. When `text` is empty (pure multi-modal
    // upstream), fall back to hashing the vector bytes so the dedup
    // key is still stable.
    let content_hash = if req.text.is_empty() {
        hash_vector_bytes(&req.vector)
    } else {
        *blake3::hash(req.text.as_bytes()).as_bytes()
    };
    let request_hash =
        encode_vector_direct_request_hash(&req, server_fp, ctx.executor.caller_agent);
    match real_writer.idempotency_lookup(write_id, Some(request_hash)) {
        crate::writer::submit::CacheLookup::Hit(cached) => {
            return reconstruct_response(ctx, &req, &cached, salience, server_fp);
        }
        crate::writer::submit::CacheLookup::Conflict => {
            return Err(OpError::Conflict(format!(
                "encode_vector_direct request_id replay with different params: \
                 request_id={}",
                hex_short(&req.request_id),
            )));
        }
        crate::writer::submit::CacheLookup::Miss => {}
    }

    // 5. Dedup against the fingerprint table (same as text-encode).
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
                embedding_model_fp: server_fp,
                pending_stages: Vec::new(),
                has_active_schema: true,
            });
        }
    }

    // 6. Reserve a fresh MemoryId.
    let memory_id = ctx
        .executor
        .writer
        .reserve_memory_id()
        .await
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::WriterFailed(e)))?;
    let created_at = now_unix_nanos();

    // 7. Compute edge outcomes in one rtxn.
    let edge_outcomes = compute_edge_outcomes(ctx, &req)?;
    let auto_edges_added = edge_outcomes
        .iter()
        .filter(|o| matches!(o, EdgeOutcome::Inserted))
        .count() as u32;

    // 8. Build the Write. The supplied vector lands verbatim in the
    //    Phase::UpsertMemory — no re-embed.
    let mut vector_arr = [0.0f32; VECTOR_DIM];
    vector_arr.copy_from_slice(&req.vector);

    let mut phases: Vec<Phase> = Vec::with_capacity(1 + req.edges.len());
    phases.push(Phase::UpsertMemory {
        id: memory_id,
        text: req.text.clone(),
        vector: Box::new(vector_arr),
        kind,
        salience: Salience::new(salience),
        context: context_id,
        created_at_unix_nanos: created_at,
        // EncodeVectorDirect carries no event-time field on the wire;
        // power-user vector writes default the timeline to write time.
        occurred_at_unix_nanos: None,
        arena_slot: memory_id.slot(),
        embedding_model_fp: server_fp,
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

    // 9. Submit.
    let write = Write::from_phases(write_id, ctx.executor.caller_agent, phases)
        .with_request_hash(request_hash);
    let ack = real_writer
        .submit(write)
        .await
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::WriterFailed(e)))?;
    debug_assert!(matches!(ack.phase_acks[0], PhaseAck::UpsertedMemory(_)));

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
        embedding_model_fp: server_fp,
        pending_stages,
        has_active_schema: true,
    })
}

/// Run the input validations the vector-direct path requires: text
/// length, kind, salience range, edge cap, finite edge weights. Unlike
/// the shrunk client ENCODE, the admin/bulk-import vector-direct op
/// still carries explicit kind/salience/edges, so it validates them
/// directly via the planner's `validate_vector_direct`. Returns the
/// validated salience so the handler doesn't re-derive it.
fn validate_common(req: &EncodeVectorDirectRequest, ctx: &OpsContext) -> Result<f32, OpError> {
    // The planner rejects empty text. Power-user vector path may
    // genuinely have empty text (multi-modal upstream); substitute a
    // placeholder for the duration of validation when `text` is empty.
    // The real text — empty or not — still rides through to apply.
    let text = if req.text.is_empty() {
        "<vector-direct>"
    } else {
        req.text.as_str()
    };
    let edges: Vec<(MemoryId, brain_core::EdgeKind, f32)> = req
        .edges
        .iter()
        .map(|e| {
            (
                MemoryId::from(e.target),
                brain_core::EdgeKind::from(e.kind),
                e.weight,
            )
        })
        .collect();
    brain_planner::validate_vector_direct(
        text,
        MemoryKind::from(req.kind),
        req.salience_hint,
        &edges,
        &ctx.planner_ctx.config,
    )?;
    Ok(req.salience_hint)
}

fn lookup_fingerprint(
    ctx: &OpsContext,
    content_hash: [u8; 32],
    context_id: ContextId,
) -> Result<Option<MemoryId>, OpError> {
    let rtxn = ctx.executor.metadata.read_txn().map_err(|e| {
        OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
    })?;
    let t = rtxn
        .open_table(brain_metadata::tables::fingerprint::FINGERPRINTS_TABLE)
        .map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;
    let key = brain_metadata::tables::fingerprint::fingerprint_key(
        ctx.executor.caller_agent,
        context_id,
        &content_hash,
    );
    Ok(t.get(&key).ok().flatten().map(|g| g.value().memory_id()))
}

fn compute_edge_outcomes(
    ctx: &OpsContext,
    req: &EncodeVectorDirectRequest,
) -> Result<Vec<EdgeOutcome>, OpError> {
    if req.edges.is_empty() {
        return Ok(Vec::new());
    }
    let rtxn = ctx.executor.metadata.read_txn().map_err(|e| {
        OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
    })?;
    let mems_table = rtxn
        .open_table(brain_metadata::tables::memory::MEMORIES_TABLE)
        .map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;
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

/// Idempotency hash for vector-direct encode. Mirrors the text-encode
/// hash (so a request_id replay between the two paths surfaces a
/// conflict rather than silently producing two memories). The supplied
/// vector is not in the hash because the fingerprint already binds it
/// — a different vector under the same fingerprint and text would be
/// a pipeline-side bug we don't need to defend against at this layer.
fn encode_vector_direct_request_hash(
    req: &EncodeVectorDirectRequest,
    embedding_model_fp: [u8; 16],
    agent: brain_core::AgentId,
) -> [u8; 32] {
    let op = EncodeOp {
        request_id: brain_core::RequestId::from(req.request_id),
        context_id: ContextId::from(req.context_id),
        kind: MemoryKind::from(req.kind),
        text: req.text.clone(),
        vector: [0.0; VECTOR_DIM],
        salience_initial: req.salience_hint,
        fingerprint: embedding_model_fp,
        edges: req
            .edges
            .iter()
            .map(|e| EncodeOpEdge {
                target: MemoryId::from(e.target),
                kind: EdgeKind::from(e.kind),
                weight: e.weight,
            })
            .collect(),
        deduplicate: req.deduplicate,
        content_hash: if req.text.is_empty() {
            hash_vector_bytes(&req.vector)
        } else {
            *blake3::hash(req.text.as_bytes()).as_bytes()
        },
        agent_id: agent,
    };
    hash_encode_request(&op)
}

/// Hash a vector blob into the same `[u8; 32]` shape the dedup key
/// expects. Used when `req.text` is empty so the dedup key remains
/// stable across replays.
fn hash_vector_bytes(vector: &[f32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    for x in vector {
        h.update(&x.to_le_bytes());
    }
    *h.finalize().as_bytes()
}

fn reconstruct_response(
    ctx: &OpsContext,
    req: &EncodeVectorDirectRequest,
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

    let created_at = {
        let rtxn = ctx.executor.metadata.read_txn().map_err(|e| {
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
        has_active_schema: true,
    })
}

fn now_unix_nanos() -> u64 {
    crate::clock::now_unix_nanos()
}

fn hex_short(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(8);
    for b in &bytes[..4] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
