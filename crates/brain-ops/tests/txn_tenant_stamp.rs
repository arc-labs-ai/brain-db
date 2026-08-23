//! W2 regression: a write committed inside a transaction must be stamped
//! with the CALLER's namespace, not the reserved system namespace.
//!
//! `TXN_COMMIT` builds one multi-phase `Write` from the buffered ops and
//! submits it through the unified writer. That builder must thread the
//! authenticated caller's namespace onto the `Write` exactly like the
//! plain (non-txn) ENCODE path does; otherwise every row a transaction
//! produces lands under `NamespaceId::SYSTEM` and becomes invisible to a
//! same-tenant read (and cross-visible to others).
//!
//! This drives the real `dispatch` → handler path (TxnBegin → Encode with
//! a txn id → TxnCommit) as a namespaced caller, then proves both:
//!   1. the stored memory row's `namespace_id` equals the caller's
//!      namespace (not SYSTEM), read straight from redb; and
//!   2. a same-tenant `MEMORY_LIST` finds the memory.

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
    MemoryListDirWire, MemoryListRequest, MemoryListSortWire, MemoryListTimeAxisWire,
    TxnBeginRequest, TxnCommitRequest,
};

// ---------------------------------------------------------------------------
// Mock dispatcher (text-driven deterministic vectors; same as the sibling
// isolation proofs).
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

const ACME_SPACE: [u8; 16] = [0xC1; 16];

fn caller(namespace: &str, space_bytes: [u8; 16]) -> RequestCaller {
    let space = brain_core::SpaceId(uuid::Uuid::from_bytes(space_bytes));
    RequestCaller::from_scope(
        space,
        [0u8; 16],
        [0u8; 16],
        namespace.to_string(),
        brain_metadata::api_keys::bits::FULL,
    )
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
fn txn_committed_memory_is_stamped_with_caller_namespace() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let c = || caller("acme", ACME_SPACE);
        let txn_id = [0x7u8; 16];

        // Resolve the caller's interned namespace id up front so we can
        // compare the stored row against it.
        let acme_ns = {
            let wtxn = fix.metadata.write_txn().unwrap();
            let id = brain_metadata::namespace::namespace_intern_or_get(&wtxn, "acme", 0).unwrap();
            wtxn.commit().unwrap();
            id
        };
        assert_ne!(
            acme_ns,
            brain_core::NamespaceId::SYSTEM,
            "test namespace must not be the system namespace"
        );

        // TxnBegin.
        let _ = unwrap_begin(
            dispatch(
                RequestBody::TxnBegin(TxnBeginRequest {
                    txn_id,
                    timeout_seconds: 60,
                }),
                c(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );

        // Encode inside the transaction.
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
                c(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        ) {
            ResponseBody::Encode(EncodeResponse { memory_id, .. }) => memory_id,
            other => panic!("expected Encode, got {other:?}"),
        };

        // TxnCommit — this is the path W2 fixes.
        let _ = unwrap_commit(
            dispatch(
                RequestBody::TxnCommit(TxnCommitRequest { txn_id }),
                c(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );

        // (1) The stored row carries the caller namespace, not SYSTEM.
        let id_bytes = memory_id.to_be_bytes();
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
            "txn-committed memory must be stamped with the caller namespace, not SYSTEM"
        );
        assert_ne!(
            row.namespace_id,
            brain_core::NamespaceId::SYSTEM.raw(),
            "txn-committed memory must not land under the system namespace"
        );
        drop(rtxn);

        // (2) A same-tenant MEMORY_LIST finds it.
        let list = match single_body(
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
                c(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        ) {
            ResponseBody::MemoryList(frame) => frame,
            other => panic!("expected MemoryList, got {other:?}"),
        };
        assert!(
            list.items.iter().any(|it| it.memory_id == id_bytes),
            "same-tenant MEMORY_LIST must find the txn-committed memory"
        );
    });
}
