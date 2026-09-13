//! Delegated transactions: a txn begun with `act_as` commits every buffered
//! write under the delegated `(namespace, space)`, not the identity of the
//! connection that happens to issue the commit.
//!
//! This is the transaction analogue of the direct delegated-write path
//! (ENCODE with `act_as`). The delegation is fixed at `TXN_BEGIN`; `TXN_COMMIT`
//! carries no `act_as` of its own and arrives under the connection principal's
//! own identity, yet the buffered writes must still land under the delegated
//! identity. Without the fix a delegated txn would commit its rows under the
//! committing connection's identity — a tenancy breach for the shared-pool
//! gateway model, where one service principal opens txns on behalf of many
//! tenants.
//!
//! The test drives the real `dispatch` → handler path with two distinct
//! identities on ONE connection:
//!   - BEGIN runs as the delegated identity ("acme") — modeling the effective
//!     caller the server materializes from an authorized `act_as`.
//!   - ENCODE (buffered) and COMMIT run as the connection's own identity
//!     ("gateway"), proving the committed row's placement is governed by the
//!     begin-time delegation and nothing else.
//!
//! It then asserts, straight from redb, that the committed row is stamped with
//! the delegated namespace + space (not the gateway's), and that a same-tenant
//! `MEMORY_LIST` as the delegated identity finds it while one as the gateway
//! identity does not.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::test_support::{run_in_glommio, single_body};
use brain_ops::{dispatch, DispatchOutcome, OpsContext, RealWriterHandle, RequestCaller};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{EncodeRequest, RequestBody};
use brain_protocol::envelope::response::{
    EncodeResponse, ResponseBody, TxnBeginResponse, TxnCommitResponse,
};
use brain_protocol::{
    ActAs, MemoryListDirWire, MemoryListRequest, MemoryListSortWire, MemoryListTimeAxisWire,
    TxnBeginRequest, TxnCommitRequest,
};

// ---------------------------------------------------------------------------
// Mock dispatcher (text-driven deterministic vectors; same as the sibling
// tenancy proofs).
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

struct Fixture {
    ctx: OpsContext,
    metadata: SharedMetadataDb,
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
    let ctx = brain_ops::test_support::ops_context_for_tests(executor, tempdir.path());
    std::mem::forget(tempdir);
    Fixture { ctx, metadata }
}

// The one physical connection every op in the delegated txn rides.
const CONN: [u8; 16] = [0x5A; 16];
// The delegated identity ("acme") the txn is begun as.
const ACME_SPACE: [u8; 16] = [0xC1; 16];
// The connection principal's own identity ("gateway"), under which COMMIT and
// the buffered ENCODE actually run.
const GATEWAY_SPACE: [u8; 16] = [0x67; 16];

/// A caller bound to `(namespace, space)` on connection [`CONN`]. Modeling the
/// server's effective caller: BEGIN uses `caller("acme", ACME_SPACE)`, the
/// delegated identity; COMMIT/ENCODE use `caller("gateway", GATEWAY_SPACE)`,
/// the connection's own identity.
fn caller(namespace: &str, space_bytes: [u8; 16]) -> RequestCaller {
    let space = brain_core::SpaceId(uuid::Uuid::from_bytes(space_bytes));
    RequestCaller::from_scope(
        space,
        [0u8; 16],
        [0u8; 16],
        namespace.to_string(),
        brain_metadata::api_keys::bits::STANDARD_SPACE,
    )
    .with_session_id(CONN)
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

#[test]
fn txn_begun_with_act_as_commits_under_delegated_identity() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let txn_id = [0x7u8; 16];

        // Intern both namespaces up front so we can compare the stored row's
        // id and prove it carries the delegated ("acme") one, not "gateway".
        let (acme_ns, gateway_ns) = {
            let wtxn = fix.metadata.write_txn().unwrap();
            let acme =
                brain_metadata::namespace::namespace_intern_or_get(&wtxn, "acme", 0).unwrap();
            let gateway =
                brain_metadata::namespace::namespace_intern_or_get(&wtxn, "gateway", 0).unwrap();
            wtxn.commit().unwrap();
            (acme, gateway)
        };
        assert_ne!(acme_ns, gateway_ns, "the two tenants must be distinct");

        // BEGIN as the delegated identity, carrying an `act_as`. In the live
        // server R1/R2 authorization runs before the handler and the effective
        // caller is already materialized; here the delegated caller stands in
        // for that materialized identity, and `act_as: Some(..)` is what tells
        // the handler to freeze it on the txn entry.
        let _ = unwrap_begin(
            dispatch(
                RequestBody::TxnBegin(TxnBeginRequest {
                    txn_id,
                    timeout_seconds: 60,
                    act_as: Some(ActAs {
                        namespace: "acme".into(),
                        space_id: "acme:main".into(),
                    }),
                }),
                caller("acme", ACME_SPACE),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );

        // ENCODE inside the txn AS THE CONNECTION'S OWN identity (gateway),
        // with no `act_as` of its own. The buffered write's final placement
        // must come from the begin-time delegation, not this caller.
        let memory_id = match single_body(
            dispatch(
                RequestBody::Encode(EncodeRequest {
                    text: "the acme deployment shipped on friday".into(),
                    session_id: 1,
                    request_id: [0x11; 16],
                    txn_id: Some(txn_id),
                    occurred_at_unix_nanos: None,
                    act_as: None,
                    wait: brain_protocol::WaitMode::Ack,
                    allow_duplicates: false,
                }),
                caller("gateway", GATEWAY_SPACE),
                &fix.ctx,
            )
            .await
            .unwrap(),
        ) {
            ResponseBody::Encode(EncodeResponse { memory_id, .. }) => memory_id,
            other => panic!("expected Encode, got {other:?}"),
        };

        // COMMIT AS THE CONNECTION'S OWN identity (gateway) — TXN_COMMIT never
        // carries an `act_as`. The write must nonetheless land under "acme".
        let _ = unwrap_commit(
            dispatch(
                RequestBody::TxnCommit(TxnCommitRequest { txn_id }),
                caller("gateway", GATEWAY_SPACE),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );

        // (1) The stored row is stamped with the DELEGATED namespace + space.
        let id_bytes = memory_id.to_be_bytes();
        {
            let rtxn = fix.metadata.read_txn().unwrap();
            let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
            let row: MemoryMetadata = t
                .get(&id_bytes)
                .unwrap()
                .expect("committed memory row present")
                .value();
            assert_eq!(
                row.namespace_id,
                acme_ns.raw(),
                "delegated txn must commit under the act_as namespace (acme), not the committing connection's (gateway)"
            );
            assert_ne!(
                row.namespace_id,
                gateway_ns.raw(),
                "delegated txn must NOT commit under the committing connection's namespace"
            );
            assert_eq!(
                row.space_id_bytes, ACME_SPACE,
                "delegated txn must commit into the act_as space, not the connection's own"
            );
        }

        // (2) A MEMORY_LIST as the delegated identity finds it; one as the
        //     gateway identity does not — end-to-end tenant scoping.
        let list_as = |c: RequestCaller| async {
            match single_body(
                dispatch(
                    RequestBody::MemoryList(MemoryListRequest {
                        sort: MemoryListSortWire::Created,
                        dir: MemoryListDirWire::Desc,
                        limit: 50,
                        cursor: Vec::new(),
                        kinds: Vec::new(),
                        include_tombstoned: false,
                        time_axis: MemoryListTimeAxisWire::Created,
                        from_unix_nanos: 0,
                        to_unix_nanos: 0,
                        salience_min: 0.0,
                        salience_max: 1.0,
                        text_contains: String::new(),
                        act_as: None,
                    }),
                    c,
                    &fix.ctx,
                )
                .await
                .unwrap(),
            ) {
                ResponseBody::MemoryList(frame) => frame
                    .items
                    .iter()
                    .map(|it| it.memory_id)
                    .collect::<Vec<_>>(),
                other => panic!("expected MemoryList, got {other:?}"),
            }
        };

        let acme_ids = list_as(caller("acme", ACME_SPACE)).await;
        assert!(
            acme_ids.contains(&id_bytes),
            "delegated identity's MEMORY_LIST must find the txn-committed memory"
        );

        let gateway_ids = list_as(caller("gateway", GATEWAY_SPACE)).await;
        assert!(
            !gateway_ids.contains(&id_bytes),
            "the committing connection's own identity must NOT see the delegated write"
        );
    });
}
