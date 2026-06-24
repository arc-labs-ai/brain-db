//! Integration tests for `handle_recall`.
//!
//! Drives the full pipeline:
//!   dispatcher → handle_recall → plan_recall_inner → execute_recall
//!   → wire RecallResponseFrame
//!
//! Pre-populates the index by calling ENCODE through the dispatcher
//! first, then runs RECALL against it.

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::test_support::{run_in_glommio, single_body};
use brain_ops::{dispatch, DispatchOutcome, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{
    EncodeRequest, MemoryKindWire, RecallRequest, RequestBody,
};
use brain_protocol::envelope::response::{EncodeResponse, RecallResponseFrame, ResponseBody};

// ---------------------------------------------------------------------------
// Mock dispatcher: text-driven deterministic vectors.
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
    tempdir: tempfile::TempDir,
    metadata: SharedMetadataDb,
}

impl Fixture {
    /// Rebuild the lexical lane from redb so recall sees a fully-indexed
    /// corpus, the way a production shard does. The write path populates
    /// redb + HNSW but no text-indexer worker runs in a unit test, so the
    /// lexical lane is empty until this is called. Invoke it after the
    /// last encode and before recall; the read path's structural
    /// abstention needs a stored memory's own words to confirm it.
    fn reindex_lexical(&mut self) {
        self.ctx.lexical_retriever = brain_ops::test_support::reindex_memory_lexical_for_tests(
            self.tempdir.path(),
            &self.metadata,
        );
    }
}

fn build_fixture() -> Fixture {
    build_fixture_with_embedder(Arc::new(MockDispatcher) as Arc<dyn Dispatcher>)
}

fn build_fixture_with_embedder(embedder: Arc<dyn Dispatcher>) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());

    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));

    let executor = ExecutorContext::new(
        embedder,
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

// `_kind` is accepted for call-site compatibility but ignored: the
// write router decides the memory kind now (always Episodic), so the
// client can no longer steer it.
fn encode_req(request_id: [u8; 16], text: &str, _kind: MemoryKindWire) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        context_id: 42,
        request_id,
        txn_id: None,
        occurred_at_unix_nanos: None,
    }
}

fn recall_req(cue: &str, max_results: u32) -> RecallRequest {
    RecallRequest {
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
    }
}

async fn encode(fix: &Fixture, request_id: [u8; 16], text: &str, kind: MemoryKindWire) -> u128 {
    let req = encode_req(request_id, text, kind);
    let outcome = dispatch(
        RequestBody::Encode(req),
        brain_ops::RequestCaller::anonymous(),
        &fix.ctx,
    )
    .await
    .unwrap();
    match single_body(outcome) {
        ResponseBody::Encode(EncodeResponse { memory_id, .. }) => memory_id,
        other => panic!("expected Encode response, got {other:?}"),
    }
}

fn unwrap_recall_resp(outcome: DispatchOutcome) -> RecallResponseFrame {
    match single_body(outcome) {
        ResponseBody::Recall(r) => r,
        other => panic!("expected ResponseBody::Recall, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. Full pipeline.
// ---------------------------------------------------------------------------

#[test]
fn recall_cue_hit_returns_member_with_fields_plumbed() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();
        let alpha = encode(&fix, [1; 16], "alpha", MemoryKindWire::Episodic).await;
        encode(&fix, [2; 16], "beta", MemoryKindWire::Episodic).await;
        encode(&fix, [3; 16], "gamma", MemoryKindWire::Episodic).await;
        fix.reindex_lexical();

        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(recall_req("alpha", 2)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(frame.is_final);
        // Recall returns the membership set, not a top-k pile. The cue
        // "alpha" is the one memory both the semantic and lexical lanes
        // agree on, so the cross-lane consensus collapses to that crisp
        // Single — "beta"/"gamma" are at most single-lane noise.
        assert_eq!(
            frame.memories.len(),
            1,
            "unique cross-lane consensus → Single"
        );
        assert_eq!(frame.cumulative_count as usize, frame.memories.len());
        assert_eq!(
            frame.memories[0].memory_id, alpha,
            "the confirmed member is the alpha memory"
        );
        // Fields plumbed through.
        let top = &frame.memories[0];
        assert_ne!(top.memory_id, 0);
        assert_eq!(top.context_id, 42);
        assert_eq!(top.kind, MemoryKindWire::Episodic);
        assert!((top.salience - 0.5).abs() < 1e-6);
        // The retrieval pipeline carries two distinct scores per hit:
        // `similarity_score` is the semantic retriever's raw cosine and
        // `confidence` mirrors it — both bounded in [0, 1]. The unbounded
        // RRF rank-fusion sum is a separate diagnostic (`fused_score`),
        // never surfaced as confidence.
        assert!(top.similarity_score > 0.0, "similarity_score populated");
        assert!(
            top.confidence > 0.0 && top.confidence <= 1.0,
            "confidence is a bounded [0,1] similarity, got {}",
            top.confidence
        );
        assert!(
            (top.confidence - top.similarity_score).abs() < 1e-6,
            "confidence mirrors similarity_score on the retrieval path"
        );
        assert_eq!(
            top.last_accessed_at_unix_nanos, top.created_at_unix_nanos,
            "v1: last_accessed mirrors created_at"
        );
        assert!(top.edges.is_none());
        // A plain ENCODE supplies no event time, so the echoed field is
        // absent — distinct from `created_at`, which is always stamped.
        assert_eq!(top.occurred_at_unix_nanos, None);
    })
}

// ---------------------------------------------------------------------------
// 1b. Client-supplied event time round-trips through to RECALL.
// ---------------------------------------------------------------------------

#[test]
fn recall_echoes_client_supplied_occurred_at() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();

        // A real-world event time well in the past — distinct from the
        // server's write time so we can prove the two don't get conflated.
        let event_time: u64 = 1_577_836_800_000_000_000; // 2020-01-01T00:00:00Z

        let req = EncodeRequest {
            text: "moved to berlin".into(),
            context_id: 42,
            request_id: [9; 16],
            txn_id: None,
            occurred_at_unix_nanos: Some(event_time),
        };
        dispatch(
            RequestBody::Encode(req),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap();
        fix.reindex_lexical();

        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(recall_req("moved to berlin", 1)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(frame.memories.len(), 1);
        let hit = &frame.memories[0];
        assert_eq!(
            hit.occurred_at_unix_nanos,
            Some(event_time),
            "client event time must survive the write→read round trip"
        );
        assert_ne!(
            hit.created_at_unix_nanos, event_time,
            "occurred_at is the client's timeline, not the server write time"
        );
    })
}

// ---------------------------------------------------------------------------
// 1c. Recency ranking: with a temporal signal (`as_of`), the more-recent
//     memory wins a relevance tie.
// ---------------------------------------------------------------------------

#[test]
fn recency_breaks_relevance_ties_toward_recent_event_time() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();

        // Reference point for the recency decay. `as_of` on the request
        // both supplies the temporal signal that gates the boost and sets
        // this reference.
        let reference: u64 = 1_900_000_000 * 1_000_000_000;
        let day = 86_400 * 1_000_000_000_u64;

        // Identical text ⇒ identical mock vectors ⇒ identical cosine, so
        // the two hits tie on pure relevance and only event-time recency
        // separates them.
        let text = "team offsite in lisbon";
        let recent = EncodeRequest {
            text: text.into(),
            context_id: 42,
            request_id: [21; 16],
            txn_id: None,
            occurred_at_unix_nanos: Some(reference - day), // yesterday
        };
        let old = EncodeRequest {
            occurred_at_unix_nanos: Some(reference - 400 * day), // >1 year ago
            request_id: [22; 16],
            ..recent.clone()
        };
        let recent_id = match single_body(
            dispatch(
                RequestBody::Encode(recent),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        ) {
            ResponseBody::Encode(EncodeResponse { memory_id, .. }) => memory_id,
            other => panic!("expected Encode, got {other:?}"),
        };
        dispatch(
            RequestBody::Encode(old),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap();

        fix.reindex_lexical();

        let mut recall = recall_req(text, 2);
        recall.as_of_record_time_unix_nanos = Some(reference);

        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(recall),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(frame.memories.len(), 2);
        assert_eq!(
            frame.memories[0].memory_id, recent_id,
            "on a relevance tie, the more recent event time ranks first when a temporal signal is present",
        );
    })
}

// ---------------------------------------------------------------------------
// 1d. List-intent recall drives the merge/diversity path end-to-end.
//     The cue carries enumerative intent ("list all ..."), so the router
//     flags list_intent and the executor runs the MMR stage over real
//     redb text. This is a smoke test that the path (text fetch →
//     tokenize → MMR reorder) executes on live data without panicking
//     and still returns the requested results.
// ---------------------------------------------------------------------------

#[test]
fn list_intent_recall_runs_merge_path_and_returns_results() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();
        encode(
            &fix,
            [1; 16],
            "she enjoys hiking in the mountains",
            MemoryKindWire::Episodic,
        )
        .await;
        encode(
            &fix,
            [2; 16],
            "she enjoys hiking up steep trails",
            MemoryKindWire::Episodic,
        )
        .await;
        encode(
            &fix,
            [3; 16],
            "she enjoys painting watercolors",
            MemoryKindWire::Episodic,
        )
        .await;
        fix.reindex_lexical();

        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(recall_req("list all the things she enjoys", 3)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(
            !frame.memories.is_empty(),
            "list-intent recall must still return results after the merge stage",
        );
        assert!(
            frame.memories.len() <= 3,
            "max_results is a safety ceiling on the membership set",
        );
    })
}

// ---------------------------------------------------------------------------
// 2. Empty index → empty frame.
// ---------------------------------------------------------------------------

#[test]
fn recall_empty_index_returns_empty_frame() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(recall_req("nothing", 10)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(frame.memories.is_empty());
        assert!(frame.is_final);
        assert_eq!(frame.cumulative_count, 0);
        assert!(frame.estimated_remaining.is_none());
    })
}

// ---------------------------------------------------------------------------
// 3. max_results bounds the membership set (a safety ceiling, not a target).
// ---------------------------------------------------------------------------

#[test]
fn recall_membership_set_bounded_by_max_results() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();
        for i in 0..5u8 {
            let mut req_id = [0u8; 16];
            req_id[0] = 0x10 + i;
            let text = format!("doc-{i}");
            encode(&fix, req_id, &text, MemoryKindWire::Episodic).await;
        }
        fix.reindex_lexical();
        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(recall_req("doc-2", 3)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        // Five near-identical docs all share the "doc" token, so several
        // belong to the cue — but the returned set never exceeds the
        // requested ceiling. max_results caps, it does not pad.
        assert!(!frame.memories.is_empty(), "the doc-2 cue has members");
        assert!(
            frame.memories.len() <= 3,
            "membership set must not exceed max_results, got {}",
            frame.memories.len(),
        );
    })
}

// ---------------------------------------------------------------------------
// 4. Kind filter.
// ---------------------------------------------------------------------------
//
// The client can no longer choose a memory's kind via ENCODE — the
// write router files every text encode as Episodic. The old
// "recall kind filter rejects off-kind hits" test relied on encoding
// Semantic memories from the client, which is no longer possible, so it
// has been removed. The kind-filter retrieval mechanism itself is still
// exercised by the planner/retriever unit tests.

// ---------------------------------------------------------------------------
// 5. Confidence floor.
// ---------------------------------------------------------------------------

#[test]
fn recall_confidence_floor_drops_low_score_hits() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        encode(&fix, [30; 16], "alpha", MemoryKindWire::Episodic).await;
        encode(
            &fix,
            [31; 16],
            "completely-different-cue",
            MemoryKindWire::Episodic,
        )
        .await;

        let mut req = recall_req("totally-unrelated-query-xyz", 10);
        // 0.999 is so strict that the unrelated cue should drop everything.
        req.confidence_threshold = 0.999;
        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        for r in &frame.memories {
            assert!(
                r.similarity_score >= 0.999,
                "every result must clear the floor; got {}",
                r.similarity_score
            );
        }
    })
}

// ---------------------------------------------------------------------------
// 6. max_results=0 → server default (the cap is a safety bound, not a
//    "give me zero" request).
// ---------------------------------------------------------------------------

#[test]
fn recall_zero_max_results_defaults_not_rejected() {
    // `max_results` is a safety cap on the returned set, not a ranking
    // knob: `0` means "server default", never "give me zero results".
    // The handler normalises it and proceeds instead of erroring.
    run_in_glommio(|| async {
        let mut fix = build_fixture();
        encode(&fix, [60; 16], "zero-cap-alpha", MemoryKindWire::Episodic).await;
        fix.reindex_lexical();
        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(recall_req("zero-cap-alpha", 0)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .expect("max_results=0 must default, not error"),
        );
        // The encoded memory is returned — the cap defaulted to a
        // generous bound rather than zero.
        assert!(
            !frame.memories.is_empty(),
            "max_results=0 should default and still return hits"
        );
    })
}

// ---------------------------------------------------------------------------
// 7. include_text — substrate path round-trip.
// ---------------------------------------------------------------------------

#[test]
fn recall_returns_text_even_when_include_text_false() {
    // `include_text` is intentionally forced on in the read path: a recalled
    // memory without its text is useless to the caller, so `handle_recall`
    // returns the remembered text regardless of the (legacy) wire flag. This
    // guards that deliberate behavior — `recall_req` defaults the flag to false,
    // yet every recalled memory must still carry its text.
    run_in_glommio(|| async {
        let mut fix = build_fixture();
        encode(&fix, [40; 16], "alpha-text-rev0", MemoryKindWire::Episodic).await;
        encode(&fix, [41; 16], "beta-text-rev0", MemoryKindWire::Episodic).await;
        fix.reindex_lexical();

        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(recall_req("alpha-text-rev0", 2)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(
            frame.memories.iter().any(|r| r.text == "alpha-text-rev0"),
            "recall must return the remembered text even with include_text=false, got {:?}",
            frame.memories.iter().map(|r| &r.text).collect::<Vec<_>>()
        );
        for r in &frame.memories {
            assert!(
                !r.text.is_empty(),
                "every recalled memory must carry its text"
            );
        }
    })
}

#[test]
fn recall_include_text_true_returns_stored_text() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();
        let ids = [
            (
                encode(&fix, [50; 16], "alpha-text-rev1", MemoryKindWire::Episodic).await,
                "alpha-text-rev1",
            ),
            (
                encode(&fix, [51; 16], "beta-text-rev1", MemoryKindWire::Episodic).await,
                "beta-text-rev1",
            ),
            (
                encode(&fix, [52; 16], "gamma-text-rev1", MemoryKindWire::Episodic).await,
                "gamma-text-rev1",
            ),
        ];
        let by_id: std::collections::HashMap<u128, &'static str> = ids.iter().copied().collect();
        fix.reindex_lexical();

        let mut req = recall_req("alpha-text-rev1", 3);
        req.include_text = true;
        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );

        // Whatever the membership set's size, every returned member must
        // carry the exact UTF-8 stored for its id.
        assert!(
            !frame.memories.is_empty(),
            "the cue has at least one member"
        );
        for r in &frame.memories {
            let want = by_id.get(&r.memory_id).copied().expect("known id");
            assert_eq!(
                r.text, want,
                "include_text=true must return the exact UTF-8 we encoded",
            );
        }
    })
}

// ---------------------------------------------------------------------------
// 8. Real-embedder gated test. Skips when env var is unset.
// ---------------------------------------------------------------------------

#[test]
fn recall_with_real_embedder_end_to_end() {
    run_in_glommio(|| async {
        let Ok(model_dir) = std::env::var("BRAIN_EMBED_MODEL_DIR") else {
            eprintln!("BRAIN_EMBED_MODEL_DIR unset; skipping BGE end-to-end test");
            return;
        };

        let model_dir = std::path::PathBuf::from(model_dir);
        let handle = brain_embed::ModelHandle::load(&brain_embed::EmbedderConfig::new(model_dir))
            .expect("BGE model loads");
        let dispatcher = brain_embed::CpuDispatcher::new(handle);
        let mut fix = build_fixture_with_embedder(Arc::new(dispatcher) as Arc<dyn Dispatcher>);

        let cats_id = encode(
            &fix,
            [0x70; 16],
            "the cat sat on the mat",
            MemoryKindWire::Episodic,
        )
        .await;
        let _physics_id = encode(
            &fix,
            [0x71; 16],
            "quantum entanglement collapses on observation",
            MemoryKindWire::Episodic,
        )
        .await;
        fix.reindex_lexical();

        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(recall_req("a cat resting on a rug", 2)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        // The physics memory shares no words and is semantically distant,
        // so it does not belong to a cat cue. The cat memory is the one
        // member — and it leads the answer.
        assert!(
            !frame.memories.is_empty(),
            "the cat cue must recall the cat memory"
        );
        assert_eq!(
            frame.memories[0].memory_id, cats_id,
            "the cat memory must lead the membership set"
        );
    })
}

// ---------------------------------------------------------------------------
// 9. handle_recall routing — no txn fuses through the retrieval lanes.
//
// RECALL is one verb with one server-side rule: a txn forces the substrate
// path (read-your-writes), everything else fuses through the retrieval
// lanes. With a real semantic lane (HNSW) and a populated lexical lane, a
// cue that matches a stored memory surfaces a fused hit carrying multiple
// contributors and a non-zero fused score. A regression that re-routed to
// substrate would zero `fused_score` and clear `contributing_retrievers`.
// ---------------------------------------------------------------------------

#[test]
fn handle_recall_no_txn_fuses_retrieval_lanes() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();
        let _mid = encode(&fix, [0xA0; 16], "beta", MemoryKindWire::Episodic).await;
        fix.reindex_lexical();

        let frame = brain_ops::recall::handle_recall(recall_req("beta", 5), &fix.ctx)
            .await
            .expect("retrieval recall");

        assert!(frame.is_final);
        assert!(
            !frame.memories.is_empty(),
            "retrieval recall returned no hits",
        );
        // The cue "beta" is the stored memory's own text, so both the
        // semantic and lexical lanes surface it — fusion records the
        // contributing lanes and a non-zero fused score.
        let any_with_retrievers = frame
            .memories
            .iter()
            .any(|r| !r.contributing_retrievers.is_empty());
        let any_nonzero_fused = frame.memories.iter().any(|r| r.fused_score > 0.0);
        assert!(
            any_with_retrievers,
            "retrieval path must populate contributing_retrievers on at least one hit",
        );
        assert!(
            any_nonzero_fused,
            "retrieval path must produce a non-zero fused_score on at least one hit",
        );
    })
}
