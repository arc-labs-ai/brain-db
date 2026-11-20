#![cfg(target_os = "linux")]

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::test_support::{ops_context_for_tests, run_in_glommio, single_body};
use brain_ops::MAX_SESSION_FILTER;
use brain_ops::{dispatch, OpError, OpsContext, RealWriterHandle, RequestCaller};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::{QueryRequest, QueryTraceRequest, RetrieverSelectionWire};

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

const SPACE: [u8; 16] = [0xD1; 16];

fn build_ctx() -> OpsContext {
    let tempdir = tempfile::tempdir().unwrap();
    let metadata: SharedMetadataDb =
        Arc::new(MetadataDb::open(tempdir.path().join("metadata.redb")).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let embedder: Arc<dyn Dispatcher> = Arc::new(MockDispatcher);
    let executor =
        ExecutorContext::new(embedder, shared, metadata, writer as Arc<dyn WriterHandle>);
    let ctx = ops_context_for_tests(executor, tempdir.path());
    std::mem::forget(tempdir);
    ctx
}

fn caller() -> RequestCaller {
    RequestCaller::from_scope(
        brain_core::SpaceId(uuid::Uuid::from_bytes(SPACE)),
        [0u8; 16],
        [0u8; 16],
        "acme".to_string(),
        brain_metadata::api_keys::bits::FULL,
    )
}

fn query(session_filter: Option<Vec<u64>>) -> QueryRequest {
    QueryRequest {
        text: "session filter boundary".to_string(),
        entity_anchor: None,
        kind_filter: Vec::new(),
        predicate_filter: Vec::new(),
        session_filter,
        time_filter: None,
        as_of_record_time_unix_nanos: None,
        confidence_min: None,
        include_tombstoned: false,
        include_superseded: false,
        limit: 10,
        retrievers: RetrieverSelectionWire::Auto,
        fusion_config: None,
        request_id: [0x11; 16],
    }
}

async fn dispatch_query(
    ctx: &OpsContext,
    session_filter: Option<Vec<u64>>,
) -> Result<brain_ops::DispatchOutcome, OpError> {
    dispatch(
        RequestBody::QueryTrace(QueryTraceRequest {
            query: query(session_filter),
            act_as: None,
        }),
        caller(),
        ctx,
    )
    .await
}

#[test]
fn query_session_filter_over_cap_is_refused() {
    run_in_glommio(|| async {
        let ctx = build_ctx();
        let err = dispatch_query(&ctx, Some(vec![7u64; MAX_SESSION_FILTER + 1]))
            .await
            .expect_err("an oversized session_filter must be rejected");

        match err {
            OpError::InvalidRequest(message) => {
                assert!(message.contains("session_filter"), "got {message}");
                assert!(
                    message.contains(&MAX_SESSION_FILTER.to_string()),
                    "got {message}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    });
}

#[test]
fn query_session_filter_at_cap_is_accepted() {
    run_in_glommio(|| async {
        let ctx = build_ctx();
        let outcome = dispatch_query(&ctx, Some(vec![7u64; MAX_SESSION_FILTER]))
            .await
            .expect("a session_filter at the cap must be accepted");

        match single_body(outcome) {
            ResponseBody::QueryTrace(_) => {}
            other => panic!("unexpected response: {other:?}"),
        }
    });
}
