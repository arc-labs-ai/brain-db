//! Integration tests for `handle_memory_aggregate` (MEMORY_AGGREGATE),
//! driven through the real `dispatch` path. The counters must agree with
//! what was written and forgotten, stay inside the caller's scope, and
//! match a full recompute after every operation.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::test_support::{run_in_glommio, single_body};
use brain_ops::{dispatch, OpsContext, RealWriterHandle, RequestCaller};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{EncodeRequest, ForgetRequest, RequestBody};
use brain_protocol::envelope::response::{EncodeResponse, ResponseBody};
use brain_protocol::{
    AggregateSourceWire, ForgetMode, MemoryAggregateRequest, MemoryAggregateResponse,
    MemoryKindWire, MemoryStatusWire,
};

// ---------------------------------------------------------------------------
// Mock dispatcher (text-driven deterministic vectors).
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
    metadata: SharedMetadataDb,
}

impl Fixture {
    fn intern_namespace(&self, name: &str) -> brain_core::NamespaceId {
        let wtxn = self.metadata.write_txn().expect("write txn");
        let id = brain_metadata::namespace::namespace_intern_or_get(&wtxn, name, 0)
            .expect("intern namespace");
        wtxn.commit().expect("commit");
        id
    }
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());

    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));

    let embedder: Arc<dyn Dispatcher> = Arc::new(MockDispatcher);
    let executor = ExecutorContext::new(
        embedder,
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );

    Fixture {
        ctx: brain_ops::test_support::ops_context_for_tests(executor, tempdir.path()),
        _tempdir: tempdir,
        metadata,
    }
}

/// Two spaces inside one namespace: aggregates must never cross the wall.
const SPACE_A: [u8; 16] = [0xA1; 16];
const SPACE_B: [u8; 16] = [0xB2; 16];

fn caller_for(namespace: &str, space: [u8; 16]) -> RequestCaller {
    let space = brain_core::SpaceId(uuid::Uuid::from_bytes(space));
    RequestCaller::from_scope(
        space,
        [0u8; 16],
        [0u8; 16],
        namespace.to_string(),
        brain_metadata::api_keys::bits::FULL,
    )
}

fn encode_req(request_id: [u8; 16], text: &str) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        session_id: 0,
        request_id,
        txn_id: None,
        occurred_at_unix_nanos: None,
        act_as: None,
        wait: brain_protocol::WaitMode::Ack,
        allow_duplicates: false,
    }
}

async fn encode(fix: &Fixture, caller: RequestCaller, request_id: [u8; 16], text: &str) -> u128 {
    let outcome = dispatch(
        RequestBody::Encode(encode_req(request_id, text)),
        caller,
        &fix.ctx,
    )
    .await
    .expect("encode dispatch");
    match single_body(outcome) {
        ResponseBody::Encode(EncodeResponse { memory_id, .. }) => memory_id,
        other => panic!("expected Encode response, got {other:?}"),
    }
}

async fn forget(fix: &Fixture, caller: RequestCaller, request_id: [u8; 16], memory_id: u128) {
    let outcome = dispatch(
        RequestBody::Forget(ForgetRequest {
            memory_id,
            mode: ForgetMode::Soft,
            request_id,
            txn_id: None,
            act_as: None,
        }),
        caller,
        &fix.ctx,
    )
    .await
    .expect("forget dispatch");
    match single_body(outcome) {
        ResponseBody::Forget(_) => {}
        other => panic!("expected Forget response, got {other:?}"),
    }
}

async fn aggregate(
    fix: &Fixture,
    caller: RequestCaller,
    req: MemoryAggregateRequest,
) -> MemoryAggregateResponse {
    let outcome = dispatch(RequestBody::MemoryAggregate(req), caller, &fix.ctx)
        .await
        .expect("memory_aggregate dispatch");
    match single_body(outcome) {
        ResponseBody::MemoryAggregate(r) => r,
        other => panic!("expected MemoryAggregate response, got {other:?}"),
    }
}

fn total_req() -> MemoryAggregateRequest {
    MemoryAggregateRequest {
        kinds: Vec::new(),
        statuses: Vec::new(),
        group_by_kind: false,
        group_by_status: false,
        group_by_day: false,
        salience_min: None,
        salience_max: None,
        created_from_unix_nanos: None,
        created_to_unix_nanos: None,
        include_min_max: false,
        act_as: None,
    }
}

fn assert_counters_consistent(fix: &Fixture) {
    let rtxn = fix.metadata.read_txn().unwrap();
    let drift =
        brain_metadata::tables::agg::verify(&rtxn, brain_metadata::salience::now_unix_nanos())
            .unwrap();
    assert!(
        drift.is_clean(),
        "counters drifted from memories: {drift:?}"
    );
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// Encode, forget, then aggregate: active totals drop the forgotten memory,
/// grouping by status shows it as tombstoned, and the other space sees
/// none of it.
#[test]
fn aggregates_follow_encode_and_forget_within_scope() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let _ns = fix.intern_namespace("acme");
        let a = || caller_for("acme", SPACE_A);
        let b = || caller_for("acme", SPACE_B);

        // Empty scope: one zero-count total.
        let empty = aggregate(&fix, a(), total_req()).await;
        assert_eq!(empty.groups.len(), 1);
        assert_eq!(empty.groups[0].count, 0);
        assert_eq!(empty.groups[0].salience_avg, None);

        let mut ids = Vec::new();
        for i in 0..5u8 {
            ids.push(encode(&fix, a(), [i + 1; 16], &format!("space a memory {i}")).await);
        }
        encode(&fix, b(), [0x50; 16], "space b memory").await;
        assert_counters_consistent(&fix);

        let total = aggregate(&fix, a(), total_req()).await;
        assert_eq!(total.groups[0].count, 5);
        let avg = total.groups[0].salience_avg.expect("non-empty avg");
        assert!(
            (0.0..=1.0).contains(&avg),
            "avg salience {avg} out of range"
        );
        assert!(
            (total.groups[0].salience_sum - avg * 5.0).abs() < 1e-6,
            "sum and avg disagree"
        );

        forget(&fix, a(), [0x90; 16], ids[0]).await;
        assert_counters_consistent(&fix);

        let after = aggregate(&fix, a(), total_req()).await;
        assert_eq!(
            after.groups[0].count, 4,
            "forgotten memory leaves the active total"
        );

        let by_status = aggregate(
            &fix,
            a(),
            MemoryAggregateRequest {
                statuses: vec![MemoryStatusWire::Active, MemoryStatusWire::Tombstoned],
                group_by_status: true,
                ..total_req()
            },
        )
        .await;
        let count_of = |s: MemoryStatusWire| {
            by_status
                .groups
                .iter()
                .find(|g| g.status == Some(s))
                .map_or(0, |g| g.count)
        };
        assert_eq!(count_of(MemoryStatusWire::Active), 4);
        assert_eq!(count_of(MemoryStatusWire::Tombstoned), 1);

        // Scope wall: space B sees only its own memory.
        let other = aggregate(&fix, b(), total_req()).await;
        assert_eq!(other.groups[0].count, 1);
    });
}

/// Grouping by kind returns one group per kind present, and a kind filter
/// that matches nothing returns an empty grouped list.
#[test]
fn groups_and_filters_by_kind() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let _ns = fix.intern_namespace("acme");
        let a = || caller_for("acme", SPACE_A);
        for i in 0..3u8 {
            encode(&fix, a(), [i + 1; 16], &format!("kind test {i}")).await;
        }

        let grouped = aggregate(
            &fix,
            a(),
            MemoryAggregateRequest {
                group_by_kind: true,
                ..total_req()
            },
        )
        .await;
        let summed: u64 = grouped.groups.iter().map(|g| g.count).sum();
        assert_eq!(summed, 3);
        assert!(grouped
            .groups
            .iter()
            .all(|g| g.kind.is_some() && g.status.is_none()));

        let none = aggregate(
            &fix,
            a(),
            MemoryAggregateRequest {
                kinds: vec![MemoryKindWire::Consolidated],
                group_by_kind: true,
                ..total_req()
            },
        )
        .await;
        assert!(
            none.groups.is_empty(),
            "no consolidated memories were written"
        );
    });
}

/// Requests the counters can't answer go to the snapshot engine: refused
/// with `CapabilityNotEnabled` when analytics is off, and when on, answered
/// from the snapshot with MIN/MAX, a salience filter and per-day groups
/// that agree with the live counters on the totals.
#[test]
fn scan_requests_route_to_the_snapshot() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();
        let _ns = fix.intern_namespace("acme");
        let a = || caller_for("acme", SPACE_A);
        for i in 0..6u8 {
            encode(&fix, a(), [i + 1; 16], &format!("snapshot memory {i}")).await;
        }
        let scan_req = MemoryAggregateRequest {
            include_min_max: true,
            group_by_day: true,
            salience_min: Some(0.0),
            ..total_req()
        };

        let refused = dispatch(
            RequestBody::MemoryAggregate(scan_req.clone()),
            a(),
            &fix.ctx,
        )
        .await;
        assert!(
            matches!(
                refused,
                Err(brain_ops::OpError::CapabilityNotEnabled { .. })
            ),
            "scan without analytics must be refused, got {refused:?}"
        );

        let analytics = brain_analytics::Analytics::start(
            fix.metadata.clone(),
            fix._tempdir.path().join("analytics"),
            brain_analytics::AnalyticsConfig::default(),
        )
        .unwrap();
        let snap = analytics.export_now().unwrap();
        fix.ctx = fix.ctx.with_analytics(Some(std::sync::Arc::new(analytics)));

        let scanned = aggregate(&fix, a(), scan_req).await;
        assert_eq!(scanned.source, AggregateSourceWire::Snapshot);
        assert!(scanned.as_of_unix_nanos >= snap.taken_at_unix_nanos);
        let scanned_count: u64 = scanned.groups.iter().map(|g| g.count).sum();
        assert!(scanned
            .groups
            .iter()
            .all(|g| g.day_start_unix_nanos.is_some()));
        assert!(scanned
            .groups
            .iter()
            .all(|g| g.salience_min.is_some() && g.salience_min <= g.salience_max));

        let live = aggregate(&fix, a(), total_req()).await;
        assert_eq!(live.source, AggregateSourceWire::Counters);
        assert_eq!(
            scanned_count, live.groups[0].count,
            "snapshot and counters agree"
        );
        let scanned_sum: f64 = scanned.groups.iter().map(|g| g.salience_sum).sum();
        // Both evaluate decay at request time (a moment apart) in f64.
        assert!(
            (scanned_sum - live.groups[0].salience_sum).abs() < 1e-6 * scanned_count.max(1) as f64,
            "snapshot and counters agree on salience: {scanned_sum} vs {}",
            live.groups[0].salience_sum
        );
    });
}
