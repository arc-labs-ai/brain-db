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

use brain_core::{MemoryId, MemoryKind, Salience, SessionId};
use brain_metadata::tables::memory::MemoryMetadata;
use brain_planner::plan_encode_inner;
use brain_protocol::envelope::request::EncodeRequest;
use brain_protocol::envelope::response::{
    EncodeResponse, EncodeStageArtifact, EncodeStageGraph, EncodeStageRecord, EncodeTrace,
    EncodeTraceArtifacts, EncodeTraceDedup, EncodeTraceEntity, EncodeTraceIndex,
    EncodeTraceRelation, EncodeTraceStage, EncodeTraceStageStatus, EncodeTraceStatement,
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
        StageKind::Hype => "hype",
    }
}

/// Router-owned memory kind for an ENCODE. A real classifier is future
/// work; until then every text encode files as Episodic.
const DEFAULT_KIND: MemoryKind = MemoryKind::Episodic;

/// Router-owned salience floor for an ENCODE. The client no longer
/// supplies a hint; the write router decides it.
const DEFAULT_SALIENCE: f32 = 0.5;

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
    // Write-completion mode: `Derived` blocks for async derivation + returns
    // the full trace; `Ack` (default) returns after the durable sync ack. This
    // is the single write knob (the `trace` bool is reads-only).
    let trace_enabled = req.wait == brain_protocol::WaitMode::Derived;
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
            artifact: None,
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
        ctx.executor.caller_space,
    );
    let session_id = SessionId::from(req.session_id);
    let kind = DEFAULT_KIND;
    let embedding_model_fp = ctx.executor.embedder.fingerprint();
    let request_hash = encode_request_hash(&req, embedding_model_fp, ctx.executor.caller_space);
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
    // Captured before `vector`/`req.text` are moved into the write phase, so
    // the persist stage's record artifact can report the stored dimensions.
    let vector_dim = vector.len();
    let text_len = req.text.len();
    if let Some(start) = t_embed {
        // embed → the actual embedding this stage produced. Trace-only clone
        // (the vector is moved into the write phase below); skipped entirely
        // when `trace = false`.
        trace_stages.push(EncodeTraceStage {
            name: "embed".into(),
            status: EncodeTraceStageStatus::Ok,
            latency_us: elapsed_us(start),
            detail: format!("dim={}", vector.len()),
            artifact: Some(EncodeStageArtifact {
                vector: vector.to_vec(),
                ..Default::default()
            }),
        });
    }
    let content_hash = *blake3::hash(req.text.as_bytes()).as_bytes();

    // Content dedup is on by default; the client can opt out per request
    // (`allow_duplicates`) to force a distinct memory for byte-identical text.
    let deduplicate = !req.allow_duplicates;

    // 3. Dedup check — default policy is content dedup. Look up
    // (space, context, content_hash). On hit, return the existing
    // memory id without submitting a Write.
    if deduplicate {
        if let Some(existing) = lookup_fingerprint(ctx, content_hash, session_id)? {
            return Ok(EncodeResponse {
                memory_id: existing.raw(),
                was_deduplicated: true,
                salience,
                auto_edges_added: 0,
                lsn: 0,
                space_id: ctx.executor.caller_space.into(),
                session_id: req.session_id,
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
            artifact: None,
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
        session_id,
        created_at_unix_nanos: created_at,
        occurred_at_unix_nanos: req.occurred_at_unix_nanos,
        arena_slot: memory_id.slot(),
        embedding_model_fp,
        content_hash: if deduplicate {
            Some(content_hash)
        } else {
            None
        },
        deduplicate,
    }];

    // 6. Submit.
    let t_persist = trace_enabled.then(Instant::now);
    let write = Write::from_phases(write_id, ctx.executor.caller_space, phases)
        .with_namespace(ctx.executor.caller_namespace)
        .with_space_string(ctx.executor.caller_space_string.clone())
        .with_request_hash(request_hash);
    let ack = real_writer
        .submit(write)
        .await
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::WriterFailed(e)))?;
    debug_assert!(matches!(ack.phase_acks[0], PhaseAck::UpsertedMemory(_)));
    if let Some(start) = t_persist {
        // persist → the durable metadata row this stage wrote, carrying the
        // real WAL log-sequence number the write landed at.
        let kind_byte = match kind {
            MemoryKind::Episodic => 0,
            MemoryKind::Semantic => 1,
            MemoryKind::Consolidated => 2,
        };
        trace_stages.push(EncodeTraceStage {
            name: "persist".into(),
            status: EncodeTraceStageStatus::Ok,
            latency_us: elapsed_us(start),
            detail: format!("lsn={}", ack.lsn_first.raw()),
            artifact: Some(EncodeStageArtifact {
                record: Some(EncodeStageRecord {
                    memory_id: memory_id.to_be_bytes(),
                    kind: kind_byte,
                    salience,
                    created_at_unix_nanos: created_at,
                    occurred_at_unix_nanos: req.occurred_at_unix_nanos.unwrap_or(0),
                    vector_dim: vector_dim as u32,
                    text_len: text_len as u32,
                    lsn: ack.lsn_first.raw(),
                }),
                ..Default::default()
            }),
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
        // extractor → the typed graph this write produced. Attach it to the
        // async `extractor` stage so the trace is per-stage (embed→vector,
        // persist→record, extractor→graph), mirroring the durable bundle the
        // extractor worker persists for later MEMORY_INSPECT. HyPE's own
        // `hype` stage is folded into `pending_stages` alongside auto_edge /
        // temporal_edge / extractor (it runs over every item the extractor
        // batch processes, so it's enqueued 1:1 with `extractor`) and was
        // already awaited by `await_stage_completions` above; by the time
        // that event fires the HyPE questions are already durable
        // (`HypeGenerator::generate_for` persists before publishing), so
        // reading the bundle now sees them.
        let bundle =
            crate::memory_artifact::read_memory_artifact(ctx.executor.metadata.as_ref(), memory_id)
                .ok()
                .flatten();

        // extractor → the derived knowledge graph.
        let graph = encode_artifacts_to_graph(&artifacts);
        if let Some(stage) = trace_stages
            .iter_mut()
            .find(|s| s.name == stage_kind_name(StageKind::Extractor))
        {
            stage.artifact = Some(EncodeStageArtifact {
                graph: Some(graph),
                ..Default::default()
            });
        }
        // persist → also carry the analyzed keyword terms (the exact tokens the
        // memory_text index matches on), read from the durable bundle.
        if let Some(kw) = bundle
            .as_ref()
            .map(|b| b.keyword_fields.clone())
            .filter(|k| !k.is_empty())
        {
            if let Some(stage) = trace_stages.iter_mut().find(|s| s.name == "persist") {
                match stage.artifact.as_mut() {
                    Some(art) => art.keyword_fields = kw,
                    None => {
                        stage.artifact = Some(EncodeStageArtifact {
                            keyword_fields: kw,
                            ..Default::default()
                        });
                    }
                }
            }
        }
        // hype → the hypothetical questions the write-time HyPE worker
        // generated, read back from the durable bundle and attached to the
        // `hype` trace stage `await_stage_completions` already recorded
        // above (real `Ok`/`Empty`/`Timeout` status from the genuine
        // `StageCompleted{Hype}` event, not a synthetic one). Absent
        // entirely when HyPE was never enqueued for this write (extraction
        // itself didn't enqueue — see the `pending_stages` construction in
        // `writer::submit`), same as any other stage that never queued.
        if let Some(hype) = bundle
            .as_ref()
            .map(|b| b.hype_questions.clone())
            .filter(|h| !h.is_empty())
        {
            if let Some(stage) = trace_stages
                .iter_mut()
                .find(|s| s.name == stage_kind_name(StageKind::Hype))
            {
                stage.artifact = Some(EncodeStageArtifact {
                    hype_questions: hype,
                    ..Default::default()
                });
            }
        }

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
        space_id: ctx.executor.caller_space.into(),
        session_id: req.session_id,
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
                    artifact: None,
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
            artifact: None,
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
            artifact: None,
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
        Some(StagePayload::Hype(p)) => format!("questions={}", p.questions_written),
        None => String::new(),
    }
}

/// Fold the trace's already-resolved [`EncodeTraceArtifacts`] (entities /
/// statements / relations) into a renderable [`EncodeStageGraph`] for the
/// `extractor` stage's per-stage artifact. Entities become nodes (they carry
/// ids); statement objects and relation endpoints are matched back to those
/// node ids by canonical name. A statement object that isn't a mentioned
/// entity in this write is a literal value (e.g. "manages **the billing
/// platform team**") — it gets a synthetic `"literal"` node (deduped by id
/// within this call) instead of the all-zero placeholder, so the rendered
/// graph carries the real value. A relation endpoint beyond the enrichment
/// cap still falls back to the zero id — relations are always entity-to-
/// entity by schema, so an unresolved endpoint there is a cap miss, not a
/// literal. The synthetic id comes from the shared
/// [`literal_node_id`](crate::memory_artifact::literal_node_id), so this live
/// trace and the durable `MEMORY_INSPECT` bundle agree on one id per fact.
fn encode_artifacts_to_graph(artifacts: &EncodeTraceArtifacts) -> EncodeStageGraph {
    use brain_protocol::envelope::response::{EncodeGraphEdge, EncodeGraphNode};

    use crate::memory_artifact::literal_node_id;

    let mut nodes: Vec<EncodeGraphNode> = artifacts
        .entities
        .iter()
        .map(|e| EncodeGraphNode {
            id: e.id,
            name: e.name.clone(),
            kind: "entity".to_string(),
            type_qname: e.type_qname.clone(),
        })
        .collect();

    let id_by_name: std::collections::HashMap<&str, [u8; 16]> = artifacts
        .entities
        .iter()
        .map(|e| (e.name.as_str(), e.id))
        .collect();
    let lookup = |name: &str| id_by_name.get(name).copied();

    let mut seen_literals: std::collections::HashSet<[u8; 16]> = std::collections::HashSet::new();
    let mut edges: Vec<EncodeGraphEdge> = Vec::new();
    for s in &artifacts.statements {
        let source = lookup(&s.subject_name).unwrap_or([0u8; 16]);
        let target = match lookup(&s.object_name) {
            Some(id) => id,
            None => {
                let lit_id = literal_node_id(&source, &s.predicate, &s.object_name);
                if seen_literals.insert(lit_id) {
                    nodes.push(EncodeGraphNode {
                        id: lit_id,
                        name: s.object_name.clone(),
                        kind: "literal".to_string(),
                        type_qname: String::new(),
                    });
                }
                lit_id
            }
        };
        edges.push(EncodeGraphEdge {
            source,
            target,
            predicate: s.predicate.clone(),
            kind: "statement".to_string(),
            confidence: s.confidence,
            event_at_unix_nanos: s.event_at_unix_nanos,
        });
    }
    for r in &artifacts.relations {
        edges.push(EncodeGraphEdge {
            source: lookup(&r.source_name).unwrap_or([0u8; 16]),
            target: lookup(&r.target_name).unwrap_or([0u8; 16]),
            predicate: r.predicate.clone(),
            kind: "relation".to_string(),
            confidence: 1.0,
            // A typed relation row carries no event time of its own.
            event_at_unix_nanos: None,
        });
    }

    EncodeStageGraph { nodes, edges }
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
                ctx.executor.caller_space,
            );
            match crate::handlers::recall::fetch_enrichment_for(&[memory_id], scope, None, &rtxn) {
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
                            event_at_unix_nanos: s.event_at_unix_nanos,
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
/// row exists for `(caller_space, context, content_hash)`.
fn lookup_fingerprint(
    ctx: &OpsContext,
    content_hash: [u8; 32],
    session_id: SessionId,
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
        ctx.executor.caller_space,
        session_id,
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
    space: brain_core::SpaceId,
) -> [u8; 32] {
    let op = brain_planner::EncodeOp {
        request_id: brain_core::RequestId::from(req.request_id),
        session_id: SessionId::from(req.session_id),
        kind: DEFAULT_KIND,
        text: req.text.clone(),
        vector: [0.0; brain_embed::VECTOR_DIM],
        salience_initial: DEFAULT_SALIENCE,
        fingerprint: embedding_model_fp,
        edges: Vec::new(),
        deduplicate: !req.allow_duplicates,
        content_hash: *blake3::hash(req.text.as_bytes()).as_bytes(),
        space_id: space,
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
        space_id: ctx.executor.caller_space.into(),
        session_id: req.session_id,
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
    let _ = ctx
        .txn_store
        .validate_active(txn_id, ctx.caller_connection_id)?;

    // 3. Build an EncodeOp shape for hashing (matches the non-txn
    //    idempotency hash so a cross-txn replay surfaces conflicts).
    //    The router-decided fields are constant policy.
    let request_hash = encode_request_hash(
        &req,
        ctx.executor.embedder.fingerprint(),
        ctx.executor.caller_space,
    );

    // 4. Intra-txn replay check.
    let replay = ctx
        .txn_store
        .with_buffer(txn_id, ctx.caller_connection_id, |buf| {
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
            space_id: ctx.executor.caller_space.into(),
            session_id: req.session_id,
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
        .with_buffer(txn_id, ctx.caller_connection_id, |buf| {
            buf.check_capacity_for_push()
        })?;

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
        brain_core::SpaceId(uuid::Uuid::nil()),
        SessionId::from(req.session_id),
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
        session_id: SessionId::from(req.session_id),
        salience_initial: salience,
        fingerprint: ctx.executor.embedder.fingerprint(),
        request_id: req.request_id,
        request_hash,
        created_at_unix_nanos: created_at,
        occurred_at_unix_nanos: req.occurred_at_unix_nanos,
        space_id: ctx.executor.caller_space,
    };

    ctx.txn_store
        .with_buffer(txn_id, ctx.caller_connection_id, |buf| {
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
        space_id: ctx.executor.caller_space.into(),
        session_id: req.session_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use brain_protocol::envelope::response::{
        EncodeTraceDedup, EncodeTraceEntity, EncodeTraceStatement,
    };

    fn priya_id() -> [u8; 16] {
        [1u8; 16]
    }

    /// Fixture: "Priya manages the billing platform team" — a subject
    /// entity, a `manages` statement whose object is a literal value with
    /// no matching entity in this write (the case a parallel fix in
    /// `brain-workers` ensures the object text is genuinely captured for,
    /// rather than left empty).
    fn literal_object_artifacts() -> EncodeTraceArtifacts {
        EncodeTraceArtifacts {
            entities: vec![EncodeTraceEntity {
                id: priya_id(),
                name: "Priya".into(),
                type_qname: "brain:person".into(),
            }],
            statements: vec![EncodeTraceStatement {
                id: [2u8; 16],
                subject_name: "Priya".into(),
                predicate: "manages".into(),
                object_name: "the billing platform team".into(),
                confidence: 0.9,
                event_at_unix_nanos: None,
            }],
            relations: Vec::new(),
            indexes: Vec::new(),
            dedup: EncodeTraceDedup {
                was_deduplicated: false,
                matched_memory_id: None,
            },
        }
    }

    #[test]
    fn literal_statement_object_renders_as_literal_node() {
        let artifacts = literal_object_artifacts();
        let graph = encode_artifacts_to_graph(&artifacts);

        let literal_node = graph
            .nodes
            .iter()
            .find(|n| n.kind == "literal")
            .expect("a literal node must be synthesized for the literal-object statement");
        assert_eq!(literal_node.name, "the billing platform team");
        assert_eq!(literal_node.type_qname, "");
        assert_ne!(
            literal_node.id, [0u8; 16],
            "must not be the zero placeholder"
        );

        let edge = &graph.edges[0];
        assert_eq!(edge.kind, "statement");
        assert_eq!(edge.source, priya_id());
        assert_eq!(
            edge.target, literal_node.id,
            "edge target must point at the synthetic literal node, not the zero id"
        );

        // Exactly one entity node (Priya) plus one literal node.
        assert_eq!(graph.nodes.len(), 2);
    }

    #[test]
    fn literal_node_id_is_deterministic() {
        let artifacts = literal_object_artifacts();
        let first = encode_artifacts_to_graph(&artifacts);
        let second = encode_artifacts_to_graph(&artifacts);

        let first_id = first
            .nodes
            .iter()
            .find(|n| n.kind == "literal")
            .expect("literal node")
            .id;
        let second_id = second
            .nodes
            .iter()
            .find(|n| n.kind == "literal")
            .expect("literal node")
            .id;
        assert_eq!(
            first_id, second_id,
            "the same fact inspected twice must synthesize the same literal node id"
        );
    }

    #[test]
    fn repeated_literal_value_dedupes_to_one_node() {
        let mut artifacts = literal_object_artifacts();
        // A second statement with the same subject/predicate/object — the
        // same literal fact restated — must not mint a second node.
        artifacts.statements.push(artifacts.statements[0].clone());
        let graph = encode_artifacts_to_graph(&artifacts);

        let literal_nodes: Vec<_> = graph.nodes.iter().filter(|n| n.kind == "literal").collect();
        assert_eq!(
            literal_nodes.len(),
            1,
            "duplicate literal facts dedupe to one node"
        );
        assert_eq!(
            graph.edges.len(),
            2,
            "both statement edges are still emitted"
        );
        assert_eq!(graph.edges[0].target, graph.edges[1].target);
    }

    /// The live ENCODE trace and the durable `MEMORY_INSPECT` bundle render
    /// the same fact through different functions over different DTOs. Both
    /// must mint the SAME synthetic literal id, or one literal would appear
    /// under two ids depending on which path the caller inspected.
    #[test]
    fn literal_node_id_agrees_across_trace_and_bundle_paths() {
        use brain_protocol::envelope::response::{
            EnrichedEntity, EnrichedStatement, GraphEnrichment,
        };

        let artifacts = literal_object_artifacts();
        let s = &artifacts.statements[0];
        let enr = GraphEnrichment {
            entities: vec![EnrichedEntity {
                id: priya_id(),
                name: artifacts.entities[0].name.clone(),
                type_qname: artifacts.entities[0].type_qname.clone(),
            }],
            statements: vec![EnrichedStatement {
                id: s.id,
                subject_name: s.subject_name.clone(),
                predicate: s.predicate.clone(),
                object_label: s.object_name.clone(),
                confidence: s.confidence,
                event_at_unix_nanos: s.event_at_unix_nanos,
            }],
            relations: Vec::new(),
        };

        let trace_graph = encode_artifacts_to_graph(&artifacts);
        let bundle_graph = crate::memory_artifact::enrichment_to_graph(Some(enr));

        let literal_of = |g: &EncodeStageGraph| {
            g.nodes
                .iter()
                .find(|n| n.kind == "literal")
                .expect("literal node")
                .clone()
        };
        let from_trace = literal_of(&trace_graph);
        let from_bundle = literal_of(&bundle_graph);

        assert_eq!(
            from_trace.id, from_bundle.id,
            "both renderers must derive the same synthetic id for the same fact"
        );
        assert_eq!(from_trace.name, from_bundle.name);
        assert_eq!(trace_graph.edges[0].target, bundle_graph.edges[0].target);
    }

    #[test]
    fn entity_statement_object_resolves_to_real_entity_node() {
        let artifacts = EncodeTraceArtifacts {
            entities: vec![
                EncodeTraceEntity {
                    id: priya_id(),
                    name: "Priya".into(),
                    type_qname: "brain:person".into(),
                },
                EncodeTraceEntity {
                    id: [3u8; 16],
                    name: "Stripe".into(),
                    type_qname: "brain:org".into(),
                },
            ],
            statements: vec![EncodeTraceStatement {
                id: [2u8; 16],
                subject_name: "Priya".into(),
                predicate: "works_at".into(),
                object_name: "Stripe".into(),
                confidence: 0.95,
                event_at_unix_nanos: None,
            }],
            relations: Vec::new(),
            indexes: Vec::new(),
            dedup: EncodeTraceDedup {
                was_deduplicated: false,
                matched_memory_id: None,
            },
        };
        let graph = encode_artifacts_to_graph(&artifacts);

        assert!(
            graph.nodes.iter().all(|n| n.kind != "literal"),
            "an entity-object statement must not synthesize a literal node"
        );
        assert_eq!(graph.edges[0].target, [3u8; 16]);
    }
}
