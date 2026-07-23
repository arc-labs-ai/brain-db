//! Integration tests for `handle_reason`. Edges are inserted via
//! wire LINK.

use std::sync::Arc;

use brain_core::{
    SpaceId, SessionId, EdgeKind, Entity, EntityId, EntityType, EvidenceEntry, EvidenceRef,
    ExtractorId, MemoryId, MemoryKind, NamespaceId, Statement, StatementKind, StatementObject,
    StatementValue, SubjectRef,
};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_metadata::MetadataDb;
use brain_metadata::RowScope;
use brain_ops::test_support::run_in_glommio;
use brain_ops::{dispatch, DispatchOutcome, ErrorCode, OpError, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{
    EdgeKindWire, LinkRequest, ObservationInput, ReasonRequest, RequestBody,
};
use brain_protocol::envelope::response::{
    InferenceKind, ReasonResponseFrame, ReasonStatus as WireReasonStatus, ResponseBody,
};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Dispatcher.
// ---------------------------------------------------------------------------

struct NopDispatcher;

impl Dispatcher for NopDispatcher {
    fn embed(&self, _: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok([0.0; VECTOR_DIM])
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0; 16]
    }
}

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

struct Fixture {
    ctx: OpsContext,
    ids: Vec<MemoryId>,
    _tempdir: tempfile::TempDir,
}

fn make_id(i: u64) -> MemoryId {
    let mut b = [0u8; 16];
    b[0..8].copy_from_slice(&i.to_be_bytes());
    MemoryId::from_be_bytes(b)
}

async fn build_fixture(n_memories: usize, edges: &[(usize, EdgeKind, usize)]) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata = MetadataDb::open(&db_path).unwrap();

    let space = SpaceId(Uuid::nil());
    let mut ids = Vec::with_capacity(n_memories);

    let wtxn = metadata.write_txn().unwrap();
    {
        let mut table = wtxn.open_table(MEMORIES_TABLE).unwrap();
        let mut texts = wtxn.open_table(TEXTS_TABLE).unwrap();
        for i in 0..n_memories {
            let id = make_id((i as u64) + 1);
            ids.push(id);
            let meta = MemoryMetadata::new_active(
                id,
                brain_core::NamespaceId::SYSTEM,
                space,
                SessionId(42),
                (i + 1) as u64,
                1,
                MemoryKind::Episodic,
                [0x11; 16],
                0.5,
                16,
                1_000_000 + i as u64,
            );
            table.insert(id.to_be_bytes(), meta).unwrap();
            let text = format!("reason fixture memory {i}");
            texts.insert(id.to_be_bytes(), text.as_bytes()).unwrap();
        }
    }
    wtxn.commit().unwrap();

    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let metadata: SharedMetadataDb = Arc::new(metadata);
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );
    let ctx = brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor);

    for (i, (src, kind, tgt)) in edges.iter().enumerate() {
        let mut request_id = [0u8; 16];
        request_id[..2].copy_from_slice(&(i as u16).to_be_bytes());
        request_id[2] = 0xEE;
        let req = LinkRequest {
            source: ids[*src].raw(),
            target: ids[*tgt].raw(),
            kind: EdgeKindWire::from(*kind),
            weight: 1.0,
            request_id,
            txn_id: None,
            act_as: None,
        };
        let _ = dispatch(
            RequestBody::Link(req),
            brain_ops::RequestCaller::for_tests(),
            &ctx,
        )
        .await
        .unwrap();
    }

    Fixture {
        ctx,
        ids,
        _tempdir: tempdir,
    }
}

fn reason_req(observation: ObservationInput, depth: u32, max_inferences: u32) -> ReasonRequest {
    reason_req_traced(observation, depth, max_inferences, false)
}

fn reason_req_traced(
    observation: ObservationInput,
    depth: u32,
    max_inferences: u32,
    trace: bool,
) -> ReasonRequest {
    ReasonRequest {
        observation,
        depth,
        confidence_threshold: 0.0,
        session_filter: None,
        max_inferences,
        budget_wall_time_ms: 1000,
        request_id: None,
        txn_id: None,
        trace,
        act_as: None,
    }
}

/// Collapse the streamed REASON frames into a single observation:
/// concatenated inference steps from every mid-stream frame and the
/// terminal frame's `reason_status` + `is_final`. Mirrors the v1
/// single-frame shape so existing assertions read unchanged.
fn collect_reason_outcome(outcome: DispatchOutcome) -> ReasonResponseFrame {
    let frames = unwrap_reason_stream(outcome);
    let mut inferences = Vec::new();
    let mut terminal = None;
    for f in frames {
        if f.is_final {
            terminal = Some(f);
        } else {
            inferences.extend(f.inferences);
        }
    }
    let terminal = terminal.expect("REASON stream must end with a terminal frame");
    ReasonResponseFrame {
        inferences,
        is_final: terminal.is_final,
        reason_status: terminal.reason_status,
        trace: terminal.trace,
    }
}

fn unwrap_reason_stream(outcome: DispatchOutcome) -> Vec<ReasonResponseFrame> {
    match outcome {
        DispatchOutcome::Stream(bodies) => bodies
            .into_iter()
            .map(|b| match b {
                ResponseBody::Reason(r) => r,
                other => panic!("expected ResponseBody::Reason in stream, got {other:?}"),
            })
            .collect(),
        DispatchOutcome::Single(other) => {
            panic!("expected DispatchOutcome::Stream of Reason frames, got Single({other:?})")
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Full pipeline: supports + contradicts → one InferenceStep.
// ---------------------------------------------------------------------------

#[test]
fn reason_full_pipeline_emits_one_inference() {
    run_in_glommio(|| async {
        let fix = build_fixture(
            4,
            &[
                (0, EdgeKind::Supports, 1),
                (0, EdgeKind::Supports, 2),
                (0, EdgeKind::Contradicts, 3),
            ],
        )
        .await;
        let req = reason_req(ObservationInput::ByMemoryId(fix.ids[0].into()), 2, 10);
        let frame = collect_reason_outcome(
            dispatch(
                RequestBody::Reason(req),
                brain_ops::RequestCaller::for_tests(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );

        assert!(frame.is_final);
        assert_eq!(frame.reason_status, Some(WireReasonStatus::Complete));
        assert_eq!(frame.inferences.len(), 1);
        let inf = &frame.inferences[0];
        assert_eq!(inf.step_index, 0);
        assert_eq!(inf.inference_kind, InferenceKind::EvidenceAccumulation);
        // base + 2 traversed supports = 3 supporting; 1 contradicting.
        assert_eq!(inf.supporting_memories.len(), 3);
        assert_eq!(inf.contradicting_memories.len(), 1);
        assert!(inf.confidence > 0.0);
        // ByMemoryId observation → claim is empty (documented v1 gap).
        assert_eq!(inf.claim, "");
    })
}

// ---------------------------------------------------------------------------
// 2. No evidence → confidence reflects only the direct-similarity base.
// ---------------------------------------------------------------------------

#[test]
fn reason_isolated_base_returns_only_self() {
    run_in_glommio(|| async {
        let fix = build_fixture(1, &[]).await;
        let req = reason_req(ObservationInput::ByMemoryId(fix.ids[0].into()), 2, 10);
        let frame = collect_reason_outcome(
            dispatch(
                RequestBody::Reason(req),
                brain_ops::RequestCaller::for_tests(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        let inf = &frame.inferences[0];
        assert_eq!(inf.supporting_memories.len(), 1);
        assert!(inf.contradicting_memories.is_empty());
        // sum_s = 1.0, sum_c = 0 → confidence = 1.0.
        assert_eq!(inf.confidence, 1.0);
        assert_eq!(frame.reason_status, Some(WireReasonStatus::Complete));
    })
}

// ---------------------------------------------------------------------------
// 3. Invalid depth → planner validation error.
// ---------------------------------------------------------------------------

#[test]
fn reason_invalid_depth_returns_plan_error() {
    run_in_glommio(|| async {
        let fix = build_fixture(1, &[]).await;
        let req = reason_req(ObservationInput::ByMemoryId(fix.ids[0].into()), 0, 5);
        let err = dispatch(
            RequestBody::Reason(req),
            brain_ops::RequestCaller::for_tests(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, OpError::PlanError(_)),
            "depth=0 must be a planner validation failure, got {err:?}"
        );
        assert_eq!(err.error_code(), ErrorCode::InvalidRequest);
    })
}

// ---------------------------------------------------------------------------
// 4. Inference kind is EvidenceAccumulation for v1.
// ---------------------------------------------------------------------------

#[test]
fn reason_kind_categorisation_uses_evidence_accumulation() {
    run_in_glommio(|| async {
        let fix = build_fixture(2, &[(0, EdgeKind::Supports, 1)]).await;
        let req = reason_req(ObservationInput::ByMemoryId(fix.ids[0].into()), 2, 10);
        let frame = collect_reason_outcome(
            dispatch(
                RequestBody::Reason(req),
                brain_ops::RequestCaller::for_tests(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(
            frame.inferences[0].inference_kind,
            InferenceKind::EvidenceAccumulation
        );
    })
}

// ---------------------------------------------------------------------------
// 5. ByText observation: claim is preserved on the wire.
// ---------------------------------------------------------------------------

#[test]
fn reason_by_text_preserves_claim() {
    run_in_glommio(|| async {
        let fix = build_fixture(1, &[]).await;
        let req = reason_req(ObservationInput::ByText("is the sky blue?".into()), 2, 5);
        let frame = collect_reason_outcome(
            dispatch(
                RequestBody::Reason(req),
                brain_ops::RequestCaller::for_tests(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        let inf = &frame.inferences[0];
        assert_eq!(inf.claim, "is the sky blue?");
        // Empty index + NopDispatcher → no base, no evidence.
        assert!(inf.supporting_memories.is_empty());
        assert!(inf.contradicting_memories.is_empty());
        assert_eq!(inf.confidence, 0.0);
    })
}

// ---------------------------------------------------------------------------
// 6. trace = false (default): terminal frame carries no trace payload.
// ---------------------------------------------------------------------------

#[test]
fn reason_trace_false_omits_trace_payload() {
    run_in_glommio(|| async {
        let fix = build_fixture(
            4,
            &[
                (0, EdgeKind::Supports, 1),
                (0, EdgeKind::Supports, 2),
                (0, EdgeKind::Contradicts, 3),
            ],
        )
        .await;
        let req = reason_req(ObservationInput::ByMemoryId(fix.ids[0].into()), 2, 10);
        let frame = collect_reason_outcome(
            dispatch(
                RequestBody::Reason(req),
                brain_ops::RequestCaller::for_tests(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(frame.trace.is_none());
    })
}

// ---------------------------------------------------------------------------
// 7. trace = true: terminal frame carries the full per-stage trace, with
//    real (non-empty) text on every id-bearing entry.
// ---------------------------------------------------------------------------

#[test]
fn reason_trace_true_populates_walk_and_scoring() {
    run_in_glommio(|| async {
        let fix = build_fixture(
            4,
            &[
                (0, EdgeKind::Supports, 1),
                (0, EdgeKind::Supports, 2),
                (0, EdgeKind::Contradicts, 3),
            ],
        )
        .await;
        let req = reason_req_traced(ObservationInput::ByMemoryId(fix.ids[0].into()), 2, 10, true);
        let frame = collect_reason_outcome(
            dispatch(
                RequestBody::Reason(req),
                brain_ops::RequestCaller::for_tests(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );

        let trace = frame
            .trace
            .expect("trace = true must populate the trace payload");

        // Base: the single ByMemoryId seed, with real text.
        assert_eq!(trace.base.candidates.len(), 1);
        assert_eq!(trace.base.candidates[0].memory_id, fix.ids[0].raw());
        assert!(!trace.base.candidates[0].text.is_empty());

        // Walk: every out-edge from node 0 is considered twice (once per
        // supports/contradicts pass); the cross-kind edges get pruned by
        // the edge-kind filter on the pass they don't belong to.
        assert!(
            !trace.walk.considered.is_empty(),
            "expected considered edges, got none"
        );
        assert!(
            !trace.walk.dropped_by_edge_kind.is_empty(),
            "expected edge-kind drops (Contradicts edge on the supports pass, Supports edges \
             on the contradicts pass), got none"
        );
        for c in &trace.walk.considered {
            assert!(!c.text.is_empty(), "considered edge candidate missing text");
        }
        for d in &trace.walk.dropped_by_edge_kind {
            assert!(
                !d.text.is_empty(),
                "dropped_by_edge_kind entry missing text"
            );
        }

        // Scoring: every surviving evidence item (direct-similarity base +
        // walked supports/contradicts) got its score components un-collapsed.
        assert!(!trace.scoring.is_empty());
        for s in &trace.scoring {
            assert!(!s.text.is_empty(), "score breakdown entry missing text");
        }

        // Centroid: singleton base (well, the walk grows it, but
        // `build_base_centroid` runs against the *initial* base set, which
        // here is a singleton) — so centroid computation is skipped, and
        // the trace says why.
        assert!(!trace.centroid.computed);
        assert!(trace.centroid.skipped_reason.is_some());
    })
}

// ---------------------------------------------------------------------------
// 8. VSA analogical inference: `analogical_fit` is a real, non-hardcoded
//    trace field, and `InferenceKind::AnalogicalInference` can genuinely
//    tag a step (not just `EvidenceAccumulation` forever).
//
// Fixture: the observation memory (ids[0]) and a Supports-reached memory
// (ids[1]) each carry a statement of the SAME predicate ("test:works_at")
// — the "X works_at Acme" / "Y works_at Stripe" analogy shape
// `executor::analogical`'s own smoke test exercises — so the supporting
// item's structural fit is nudged away from neutral. A Contradicts-reached
// memory (ids[2]) carries no statement at all, so its fit stays the
// documented neutral default. With zero contradicting evidence the
// aggregate confidence is scale-invariant (any positive nudge cancels out
// of the ratio), so a Contradicts edge is required to make the nudge move
// the aggregate enough to flip the step's tagged kind.
// ---------------------------------------------------------------------------

fn seed_entity(wtxn: &redb::WriteTransaction, scope: RowScope, name: &str) -> EntityId {
    let id = EntityId::new();
    let normalized = brain_metadata::entity::ops::normalize_name(name);
    let e = Entity::new_active(
        id,
        EntityType::PERSON_ID,
        name.to_string(),
        normalized,
        1_700_000_000_000_000_000,
    );
    brain_metadata::entity::ops::entity_put(wtxn, scope, &e).expect("seed entity");
    id
}

#[test]
fn reason_analogical_fit_populates_trace_and_can_tag_inference_kind() {
    run_in_glommio(|| async {
        let fix = build_fixture(
            3,
            &[(0, EdgeKind::Supports, 1), (0, EdgeKind::Contradicts, 2)],
        )
        .await;

        let scope = RowScope::new(NamespaceId::SYSTEM, SpaceId(Uuid::nil()));
        let wtxn = fix.ctx.executor.metadata.write_txn().expect("write txn");
        let alice = seed_entity(&wtxn, scope, "Alice");
        let bob = seed_entity(&wtxn, scope, "Bob");
        let predicate_id = brain_metadata::schema::predicate::predicate_intern(
            &wtxn,
            "test",
            "works_at",
            Some(StatementKind::Fact),
            /* object: Value */ 2,
            /* schema_version */ 1,
            "",
            /* is_stateful */ false,
            1_700_000_000_000_000_000,
        )
        .expect("intern predicate");

        let evidence = |mid: MemoryId| {
            EvidenceRef::inline_from_slice(&[EvidenceEntry {
                memory_id: mid,
                confidence_milli: 0,
                timestamp_unix_nanos: 1_700_000_000_000_000_000,
                extractor_id: ExtractorId::from(0),
            }])
        };

        // Observation triple: Alice works_at Acme, evidenced by ids[0].
        let obs_stmt = Statement::new_root(
            brain_core::StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(alice),
            predicate_id,
            StatementObject::Value(StatementValue::Text("Acme".into())),
            0.9,
            evidence(fix.ids[0]),
            ExtractorId::from(0),
            1_700_000_000_000_000_000,
            1,
        );
        brain_metadata::statement::statement_create(&wtxn, scope, &obs_stmt, 0)
            .expect("create observation statement");

        // Candidate triple, SAME predicate: Bob works_at Stripe, evidenced
        // by ids[1] (the Supports-reached memory).
        let cand_stmt = Statement::new_root(
            brain_core::StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(bob),
            predicate_id,
            StatementObject::Value(StatementValue::Text("Stripe".into())),
            0.9,
            evidence(fix.ids[1]),
            ExtractorId::from(0),
            1_700_000_000_000_000_000,
            1,
        );
        brain_metadata::statement::statement_create(&wtxn, scope, &cand_stmt, 0)
            .expect("create candidate statement");
        wtxn.commit().expect("commit seed txn");

        // ids[2] (the Contradicts-reached memory) deliberately carries no
        // statement at all — `resolve_statement_triple` returns `None` for
        // it, so its `analogical_fit` must stay the documented neutral
        // default regardless of the nudge applied to ids[1].

        let req = reason_req_traced(ObservationInput::ByMemoryId(fix.ids[0].into()), 2, 10, true);
        let frame = collect_reason_outcome(
            dispatch(
                RequestBody::Reason(req),
                brain_ops::RequestCaller::for_tests(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );

        let trace = frame
            .trace
            .expect("trace = true must populate the trace payload");
        assert!(
            !trace.scoring.is_empty(),
            "expected score-breakdown entries"
        );

        let no_triple_entry = trace
            .scoring
            .iter()
            .find(|s| s.memory_id == fix.ids[2].raw())
            .expect("ids[2] must appear in the score breakdown");
        assert_eq!(
            no_triple_entry.analogical_fit, 1.0,
            "an item with no resolvable statement triple must keep the neutral default"
        );

        let matched_entry = trace
            .scoring
            .iter()
            .find(|s| s.memory_id == fix.ids[1].raw())
            .expect("ids[1] must appear in the score breakdown");
        assert!(
            (matched_entry.analogical_fit - 1.0).abs() > 1e-6,
            "a same-predicate resolvable triple must nudge analogical_fit away from neutral, \
             got {}",
            matched_entry.analogical_fit,
        );
        assert!(
            (0.8..=1.2).contains(&matched_entry.analogical_fit),
            "analogical_fit must stay within the documented bounded re-rank range, got {}",
            matched_entry.analogical_fit,
        );

        // The real regression: this must be able to come back as
        // `AnalogicalInference`, not the old hardcoded
        // `EvidenceAccumulation` constant.
        assert_eq!(
            frame.inferences[0].inference_kind,
            InferenceKind::AnalogicalInference,
            "a same-predicate analogical match that measurably shifts the aggregate confidence \
             must tag the step AnalogicalInference",
        );
    })
}
