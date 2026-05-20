//! Integration tests for transactions (sub-task 7.9).
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
use brain_ops::{dispatch, ErrorCode, OpError, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::request::{
    EdgeKindWire, EncodeRequest, ForgetMode, ForgetRequest, LinkRequest, MemoryKindWire,
    ObservationInput, PlanBudget, PlanRequest, PlanState, ReasonRequest, RecallRequest,
    RequestBody, TxnAbortRequest, TxnBeginRequest, TxnCommitRequest, UnlinkRequest,
};
use brain_protocol::response::{
    EncodeResponse, ForgetResponse, LinkResponse, PlanResponseFrame, PlanStatus as WirePlanStatus,
    ReasonResponseFrame, RecallResponseFrame, ResponseBody, TxnAbortResponse, TxnBeginResponse,
    TxnCommitResponse, UnlinkResponse,
};
use parking_lot::Mutex;

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
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );
    Fixture {
        ctx: OpsContext::new(executor),
        _tempdir: tempdir,
    }
}

// ---------------------------------------------------------------------------
// Convenience builders.
// ---------------------------------------------------------------------------

fn encode_req(request_id: [u8; 16], text: &str, txn: Option<[u8; 16]>) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        context_id: 42,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: vec![],
        request_id,
        txn_id: txn,
        deduplicate: false,
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

fn recall_req(cue: &str, top_k: u32, txn: Option<[u8; 16]>) -> RecallRequest {
    RecallRequest {
        cue_text: cue.into(),
        top_k,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        include_edges: false,
        include_graph: false,
        include_text: false,
        request_id: None,
        txn_id: txn,
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
    match dispatch(
        RequestBody::Encode(encode_req(rid, text, txn)),
        brain_ops::RequestCaller::anonymous(),
        &fix.ctx,
    )
    .await
    .unwrap()
    {
        ResponseBody::Encode(EncodeResponse { memory_id, .. }) => memory_id,
        other => panic!("expected Encode, got {other:?}"),
    }
}

fn unwrap_begin(r: ResponseBody) -> TxnBeginResponse {
    match r {
        ResponseBody::TxnBegin(b) => b,
        other => panic!("expected TxnBegin, got {other:?}"),
    }
}
fn unwrap_commit(r: ResponseBody) -> TxnCommitResponse {
    match r {
        ResponseBody::TxnCommit(c) => c,
        other => panic!("expected TxnCommit, got {other:?}"),
    }
}
fn unwrap_abort(r: ResponseBody) -> TxnAbortResponse {
    match r {
        ResponseBody::TxnAbort(a) => a,
        other => panic!("expected TxnAbort, got {other:?}"),
    }
}
fn unwrap_link(r: ResponseBody) -> LinkResponse {
    match r {
        ResponseBody::Link(l) => l,
        other => panic!("expected Link, got {other:?}"),
    }
}
fn unwrap_unlink(r: ResponseBody) -> UnlinkResponse {
    match r {
        ResponseBody::Unlink(u) => u,
        other => panic!("expected Unlink, got {other:?}"),
    }
}
#[allow(dead_code)]
fn unwrap_forget(r: ResponseBody) -> ForgetResponse {
    match r {
        ResponseBody::Forget(f) => f,
        other => panic!("expected Forget, got {other:?}"),
    }
}
fn unwrap_recall(r: ResponseBody) -> RecallResponseFrame {
    match r {
        ResponseBody::Recall(r) => r,
        other => panic!("expected Recall, got {other:?}"),
    }
}
fn unwrap_plan(r: ResponseBody) -> PlanResponseFrame {
    match r {
        ResponseBody::Plan(p) => p,
        other => panic!("expected Plan, got {other:?}"),
    }
}
fn unwrap_reason(r: ResponseBody) -> ReasonResponseFrame {
    match r {
        ResponseBody::Reason(r) => r,
        other => panic!("expected Reason, got {other:?}"),
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
            !frame.results.iter().any(|r| r.memory_id == mid),
            "pending memory must be invisible to non-txn RECALL"
        );
    })
}

#[test]
fn commit_makes_buffered_writes_visible() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn = [20; 16];
        let _ = begin(&fix, txn, 60).await;
        let mid = encode(&fix, [21; 16], "committed", Some(txn)).await;

        let c = commit(&fix, txn).await;
        assert_eq!(c.operations_applied, 1);

        let frame = unwrap_recall(
            dispatch(
                RequestBody::Recall(recall_req("committed", 10, None)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(frame.results.iter().any(|r| r.memory_id == mid));
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
        assert!(!frame.results.iter().any(|r| r.memory_id == mid));
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
            frame.results.iter().any(|r| r.memory_id == mid),
            "in-txn RECALL must see pending memory; got {:?}",
            frame.results
        );
    })
}

#[test]
fn recall_in_txn_drops_pending_tombstone() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        // First commit a memory.
        let committed_mid = encode(&fix, [60; 16], "doomed", None).await;

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
        assert!(!in_txn.results.iter().any(|r| r.memory_id == committed_mid));

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
        assert!(outside.results.iter().any(|r| r.memory_id == committed_mid));

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
fn commit_with_unknown_txn_id_returns_txn_not_found() {
    // Mirrors `op_with_unknown_txn_id_*` but exercises the
    // handle_txn_commit path explicitly — the user's REPL trap.
    run_in_glommio(|| async {
        let fix = build_fixture();
        let err = dispatch(
            RequestBody::TxnCommit(TxnCommitRequest { txn_id: [88; 16] }),
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
fn abort_with_unknown_txn_id_returns_txn_not_found() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let err = dispatch(
            RequestBody::TxnAbort(TxnAbortRequest { txn_id: [89; 16] }),
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
fn commit_against_aborted_txn_returns_txn_expired() {
    // Symmetric to the committed case: aborting then re-committing
    // the same id should surface TxnExpired, not TxnNotFound.
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn = [130; 16];
        let _ = begin(&fix, txn, 60).await;
        let _ = dispatch(
            RequestBody::TxnAbort(TxnAbortRequest { txn_id: txn }),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap();

        let err = dispatch(
            RequestBody::TxnCommit(TxnCommitRequest { txn_id: txn }),
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
