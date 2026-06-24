//! Correctness tripwire.
//!
//! Deep per-op coverage lives in the per-operation test files
//! (`encode.rs`, `recall.rs`, `forget.rs`, `link.rs`, `txn.rs`,
//! `plan.rs`, `reason.rs`, `subscribe.rs`). This file keeps a single
//! end-to-end smoke test: an encoded memory must be recallable by its
//! own cue. If that breaks, the whole write→index→read path is broken
//! and the per-op suites will say where.

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::test_support::run_in_glommio;
use brain_ops::{dispatch, DispatchOutcome, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{EncodeRequest, RecallRequest, RequestBody};
use brain_protocol::envelope::response::{EncodeResponse, RecallResponseFrame, ResponseBody};

struct MockDispatcher;

impl Dispatcher for MockDispatcher {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        let mut v = [0.0f32; VECTOR_DIM];
        for (i, byte) in text.as_bytes().iter().enumerate() {
            v[i % VECTOR_DIM] += f32::from(*byte) / 255.0;
        }
        // The real embedder always L2-normalizes its output, and the
        // HNSW (DistCosine) only yields cosine in [-1, 1] for unit
        // input. The mock must honor that contract — otherwise a longer
        // string's larger-magnitude vector dominates the raw dot product
        // and wins every cue, so a memory never tops its own exact cue.
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        Ok(v)
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
    // The byte-additive mock has no asymmetric-retrieval model, so the
    // BGE query prefix would just be noise. Embed the bare text instead,
    // which is the mock-world equivalent of "query and passage of the
    // same surface are comparable".
    fn embed_query(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        self.embed(text)
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0xAB; 16]
    }
}

struct Fixture {
    ctx: OpsContext,
    tempdir: tempfile::TempDir,
    metadata: SharedMetadataDb,
}

impl Fixture {
    /// Rebuild the lexical lane from redb so recall sees a fully-indexed
    /// corpus (no text-indexer worker runs in a unit test). Call after
    /// the last encode and before recall.
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

fn encode_req(rid: [u8; 16], text: &str) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        context_id: 42,
        request_id: rid,
        txn_id: None,
        occurred_at_unix_nanos: None,
    }
}

fn single_body(outcome: DispatchOutcome) -> ResponseBody {
    match outcome {
        DispatchOutcome::Single(b) => b,
        DispatchOutcome::Stream(_) => panic!("expected DispatchOutcome::Single, got Stream"),
    }
}

async fn encode(fix: &Fixture, rid: [u8; 16], text: &str) -> u128 {
    match single_body(
        dispatch(
            RequestBody::Encode(encode_req(rid, text)),
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

async fn recall(fix: &Fixture, cue: &str, max_results: u32) -> RecallResponseFrame {
    let req = RecallRequest {
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
        txn_id: None,
        agent_filter: Vec::new(),
        include_other_agents: false,
    };
    match single_body(
        dispatch(
            RequestBody::Recall(req),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap(),
    ) {
        ResponseBody::Recall(f) => f,
        other => panic!("expected Recall, got {other:?}"),
    }
}

/// End-to-end tripwire: every encoded memory tops its own exact cue.
#[test]
fn encoded_memories_are_recallable() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();
        let texts = ["alpha", "beta", "gamma", "delta", "epsilon"];
        let mut ids = Vec::new();
        for (i, text) in texts.iter().enumerate() {
            let mut rid = [0u8; 16];
            rid[0] = (i + 1) as u8;
            ids.push(encode(&fix, rid, text).await);
        }
        fix.reindex_lexical();
        for (i, text) in texts.iter().enumerate() {
            let frame = recall(&fix, text, 1).await;
            assert_eq!(frame.memories.len(), 1, "top-1 must exist for {text}");
            assert_eq!(
                frame.memories[0].memory_id, ids[i],
                "top-1 for {text} must be the memory we just encoded"
            );
            assert!(
                frame.memories[0].similarity_score > 0.99,
                "exact-cue similarity for {text} must be ~1.0, got {}",
                frame.memories[0].similarity_score
            );
        }
    })
}
