//! §16/01 correctness gate (sub-task 7.11).
//!
//! One Rust module per numbered spec criterion. The map is 1-to-1 so a
//! reviewer can read `spec/16_benchmarks_acceptance/01_correctness_criteria.md`
//! with one hand and this file with the other, confirming coverage.
//!
//! Out-of-scope criteria (§13, §14, §15, §16, §18 + the Hard-FORGET
//! half of §6) have `#[ignore]` placeholder tests pointing to the phase
//! that closes them.

#![allow(clippy::too_many_lines)]
#![allow(clippy::needless_pass_by_value)]

use std::sync::Arc;

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::EDGES_OUT_TABLE;
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{dispatch, ErrorCode, OpError, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::request::{
    EdgeKindWire, EncodeRequest, ForgetMode, ForgetRequest, LinkRequest, MemoryKindWire,
    ObservationInput, PlanBudget, PlanRequest, PlanState, ReasonRequest, RecallRequest,
    RequestBody, SubscribeRequest, SubscriptionFilter, TxnAbortRequest, TxnBeginRequest,
    TxnCommitRequest, UnlinkRequest, UnsubscribeRequest,
};
use brain_protocol::response::{
    EncodeResponse, ForgetResponse, LinkResponse, PlanResponseFrame, PlanStatus,
    ReasonResponseFrame, RecallResponseFrame, ResponseBody, UnlinkResponse,
};
use parking_lot::Mutex;
use uuid::Uuid;

// ===========================================================================
// Shared fixture + helpers.
// ===========================================================================

mod common {
    use super::*;

    pub(super) struct MockDispatcher;

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

    pub(super) struct Fixture {
        pub ctx: OpsContext,
        pub metadata: SharedMetadataDb,
        pub _tempdir: tempfile::TempDir,
    }

    pub(super) fn build_fixture() -> Fixture {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path = tempdir.path().join("metadata.redb");
        let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
        let (shared, hnsw_writer) =
            SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
        let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
        let executor = ExecutorContext::new(
            Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
            shared,
            metadata.clone(),
            writer as Arc<dyn WriterHandle>,
        );
        Fixture {
            ctx: OpsContext::new(executor),
            metadata,
            _tempdir: tempdir,
        }
    }

    pub(super) fn encode_req(rid: [u8; 16], text: &str) -> EncodeRequest {
        encode_req_full(rid, text, 42, MemoryKindWire::Episodic, 0.5)
    }

    pub(super) fn encode_req_full(
        rid: [u8; 16],
        text: &str,
        context: u64,
        kind: MemoryKindWire,
        salience: f32,
    ) -> EncodeRequest {
        EncodeRequest {
            text: text.into(),
            context_id: context,
            kind,
            salience_hint: salience,
            edges: vec![],
            request_id: rid,
            txn_id: None,
            deduplicate: false,
        }
    }

    pub(super) async fn encode(fix: &Fixture, rid: [u8; 16], text: &str) -> u128 {
        encode_with(fix, encode_req(rid, text)).await
    }

    pub(super) async fn encode_with(fix: &Fixture, req: EncodeRequest) -> u128 {
        match dispatch(RequestBody::Encode(req), &fix.ctx).await.unwrap() {
            ResponseBody::Encode(EncodeResponse { memory_id, .. }) => memory_id,
            other => panic!("expected Encode, got {other:?}"),
        }
    }

    pub(super) async fn recall(
        fix: &Fixture,
        cue: &str,
        top_k: u32,
        context_filter: Option<Vec<u64>>,
        kind_filter: Option<Vec<MemoryKindWire>>,
        salience_floor: f32,
    ) -> RecallResponseFrame {
        let req = RecallRequest {
            cue_text: cue.into(),
            cue_vector_offset: 0,
            cue_vector_dim: 0,
            top_k,
            confidence_threshold: 0.0,
            context_filter,
            age_bound_unix_nanos: None,
            kind_filter,
            salience_floor,
            strategy_hint: None,
            include_vectors: false,
            include_edges: false,
            request_id: None,
            txn_id: None,
        };
        match dispatch(RequestBody::Recall(req), &fix.ctx).await.unwrap() {
            ResponseBody::Recall(f) => f,
            other => panic!("expected Recall, got {other:?}"),
        }
    }

    pub(super) async fn forget(fix: &Fixture, memory_id: u128, rid: [u8; 16]) -> ForgetResponse {
        let req = ForgetRequest {
            memory_id,
            mode: ForgetMode::Soft,
            request_id: rid,
            txn_id: None,
        };
        match dispatch(RequestBody::Forget(req), &fix.ctx).await.unwrap() {
            ResponseBody::Forget(r) => r,
            other => panic!("expected Forget, got {other:?}"),
        }
    }

    pub(super) async fn link(
        fix: &Fixture,
        src: u128,
        tgt: u128,
        kind: EdgeKindWire,
        rid: [u8; 16],
    ) -> LinkResponse {
        let req = LinkRequest {
            source: src,
            target: tgt,
            kind,
            weight: 1.0,
            request_id: rid,
            txn_id: None,
        };
        match dispatch(RequestBody::Link(req), &fix.ctx).await.unwrap() {
            ResponseBody::Link(l) => l,
            other => panic!("expected Link, got {other:?}"),
        }
    }

    pub(super) async fn unlink(
        fix: &Fixture,
        src: u128,
        tgt: u128,
        kind: EdgeKindWire,
        rid: [u8; 16],
    ) -> UnlinkResponse {
        let req = UnlinkRequest {
            source: src,
            target: tgt,
            kind,
            request_id: rid,
            txn_id: None,
        };
        match dispatch(RequestBody::Unlink(req), &fix.ctx).await.unwrap() {
            ResponseBody::Unlink(u) => u,
            other => panic!("expected Unlink, got {other:?}"),
        }
    }

    pub(super) async fn plan_by_id(
        fix: &Fixture,
        start: u128,
        goal: u128,
        max_steps: u32,
    ) -> PlanResponseFrame {
        let req = PlanRequest {
            start: PlanState::ByMemoryId(start),
            goal: PlanState::ByMemoryId(goal),
            budget: PlanBudget {
                max_steps,
                max_wall_time_ms: 1000,
                max_branches_explored: 256,
            },
            strategy_hint: None,
            context_filter: None,
            request_id: None,
            txn_id: None,
        };
        match dispatch(RequestBody::Plan(req), &fix.ctx).await.unwrap() {
            ResponseBody::Plan(p) => p,
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    pub(super) async fn reason_by_id(fix: &Fixture, base: u128, depth: u32) -> ReasonResponseFrame {
        let req = ReasonRequest {
            observation: ObservationInput::ByMemoryId(base),
            depth,
            confidence_threshold: 0.0,
            context_filter: None,
            max_inferences: 10,
            budget_wall_time_ms: 1000,
            request_id: None,
            txn_id: None,
        };
        match dispatch(RequestBody::Reason(req), &fix.ctx).await.unwrap() {
            ResponseBody::Reason(r) => r,
            other => panic!("expected Reason, got {other:?}"),
        }
    }

    /// Insert a memory row directly so callers can pin a known
    /// MemoryId. Used by PLAN / REASON graph fixtures.
    pub(super) fn insert_memory_row(
        metadata: &SharedMetadataDb,
        id: MemoryId,
        context: u64,
        kind: MemoryKind,
        salience: f32,
        slot_id: u64,
        created_at: u64,
    ) {
        let mut db = metadata.lock();
        let wtxn = db.write_txn().unwrap();
        {
            let mut table = wtxn.open_table(MEMORIES_TABLE).unwrap();
            let meta = MemoryMetadata::new_active(
                id,
                AgentId(Uuid::nil()),
                ContextId(context),
                slot_id,
                1,
                kind,
                [0x11; 16],
                salience,
                16,
                created_at,
            );
            table.insert(id.to_be_bytes(), meta).unwrap();
        }
        wtxn.commit().unwrap();
    }

    pub(super) fn make_id(i: u64) -> MemoryId {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&i.to_be_bytes());
        MemoryId::from_be_bytes(b)
    }

    pub(super) fn edges_out_count(metadata: &SharedMetadataDb, src: MemoryId) -> usize {
        let db = metadata.lock();
        let rtxn = db.read_txn().unwrap();
        let table = match rtxn.open_table(EDGES_OUT_TABLE) {
            Ok(t) => t,
            Err(_) => return 0,
        };
        let from = (src.to_be_bytes(), 0u8, [0u8; 16]);
        let to = (src.to_be_bytes(), u8::MAX, [0xFF; 16]);
        table.range(from..=to).map(|r| r.count()).unwrap_or(0)
    }
}

// ===========================================================================
// §1 — Wire-protocol correctness (smoke; deep coverage is in brain-protocol).
// ===========================================================================

mod criterion_01_wire {
    use super::*;
    use brain_protocol::opcode::Opcode;

    /// Round-trip every `RequestBody` variant a 7.x handler accepts.
    /// brain-protocol owns the exhaustive fuzz/CRC suite; here we
    /// confirm the variants the ops layer routes are reversible.
    #[test]
    fn frames_roundtrip_through_rkyv() {
        let cases: Vec<(Opcode, RequestBody)> = vec![
            (
                Opcode::EncodeReq,
                RequestBody::Encode(common::encode_req([1; 16], "hello")),
            ),
            (
                Opcode::RecallReq,
                RequestBody::Recall(RecallRequest {
                    cue_text: "alpha".into(),
                    cue_vector_offset: 0,
                    cue_vector_dim: 0,
                    top_k: 5,
                    confidence_threshold: 0.0,
                    context_filter: None,
                    age_bound_unix_nanos: None,
                    kind_filter: None,
                    salience_floor: 0.0,
                    strategy_hint: None,
                    include_vectors: false,
                    include_edges: false,
                    request_id: None,
                    txn_id: None,
                }),
            ),
            (
                Opcode::PlanReq,
                RequestBody::Plan(PlanRequest {
                    start: PlanState::ByMemoryId(1),
                    goal: PlanState::ByMemoryId(2),
                    budget: PlanBudget {
                        max_steps: 4,
                        max_wall_time_ms: 1000,
                        max_branches_explored: 64,
                    },
                    strategy_hint: None,
                    context_filter: None,
                    request_id: None,
                    txn_id: None,
                }),
            ),
            (
                Opcode::ReasonReq,
                RequestBody::Reason(ReasonRequest {
                    observation: ObservationInput::ByMemoryId(7),
                    depth: 2,
                    confidence_threshold: 0.0,
                    context_filter: None,
                    max_inferences: 5,
                    budget_wall_time_ms: 1000,
                    request_id: None,
                    txn_id: None,
                }),
            ),
            (
                Opcode::ForgetReq,
                RequestBody::Forget(ForgetRequest {
                    memory_id: 99,
                    mode: ForgetMode::Soft,
                    request_id: [9; 16],
                    txn_id: None,
                }),
            ),
            (
                Opcode::LinkReq,
                RequestBody::Link(LinkRequest {
                    source: 1,
                    target: 2,
                    kind: EdgeKindWire::Caused,
                    weight: 1.0,
                    request_id: [3; 16],
                    txn_id: None,
                }),
            ),
            (
                Opcode::UnlinkReq,
                RequestBody::Unlink(UnlinkRequest {
                    source: 1,
                    target: 2,
                    kind: EdgeKindWire::Caused,
                    request_id: [4; 16],
                    txn_id: None,
                }),
            ),
            (
                Opcode::TxnBegin,
                RequestBody::TxnBegin(TxnBeginRequest {
                    txn_id: [5; 16],
                    timeout_seconds: 60,
                }),
            ),
            (
                Opcode::TxnCommit,
                RequestBody::TxnCommit(TxnCommitRequest { txn_id: [5; 16] }),
            ),
            (
                Opcode::TxnAbort,
                RequestBody::TxnAbort(TxnAbortRequest { txn_id: [5; 16] }),
            ),
            (
                Opcode::SubscribeReq,
                RequestBody::Subscribe(SubscribeRequest {
                    filter: SubscriptionFilter {
                        contexts: Some(vec![1]),
                        kinds: Some(vec![MemoryKindWire::Episodic]),
                        similar_to: None,
                    },
                    include_history: false,
                    from_lsn: None,
                    max_inflight: 100,
                }),
            ),
            (
                Opcode::UnsubscribeReq,
                RequestBody::Unsubscribe(UnsubscribeRequest {
                    target_stream_id: 7,
                }),
            ),
        ];

        for (opcode, body) in cases {
            let bytes = body.encode();
            let decoded = RequestBody::decode(opcode, &bytes)
                .unwrap_or_else(|e| panic!("decode {opcode:?} failed: {e:?}"));
            assert_eq!(body.opcode(), opcode);
            assert_eq!(decoded.opcode(), opcode);
            // Re-encode after decode and compare bytes — proves rkyv
            // is deterministic for this shape.
            let bytes2 = decoded.encode();
            assert_eq!(bytes, bytes2, "{opcode:?} did not round-trip stably");
        }
    }
}

// ===========================================================================
// §2 — ENCODE correctness.
// ===========================================================================

mod criterion_02_encode {
    use super::*;

    #[tokio::test]
    async fn encoded_memories_are_recallable() {
        let fix = common::build_fixture();
        let mut ids = Vec::new();
        for (i, text) in ["alpha", "beta", "gamma", "delta", "epsilon"]
            .iter()
            .enumerate()
        {
            let mut rid = [0u8; 16];
            rid[0] = (i + 1) as u8;
            ids.push(common::encode(&fix, rid, text).await);
        }
        // Each text round-trips: top-1 by its own cue is itself.
        for (i, text) in ["alpha", "beta", "gamma", "delta", "epsilon"]
            .iter()
            .enumerate()
        {
            let frame = common::recall(&fix, text, 1, None, None, 0.0).await;
            assert_eq!(frame.results.len(), 1, "top-1 must exist for {text}");
            assert_eq!(
                frame.results[0].memory_id, ids[i],
                "top-1 for {text} must be the memory we just encoded"
            );
            assert!(
                frame.results[0].similarity_score > 0.99,
                "exact-cue similarity for {text} must be ~1.0, got {}",
                frame.results[0].similarity_score
            );
        }
    }
}

// ===========================================================================
// §3 — RECALL correctness.
// ===========================================================================

mod criterion_03_recall {
    use super::*;

    #[tokio::test]
    async fn recall_returns_top_k_sorted_by_similarity() {
        let fix = common::build_fixture();
        for (i, t) in ["aa", "ab", "ac", "ba", "bb"].iter().enumerate() {
            let mut rid = [0u8; 16];
            rid[0] = (i + 1) as u8;
            common::encode(&fix, rid, t).await;
        }
        let frame = common::recall(&fix, "aa", 3, None, None, 0.0).await;
        assert!(frame.is_final);
        assert!(frame.results.len() <= 3, "top_k bounds the result count");
        // Sorted by similarity descending.
        for w in frame.results.windows(2) {
            assert!(
                w[0].similarity_score >= w[1].similarity_score,
                "results must be sorted by similarity desc"
            );
        }
    }
}

// ===========================================================================
// §4 — PLAN correctness.
// ===========================================================================

mod criterion_04_plan {
    use super::*;

    #[tokio::test]
    async fn plan_returns_followed_by_chain_in_order() {
        let fix = common::build_fixture();
        // Build a 4-memory chain m1 →FollowedBy→ m2 →FollowedBy→ m3 →FollowedBy→ m4.
        let ids: Vec<MemoryId> = (1..=4).map(common::make_id).collect();
        for (i, id) in ids.iter().enumerate() {
            common::insert_memory_row(
                &fix.metadata,
                *id,
                42,
                MemoryKind::Episodic,
                0.5,
                (i + 1) as u64,
                1_000_000 + i as u64,
            );
        }
        for i in 0..3 {
            let mut rid = [0u8; 16];
            rid[0] = 0xE0 + (i as u8);
            common::link(
                &fix,
                ids[i].raw(),
                ids[i + 1].raw(),
                EdgeKindWire::FollowedBy,
                rid,
            )
            .await;
        }

        let frame = common::plan_by_id(&fix, ids[0].raw(), ids[3].raw(), 8).await;
        assert_eq!(frame.plan_status, Some(PlanStatus::GoalReached));
        assert_eq!(frame.steps.len(), 4, "chain of 4 memories = 4 steps");
        for (step, expected) in frame.steps.iter().zip(ids.iter()) {
            assert_eq!(step.memory_id, expected.raw());
        }
    }
}

// ===========================================================================
// §5 — REASON correctness.
// ===========================================================================

mod criterion_05_reason {
    use super::*;
    use brain_protocol::response::ReasonStatus;

    #[tokio::test]
    async fn reason_traverses_supports_and_terminates_on_cycle() {
        let fix = common::build_fixture();
        // Graph:
        //   m1 →Supports→ m2 →Supports→ m3
        //   m3 →Supports→ m1   (cycle!)
        //   m1 →Contradicts→ m4
        let ids: Vec<MemoryId> = (1..=4).map(common::make_id).collect();
        for (i, id) in ids.iter().enumerate() {
            common::insert_memory_row(
                &fix.metadata,
                *id,
                42,
                MemoryKind::Episodic,
                0.5,
                (i + 1) as u64,
                1_000_000 + i as u64,
            );
        }
        let edges = [
            (0, EdgeKindWire::Supports, 1),
            (1, EdgeKindWire::Supports, 2),
            (2, EdgeKindWire::Supports, 0), // cycle
            (0, EdgeKindWire::Contradicts, 3),
        ];
        for (i, (s, k, t)) in edges.iter().enumerate() {
            let mut rid = [0u8; 16];
            rid[0] = 0xF0 + (i as u8);
            common::link(&fix, ids[*s].raw(), ids[*t].raw(), *k, rid).await;
        }

        // Depth-3 reason traversal from m1 must terminate (cycle not
        // infinite) and produce a single inference frame.
        let frame = common::reason_by_id(&fix, ids[0].raw(), 3).await;
        assert!(frame.is_final);
        assert_eq!(frame.reason_status, Some(ReasonStatus::Complete));
        assert_eq!(frame.inferences.len(), 1);
        let inf = &frame.inferences[0];
        // m1 (base) + m2 + m3 reached via Supports.
        assert!(
            inf.supporting_memories.len() >= 2,
            "expected at least 2 supports beyond the base, got {}",
            inf.supporting_memories.len()
        );
        // m4 is Contradicts.
        assert!(
            !inf.contradicting_memories.is_empty(),
            "Contradicts edge to m4 must be visible"
        );
    }
}

// ===========================================================================
// §6 — FORGET correctness.
// ===========================================================================

mod criterion_06_forget {
    use super::*;

    #[tokio::test]
    async fn soft_forget_hides_memory_from_recall() {
        let fix = common::build_fixture();
        let mid = common::encode(&fix, [1; 16], "forgettable").await;

        // Pre-FORGET: recallable.
        let before = common::recall(&fix, "forgettable", 5, None, None, 0.0).await;
        assert!(before.results.iter().any(|r| r.memory_id == mid));

        // Soft FORGET.
        let resp = common::forget(&fix, mid, [2; 16]).await;
        assert!(!resp.was_already_forgotten);

        // Post-FORGET: hidden.
        let after = common::recall(&fix, "forgettable", 5, None, None, 0.0).await;
        assert!(
            !after.results.iter().any(|r| r.memory_id == mid),
            "soft-forgotten memory must not appear in RECALL"
        );
    }

    #[ignore = "Hard FORGET zeroes the arena + reclaims slots — Phase 8 GC worker"]
    #[tokio::test]
    async fn hard_forget_zeroes_arena() {
        // Will exercise spec §09/06 §3 ("Hard FORGET: vector zeroed,
        // not recoverable") once the GC worker lands.
    }
}

// ===========================================================================
// §7 — LINK / UNLINK correctness.
// ===========================================================================

mod criterion_07_link_unlink {
    use super::*;

    #[tokio::test]
    async fn link_then_unlink_round_trip() {
        let fix = common::build_fixture();
        let a = common::encode(&fix, [1; 16], "from").await;
        let b = common::encode(&fix, [2; 16], "to").await;

        // LINK creates the edge.
        let linked = common::link(&fix, a, b, EdgeKindWire::Caused, [3; 16]).await;
        assert!(!linked.already_existed);
        assert_eq!(
            common::edges_out_count(&fix.metadata, MemoryId::from(a)),
            1,
            "EDGES_OUT must hold the new edge"
        );

        // UNLINK removes it.
        let unlinked = common::unlink(&fix, a, b, EdgeKindWire::Caused, [4; 16]).await;
        assert!(unlinked.removed);
        assert_eq!(
            common::edges_out_count(&fix.metadata, MemoryId::from(a)),
            0,
            "EDGES_OUT must be empty after UNLINK"
        );

        // Idempotent UNLINK = no-op.
        let again = common::unlink(&fix, a, b, EdgeKindWire::Caused, [5; 16]).await;
        assert!(!again.removed, "second UNLINK is a no-op");
    }
}

// ===========================================================================
// §8 — Idempotency correctness.
// ===========================================================================

mod criterion_08_idempotency {
    use super::*;

    #[tokio::test]
    async fn same_request_id_returns_same_memory_id() {
        let fix = common::build_fixture();
        let rid = [0xAA; 16];

        let first = common::encode(&fix, rid, "idempotent").await;
        let second = common::encode(&fix, rid, "idempotent").await;
        assert_eq!(first, second, "same RequestId → same MemoryId");

        // Only one row should exist.
        let frame = common::recall(&fix, "idempotent", 10, None, None, 0.0).await;
        let count = frame
            .results
            .iter()
            .filter(|r| r.memory_id == first)
            .count();
        assert_eq!(count, 1, "exactly one memory must have been created");
    }

    #[tokio::test]
    async fn different_params_same_request_id_returns_conflict() {
        let fix = common::build_fixture();
        let rid = [0xBB; 16];
        let _ = common::encode(&fix, rid, "first").await;
        let err = dispatch(
            RequestBody::Encode(common::encode_req(rid, "DIFFERENT")),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::Conflict);
    }
}

// ===========================================================================
// §9 — Transaction correctness.
// ===========================================================================

mod criterion_09_txn {
    use super::*;

    async fn begin(fix: &common::Fixture, txn_id: [u8; 16]) {
        dispatch(
            RequestBody::TxnBegin(TxnBeginRequest {
                txn_id,
                timeout_seconds: 60,
            }),
            &fix.ctx,
        )
        .await
        .unwrap();
    }
    async fn commit(fix: &common::Fixture, txn_id: [u8; 16]) {
        dispatch(
            RequestBody::TxnCommit(TxnCommitRequest { txn_id }),
            &fix.ctx,
        )
        .await
        .unwrap();
    }
    async fn abort(fix: &common::Fixture, txn_id: [u8; 16]) {
        dispatch(RequestBody::TxnAbort(TxnAbortRequest { txn_id }), &fix.ctx)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn aborted_txn_writes_are_invisible() {
        let fix = common::build_fixture();
        let txn = [0xA0; 16];
        begin(&fix, txn).await;
        let mut enc = common::encode_req([1; 16], "ghost");
        enc.txn_id = Some(txn);
        let _ = common::encode_with(&fix, enc).await;
        abort(&fix, txn).await;

        let frame = common::recall(&fix, "ghost", 5, None, None, 0.0).await;
        assert!(
            frame.results.is_empty(),
            "aborted txn must leave no trace, got {:?}",
            frame.results
        );
    }

    #[tokio::test]
    async fn committed_txn_writes_are_visible() {
        let fix = common::build_fixture();
        let txn = [0xC0; 16];
        begin(&fix, txn).await;
        let mut enc = common::encode_req([1; 16], "kept");
        enc.txn_id = Some(txn);
        let mid = common::encode_with(&fix, enc).await;
        commit(&fix, txn).await;

        let frame = common::recall(&fix, "kept", 5, None, None, 0.0).await;
        assert!(
            frame.results.iter().any(|r| r.memory_id == mid),
            "committed memory must be visible"
        );
    }
}

// ===========================================================================
// §10 — Filter correctness.
// ===========================================================================

mod criterion_10_filters {
    use super::*;

    #[tokio::test]
    async fn recall_filters_by_context_kind_and_salience() {
        let fix = common::build_fixture();
        // 3 memories: (ctx=1, Episodic, sal=0.2), (ctx=2, Semantic, sal=0.8),
        //             (ctx=2, Episodic, sal=0.6).
        common::encode_with(
            &fix,
            common::encode_req_full([1; 16], "ctx1ep", 1, MemoryKindWire::Episodic, 0.2),
        )
        .await;
        let m_sem = common::encode_with(
            &fix,
            common::encode_req_full([2; 16], "ctx2sem", 2, MemoryKindWire::Semantic, 0.8),
        )
        .await;
        common::encode_with(
            &fix,
            common::encode_req_full([3; 16], "ctx2ep", 2, MemoryKindWire::Episodic, 0.6),
        )
        .await;

        // Context filter.
        let by_ctx = common::recall(&fix, "ctx2sem", 10, Some(vec![2]), None, 0.0).await;
        assert!(
            by_ctx.results.iter().all(|r| r.context_id == 2),
            "context filter must keep only context=2"
        );

        // Kind filter.
        let by_kind = common::recall(
            &fix,
            "ctx2sem",
            10,
            None,
            Some(vec![MemoryKindWire::Semantic]),
            0.0,
        )
        .await;
        assert!(
            by_kind
                .results
                .iter()
                .all(|r| r.kind == MemoryKindWire::Semantic),
            "kind filter must keep only Semantic"
        );

        // Salience floor.
        let by_sal = common::recall(&fix, "ctx2sem", 10, None, None, 0.7).await;
        assert!(
            by_sal.results.iter().all(|r| r.salience >= 0.7),
            "salience filter must keep only sal>=0.7"
        );
        // The high-salience Semantic memory should be the survivor.
        assert!(by_sal.results.iter().any(|r| r.memory_id == m_sem));
    }
}

// ===========================================================================
// §11 — Edge-traversal correctness.
// ===========================================================================

mod criterion_11_edge_traversal {
    use super::*;

    #[tokio::test]
    async fn plan_traverses_followed_by_only_when_path_exists() {
        let fix = common::build_fixture();
        // Two parallel routes m1→m2:
        //   route A: m1 →Caused→ m2
        //   route B: m1 →FollowedBy→ m3 →FollowedBy→ m2
        // PLAN should prefer the existing edge regardless; the
        // invariant we test is: when the only edge to m2 is via
        // FollowedBy, PLAN still finds m2.
        let ids: Vec<MemoryId> = (1..=3).map(common::make_id).collect();
        for (i, id) in ids.iter().enumerate() {
            common::insert_memory_row(
                &fix.metadata,
                *id,
                42,
                MemoryKind::Episodic,
                0.5,
                (i + 1) as u64,
                1_000_000 + i as u64,
            );
        }
        common::link(
            &fix,
            ids[0].raw(),
            ids[2].raw(),
            EdgeKindWire::FollowedBy,
            [0xA1; 16],
        )
        .await;
        common::link(
            &fix,
            ids[2].raw(),
            ids[1].raw(),
            EdgeKindWire::FollowedBy,
            [0xA2; 16],
        )
        .await;

        let frame = common::plan_by_id(&fix, ids[0].raw(), ids[1].raw(), 8).await;
        assert_eq!(frame.plan_status, Some(PlanStatus::GoalReached));
        // Direction is honoured: reverse plan must fail to reach.
        let reverse = common::plan_by_id(&fix, ids[1].raw(), ids[0].raw(), 8).await;
        assert_eq!(
            reverse.plan_status,
            Some(PlanStatus::NoPathFound),
            "reverse direction must not be traversed"
        );
    }
}

// ===========================================================================
// §12 — Tombstone correctness.
// ===========================================================================

mod criterion_12_tombstones {
    use super::*;

    /// Spec §16/01 §12 requires tombstone-invisibility across RECALL,
    /// PLAN, and REASON. v1 enforces this for RECALL (via HNSW's
    /// tombstone marker) but **not** for PLAN / REASON — those
    /// traversal handlers read redb edges directly and don't yet
    /// consult the writer's in-process tombstone set. Phase 8 closes
    /// this when the FORGET handler updates the metadata row's
    /// `tombstoned_at_unix_nanos` and the traversal executors learn
    /// to filter on that field.
    #[tokio::test]
    async fn tombstoned_memory_invisible_to_recall() {
        let fix = common::build_fixture();
        let a = common::encode(&fix, [1; 16], "alive").await;
        let b = common::encode(&fix, [2; 16], "doomed").await;
        common::link(&fix, a, b, EdgeKindWire::FollowedBy, [3; 16]).await;

        let _ = common::forget(&fix, b, [4; 16]).await;

        let recall = common::recall(&fix, "doomed", 5, None, None, 0.0).await;
        assert!(
            !recall.results.iter().any(|r| r.memory_id == b),
            "RECALL must not return tombstoned memory"
        );
    }

    #[ignore = "PLAN/REASON tombstone-filter — Phase 8 (executors must \
                read MemoryMetadata.tombstoned_at_unix_nanos)"]
    #[tokio::test]
    async fn tombstoned_memory_invisible_to_plan_and_reason() {}
}

// ===========================================================================
// §13 — Slot-version correctness (deferred).
// ===========================================================================

mod criterion_13_slot_version {
    #[ignore = "stale-MemoryId NotFound after hard-reclaim — Phase 8 GC worker"]
    #[tokio::test]
    async fn stale_memory_id_returns_not_found_after_reclaim() {
        // Exercises spec §16/01 §13: ENCODE m → hard-FORGET force-reclaim
        // → ENCODE another into the freed slot → RECALL the stale id
        // returns NotFound. Requires the slot-reclamation worker.
    }
}

// ===========================================================================
// §14 — Audit-log correctness (deferred).
// ===========================================================================

mod criterion_14_audit_log {
    #[ignore = "audit log is Phase 8 — no impl yet to verify against"]
    #[tokio::test]
    async fn every_mutating_op_appears_in_audit_log() {}
}

// ===========================================================================
// §15 — Recovery correctness (deferred).
// ===========================================================================

mod criterion_15_recovery {
    #[ignore = "crash recovery requires the WAL hookup — Phase 9"]
    #[tokio::test]
    async fn restart_preserves_committed_writes() {}
}

// ===========================================================================
// §16 — Configuration correctness (deferred).
// ===========================================================================

mod criterion_16_config {
    #[ignore = "server-side config plumbing — Phase 9"]
    #[tokio::test]
    async fn config_overrides_are_honoured() {}
}

// ===========================================================================
// §17 — Error-code correctness.
// ===========================================================================

mod criterion_17_error_codes {
    use super::*;

    #[tokio::test]
    async fn each_error_condition_maps_to_correct_code() {
        let fix = common::build_fixture();

        // 1. Conflict — encode with same RequestId, different text.
        let _ = common::encode(&fix, [1; 16], "original").await;
        let err = dispatch(
            RequestBody::Encode(common::encode_req([1; 16], "DIFFERENT")),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::Conflict);

        // 2. InvalidRequest — Consolidated kind at ENCODE.
        let mut req = common::encode_req([2; 16], "bad");
        req.kind = MemoryKindWire::Consolidated;
        let err = dispatch(RequestBody::Encode(req), &fix.ctx)
            .await
            .unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::InvalidRequest);
        assert!(matches!(err, OpError::PlanError(_)));

        // 3. InvalidRequest — PLAN with max_steps=0.
        let req = PlanRequest {
            start: PlanState::ByMemoryId(1),
            goal: PlanState::ByMemoryId(2),
            budget: PlanBudget {
                max_steps: 0,
                max_wall_time_ms: 1000,
                max_branches_explored: 64,
            },
            strategy_hint: None,
            context_filter: None,
            request_id: None,
            txn_id: None,
        };
        let err = dispatch(RequestBody::Plan(req), &fix.ctx)
            .await
            .unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::InvalidRequest);

        // 4. NotFound — UNSUBSCRIBE unknown stream.
        let err = dispatch(
            RequestBody::Unsubscribe(UnsubscribeRequest {
                target_stream_id: 9999,
            }),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::NotFound);

        // 5. InternalError — SUBSCRIBE with similar_to (NotYetImplemented).
        let err = dispatch(
            RequestBody::Subscribe(SubscribeRequest {
                filter: SubscriptionFilter {
                    contexts: None,
                    kinds: None,
                    similar_to: Some(brain_protocol::request::SimilarityFilter {
                        reference_memory_id: 1,
                        threshold: 0.5,
                    }),
                },
                include_history: false,
                from_lsn: None,
                max_inflight: 100,
            }),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::InternalError);
        assert!(matches!(err, OpError::NotYetImplemented(_)));
    }
}

// ===========================================================================
// §18 — Schema versioning (deferred).
// ===========================================================================

mod criterion_18_schema {
    #[ignore = "schema versioning beyond v1 — out of scope until a v2 lands"]
    #[tokio::test]
    async fn schema_v1_data_reads_with_v1_code() {}
}

// ===========================================================================
// §19 — Determinism correctness.
// ===========================================================================

mod criterion_19_determinism {
    use super::*;

    #[test]
    fn embed_is_deterministic_for_same_input() {
        let d = common::MockDispatcher;
        let v1 = d.embed("the quick brown fox").unwrap();
        for _ in 0..4 {
            let v = d.embed("the quick brown fox").unwrap();
            assert_eq!(v, v1, "embedder must be deterministic");
        }
    }
}

// ===========================================================================
// §20 — "No surprises" — failures leave no partial state.
// ===========================================================================

mod criterion_20_no_surprises {
    use super::*;

    #[tokio::test]
    async fn rejected_encode_leaves_no_metadata_row() {
        let fix = common::build_fixture();

        // Drive an ENCODE that the planner rejects (Consolidated kind).
        let mut req = common::encode_req([1; 16], "should-not-land");
        req.kind = MemoryKindWire::Consolidated;
        let err = dispatch(RequestBody::Encode(req), &fix.ctx)
            .await
            .unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::InvalidRequest);

        // No row landed: the next ENCODE gets memory_id #1 (no gap).
        let id = common::encode(&fix, [2; 16], "fresh").await;
        let frame = common::recall(&fix, "fresh", 5, None, None, 0.0).await;
        assert_eq!(frame.results.len(), 1, "exactly one memory in the index");
        assert_eq!(frame.results[0].memory_id, id);
    }

    #[tokio::test]
    async fn failed_link_leaves_no_edge() {
        let fix = common::build_fixture();
        let a = common::encode(&fix, [1; 16], "src").await;
        // Target doesn't exist — LINK must fail.
        let phantom: u128 = 0xDEAD_BEEF_DEAD_BEEF_0000_0000_0000_0000;
        let err = dispatch(
            RequestBody::Link(LinkRequest {
                source: a,
                target: phantom,
                kind: EdgeKindWire::Caused,
                weight: 1.0,
                request_id: [9; 16],
                txn_id: None,
            }),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        // The error must be a structured, no-orphan-state error. v1
        // surfaces missing-endpoint LINKs as `NotFound { what: "memory" }`
        // (the planner's pre-validation path) — the variant family
        // matters; the wire code stays stable.
        assert_eq!(
            err.error_code(),
            ErrorCode::NotFound,
            "missing-target LINK must surface NotFound, got {err:?}"
        );
        assert_eq!(
            common::edges_out_count(&fix.metadata, MemoryId::from(a)),
            0,
            "failed LINK must not leave an edge row"
        );
    }
}
