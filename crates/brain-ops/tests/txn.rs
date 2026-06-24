//! Integration tests for transactions.
//!
//! Covers:
//! - Lifecycle (begin / commit / abort + replay + sweep)
//! - Buffering & rollback (operations buffered, atomic apply, true rollback)
//! - Read-your-writes (RECALL / PLAN / REASON within a txn see pending writes)
//! - Validation + replay error paths

use std::sync::Arc;
use std::time::Duration;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::test_support::run_in_glommio;
use brain_ops::{dispatch, DispatchOutcome, ErrorCode, OpError, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{
    EdgeKindWire, EncodeRequest, ForgetMode, ForgetRequest, LinkRequest, ObservationInput,
    PlanBudget, PlanRequest, PlanState, ReasonRequest, RecallRequest, RequestBody, TxnAbortRequest,
    TxnBeginRequest, TxnCommitRequest, UnlinkRequest,
};
use brain_protocol::envelope::response::{
    EncodeResponse, ForgetResponse, LinkResponse, PlanResponseFrame, PlanStatus as WirePlanStatus,
    ReasonResponseFrame, RecallResponseFrame, ResponseBody, TxnAbortResponse, TxnBeginResponse,
    TxnCommitResponse, UnlinkResponse,
};

// ---------------------------------------------------------------------------
// Mock dispatcher: deterministic per-text vector.
// ---------------------------------------------------------------------------

struct MockDispatcher;

impl Dispatcher for MockDispatcher {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        let mut v = [0.0f32; VECTOR_DIM];
        for (i, byte) in text.as_bytes().iter().enumerate() {
            v[i % VECTOR_DIM] += f32::from(*byte) / 255.0;
        }
        Ok(v)
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0xAB; 16]
    }
}

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

struct Fixture {
    ctx: OpsContext,
    tempdir: tempfile::TempDir,
    metadata: SharedMetadataDb,
}

impl Fixture {
    /// Rebuild the lexical lane from redb so a *committed*, non-txn recall
    /// sees a fully-indexed corpus (no text-indexer worker runs in a unit
    /// test). Call after the writes are committed and before the recall.
    /// In-txn pending writes are not in redb, so this does not affect them.
    fn reindex_lexical(&mut self) {
        self.ctx.lexical_retriever = brain_ops::test_support::reindex_memory_lexical_for_tests(
            self.tempdir.path(),
            &self.metadata,
        );
    }
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );
    Fixture {
        ctx: brain_ops::test_support::ops_context_for_tests(executor, tempdir.path()),
        tempdir,
        metadata,
    }
}

// ---------------------------------------------------------------------------
// Convenience builders.
// ---------------------------------------------------------------------------

fn encode_req(request_id: [u8; 16], text: &str, txn: Option<[u8; 16]>) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        context_id: 42,
        request_id,
        txn_id: txn,
        occurred_at_unix_nanos: None,
    }
}

fn link_req(
    src: u128,
    tgt: u128,
    kind: EdgeKindWire,
    weight: f32,
    request_id: [u8; 16],
    txn: Option<[u8; 16]>,
) -> LinkRequest {
    LinkRequest {
        source: src,
        target: tgt,
        kind,
        weight,
        request_id,
        txn_id: txn,
    }
}

fn unlink_req(
    src: u128,
    tgt: u128,
    kind: EdgeKindWire,
    request_id: [u8; 16],
    txn: Option<[u8; 16]>,
) -> UnlinkRequest {
    UnlinkRequest {
        source: src,
        target: tgt,
        kind,
        request_id,
        txn_id: txn,
    }
}

fn forget_req(memory_id: u128, request_id: [u8; 16], txn: Option<[u8; 16]>) -> ForgetRequest {
    ForgetRequest {
        memory_id,
        mode: ForgetMode::Soft,
        request_id,
        txn_id: txn,
    }
}

fn recall_req(cue: &str, max_results: u32, txn: Option<[u8; 16]>) -> RecallRequest {
    RecallRequest {
        cue_text: cue.into(),
        subject_name: String::new(),
        max_results,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        as_of_record_time_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        include_edges: false,
        include_graph: false,
        include_text: false,
        request_id: None,
        txn_id: txn,
        agent_filter: Vec::new(),
        include_other_agents: false,
    }
}

fn plan_req(start: u128, goal: u128, max_depth: u32, txn: Option<[u8; 16]>) -> PlanRequest {
    PlanRequest {
        start: PlanState::ByMemoryId(start),
        goal: PlanState::ByMemoryId(goal),
        budget: PlanBudget {
            max_steps: max_depth,
            max_wall_time_ms: 1000,
            max_branches_explored: 256,
        },
        strategy_hint: None,
        context_filter: None,
        request_id: None,
        txn_id: txn,
    }
}

fn reason_req(base: u128, depth: u32, txn: Option<[u8; 16]>) -> ReasonRequest {
    ReasonRequest {
        observation: ObservationInput::ByMemoryId(base),
        depth,
        confidence_threshold: 0.0,
        context_filter: None,
        max_inferences: 10,
        budget_wall_time_ms: 1000,
        request_id: None,
        txn_id: txn,
    }
}

async fn encode(fix: &Fixture, rid: [u8; 16], text: &str, txn: Option<[u8; 16]>) -> u128 {
    match single_body(
        dispatch(
            RequestBody::Encode(encode_req(rid, text, txn)),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap(),
    ) {
        ResponseBody::Encode(EncodeResponse { memory_id, .. }) => memory_id,
        other => panic!("expected Encode, got {other:?}"),
    }
}

/// Unwrap a non-streaming dispatch outcome. Streaming ops (PLAN /
/// REASON) collect via `unwrap_plan` / `unwrap_reason`, which drain
/// the per-frame projections.
fn single_body(outcome: DispatchOutcome) -> ResponseBody {
    match outcome {
        DispatchOutcome::Single(b) => b,
        DispatchOutcome::Stream(_) => panic!("expected DispatchOutcome::Single, got Stream"),
    }
}

fn unwrap_begin(r: DispatchOutcome) -> TxnBeginResponse {
    match single_body(r) {
        ResponseBody::TxnBegin(b) => b,
        other => panic!("expected TxnBegin, got {other:?}"),
    }
}
fn unwrap_commit(r: DispatchOutcome) -> TxnCommitResponse {
    match single_body(r) {
        ResponseBody::TxnCommit(c) => c,
        other => panic!("expected TxnCommit, got {other:?}"),
    }
}
fn unwrap_abort(r: DispatchOutcome) -> TxnAbortResponse {
    match single_body(r) {
        ResponseBody::TxnAbort(a) => a,
        other => panic!("expected TxnAbort, got {other:?}"),
    }
}
fn unwrap_link(r: DispatchOutcome) -> LinkResponse {
    match single_body(r) {
        ResponseBody::Link(l) => l,
        other => panic!("expected Link, got {other:?}"),
    }
}
fn unwrap_unlink(r: DispatchOutcome) -> UnlinkResponse {
    match single_body(r) {
        ResponseBody::Unlink(u) => u,
        other => panic!("expected Unlink, got {other:?}"),
    }
}
#[allow(dead_code)]
fn unwrap_forget(r: DispatchOutcome) -> ForgetResponse {
    match single_body(r) {
        ResponseBody::Forget(f) => f,
        other => panic!("expected Forget, got {other:?}"),
    }
}
fn unwrap_recall(r: DispatchOutcome) -> RecallResponseFrame {
    match single_body(r) {
        ResponseBody::Recall(r) => r,
        other => panic!("expected Recall, got {other:?}"),
    }
}

/// Collapse the streamed PLAN frames into the v1 single-frame shape
/// the tests assert against.
fn unwrap_plan(outcome: DispatchOutcome) -> PlanResponseFrame {
    let mut steps = Vec::new();
    let mut terminal: Option<PlanResponseFrame> = None;
    match outcome {
        DispatchOutcome::Stream(bodies) => {
            for b in bodies {
                match b {
                    ResponseBody::Plan(f) if f.is_final => terminal = Some(f),
                    ResponseBody::Plan(f) => steps.extend(f.steps),
                    other => panic!("expected Plan frame, got {other:?}"),
                }
            }
        }
        DispatchOutcome::Single(other) => {
            panic!("expected Stream of Plan, got Single({other:?})")
        }
    }
    let t = terminal.expect("PLAN stream must end with a terminal frame");
    PlanResponseFrame {
        steps,
        is_final: t.is_final,
        plan_status: t.plan_status,
    }
}

/// Collapse the streamed REASON frames into the v1 single-frame shape.
fn unwrap_reason(outcome: DispatchOutcome) -> ReasonResponseFrame {
    let mut inferences = Vec::new();
    let mut terminal: Option<ReasonResponseFrame> = None;
    match outcome {
        DispatchOutcome::Stream(bodies) => {
            for b in bodies {
                match b {
                    ResponseBody::Reason(f) if f.is_final => terminal = Some(f),
                    ResponseBody::Reason(f) => inferences.extend(f.inferences),
                    other => panic!("expected Reason frame, got {other:?}"),
                }
            }
        }
        DispatchOutcome::Single(other) => {
            panic!("expected Stream of Reason, got Single({other:?})")
        }
    }
    let t = terminal.expect("REASON stream must end with a terminal frame");
    ReasonResponseFrame {
        inferences,
        is_final: t.is_final,
        reason_status: t.reason_status,
    }
}

async fn begin(fix: &Fixture, txn_id: [u8; 16], timeout_seconds: u32) -> TxnBeginResponse {
    unwrap_begin(
        dispatch(
            RequestBody::TxnBegin(TxnBeginRequest {
                txn_id,
                timeout_seconds,
            }),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap(),
    )
}

async fn commit(fix: &Fixture, txn_id: [u8; 16]) -> TxnCommitResponse {
    unwrap_commit(
        dispatch(
            RequestBody::TxnCommit(TxnCommitRequest { txn_id }),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap(),
    )
}

async fn abort(fix: &Fixture, txn_id: [u8; 16]) -> TxnAbortResponse {
    unwrap_abort(
        dispatch(
            RequestBody::TxnAbort(TxnAbortRequest { txn_id }),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap(),
    )
}

// =============================================================================
// Lifecycle (3 tests)
// =============================================================================

#[test]
fn txn_begin_clamps_timeout_to_bounds() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        // Default (0) → 30 sec.
        let b1 = begin(&fix, [1; 16], 0).await;
        assert_eq!(b1.timeout_seconds, 30);
        // Above max (500) → clamped to 300.
        let b2 = begin(&fix, [2; 16], 500).await;
        assert_eq!(b2.timeout_seconds, 300);
        // Below min (0 was tested above; 1 is fine).
        let b3 = begin(&fix, [3; 16], 1).await;
        assert_eq!(b3.timeout_seconds, 1);
    })
}

#[test]
fn txn_begin_replays_on_same_id() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let b1 = begin(&fix, [4; 16], 60).await;
        let b2 = begin(&fix, [4; 16], 60).await;
        assert_eq!(b1.started_at_unix_nanos, b2.started_at_unix_nanos);
        assert_eq!(b1.timeout_seconds, b2.timeout_seconds);
    })
}

#[test]
fn expired_txn_swept_on_next_op() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn = [5; 16];
        let _ = begin(&fix, txn, 1).await; // 1-sec timeout.
        std::thread::sleep(Duration::from_millis(1100));
        // Now an in-txn encode must fail with TxnExpired.
        let err = dispatch(
            RequestBody::Encode(encode_req([99; 16], "x", Some(txn))),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, OpError::TxnExpired));
    })
}

// =============================================================================
// Buffering & rollback (4 tests)
// =============================================================================

#[test]
fn encode_in_txn_not_visible_outside() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn = [10; 16];
        let _ = begin(&fix, txn, 60).await;

        let mid = encode(&fix, [11; 16], "secret", Some(txn)).await;
        assert_ne!(mid, 0);

        // Non-txn RECALL must NOT see the pending memory.
        let frame = unwrap_recall(
            dispatch(
                RequestBody::Recall(recall_req("secret", 10, None)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(
            !frame.memories.iter().any(|r| r.memory_id == mid),
            "pending memory must be invisible to non-txn RECALL"
        );
    })
}

#[test]
fn commit_makes_buffered_writes_visible() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();
        let txn = [20; 16];
        let _ = begin(&fix, txn, 60).await;
        let mid = encode(&fix, [21; 16], "committed", Some(txn)).await;

        let c = commit(&fix, txn).await;
        assert_eq!(c.operations_applied, 1);
        // The committed row is now in redb; index it lexically so the
        // post-commit retrieval recall sees a fully-indexed corpus.
        fix.reindex_lexical();

        let frame = unwrap_recall(
            dispatch(
                RequestBody::Recall(recall_req("committed", 10, None)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(frame.memories.iter().any(|r| r.memory_id == mid));
    })
}

#[test]
fn abort_discards_buffered_writes() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn = [30; 16];
        let _ = begin(&fix, txn, 60).await;
        let mid = encode(&fix, [31; 16], "aborted", Some(txn)).await;

        let a = abort(&fix, txn).await;
        assert_eq!(a.operations_discarded, 1);

        // Memory must NOT be in non-txn RECALL after abort.
        let frame = unwrap_recall(
            dispatch(
                RequestBody::Recall(recall_req("aborted", 10, None)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(!frame.memories.iter().any(|r| r.memory_id == mid));
    })
}

#[test]
fn commit_is_atomic_link_to_phantom_fails_whole_txn() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn = [40; 16];
        let _ = begin(&fix, txn, 60).await;

        // Successful in-txn encode.
        let mid = encode(&fix, [41; 16], "alpha", Some(txn)).await;

        // LINK in-txn from `mid` to a phantom id. The buffer path
        // validates endpoint existence at preview time → NotFound error.
        let phantom: u128 = 0xDEAD_BEEF_DEAD_BEEF_0000_0000_0000_0000;
        let err = dispatch(
            RequestBody::Link(link_req(
                mid,
                phantom,
                EdgeKindWire::Caused,
                1.0,
                [42; 16],
                Some(txn),
            )),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, OpError::NotFound { .. }));

        // The original encode is still buffered; commit applies it fine.
        let c = commit(&fix, txn).await;
        assert_eq!(c.operations_applied, 1);
    })
}

// =============================================================================
// Read-your-writes (5 tests)
// =============================================================================

#[test]
fn recall_in_txn_sees_pending_encode() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn = [50; 16];
        let _ = begin(&fix, txn, 60).await;
        let mid = encode(&fix, [51; 16], "in-flight", Some(txn)).await;

        let frame = unwrap_recall(
            dispatch(
                RequestBody::Recall(recall_req("in-flight", 10, Some(txn))),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(
            frame.memories.iter().any(|r| r.memory_id == mid),
            "in-txn RECALL must see pending memory; got {:?}",
            frame.memories
        );
    })
}

#[test]
fn recall_in_txn_drops_pending_tombstone() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();
        // First commit a memory.
        let committed_mid = encode(&fix, [60; 16], "doomed", None).await;
        // It's committed to redb — index it lexically so the committed
        // retrieval lane surfaces it (the in-txn overlay then drops it as
        // tombstoned; the non-txn recall still sees it).
        fix.reindex_lexical();

        // Now open a txn and FORGET it.
        let txn = [61; 16];
        let _ = begin(&fix, txn, 60).await;
        let _ = dispatch(
            RequestBody::Forget(forget_req(committed_mid, [62; 16], Some(txn))),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap();

        // In-txn RECALL must NOT see the tombstoned memory.
        let in_txn = unwrap_recall(
            dispatch(
                RequestBody::Recall(recall_req("doomed", 10, Some(txn))),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(!in_txn.memories.iter().any(|r| r.memory_id == committed_mid));

        // Non-txn RECALL still sees it (uncommitted abort below).
        let outside = unwrap_recall(
            dispatch(
                RequestBody::Recall(recall_req("doomed", 10, None)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(outside
            .memories
            .iter()
            .any(|r| r.memory_id == committed_mid));

        // Abort the txn — the tombstone goes away.
        let _ = abort(&fix, txn).await;
    })
}

#[test]
fn plan_in_txn_traverses_pending_link() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let a = encode(&fix, [70; 16], "alpha", None).await;
        let b = encode(&fix, [71; 16], "beta", None).await;

        let txn = [72; 16];
        let _ = begin(&fix, txn, 60).await;
        let _ = dispatch(
            RequestBody::Link(link_req(
                a,
                b,
                EdgeKindWire::Caused,
                1.0,
                [73; 16],
                Some(txn),
            )),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap();

        // In-txn PLAN sees the pending link.
        let in_txn = unwrap_plan(
            dispatch(
                RequestBody::Plan(plan_req(a, b, 3, Some(txn))),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(in_txn.plan_status, Some(WirePlanStatus::GoalReached));
        assert_eq!(in_txn.steps.len(), 2);

        // Non-txn PLAN doesn't.
        let outside = unwrap_plan(
            dispatch(
                RequestBody::Plan(plan_req(a, b, 3, None)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(outside.plan_status, Some(WirePlanStatus::NoPathFound));

        let _ = abort(&fix, txn).await;
    })
}

#[test]
fn reason_in_txn_picks_up_pending_supports_edge() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let a = encode(&fix, [80; 16], "claim", None).await;
        let b = encode(&fix, [81; 16], "evidence", None).await;

        let txn = [82; 16];
        let _ = begin(&fix, txn, 60).await;
        let _ = dispatch(
            RequestBody::Link(link_req(
                a,
                b,
                EdgeKindWire::Supports,
                1.0,
                [83; 16],
                Some(txn),
            )),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap();

        let frame = unwrap_reason(
            dispatch(
                RequestBody::Reason(reason_req(a, 2, Some(txn))),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        let inf = &frame.inferences[0];
        assert!(
            inf.supporting_memories.contains(&b),
            "REASON in-txn must include the pending-link target as supporting; got {:?}",
            inf.supporting_memories
        );

        let _ = abort(&fix, txn).await;
    })
}

#[test]
fn unlink_in_txn_hides_committed_edge() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let a = encode(&fix, [90; 16], "a", None).await;
        let b = encode(&fix, [91; 16], "b", None).await;
        // Commit a LINK outside the txn.
        let _ = unwrap_link(
            dispatch(
                RequestBody::Link(link_req(a, b, EdgeKindWire::Caused, 1.0, [92; 16], None)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );

        // Non-txn PLAN finds it.
        let outside = unwrap_plan(
            dispatch(
                RequestBody::Plan(plan_req(a, b, 3, None)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(outside.plan_status, Some(WirePlanStatus::GoalReached));

        // In a txn, UNLINK; PLAN in-txn should not find the path.
        let txn = [93; 16];
        let _ = begin(&fix, txn, 60).await;
        let u = unwrap_unlink(
            dispatch(
                RequestBody::Unlink(unlink_req(a, b, EdgeKindWire::Caused, [94; 16], Some(txn))),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(u.removed);

        let in_txn = unwrap_plan(
            dispatch(
                RequestBody::Plan(plan_req(a, b, 3, Some(txn))),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(in_txn.plan_status, Some(WirePlanStatus::NoPathFound));

        // Abort restores the edge.
        let _ = abort(&fix, txn).await;
        let restored = unwrap_plan(
            dispatch(
                RequestBody::Plan(plan_req(a, b, 3, None)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(restored.plan_status, Some(WirePlanStatus::GoalReached));
    })
}

// =============================================================================
// Validation + replay (3 tests)
// =============================================================================

#[test]
fn op_with_unknown_txn_id_returns_txn_not_found() {
    // The id has never been created — distinct from "was active and
    // expired". Used by the shell to tell a typo from a stale txn.
    run_in_glommio(|| async {
        let fix = build_fixture();
        let err = dispatch(
            RequestBody::Encode(encode_req([100; 16], "x", Some([99; 16]))),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, OpError::TxnNotFound), "got {err:?}");
        assert_eq!(err.error_code(), ErrorCode::TxnNotFound);
    })
}

#[test]
fn op_against_committed_txn_returns_txn_expired() {
    // The id IS real — it was Active, then we committed it. Subsequent
    // ops against it must surface as `TxnExpired` (not `TxnNotFound`)
    // so the shell shows a different message than the typo case.
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn = [110; 16];
        let _ = begin(&fix, txn, 60).await;
        let _ = encode(&fix, [111; 16], "first", Some(txn)).await;
        let _ = commit(&fix, txn).await;

        let err = dispatch(
            RequestBody::Encode(encode_req([112; 16], "after", Some(txn))),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, OpError::TxnExpired), "got {err:?}");
        assert_eq!(err.error_code(), ErrorCode::TxnExpired);
    })
}

#[test]
fn commit_replay_returns_cached_response() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn = [120; 16];
        let _ = begin(&fix, txn, 60).await;
        let _ = encode(&fix, [121; 16], "once", Some(txn)).await;

        let c1 = commit(&fix, txn).await;
        let c2 = commit(&fix, txn).await;
        assert_eq!(c1.committed_at_unix_nanos, c2.committed_at_unix_nanos);
        assert_eq!(c1.operations_applied, c2.operations_applied);
    })
}

// =============================================================================
// Activity-based timeout extension
// =============================================================================
//
// An interactive REPL session spans seconds of typing. A pure
// wall-clock timeout would expire txns mid-session even when the
// user is actively making progress. Every in-txn op resets the
// deadline; the timeout is an *idle* bound, not a hard wall clock.

#[test]
fn txn_extends_on_each_op_inside_window() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn = [140; 16];
        // 1 s window. Without activity extension, two encodes spaced
        // ~600 ms apart would push the second past the original
        // deadline. With extension, every encode resets the clock.
        let _ = begin(&fix, txn, 1).await;
        let _ = encode(&fix, [141; 16], "a", Some(txn)).await;
        glommio::timer::sleep(std::time::Duration::from_millis(600)).await;
        let _ = encode(&fix, [142; 16], "b", Some(txn)).await;
        glommio::timer::sleep(std::time::Duration::from_millis(600)).await;
        let _ = encode(&fix, [143; 16], "c", Some(txn)).await;
        glommio::timer::sleep(std::time::Duration::from_millis(600)).await;
        // ~1.8 s total wall-clock since begin, but the last reset
        // was ~600 ms ago — commit must still succeed.
        let resp = commit(&fix, txn).await;
        assert_eq!(resp.operations_applied, 3);
    })
}

#[test]
fn txn_expires_after_idle_window() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn = [150; 16];
        let _ = begin(&fix, txn, 1).await;
        let _ = encode(&fix, [151; 16], "alive", Some(txn)).await;
        // Sleep past the idle window with no activity.
        glommio::timer::sleep(std::time::Duration::from_millis(1500)).await;
        // Commit must report expired.
        let err = dispatch(
            RequestBody::TxnCommit(TxnCommitRequest { txn_id: txn }),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, OpError::TxnExpired), "got {err:?}");
    })
}

// =============================================================================
// Connection-drop auto-abort
//
// A client whose TCP/TLS connection dies before TXN_COMMIT must see
// none of its buffered operations applied. The connection layer fans
// `TxnStore::abort_orphaned_for_session` across every shard the
// moment it observes the disconnect; these tests exercise that sweep
// directly against `TxnStore`.
// =============================================================================

/// Open a txn that's linked to the given wire `session_id`. Mirrors
/// `begin` but carries a non-anonymous caller so the entry inherits
/// the session linkage the connection layer would stamp in production.
async fn begin_with_session(
    fix: &Fixture,
    txn_id: [u8; 16],
    timeout_seconds: u32,
    session_id: [u8; 16],
) -> TxnBeginResponse {
    let caller = brain_ops::RequestCaller::anonymous().with_session_id(session_id);
    unwrap_begin(
        dispatch(
            RequestBody::TxnBegin(TxnBeginRequest {
                txn_id,
                timeout_seconds,
            }),
            caller,
            &fix.ctx,
        )
        .await
        .unwrap(),
    )
}

#[test]
fn session_drop_aborts_open_txns() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let session = [0xAA; 16];
        let txn = [200; 16];
        let _ = begin_with_session(&fix, txn, 30, session).await;
        // Buffer some work — these must NOT land after the sweep.
        let _ = encode(&fix, [201; 16], "draft-1", Some(txn)).await;
        let _ = encode(&fix, [202; 16], "draft-2", Some(txn)).await;

        // Simulate the connection drop hook: connection layer fans
        // `abort_orphaned_for_session` to every shard. Here we hit
        // the single shard directly.
        let aborted = fix.ctx.txn_store.abort_orphaned_for_session(session);
        assert_eq!(aborted, vec![txn], "exactly the dropped session's txn");

        // Subsequent ops on the txn must see it as Expired (the
        // post-sweep state is Aborted, validate_active returns
        // TxnExpired for any non-Active state).
        let err = dispatch(
            RequestBody::TxnCommit(TxnCommitRequest { txn_id: txn }),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, OpError::TxnExpired), "got {err:?}");

        // And the buffered drafts are gone — a fresh recall over
        // committed state finds nothing.
        let mut req = recall_req("draft", 5, None);
        req.include_text = true;
        let recall = unwrap_recall(
            dispatch(
                RequestBody::Recall(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(
            recall.memories.is_empty(),
            "buffered encodes leaked into committed state: {:?}",
            recall.memories
        );
    })
}

#[test]
fn session_drop_does_not_affect_other_sessions() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();
        let session_a = [0xAA; 16];
        let session_b = [0xBB; 16];
        let txn_a = [210; 16];
        let txn_b = [211; 16];
        let _ = begin_with_session(&fix, txn_a, 30, session_a).await;
        let _ = begin_with_session(&fix, txn_b, 30, session_b).await;
        let _ = encode(&fix, [212; 16], "from-b", Some(txn_b)).await;

        // Drop session A. Session B's txn must keep working.
        let aborted = fix.ctx.txn_store.abort_orphaned_for_session(session_a);
        assert_eq!(aborted, vec![txn_a]);

        // Session B can commit. Its encode must land.
        let resp = commit(&fix, txn_b).await;
        assert_eq!(resp.operations_applied, 1);
        // Committed to redb — index it lexically so the non-txn recall
        // surfaces it.
        fix.reindex_lexical();

        let mut req = recall_req("from-b", 5, None);
        req.include_text = true;
        let recall = unwrap_recall(
            dispatch(
                RequestBody::Recall(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(
            recall.memories.len(),
            1,
            "session B's committed encode should be visible"
        );
    })
}

#[test]
fn reconnect_after_drop_sees_clean_state() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let session_1 = [0xAA; 16];
        let session_2 = [0xCC; 16];
        let old_txn = [220; 16];
        let _ = begin_with_session(&fix, old_txn, 30, session_1).await;
        let _ = encode(&fix, [221; 16], "lost-draft", Some(old_txn)).await;

        // Original connection dies.
        let aborted = fix.ctx.txn_store.abort_orphaned_for_session(session_1);
        assert_eq!(aborted, vec![old_txn]);

        // Client reconnects with a brand-new session + a brand-new
        // txn id. The fresh begin must succeed and operate on a
        // clean buffer — the previous draft must not leak through.
        let new_txn = [222; 16];
        let _ = begin_with_session(&fix, new_txn, 30, session_2).await;
        let _ = encode(&fix, [223; 16], "fresh-draft", Some(new_txn)).await;

        // Recall inside the new txn sees only its own pending encode,
        // not the dropped one.
        let mut req = recall_req("draft", 5, Some(new_txn));
        req.include_text = true;
        let recall = unwrap_recall(
            dispatch(
                RequestBody::Recall(req),
                brain_ops::RequestCaller::anonymous().with_session_id(session_2),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        let texts: Vec<&str> = recall.memories.iter().map(|h| h.text.as_str()).collect();
        assert!(
            texts.iter().any(|t| t.contains("fresh-draft")),
            "new-session recall must see its own pending encode: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("lost-draft")),
            "dropped-session buffer must NOT leak into new session: {texts:?}"
        );

        // Commit the new session's txn — the encode lands cleanly.
        let resp = commit(&fix, new_txn).await;
        assert_eq!(resp.operations_applied, 1);
    })
}

#[test]
fn auto_abort_sweep_is_noop_for_zero_session_id() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn = [230; 16];
        // Default begin() uses the anonymous caller, which carries
        // session_id == [0; 16]. The sweep must not touch such entries
        // — otherwise in-process embedded callers (tests, harnesses)
        // would wipe their own txns by accident.
        let _ = begin(&fix, txn, 30).await;
        let _ = encode(&fix, [231; 16], "still-here", Some(txn)).await;

        let aborted = fix.ctx.txn_store.abort_orphaned_for_session([0u8; 16]);
        assert!(aborted.is_empty(), "zero session_id must be a no-op");

        // Txn is still committable.
        let resp = commit(&fix, txn).await;
        assert_eq!(resp.operations_applied, 1);
    })
}

// =============================================================================
// Per-transaction op cap (MAX_TXN_OPS = 1000)
// =============================================================================
//
// The cap is enforced two ways: append-time (so the 1001st buffered op
// fails fast) and commit-time (defense-in-depth for buffer mutations
// that slip past the append guard). The append-time check sits AFTER
// the intra-txn replay cache so an idempotent re-submit at the cap
// boundary still replays.

/// `i`-th request_id within these tests. Disambiguates the per-op
/// idempotency hash so each buffered op hits a fresh buffer slot.
fn rid(prefix: u8, i: u32) -> [u8; 16] {
    let mut id = [prefix; 16];
    id[12..16].copy_from_slice(&i.to_be_bytes());
    id
}

/// Buffer one ENCODE inside `txn`, returning the dispatch result so
/// callers can assert on success/failure. Each call grows the buffer
/// by exactly one (the encoder mints a fresh `MemoryId` per call).
async fn buffer_encode(fix: &Fixture, txn: [u8; 16], i: u32) -> Result<EncodeResponse, OpError> {
    let text = format!("cap-fill-{i}");
    dispatch(
        RequestBody::Encode(encode_req(rid(0xE0, i), &text, Some(txn))),
        brain_ops::RequestCaller::anonymous(),
        &fix.ctx,
    )
    .await
    .map(|r| match single_body(r) {
        ResponseBody::Encode(e) => e,
        other => panic!("expected Encode, got {other:?}"),
    })
}

#[test]
fn txn_at_exactly_1000_commits_fine() {
    // Boundary test: a buffer filled to exactly MAX_TXN_OPS must still
    // commit. The 1001st op is what fails — not the 1000th.
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn = [160; 16];
        let _ = begin(&fix, txn, 300).await; // generous timeout
        for i in 0..1000u32 {
            buffer_encode(&fix, txn, i)
                .await
                .unwrap_or_else(|e| panic!("encode #{i} failed: {e:?}"));
        }
        let c = commit(&fix, txn).await;
        assert_eq!(
            c.operations_applied, 1000,
            "exactly-MAX_TXN_OPS commit must apply all ops"
        );
    })
}

#[test]
fn txn_rejects_op_beyond_1000_cap() {
    // Append-time check: the 1000th op succeeds, the 1001st returns
    // TransactionTooLarge. The agent learns about the cap immediately
    // rather than burning thousands of doomed ops only to be rejected
    // at TXN_COMMIT.
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn = [161; 16];
        let _ = begin(&fix, txn, 300).await;
        for i in 0..1000u32 {
            buffer_encode(&fix, txn, i)
                .await
                .unwrap_or_else(|e| panic!("encode #{i} failed: {e:?}"));
        }
        // The 1001st op must fail with TransactionTooLarge carrying
        // the current ops count + cap, so the client can surface a useful
        // message.
        let err = buffer_encode(&fix, txn, 1000).await.unwrap_err();
        match err {
            OpError::TransactionTooLarge { ops, cap } => {
                assert_eq!(ops, 1000, "ops should report the current buffered count");
                assert_eq!(cap, 1000, "cap should report MAX_TXN_OPS");
            }
            other => panic!("expected TransactionTooLarge, got {other:?}"),
        }
        // Wire-level error code maps to the dedicated TransactionTooLarge
        // code (not generic Conflict or InvalidArgument) so clients
        // can detect it programmatically.
        let err = buffer_encode(&fix, txn, 1001).await.unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::TransactionTooLarge);

        // The buffer is intact at exactly 1000 — commit must succeed.
        // (Append-time rejection MUST NOT corrupt the buffer.)
        let c = commit(&fix, txn).await;
        assert_eq!(c.operations_applied, 1000);
    })
}

#[test]
fn txn_replay_still_works_when_buffer_is_full() {
    // Edge case: at the cap, an idempotent re-submit of an already-
    // buffered request_id MUST still replay (return cached response)
    // — the replay path doesn't grow the buffer, so the cap doesn't
    // apply. Otherwise an agent's automatic retry on a flaky
    // connection would fail the moment it hit the cap.
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn = [162; 16];
        let _ = begin(&fix, txn, 300).await;
        let mut first_id: u128 = 0;
        for i in 0..1000u32 {
            let resp = buffer_encode(&fix, txn, i)
                .await
                .unwrap_or_else(|e| panic!("encode #{i} failed: {e:?}"));
            if i == 500 {
                first_id = resp.memory_id;
            }
        }
        // Re-submit op #500 — same request_id, same params — should
        // hit the replay cache and return the same memory id, not
        // bump the buffer to 1001.
        let replay = buffer_encode(&fix, txn, 500)
            .await
            .expect("idempotent replay at-cap must succeed");
        assert_eq!(
            replay.memory_id, first_id,
            "replayed encode must return the cached memory id"
        );
        // Commit still works.
        let c = commit(&fix, txn).await;
        assert_eq!(c.operations_applied, 1000);
    })
}
