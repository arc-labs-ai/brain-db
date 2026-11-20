//! QUERY must survive a plan that carries a semantic lane with no cue text.
//!
//! The retrieval router adds `Retriever::Semantic` whenever the request has an
//! entity anchor, and gates only `Retriever::Lexical` behind the presence of
//! text (`brain-planner`'s `entity_anchor_without_text_selects_graph_and_semantic_only`
//! pins that rule). An anchored QUERY carrying `text: ""` therefore produces a
//! plan whose semantic lane has nothing to embed.
//!
//! The executor owns that case: `invoke_semantic` reports
//! `Skipped("no query text")` and the graph lane answers the query. This proof
//! drives the real `dispatch` → handler path to show the handler hands that
//! case down rather than resolving it itself, because a wire request with an
//! empty `text` and an anchor is well-formed and reaches the shard.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::test_support::{ops_context_for_tests, run_in_glommio, single_body};
use brain_ops::{dispatch, OpsContext, RealWriterHandle, RequestCaller};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::{QueryRequest, QueryTraceRequest, RetrieverSelectionWire};

/// Text-driven deterministic vectors — the embedder is irrelevant to this
/// proof, which is about the path taken when there is no text to embed.
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
    // The context holds paths into the tempdir for the life of the test.
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

/// An anchored QUERY carrying no cue text is a well-formed wire request. It
/// must return a trace, not fail and not abort the shard.
#[test]
fn an_anchored_query_without_text_still_answers() {
    run_in_glommio(|| async {
        let ctx = build_ctx();

        // Any anchor will do: the router keys the semantic lane on the
        // anchor's presence, not on what the graph walk finds behind it.
        let anchor: [u8; 16] = [0x7E; 16];

        let query = QueryRequest {
            // The condition under proof: an anchor, and nothing to embed.
            text: String::new(),
            entity_anchor: Some(anchor),
            kind_filter: Vec::new(),
            predicate_filter: Vec::new(),
            session_filter: None,
            time_filter: None,
            as_of_record_time_unix_nanos: None,
            confidence_min: None,
            include_tombstoned: false,
            include_superseded: false,
            limit: 10,
            retrievers: RetrieverSelectionWire::Auto,
            fusion_config: None,
            request_id: [0x11; 16],
        };

        let outcome = dispatch(
            RequestBody::QueryTrace(QueryTraceRequest {
                query,
                act_as: None,
            }),
            caller(),
            &ctx,
        )
        .await
        .expect("an anchored, textless QUERY is answerable");

        match single_body(outcome) {
            ResponseBody::QueryTrace(r) => {
                assert!(
                    !r.trace_text.is_empty(),
                    "the trace must describe the plan that ran",
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    });
}
