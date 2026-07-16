//! ENCODE handler — the write router.
//!
//! ENCODE expresses *intent*: the text to remember, where it belongs,
//! and when its content happened. The mechanical decisions — memory
//! `kind`, `salience`, whether the write deduplicates, and which edges
//! get wired — are decided here, server-side, not by the client. The
//! wire `EncodeRequest` no longer carries them.
//!
//! Without `txn_id`: validate + embed + reserve id + dedup check +
//! build a single-phase Write (UpsertMemory) and submit.
//! With `txn_id`: validate + embed + reserve a `MemoryId`, push to
//! the buffer, return a preview response. Writes happen at TXN_COMMIT.

use std::time::{Duration, Instant};

use brain_core::{ContextId, MemoryId, MemoryKind, Salience};
use brain_metadata::tables::memory::MemoryMetadata;
use brain_planner::plan_encode_inner;
use brain_protocol::envelope::request::EncodeRequest;
use brain_protocol::envelope::response::{
    EncodeResponse, EncodeTrace, EncodeTraceArtifacts, EncodeTraceDedup, EncodeTraceEntity,
    EncodeTraceIndex, EncodeTraceRelation, EncodeTraceStage, EncodeTraceStageStatus,
    EncodeTraceStatement,
};
use brain_protocol::StageKind;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::handlers::link::downcast_writer_pub;
use crate::txn::{BufferedEncode, BufferedReplay};
use crate::write::{Phase, PhaseAck, Write, WriteId};

/// Microseconds elapsed since `start`, saturating into `u64`.
fn elapsed_us(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// Human-readable phase name for an async [`StageKind`], matching the
/// synchronous phase-name vocabulary used in [`EncodeTrace::stages`].
fn stage_kind_name(kind: StageKind) -> &'static str {
    match kind {
        StageKind::AutoEdge => "auto_edge",
        StageKind::TemporalEdge => "temporal_edge",
        StageKind::Extractor => "extractor",
    }
}

/// Router-owned memory kind for an ENCODE. A real classifier is future
/// work; until then every text encode files as Episodic.
const DEFAULT_KIND: MemoryKind = MemoryKind::Episodic;

/// Router-owned salience floor for an ENCODE. The client no longer
/// supplies a hint; the write router decides it.
const DEFAULT_SALIENCE: f32 = 0.5;

/// Router policy: the write path stores faithfully — it does NOT collapse
/// distinct encodes that happen to share text. Exact request *replays* are
/// already deduplicated by `request_id` (idempotency); two genuinely
/// separate observations of the same text (e.g. the same fact re-stated at
/// a different `occurred_at`) are distinct memories and must both persist.
/// Redundancy across near-duplicate memories is reconciled asynchronously
/// by the near-dup consolidation worker, not by dropping writes here. The
/// DB owns this decision; the client cannot toggle it.
const ROUTER_DEDUPLICATE: bool = false;

#[tracing::instrument(name = "brain.encode", skip_all)]
pub async fn handle_encode(
    mut req: EncodeRequest,
    ctx: &OpsContext,
) -> Result<EncodeResponse, OpError> {
    if let Some(txn_id) = req.txn_id {
        return handle_encode_in_txn(req, txn_id, ctx).await;
    }

    // Opt-in synchronous write-analysis trace. When set we time each
    // synchronous phase and, after the durable write, wait for THIS write's
    // async stages to drain so the whole timeline comes back in one
    // response. When unset every `.then(...)` below is `None`, so the hot
    // path allocates nothing and takes no timestamps — `trace = false` is
    // byte-identical to before.
    let trace_enabled = req.trace;
    let pipeline_start = trace_enabled.then(Instant::now);
    let mut trace_stages: Vec<EncodeTraceStage> = Vec::new();
    // Subscribe to the stage bus BEFORE the write so no `StageCompleted`
    // published between submit and the drain can be missed (same
    // subscribe-first discipline the SUBSCRIBE registry uses).
    let mut stage_rx = trace_enabled.then(|| ctx.events.receiver());

    // 1. Input validation via the existing planner check. The plan now
    // reports the router's policy kind/salience (the client no longer
    // supplies them); we read them straight off the constants for
    // clarity and use the plan only as the validation gate.
    let t_validate = trace_enabled.then(Instant::now);
    let _plan = plan_encode_inner(&req, &ctx.planner_ctx)?;
    if let Some(start) = t_validate {
        trace_stages.push(EncodeTraceStage {
            name: "validate".into(),
            status: EncodeTraceStageStatus::Ok,
            latency_us: elapsed_us(start),
            detail: String::new(),
        });
    }
    let salience = DEFAULT_SALIENCE;

    // 1b. Idempotency replay short-circuit. The same
    // request_id arriving twice must return the original response. The
    // writer's idempotency cache is keyed by WriteId, but it lives
    // behind submit() — by then we've already burned embedding work
    // and a slot reservation. Peek it up-front so a replay is free.
    // A mismatched request hash on the same WriteId is a Conflict.
    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(
        brain_core::RequestId::from(req.request_id),
        ctx.executor.caller_agent,
    );
    let context_id = ContextId::from(req.context_id);
    let kind = DEFAULT_KIND;
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
    let t_embed = trace_enabled.then(Instant::now);
    let vector = ctx
        .executor
        .embedder
        .embed(&req.text)
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::EmbedFailed(e)))?;
    if let Some(start) = t_embed {
        trace_stages.push(EncodeTraceStage {
            name: "embed".into(),
            status: EncodeTraceStageStatus::Ok,
            latency_us: elapsed_us(start),
            detail: format!("dim={}", vector.len()),
        });
    }
    let content_hash = *blake3::hash(req.text.as_bytes()).as_bytes();

    // 3. Dedup check — DB policy is always-on dedup. Look up
    // (agent, context, content_hash). On hit, return the existing
    // memory id without submitting a Write.
    if ROUTER_DEDUPLICATE {
        if let Some(existing) = lookup_fingerprint(ctx, content_hash, context_id)? {
            return Ok(EncodeResponse {
                memory_id: existing.raw(),
                was_deduplicated: true,
                salience,
                auto_edges_added: 0,
                lsn: 0,
                agent_id: ctx.executor.caller_agent.into(),
                context_id: req.context_id,
                kind: kind.into(),
                created_at_unix_nanos: 0,
                edges_out_count: 0,
                embedding_model_fp,
                // Dedup hit — no fresh write, so no background stages
                // were queued. The client has nothing to wait for.
                pending_stages: Vec::new(),
                has_active_schema: true,
                trace: None,
            });
        }
    }

    // 4. Reserve a fresh MemoryId via the writer's slot allocator.
    let t_reserve = trace_enabled.then(Instant::now);
    let memory_id = ctx
        .executor
        .writer
        .reserve_memory_id()
        .await
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::WriterFailed(e)))?;
    if let Some(start) = t_reserve {
        trace_stages.push(EncodeTraceStage {
            name: "reserve".into(),
            status: EncodeTraceStageStatus::Ok,
            latency_us: elapsed_us(start),
            detail: format!("memory_id={}", memory_id.raw()),
        });
    }
    let created_at = now_unix_nanos();

    // Stage journal (S1 embed). The embed itself ran above, before the id
    // was reserved; we log it here so every write stage shares one
    // `memory_id` key and the eval probe can stitch a single write's
    // stages back together. Debug-level, so it costs nothing in prod.
    tracing::debug!(
        target: "brain_debug::stage",
        stage = "S1_embed",
        memory_id = memory_id.raw(),
        text_len = req.text.len(),
        vector_dim = vector.len(),
        "write stage: text embedded",
    );

    // 5. Build the single-phase Write: UpsertMemory. ENCODE carries no
    // client edges — auto/temporal-edge derivation is the workers' job,
    // enqueued post-commit by submit(). `req.text` is no longer read
    // after this point (the embedding ran off `&req.text` above and
    // `req.text` doesn't surface in the response). Move the string into
    // the phase instead of cloning it — clients can ship multi-KB
    // memories and that clone showed up in hot-path allocator traces.
    let auto_edges_added: u32 = 0;
    let phases: Vec<Phase> = vec![Phase::UpsertMemory {
        id: memory_id,
        text: std::mem::take(&mut req.text),
        vector: Box::new(vector),
        kind,
        salience: Salience::new(salience),
        context: context_id,
        created_at_unix_nanos: created_at,
        occurred_at_unix_nanos: req.occurred_at_unix_nanos,
        arena_slot: memory_id.slot(),
        embedding_model_fp,
        content_hash: if ROUTER_DEDUPLICATE {
            Some(content_hash)
        } else {
            None
        },
        deduplicate: ROUTER_DEDUPLICATE,
    }];

    // 6. Submit.
    let t_persist = trace_enabled.then(Instant::now);
    let write = Write::from_phases(write_id, ctx.executor.caller_agent, phases)
        .with_namespace(ctx.executor.caller_namespace)
        .with_request_hash(request_hash);
    let ack = real_writer
        .submit(write)
        .await
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::WriterFailed(e)))?;
    debug_assert!(matches!(ack.phase_acks[0], PhaseAck::UpsertedMemory(_)));
    if let Some(start) = t_persist {
        trace_stages.push(EncodeTraceStage {
            name: "persist".into(),
            status: EncodeTraceStageStatus::Ok,
            latency_us: elapsed_us(start),
            detail: format!("lsn={}", ack.lsn_first.raw()),
        });
    }

    // Stage journal (S2 WAL fsync + S3 arena/redb/HNSW persist). The ack
    // returning at all is the durability signal: WAL-before-ack means the
    // record is fsynced and the memory row + arena slot + HNSW point are
    // live. `lsn` is the durable log position; the extractor stages fire
    // asynchronously off `pending_stages`.
    tracing::debug!(
        target: "brain_debug::stage",
        stage = "S2_S3_durable",
        memory_id = memory_id.raw(),
        lsn = ack.lsn_first.raw(),
        "write stage: WAL durable + memory persisted",
    );

    // Project the write's pending background stages onto the wire
    // response. Clients waiting via `--wait` decrement this list as
    // `StageCompleted` events arrive on the subscribe stream. This is the
    // documented async-completion join key (lsn + pending_stages) and is
    // ALWAYS surfaced, trace or not — the synchronous drain below is an
    // additive convenience for opted-in callers, not a replacement.
    let pending_stages: Vec<StageKind> = ack
        .pending_stages
        .iter()
        .filter(|s| s.memory_id == memory_id)
        .map(|s| s.stage_kind)
        .collect();

    // Synchronous trace: wait for this write's async stages to drain, then
    // resolve the artifacts they produced, and assemble the full timeline.
    // `trace = false` skips this entirely (nothing timed, no bus receiver,
    // no wait).
    let trace = if trace_enabled {
        if let Some(rx) = stage_rx.as_mut() {
            await_stage_completions(
                memory_id,
                &pending_stages,
                ctx.encode_trace_drain_window,
                rx,
                &mut trace_stages,
            )
            .await;
        }
        let artifacts = build_encode_artifacts(ctx, memory_id, false, None);
        Some(EncodeTrace {
            stages: trace_stages,
            artifacts,
            total_latency_us: pipeline_start.map(elapsed_us).unwrap_or(0),
        })
    } else {
        None
    };

    Ok(EncodeResponse {
        memory_id: memory_id.into(),
        was_deduplicated: false,
        salience,
        auto_edges_added,
        lsn: ack.lsn_first.raw(),
        agent_id: ctx.executor.caller_agent.into(),
        context_id: req.context_id,
        kind: kind.into(),
        created_at_unix_nanos: created_at,
        edges_out_count: auto_edges_added,
        embedding_model_fp,
        pending_stages,
        has_active_schema: true,
        trace,
    })
}

/// Wait for THIS write's queued async stages (`pending`) to publish their
/// `StageCompleted` events on the per-shard bus, appending one
/// [`EncodeTraceStage`] per completion in arrival order. Bounded by
/// `drain_window` (`OpsContext::encode_trace_drain_window`): any stage still
/// outstanding when the deadline fires is appended as `Timeout` (its event
/// still flows through SUBSCRIBE later) so the ENCODE never hangs. The wait
/// is event-driven and returns the instant the last stage completes, so the
/// window bounds only a stalled worker, not the happy path. Filters strictly
/// on `memory_id` so a concurrent write's stages can't be mis-attributed.
///
/// The Glommio-timer race mirrors [`crate::handlers::subscribe`]: the
/// receiver is a `tokio::sync::broadcast` channel polled with
/// `futures_lite`, and the deadline uses the shard timer. The non-Linux
/// build has no shard timer, so it records the queued stages as `Timeout`
/// immediately rather than blocking.
#[cfg(target_os = "linux")]
async fn await_stage_completions(
    memory_id: MemoryId,
    pending: &[StageKind],
    drain_window: Duration,
    rx: &mut tokio::sync::broadcast::Receiver<crate::subscribe::EventEnvelope>,
    stages: &mut Vec<EncodeTraceStage>,
) {
    use brain_protocol::envelope::response::EventType;
    use futures_lite::FutureExt;
    use tokio::sync::broadcast::error::RecvError;

    let stage_start = Instant::now();
    let deadline = stage_start + drain_window;
    let mut remaining: Vec<StageKind> = pending.to_vec();

    while !remaining.is_empty() {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let wait = deadline - now;
        let recv_arm = async { Some(rx.recv().await) };
        let timer_arm = async {
            glommio::timer::sleep(wait).await;
            None
        };
        match recv_arm.or(timer_arm).await {
            Some(Ok(env)) => {
                if env.event_type != EventType::StageCompleted || env.memory_id != memory_id {
                    continue;
                }
                let Some(kind) = env.stage_kind else { continue };
                let Some(pos) = remaining.iter().position(|k| *k == kind) else {
                    continue;
                };
                remaining.remove(pos);
                stages.push(EncodeTraceStage {
                    name: stage_kind_name(kind).into(),
                    status: stage_status_from_env(&env),
                    latency_us: elapsed_us(stage_start),
                    detail: stage_detail_from_env(&env),
                });
            }
            // A lagged trace subscriber just retries — the write is already
            // durable, this is best-effort observability.
            Some(Err(RecvError::Lagged(_))) => continue,
            Some(Err(RecvError::Closed)) => break,
            // Timer fired first.
            None => break,
        }
    }

    for kind in remaining {
        stages.push(EncodeTraceStage {
            name: stage_kind_name(kind).into(),
            status: EncodeTraceStageStatus::Timeout,
            latency_us: 0,
            detail: "stage did not complete within the trace wait window".into(),
        });
    }
}

#[cfg(not(target_os = "linux"))]
async fn await_stage_completions(
    _memory_id: MemoryId,
    pending: &[StageKind],
    _drain_window: Duration,
    _rx: &mut tokio::sync::broadcast::Receiver<crate::subscribe::EventEnvelope>,
    stages: &mut Vec<EncodeTraceStage>,
) {
    // No shard timer off Linux (Brain runs on Linux); record the queued
    // stages as Timeout so the trace shape is identical without blocking.
    for kind in pending {
        stages.push(EncodeTraceStage {
            name: stage_kind_name(*kind).into(),
            status: EncodeTraceStageStatus::Timeout,
            latency_us: 0,
            detail: "stage drain unavailable off Linux".into(),
        });
    }
}

/// Map a drained stage's `StageOutcome` to the trace status. `Empty`
/// (ran-but-produced-nothing) is `Ok` for trace purposes — the stage
/// completed cleanly; the empty artifact lists tell the rest of the story.
#[cfg(target_os = "linux")]
fn stage_status_from_env(env: &crate::subscribe::EventEnvelope) -> EncodeTraceStageStatus {
    use brain_protocol::StageOutcome;
    match env.stage_outcome {
        Some(StageOutcome::Failed) => EncodeTraceStageStatus::Failed,
        _ => EncodeTraceStageStatus::Ok,
    }
}

/// Render a drained stage's payload into a human-readable `detail` string:
/// produced counts for the extractor, edge counts for the edge stages.
#[cfg(target_os = "linux")]
fn stage_detail_from_env(env: &crate::subscribe::EventEnvelope) -> String {
    use brain_protocol::StagePayload;
    match &env.stage_payload {
        Some(StagePayload::Extractor(p)) => {
            let mut s = format!(
                "entities={} statements={} relations={} audit={:?}",
                p.entity_count, p.statement_count, p.relation_count, p.audit_status
            );
            if !p.error_message.is_empty() {
                s.push_str(&format!(" error={}", p.error_message));
            }
            s
        }
        Some(StagePayload::AutoEdge(p)) => format!("edges={}", p.edges_written),
        Some(StagePayload::TemporalEdge(p)) => format!("edges={}", p.edges_written),
        None => String::new(),
    }
}

/// Resolve the typed-graph rows + index state this write produced into the
/// trace's `artifacts` section. Entities / statements / relations are read
/// back through the same enrichment resolver RECALL uses (`include_graph`),
/// so the content is real (canonical names, predicates, confidences), not
/// just counts. Index membership is derived from what the write wired
/// rather than probed — the memory always lands in the HNSW, text indexing
/// depends on the dispatchers being provisioned, and statement indexing
/// only applies when statements were produced.
fn build_encode_artifacts(
    ctx: &OpsContext,
    memory_id: MemoryId,
    was_deduplicated: bool,
    matched_memory_id: Option<MemoryId>,
) -> EncodeTraceArtifacts {
    let (entities, statements, relations) = match ctx.executor.metadata.read_txn() {
        Ok(rtxn) => {
            let scope = brain_metadata::RowScope::new(
                ctx.executor.caller_namespace,
                ctx.executor.caller_agent,
            );
            match crate::handlers::recall::fetch_enrichment_for(&[memory_id], scope, &rtxn) {
                Ok(mut enr) => {
                    let g = enr.pop().unwrap_or_else(|| {
                        brain_protocol::envelope::response::GraphEnrichment {
                            entities: Vec::new(),
                            statements: Vec::new(),
                            relations: Vec::new(),
                        }
                    });
                    let entities: Vec<EncodeTraceEntity> = g
                        .entities
                        .into_iter()
                        .map(|e| EncodeTraceEntity {
                            id: e.id,
                            name: e.name,
                            type_qname: e.type_qname,
                        })
                        .collect();
                    let statements: Vec<EncodeTraceStatement> = g
                        .statements
                        .into_iter()
                        .map(|s| EncodeTraceStatement {
                            id: s.id,
                            subject_name: s.subject_name,
                            predicate: s.predicate,
                            object_name: s.object_label,
                            confidence: s.confidence,
                        })
                        .collect();
                    let relations: Vec<EncodeTraceRelation> = g
                        .relations
                        .into_iter()
                        .map(|r| EncodeTraceRelation {
                            source_name: r.from_name,
                            predicate: r.predicate,
                            target_name: r.to_name,
                        })
                        .collect();
                    (entities, statements, relations)
                }
                Err(_) => (Vec::new(), Vec::new(), Vec::new()),
            }
        }
        Err(_) => (Vec::new(), Vec::new(), Vec::new()),
    };

    let ok_or_skip = |present: bool| {
        if present {
            EncodeTraceStageStatus::Ok
        } else {
            EncodeTraceStageStatus::Skipped
        }
    };
    let has_statements = !statements.is_empty();
    let indexes = vec![
        // The memory HNSW is mandatory and always receives the new vector.
        EncodeTraceIndex {
            name: "memory_hnsw".into(),
            status: EncodeTraceStageStatus::Ok,
        },
        EncodeTraceIndex {
            name: "memory_text".into(),
            status: ok_or_skip(ctx.memory_text_dispatcher.is_some()),
        },
        EncodeTraceIndex {
            name: "statement_text".into(),
            status: ok_or_skip(has_statements && ctx.statement_text_dispatcher.is_some()),
        },
    ];

    EncodeTraceArtifacts {
        entities,
        statements,
        relations,
        indexes,
        dedup: EncodeTraceDedup {
            was_deduplicated,
            matched_memory_id: matched_memory_id.map(|m| m.to_be_bytes()),
        },
    }
}

/// Look up a content-hash fingerprint to deduplicate against an
/// existing memory. Returns `Some(MemoryId)` if a
/// row exists for `(caller_agent, context, content_hash)`.
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

/// Hash of the encode request used for idempotency conflict
/// detection. Mirrors [`crate::state::idempotency::hash_encode_request`]
/// but operates directly on [`EncodeRequest`] so the non-TXN path can
/// stamp the writer's cache without first building an EncodeOp.
///
/// The router-decided machinery (kind / salience / dedup / edges) is
/// fixed policy, so it contributes constant bytes to the hash. The hash
/// still distinguishes encodes by text / context / fingerprint, which is
/// what idempotency-conflict detection needs.
fn encode_request_hash(
    req: &EncodeRequest,
    embedding_model_fp: [u8; 16],
    agent: brain_core::AgentId,
) -> [u8; 32] {
    let op = brain_planner::EncodeOp {
        request_id: brain_core::RequestId::from(req.request_id),
        context_id: ContextId::from(req.context_id),
        kind: DEFAULT_KIND,
        text: req.text.clone(),
        vector: [0.0; brain_embed::VECTOR_DIM],
        salience_initial: DEFAULT_SALIENCE,
        fingerprint: embedding_model_fp,
        edges: Vec::new(),
        deduplicate: ROUTER_DEDUPLICATE,
        content_hash: *blake3::hash(req.text.as_bytes()).as_bytes(),
        agent_id: agent,
    };
    crate::state::idempotency::hash_encode_request(&op)
}

fn now_unix_nanos() -> u64 {
    crate::clock::now_unix_nanos()
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
        kind: DEFAULT_KIND.into(),
        created_at_unix_nanos: created_at,
        edges_out_count: auto_edges_added,
        embedding_model_fp,
        pending_stages,
        has_active_schema: true,
        trace: None,
    })
}

async fn handle_encode_in_txn(
    req: EncodeRequest,
    txn_id: [u8; 16],
    ctx: &OpsContext,
) -> Result<EncodeResponse, OpError> {
    // 1. Validate via the planner first — same input check the non-txn
    //    path runs (text non-empty + size cap). Kind/salience/dedup are
    //    router policy, not client input.
    let _plan = plan_encode_inner(&req, &ctx.planner_ctx)?;
    let salience = DEFAULT_SALIENCE;

    // 2. Validate the txn is Active.
    let _ = ctx.txn_store.validate_active(txn_id)?;

    // 3. Build an EncodeOp shape for hashing (matches the non-txn
    //    idempotency hash so a cross-txn replay surfaces conflicts).
    //    The router-decided fields are constant policy.
    let request_hash = encode_request_hash(
        &req,
        ctx.executor.embedder.fingerprint(),
        ctx.executor.caller_agent,
    );

    // 4. Intra-txn replay check.
    let replay = ctx.txn_store.with_buffer(txn_id, |buf| {
        if let Some(prior_hash) = buf.request_hashes.get(&req.request_id) {
            if prior_hash != &request_hash {
                return Err(OpError::Conflict(format!(
                    "encode in-txn request_id replay with different params: txn={}",
                    hex_short(&txn_id)
                )));
            }
            // Same request → return cached preview. ENCODE carries no
            // client edges, so the replayed auto-edge count is always 0.
            if let Some(BufferedReplay::Encode { memory_id, .. }) =
                buf.request_id_cache.get(&req.request_id)
            {
                return Ok(Some((*memory_id, 0u32)));
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
            kind: DEFAULT_KIND.into(),
            created_at_unix_nanos: 0,
            edges_out_count: auto_edges_added,
            embedding_model_fp: ctx.executor.embedder.fingerprint(),
            // Buffered inside a txn — no background work has been
            // queued yet (workers fire post-commit). The COMMIT
            // ack carries the aggregated stages for the whole txn.
            pending_stages: Vec::new(),
            has_active_schema: true,
            trace: None,
        });
    }

    // 4a. Reject the 1001st op now — after the replay-cache miss
    //     (an idempotent re-submit against a full buffer must still
    //     replay) but before we burn embed + writer-reserve work on a
    //     doomed buffer.
    ctx.txn_store
        .with_buffer(txn_id, |buf| buf.check_capacity_for_push())?;

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

    // 7. ENCODE carries no client edges — the auto/temporal-edge
    //    workers derive edges post-commit. The buffer therefore stores
    //    zero edges; the COMMIT path runs unchanged with an empty list.
    let auto_edges_added: u32 = 0;

    // 8. Build the BufferedEncode and push. Slot version comes from
    //    the reserved id (`reserve_memory_id` consults the
    //    slot-version table) so reclaimed-then-reused slots get the
    //    bumped version, not a hardcoded 1.
    let metadata = MemoryMetadata::new_active(
        memory_id,
        ctx.executor.caller_namespace,
        brain_core::AgentId(uuid::Uuid::nil()),
        ContextId::from(req.context_id),
        memory_id.slot(),
        memory_id.version(),
        DEFAULT_KIND,
        ctx.executor.embedder.fingerprint(),
        salience,
        req.text.len() as u32,
        created_at,
    )
    .with_occurred_at(req.occurred_at_unix_nanos);

    let buffered = BufferedEncode {
        memory_id,
        metadata,
        text: req.text.clone(),
        vector,
        edges: Vec::new(),
        kind: DEFAULT_KIND,
        context_id: ContextId::from(req.context_id),
        salience_initial: salience,
        fingerprint: ctx.executor.embedder.fingerprint(),
        request_id: req.request_id,
        request_hash,
        created_at_unix_nanos: created_at,
        occurred_at_unix_nanos: req.occurred_at_unix_nanos,
        agent_id: ctx.executor.caller_agent,
    };

    ctx.txn_store.with_buffer(txn_id, |buf| {
        buf.encodes.push(buffered);
        buf.request_hashes.insert(req.request_id, request_hash);
        buf.request_id_cache.insert(
            req.request_id,
            BufferedReplay::Encode {
                memory_id,
                edge_outcomes: Vec::new(),
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
        kind: DEFAULT_KIND.into(),
        created_at_unix_nanos: created_at,
        edges_out_count: auto_edges_added,
        embedding_model_fp: ctx.executor.embedder.fingerprint(),
        // Workers fire post-commit; the COMMIT ack carries the
        // aggregated stages for the whole txn.
        pending_stages: Vec::new(),
        has_active_schema: true,
        trace: None,
    })
}

fn hex_short(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(8);
    for b in &bytes[..4] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
