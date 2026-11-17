//! Regression: a vector-direct write must be stamped with the CALLER's
//! namespace, not the reserved system namespace.
//!
//! `Write::from_phases` defaults the namespace to `NamespaceId::SYSTEM`, so
//! a handler that does not thread the authenticated caller's namespace onto
//! the `Write` produces rows whose `(namespace, space)` scope key cannot
//! match a same-tenant read. The vector-direct path did exactly that: the
//! response carried a memory id and a kind, and the memory was then absent
//! from `MEMORY_LIST` and unreadable by id — a write that reported success
//! and lost the data.
//!
//! The sibling proof for the transaction path is `txn_tenant_stamp.rs`. The
//! existing `encode_vector_direct.rs` cases all assert on the RESPONSE,
//! which is why they stayed green through the fault; this one reads the
//! stored row back.
//!
//! Vector-direct is also the only way to produce a Semantic memory, so the
//! Semantic kind was unreachable in practice for as long as this was broken.
//! Both kinds a client may write are therefore covered here.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::test_support::{run_in_glommio, single_body};
use brain_ops::{dispatch, DispatchOutcome, OpsContext, RealWriterHandle, RequestCaller};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{EncodeVectorDirectRequest, MemoryKindWire, RequestBody};
use brain_protocol::envelope::response::{EncodeResponse, ResponseBody};
use brain_protocol::{
    MemoryListDirWire, MemoryListRequest, MemoryListSortWire, MemoryListTimeAxisWire,
};

const MOCK_FP: [u8; 16] = [0xAB; 16];
const ACME_SPACE: [u8; 16] = [0xC1; 16];

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
        MOCK_FP
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

fn unit_vector(slot: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; VECTOR_DIM];
    v[slot % VECTOR_DIM] = 1.0;
    v
}

fn vector_direct_req(
    request_id: [u8; 16],
    slot: usize,
    kind: MemoryKindWire,
) -> EncodeVectorDirectRequest {
    EncodeVectorDirectRequest {
        text: format!("vector-direct slot {slot}"),
        vector: unit_vector(slot),
        model_fingerprint: MOCK_FP,
        session_id: 42,
        kind,
        salience_hint: 0.5,
        edges: vec![],
        request_id,
        txn_id: None,
        deduplicate: false,
        act_as: None,
    }
}

fn unwrap_resp(outcome: DispatchOutcome) -> EncodeResponse {
    match single_body(outcome) {
        ResponseBody::EncodeVectorDirect(r) => r,
        other => panic!("expected ResponseBody::EncodeVectorDirect, got {other:?}"),
    }
}

fn list_request() -> MemoryListRequest {
    MemoryListRequest {
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
    }
}

/// Both kinds a client may write through this path, because the fault was
/// in the write and not in the kind: Consolidated is refused here by design
/// and is produced only by the consolidation worker.
#[test]
fn vector_direct_memory_is_stamped_with_caller_namespace() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let c = || caller("acme", ACME_SPACE);

        // Resolve the caller's interned namespace id up front, so the stored
        // row is compared against a real id rather than merely "not SYSTEM".
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

        for (slot, kind) in [
            (1usize, MemoryKindWire::Episodic),
            (2usize, MemoryKindWire::Semantic),
        ] {
            let mut request_id = [0u8; 16];
            request_id[0] = u8::try_from(slot).unwrap();

            let resp = unwrap_resp(
                dispatch(
                    RequestBody::EncodeVectorDirect(vector_direct_req(request_id, slot, kind)),
                    c(),
                    &fix.ctx,
                )
                .await
                .unwrap(),
            );

            // (1) The stored row carries the caller namespace, not SYSTEM.
            let rtxn = fix.metadata.read_txn().unwrap();
            let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
            let row: MemoryMetadata = t
                .get(&resp.memory_id.to_be_bytes())
                .unwrap()
                .unwrap_or_else(|| panic!("{kind:?} vector-direct memory row must be present"))
                .value();
            assert_eq!(
                row.namespace_id,
                acme_ns.raw(),
                "{kind:?} vector-direct memory must carry the caller namespace"
            );
            assert_ne!(
                row.namespace_id,
                brain_core::NamespaceId::SYSTEM.raw(),
                "{kind:?} vector-direct memory must not land under the system namespace"
            );
            drop(rtxn);

            // (2) The caller can actually read it back. This is the half the
            //     response-only tests could not have caught: the id came back
            //     correctly while the memory was invisible to its own writer.
            let list = match single_body(
                dispatch(RequestBody::MemoryList(list_request()), c(), &fix.ctx)
                    .await
                    .unwrap(),
            ) {
                ResponseBody::MemoryList(frame) => frame,
                other => panic!("expected MemoryList, got {other:?}"),
            };
            assert!(
                list.items.iter().any(|it| it.memory_id == resp.memory_id),
                "a same-tenant MEMORY_LIST must find the {kind:?} vector-direct memory"
            );
        }
    });
}
