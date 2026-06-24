//! Integration tests for SUBSCRIBE.
//!
//! Covers:
//! - **Lifecycle**: register/unregister, NotFound on unknown stream,
//!   NotYetImplemented on `similar_to`.
//! - **Publication**: encode and forget publish events with
//!   monotonically increasing LSNs; TXN_COMMIT publishes all
//!   buffered events in order; TXN_ABORT publishes nothing.
//! - **Filter**: contexts / kinds / null / combined.
//! - **Dispatcher** (`handle_subscribe`): first-event match,
//!   timeout, `LsnTooOld` for `from_lsn=Some`.
//! - **Backpressure**: a lagged subscriber surfaces `Overloaded`,
//!   `final_lsn` stays frozen at `started_at_lsn`.
//!
//! Runs on Glommio via `run_in_glommio` — same runtime production
//! uses (`brain_ops::dispatch` lives inside the per-shard executor).

use std::sync::Arc;
use std::time::Duration;

use brain_core::{ContextId, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::test_support::{run_in_glommio, single_body};
use brain_ops::{
    dispatch, ErrorCode, EventBus, EventEnvelope, OpError, OpsContext, RealWriterHandle,
};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{
    EncodeRequest, ForgetMode, ForgetRequest, MemoryKindWire, RequestBody, SimilarityFilter,
    SubscribeRequest, SubscriptionFilter, TxnBeginRequest, TxnCommitRequest, UnsubscribeRequest,
};
use brain_protocol::envelope::response::{EventType, ResponseBody};
use futures_lite::FutureExt;
use tokio::sync::broadcast;

// ---------------------------------------------------------------------------
// Mock dispatcher.
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
// Fixture: builds an OpsContext where the writer publishes onto the
// same bus the context's registry reads from.
// ---------------------------------------------------------------------------

struct Fixture {
    ctx: OpsContext,
    bus: Arc<EventBus>,
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    build_fixture_with(EventBus::default())
}

fn build_fixture_with_capacity(cap: usize) -> Fixture {
    build_fixture_with(EventBus::new(cap))
}

fn build_fixture_with(bus: EventBus) -> Fixture {
    let bus = Arc::new(bus);
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer =
        Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer).with_event_bus(bus.clone()));
    let executor = ExecutorContext::new(
        Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );
    let ctx = brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor)
        .with_event_bus(bus.clone())
        .with_subscribe_poll_window(Duration::from_millis(200));
    Fixture {
        ctx,
        bus,
        _tempdir: tempdir,
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

// `_kind` is accepted for call-site compatibility but ignored: the
// write router decides the memory kind now (always Episodic).
fn encode_req(
    request_id: [u8; 16],
    text: &str,
    context_id: u64,
    _kind: MemoryKindWire,
) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        context_id,
        request_id,
        txn_id: None,
        occurred_at_unix_nanos: None,
    }
}

fn empty_filter() -> SubscriptionFilter {
    SubscriptionFilter {
        contexts: None,
        kinds: None,
        similar_to: None,
        agents: None,
    }
}

fn sub_req(filter: SubscriptionFilter) -> SubscribeRequest {
    SubscribeRequest {
        filter,
        include_history: false,
        from_lsn: None,
        max_inflight: 100,
    }
}

async fn do_encode(ctx: &OpsContext, req: EncodeRequest) -> u128 {
    let outcome = dispatch(
        RequestBody::Encode(req),
        brain_ops::RequestCaller::anonymous(),
        ctx,
    )
    .await
    .unwrap();
    match single_body(outcome) {
        ResponseBody::Encode(r) => r.memory_id,
        other => panic!("expected Encode resp, got {other:?}"),
    }
}

async fn do_forget(ctx: &OpsContext, memory_id: u128, request_id: [u8; 16]) {
    let req = ForgetRequest {
        memory_id,
        mode: ForgetMode::Soft,
        request_id,
        txn_id: None,
    };
    let outcome = dispatch(
        RequestBody::Forget(req),
        brain_ops::RequestCaller::anonymous(),
        ctx,
    )
    .await
    .unwrap();
    match single_body(outcome) {
        ResponseBody::Forget(_) => {}
        other => panic!("expected Forget resp, got {other:?}"),
    }
}

/// Drain matching events out of a receiver with a bounded wait.
/// Races the broadcast `recv()` against a Glommio timer.
async fn try_recv(
    rx: &mut broadcast::Receiver<EventEnvelope>,
    timeout: Duration,
) -> Option<EventEnvelope> {
    let recv_arm = async { Some(rx.recv().await) };
    let timer_arm = async {
        glommio::timer::sleep(timeout).await;
        None
    };
    match recv_arm.or(timer_arm).await {
        Some(Ok(env)) => Some(env),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Lifecycle (3).
// ---------------------------------------------------------------------------

#[test]
fn lifecycle_subscribe_then_unsubscribe_returns_final_lsn() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let handle = fix
            .ctx
            .subscriptions
            .register(&sub_req(empty_filter()))
            .unwrap();
        assert_eq!(handle.target_stream_id, 1);
        assert_eq!(handle.started_at_lsn, 0);
        let final_lsn = fix
            .ctx
            .subscriptions
            .unregister(handle.target_stream_id)
            .unwrap();
        assert_eq!(final_lsn, 0);
        assert_eq!(fix.ctx.subscriptions.active_count(), 0);
    })
}

#[test]
fn lifecycle_unsubscribe_unknown_stream_id_returns_not_found() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let resp = dispatch(
            RequestBody::Unsubscribe(UnsubscribeRequest {
                target_stream_id: 99,
            }),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await;
        let err = resp.unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::NotFound);
    })
}

#[test]
fn lifecycle_similar_to_filter_returns_not_yet_implemented() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let mut filter = empty_filter();
        filter.similar_to = Some(SimilarityFilter {
            reference_memory_id: 1,
            threshold: 0.5,
        });
        let err = match fix.ctx.subscriptions.register(&sub_req(filter)) {
            Err(e) => e,
            Ok(_) => panic!("expected NotYetImplemented"),
        };
        assert!(matches!(err, OpError::NotYetImplemented(_)), "got {err:?}");
    })
}

// ---------------------------------------------------------------------------
// Event publication (4).
// ---------------------------------------------------------------------------

#[test]
fn publish_encode_emits_event_with_increasing_lsn() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let mut rx = fix.bus.receiver();
        let _id1 = do_encode(
            &fix.ctx,
            encode_req([1; 16], "alpha", 42, MemoryKindWire::Episodic),
        )
        .await;
        let _id2 = do_encode(
            &fix.ctx,
            encode_req([2; 16], "beta", 42, MemoryKindWire::Episodic),
        )
        .await;

        let e1 = try_recv(&mut rx, Duration::from_millis(500))
            .await
            .expect("first event");
        let e2 = try_recv(&mut rx, Duration::from_millis(500))
            .await
            .expect("second event");
        assert_eq!(e1.event_type, EventType::Encoded);
        assert_eq!(e2.event_type, EventType::Encoded);
        assert!(
            e2.lsn > e1.lsn,
            "lsn must strictly increase: {} then {}",
            e1.lsn,
            e2.lsn
        );
        assert_eq!(e1.context_id, ContextId(42));
        assert_eq!(e1.kind, MemoryKind::Episodic);
        assert_eq!(e1.text.as_deref(), Some("alpha"));
    })
}

#[test]
fn publish_txn_commit_emits_all_buffered_events_in_order() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let mut rx = fix.bus.receiver();
        let txn_id = [9; 16];

        // BEGIN.
        dispatch(
            RequestBody::TxnBegin(TxnBeginRequest {
                txn_id,
                timeout_seconds: 60,
            }),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap();

        // Two encodes inside the txn — preview returns, no events yet.
        let id1 = match single_body(
            dispatch(
                RequestBody::Encode(EncodeRequest {
                    text: "one".into(),
                    context_id: 100,
                    request_id: [0xA; 16],
                    txn_id: Some(txn_id),
                    occurred_at_unix_nanos: None,
                }),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        ) {
            ResponseBody::Encode(r) => r.memory_id,
            other => panic!("got {other:?}"),
        };
        let id2 = match single_body(
            dispatch(
                RequestBody::Encode(EncodeRequest {
                    text: "two".into(),
                    context_id: 100,
                    request_id: [0xB; 16],
                    txn_id: Some(txn_id),
                    occurred_at_unix_nanos: None,
                }),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        ) {
            ResponseBody::Encode(r) => r.memory_id,
            other => panic!("got {other:?}"),
        };

        // No events before commit.
        assert!(
            try_recv(&mut rx, Duration::from_millis(50)).await.is_none(),
            "encodes-in-txn must not publish before commit"
        );

        // COMMIT.
        dispatch(
            RequestBody::TxnCommit(TxnCommitRequest { txn_id }),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap();

        let e1 = try_recv(&mut rx, Duration::from_millis(500))
            .await
            .expect("first commit event");
        let e2 = try_recv(&mut rx, Duration::from_millis(500))
            .await
            .expect("second commit event");
        assert_eq!(e1.event_type, EventType::Encoded);
        assert_eq!(e2.event_type, EventType::Encoded);
        assert_eq!(e1.memory_id, MemoryId::from(id1));
        assert_eq!(e2.memory_id, MemoryId::from(id2));
        assert!(e2.lsn > e1.lsn);
    })
}

// ---------------------------------------------------------------------------
// Filter matching (4).
// ---------------------------------------------------------------------------

#[test]
fn filter_context_drops_off_context_events() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let mut filter = empty_filter();
        filter.contexts = Some(vec![42]);
        let handle = fix.ctx.subscriptions.register(&sub_req(filter)).unwrap();
        let mut rx = handle.receiver;

        do_encode(
            &fix.ctx,
            encode_req([1; 16], "off", 7, MemoryKindWire::Episodic),
        )
        .await;
        do_encode(
            &fix.ctx,
            encode_req([2; 16], "on", 42, MemoryKindWire::Episodic),
        )
        .await;

        let mut matched = 0;
        for _ in 0..2 {
            if let Some(env) = try_recv(&mut rx, Duration::from_millis(200)).await {
                if handle.filter.matches(&env) {
                    matched += 1;
                    assert_eq!(env.context_id, ContextId(42));
                }
            }
        }
        assert_eq!(matched, 1);
    })
}

// The subscription kind-filter tests (`filter_kind_drops_off_kind_events`,
// `filter_context_and_kind_combine_as_and`) relied on the client
// encoding a Semantic-kind memory so a Semantic event would flow onto
// the bus. The write router now files every encode as Episodic, so
// those scenarios can no longer be set up through ENCODE and have been
// removed. The filter-matching logic itself (kind/context AND-combine)
// is covered by `SubscriptionFilter::matches` unit tests.

#[test]
fn filter_null_passes_every_event() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let handle = fix
            .ctx
            .subscriptions
            .register(&sub_req(empty_filter()))
            .unwrap();
        let mut rx = handle.receiver;

        do_encode(
            &fix.ctx,
            encode_req([1; 16], "a", 1, MemoryKindWire::Episodic),
        )
        .await;
        do_encode(
            &fix.ctx,
            encode_req([2; 16], "b", 99, MemoryKindWire::Semantic),
        )
        .await;

        let mut matched = 0;
        for _ in 0..2 {
            if let Some(env) = try_recv(&mut rx, Duration::from_millis(200)).await {
                if handle.filter.matches(&env) {
                    matched += 1;
                }
            }
        }
        assert_eq!(matched, 2);
    })
}

// ---------------------------------------------------------------------------
// One-shot dispatcher (3).
// ---------------------------------------------------------------------------

#[test]
fn dispatcher_times_out_when_no_event_matches() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let err = dispatch(
            RequestBody::Subscribe(sub_req(empty_filter())),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::Overloaded);
        assert!(err.retryable());
    })
}

#[test]
fn dispatcher_with_from_lsn_returns_lsn_too_old() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let mut req = sub_req(empty_filter());
        req.from_lsn = Some(1);
        let err = dispatch(
            RequestBody::Subscribe(req),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::NotFound);
    })
}

// ---------------------------------------------------------------------------
// Backpressure (1).
// ---------------------------------------------------------------------------

#[test]
fn lagged_subscriber_freezes_final_lsn_and_reports_overloaded() {
    run_in_glommio(|| async {
        // Small capacity so a few publishes overflow.
        let fix = build_fixture_with_capacity(4);

        // Build a registry handle but DON'T pump the receiver.
        let handle = fix
            .ctx
            .subscriptions
            .register(&sub_req(empty_filter()))
            .unwrap();
        let stream_id = handle.target_stream_id;
        let started = handle.started_at_lsn;
        let mut rx = handle.receiver;

        // Overflow the bus.
        for _ in 0..50 {
            fix.bus.publish(EventEnvelope {
                lsn: 0,
                event_type: EventType::Encoded,
                memory_id: MemoryId::from(1u128),
                context_id: ContextId(1),
                kind: MemoryKind::Episodic,
                salience: 0.5,
                timestamp_unix_nanos: 0,
                text: None,
                graph_payload: None,
                edge_payload: None,
                stage_kind: None,
                stage_outcome: None,
                stage_payload: None,
                agent_id: brain_core::AgentId::default(),
            });
        }

        // First recv should report Lagged.
        let recv = rx.try_recv();
        assert!(
            matches!(recv, Err(broadcast::error::TryRecvError::Lagged(_))),
            "expected Lagged, got {recv:?}"
        );

        // final_lsn must not have advanced.
        assert_eq!(fix.ctx.subscriptions.final_lsn(stream_id), Some(started));
    })
}

// ---------------------------------------------------------------------------
// Cross-handler ordering (1).
// ---------------------------------------------------------------------------

#[test]
fn encode_then_forget_preserve_lsn_order() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let mut rx = fix.bus.receiver();
        let mid = do_encode(
            &fix.ctx,
            encode_req([1; 16], "x", 1, MemoryKindWire::Episodic),
        )
        .await;
        do_forget(&fix.ctx, mid, [2; 16]).await;

        let e1 = try_recv(&mut rx, Duration::from_millis(500))
            .await
            .expect("encode event");
        let e2 = try_recv(&mut rx, Duration::from_millis(500))
            .await
            .expect("forget event");
        assert_eq!(e1.event_type, EventType::Encoded);
        assert_eq!(e2.event_type, EventType::Forgotten);
        assert!(
            e2.lsn > e1.lsn,
            "forget LSN {} must follow encode LSN {}",
            e2.lsn,
            e1.lsn
        );
    })
}

// ---------------------------------------------------------------------------
// from_wal_record — unified-edge change feed.
// ---------------------------------------------------------------------------

mod wal_record_projection {
    use super::*;
    use brain_core::{
        AgentId, EdgeKind, EdgeKindRef, EdgeOrigin, EntityId, NodeRef, RelationId, RelationTypeId,
        RequestId,
    };
    use brain_storage::wal::payload::{
        EdgePayload, EncodePayload, ForgetMode, ForgetPayload, ForgetReason, LinkPayload,
        RelationLinkPayload, RelationSupersedePayload, RelationTombstonePayload, UnlinkPayload,
        WalPayload,
    };
    use brain_storage::wal::record::{Lsn, WalRecord};

    fn mid(slot: u64) -> MemoryId {
        MemoryId::pack(1, slot, 1)
    }

    fn entid(byte: u8) -> EntityId {
        let mut b = [0u8; 16];
        b[15] = byte;
        EntityId::from(b)
    }

    fn relid(byte: u8) -> RelationId {
        let mut b = [0u8; 16];
        b[0] = 0xA0;
        b[15] = byte;
        RelationId::from(b)
    }

    fn rec(p: WalPayload) -> WalRecord {
        WalRecord::from_typed(Lsn(7), 0, 1_700_000_000_000_000_000, 0xCAFE, &p)
    }

    #[test]
    fn link_payload_projects_to_edge_added_event() {
        let r = rec(WalPayload::Link(LinkPayload {
            source: NodeRef::Memory(mid(1)),
            target: NodeRef::Memory(mid(2)),
            edge_kind: EdgeKindRef::Builtin(EdgeKind::Caused),
            weight: 0.7,
            origin: EdgeOrigin::Explicit,
        }));
        let envs = EventEnvelope::from_wal_record(&r);
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].event_type, EventType::EdgeAdded);
        let ep = envs[0].edge_payload.as_ref().expect("edge_payload");
        assert_eq!(ep.edge_kind_tag, 0, "Builtin tag");
        assert_eq!(ep.edge_kind_byte, EdgeKind::Caused as u8);
        assert!((ep.weight - 0.7).abs() < 1e-6);
        assert!(ep.relation_id.is_none());
        assert!(ep.relation_type_id.is_none());
        assert!(envs[0].graph_payload.is_none());
    }

    #[test]
    fn unlink_payload_projects_to_edge_removed_event() {
        let r = rec(WalPayload::Unlink(UnlinkPayload {
            source: NodeRef::Memory(mid(1)),
            target: NodeRef::Memory(mid(2)),
            edge_kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            edge_seq: 0,
        }));
        let envs = EventEnvelope::from_wal_record(&r);
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].event_type, EventType::EdgeRemoved);
        let ep = envs[0].edge_payload.as_ref().unwrap();
        assert_eq!(ep.edge_kind_tag, 0);
        assert_eq!(ep.edge_kind_byte, EdgeKind::SimilarTo as u8);
    }

    #[test]
    fn relation_link_projects_to_edge_added_with_relation_id() {
        let p = RelationLinkPayload {
            relation_id: relid(5),
            from: NodeRef::Entity(entid(2)),
            to: NodeRef::Entity(entid(3)),
            relation_type_id: RelationTypeId::from(42),
            chain_root: relid(5),
            confidence: 0.9,
            valid_from_unix_nanos: None,
            valid_to_unix_nanos: None,
            supersedes: None,
            evidence: vec![],
            extractor_id: 1,
            is_symmetric: false,
            properties_blob: vec![],
            agent_id: AgentId::default(),
            relation_type_intern_hint: None,
        };
        let r = rec(WalPayload::RelationLink(p));
        let envs = EventEnvelope::from_wal_record(&r);
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].event_type, EventType::EdgeAdded);
        let ep = envs[0].edge_payload.as_ref().unwrap();
        assert_eq!(ep.edge_kind_tag, 2, "Typed tag");
        assert_eq!(ep.relation_type_id, Some(42));
        assert_eq!(ep.relation_id, Some(relid(5).to_bytes()));
        assert!(ep.superseded_relation_id.is_none());
    }

    #[test]
    fn relation_supersede_projects_to_edge_superseded() {
        let new = RelationLinkPayload {
            relation_id: relid(6),
            from: NodeRef::Entity(entid(2)),
            to: NodeRef::Entity(entid(4)),
            relation_type_id: RelationTypeId::from(42),
            chain_root: relid(5),
            confidence: 0.9,
            valid_from_unix_nanos: None,
            valid_to_unix_nanos: None,
            supersedes: Some(relid(5)),
            evidence: vec![],
            extractor_id: 1,
            is_symmetric: false,
            properties_blob: vec![],
            agent_id: AgentId::default(),
            relation_type_intern_hint: None,
        };
        let r = rec(WalPayload::RelationSupersede(RelationSupersedePayload {
            old_relation_id: relid(5),
            new,
        }));
        let envs = EventEnvelope::from_wal_record(&r);
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].event_type, EventType::EdgeSuperseded);
        let ep = envs[0].edge_payload.as_ref().unwrap();
        assert_eq!(ep.relation_id, Some(relid(6).to_bytes()));
        assert_eq!(ep.superseded_relation_id, Some(relid(5).to_bytes()));
    }

    #[test]
    fn relation_tombstone_projects_to_edge_removed_with_relation_id() {
        let r = rec(WalPayload::RelationTombstone(RelationTombstonePayload {
            relation_id: relid(7),
            reason: "test".into(),
            at_unix_nanos: 1,
            agent_id: AgentId::default(),
        }));
        let envs = EventEnvelope::from_wal_record(&r);
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].event_type, EventType::EdgeRemoved);
        let ep = envs[0].edge_payload.as_ref().unwrap();
        assert_eq!(ep.relation_id, Some(relid(7).to_bytes()));
    }

    #[test]
    fn encode_with_edges_emits_encoded_plus_one_edge_added_per_edge() {
        let p = EncodePayload {
            memory_id: mid(1),
            request_id: RequestId::default(),
            agent_id: AgentId::default(),
            context_id: ContextId(0),
            kind: MemoryKind::Episodic,
            salience_initial: 0.5,
            embedding_model_fp: [0xAB; 16],
            text: "hello".into(),
            vector: vec![0.0; VECTOR_DIM],
            edges: vec![
                EdgePayload {
                    source: NodeRef::Memory(mid(1)),
                    target: NodeRef::Memory(mid(2)),
                    kind: EdgeKindRef::Builtin(EdgeKind::Caused),
                    weight: 0.5,
                    origin: EdgeOrigin::Explicit,
                },
                EdgePayload {
                    source: NodeRef::Memory(mid(1)),
                    target: NodeRef::Memory(mid(3)),
                    kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
                    weight: 0.8,
                    origin: EdgeOrigin::AutoDerived,
                },
            ],
            request_hash: [0; 32],
            response_payload: vec![],
            deduplicate: false,
            occurred_at_unix_nanos: None,
        };
        let r = rec(WalPayload::Encode(p));
        let envs = EventEnvelope::from_wal_record(&r);
        assert_eq!(envs.len(), 3, "1 Encoded + 2 EdgeAdded");
        assert_eq!(envs[0].event_type, EventType::Encoded);
        assert!(envs[0].edge_payload.is_none());
        assert_eq!(envs[1].event_type, EventType::EdgeAdded);
        assert_eq!(envs[2].event_type, EventType::EdgeAdded);
        // All three envelopes share the LSN — replay frames them
        // separately and the per-shard tail subscription stays in
        // monotonic order.
        assert_eq!(envs[0].lsn, envs[1].lsn);
        assert_eq!(envs[1].lsn, envs[2].lsn);
    }

    #[test]
    fn forget_still_projects_to_single_forgotten_envelope() {
        let r = rec(WalPayload::Forget(ForgetPayload {
            memory_id: mid(1),
            request_id: RequestId::default(),
            agent_id: AgentId::default(),
            mode: ForgetMode::Soft,
            reason: ForgetReason::ClientRequest,
        }));
        let envs = EventEnvelope::from_wal_record(&r);
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].event_type, EventType::Forgotten);
        assert!(envs[0].edge_payload.is_none());
    }

    // ----- typed-graph change-feed event records (publish_graph) --------
    //
    // Entity/statement/schema events ride a separate flagged WAL record
    // whose body is `agent_id (16 B) || CBOR(GraphEventPayload)` — the same
    // opaque-body envelope the durable records use. These tests pin that
    // framing: from_wal_record must strip the agent prefix and decode the
    // CBOR back to the event, and must do so ONLY for flagged records.

    fn entity_created_event_record(flags: u8) -> WalRecord {
        use brain_protocol::{EntityCreatedEvent, GraphEventPayload};
        let ev = GraphEventPayload::EntityCreated(EntityCreatedEvent {
            entity_id: entid(9).to_bytes(),
            entity_type_id: 1,
            canonical_name: "Priya Patel".into(),
        });
        // Mirror publish_graph: agent_id (16 B) prefix, then CBOR.
        let mut body = Vec::with_capacity(16);
        body.extend_from_slice(&[0xAB; 16]);
        ciborium::into_writer(&ev, &mut body).unwrap();
        WalRecord {
            lsn: Lsn(7),
            kind: brain_storage::wal::kinds::WalRecordKind::EntityCreate,
            flags,
            timestamp_ns: 1_700_000_000_000_000_000,
            agent_id_lo64: 0,
            payload: body,
        }
    }

    #[test]
    fn flagged_entity_event_record_projects_to_entity_created() {
        let r = entity_created_event_record(brain_storage::wal::record::FLAG_SUBSCRIBE_EVENT);
        let envs = EventEnvelope::from_wal_record(&r);
        assert_eq!(envs.len(), 1, "one EntityCreated event");
        assert_eq!(envs[0].event_type, EventType::EntityCreated);
        match envs[0].graph_payload.as_ref().expect("graph_payload") {
            brain_protocol::GraphEventPayload::EntityCreated(e) => {
                assert_eq!(e.canonical_name, "Priya Patel");
                assert_eq!(e.entity_type_id, 1);
            }
            other => panic!("expected EntityCreated, got {other:?}"),
        }
    }

    #[test]
    fn unflagged_phasebody_record_is_not_projected() {
        // Without the subscribe-event flag a record is a durable write,
        // reconstructed by recovery — not surfaced as a change-feed event.
        let r = entity_created_event_record(0);
        assert!(
            EventEnvelope::from_wal_record(&r).is_empty(),
            "unflagged opaque-body records must not project to events"
        );
    }
}
