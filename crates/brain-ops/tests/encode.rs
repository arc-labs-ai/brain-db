//! Integration tests for `handle_encode` (sub-task 7.3).
//!
//! Drives the full pipeline:
//!   dispatcher → handle_encode → plan_encode_inner → execute_encode
//!   → RealWriterHandle → metadata + HNSW
//!
//! Embedder is a deterministic mock for offline runs. One test
//! exercises the real BGE dispatcher when `BRAIN_EMBED_MODEL_DIR` is
//! set.

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::test_support::run_in_glommio;
use brain_ops::{dispatch, OpError, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::request::{
    EdgeKindWire, EdgeRequest, EncodeRequest, MemoryKindWire, RequestBody,
};
use brain_protocol::response::{EncodeResponse, ResponseBody};
use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// Mock dispatcher: deterministic per-text vector + stable fingerprint.
// ---------------------------------------------------------------------------

struct MockDispatcher;

impl Dispatcher for MockDispatcher {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        let mut v = [0.0f32; VECTOR_DIM];
        // Hash text bytes into a few slots so distinct texts yield
        // distinct vectors. Norm doesn't matter for these tests.
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
}

fn build_fixture() -> Fixture {
    build_fixture_with_embedder(Arc::new(MockDispatcher) as Arc<dyn Dispatcher>)
}

fn build_fixture_with_embedder(embedder: Arc<dyn Dispatcher>) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));

    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));

    let executor =
        ExecutorContext::new(embedder, shared, metadata, writer as Arc<dyn WriterHandle>);

    Fixture {
        ctx: OpsContext::new(executor),
        _tempdir: tempdir,
    }
}

fn encode_req(request_id: [u8; 16], text: &str) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        context_id: 42,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: vec![],
        request_id,
        txn_id: None,
        deduplicate: false,
    }
}

fn unwrap_encode_resp(body: ResponseBody) -> EncodeResponse {
    match body {
        ResponseBody::Encode(r) => r,
        other => panic!("expected ResponseBody::Encode, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. Full pipeline.
// ---------------------------------------------------------------------------

#[test]
fn encode_full_pipeline_returns_memory_id() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let req = encode_req([1; 16], "hello world");
        let resp = dispatch(
            RequestBody::Encode(req),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap();
        let enc = unwrap_encode_resp(resp);

        assert_ne!(enc.memory_id, 0, "memory_id must be non-zero");
        assert!(!enc.was_deduplicated);
        assert_eq!(enc.salience, 0.5, "salience echoes the request hint");
        assert_eq!(enc.auto_edges_added, 0);
    })
}

// ---------------------------------------------------------------------------
// 2. Idempotency replay is transparent.
// ---------------------------------------------------------------------------
//
// Spec §09/02 §4: same `RequestId` retried returns the original
// response. Spec §09/02 §4a: idempotency replay does NOT set
// `was_deduplicated` — that flag is for fingerprint dedup
// (`deduplicate = true`) only, see §07/07 §6. The two mechanisms
// are intentionally separate.

#[test]
fn encode_replay_returns_same_response_transparently() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let req = encode_req([2; 16], "replay me");

        let first = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(req.clone()),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(
            !first.was_deduplicated,
            "fresh encode without --deduplicate must not report dedup",
        );

        let second = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(
            first.memory_id, second.memory_id,
            "retry returns the same MemoryId (idempotency)",
        );
        assert!(
            !second.was_deduplicated,
            "replay is transparent — was_deduplicated mirrors the original (false here)",
        );
    })
}

// ---------------------------------------------------------------------------
// 3. Conflict path.
// ---------------------------------------------------------------------------

#[test]
fn encode_conflict_returns_conflict_error_code() {
    run_in_glommio(|| async {
        use brain_ops::ErrorCode;

        let fix = build_fixture();
        let first = encode_req([3; 16], "original");
        let conflicting = encode_req([3; 16], "DIFFERENT");

        let _ok = dispatch(
            RequestBody::Encode(first),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap();
        let err = dispatch(
            RequestBody::Encode(conflicting),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::Conflict);
    })
}

// ---------------------------------------------------------------------------
// 4. Consolidated kind rejected at planning.
// ---------------------------------------------------------------------------

#[test]
fn encode_consolidated_kind_rejected() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let mut req = encode_req([4; 16], "no consolidated");
        req.kind = MemoryKindWire::Consolidated;

        let err = dispatch(
            RequestBody::Encode(req),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, OpError::PlanError(_)),
            "Consolidated rejection comes from the planner, got {err:?}"
        );
        assert_eq!(
            err.error_code(),
            brain_ops::ErrorCode::InvalidRequest,
            "Consolidated kind must map to InvalidRequest"
        );
    })
}

// ---------------------------------------------------------------------------
// 5. Edges: insert count is reflected in auto_edges_added.
// ---------------------------------------------------------------------------

#[test]
fn encode_auto_edges_added_counts_inserted_only() {
    run_in_glommio(|| async {
        let fix = build_fixture();

        // First, write a target memory we can link to.
        let target = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req([5; 16], "target")),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_ne!(target.memory_id, 0);

        // Now encode with two edges: one valid, one to a non-existent id.
        let mut req = encode_req([6; 16], "linker");
        req.edges = vec![
            EdgeRequest {
                target: target.memory_id,
                kind: EdgeKindWire::References,
                weight: 0.5,
            },
            EdgeRequest {
                target: 0xDEAD_BEEF_u128,
                kind: EdgeKindWire::References,
                weight: 0.5,
            },
        ];
        let resp = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(
            resp.auto_edges_added, 1,
            "only the edge to the live target counts"
        );
    })
}

// ---------------------------------------------------------------------------
// 5b. Fingerprint dedup (spec §07/07 §6 + §09/02 §4a).
// ---------------------------------------------------------------------------
//
// Distinct from idempotency (§4). Opt-in via `EncodeRequest.deduplicate`;
// scoped per `(shard, agent_id, context_id)`; tombstone-aware.

fn encode_req_with_dedup(
    request_id: [u8; 16],
    text: &str,
    context_id: u64,
    deduplicate: bool,
) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        context_id,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: vec![],
        request_id,
        txn_id: None,
        deduplicate,
    }
}

#[test]
fn dedup_off_always_returns_fresh_slot() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let a = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([1; 16], "same text", 1, false)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        let b = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([2; 16], "same text", 1, false)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_ne!(a.memory_id, b.memory_id, "no dedup → distinct slots");
        assert!(!a.was_deduplicated);
        assert!(!b.was_deduplicated);
    })
}

#[test]
fn dedup_hit_returns_existing_memory_id() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let first = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([3; 16], "dedup me", 1, true)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(
            !first.was_deduplicated,
            "first encode is a fresh slot (miss)"
        );

        // Same text + same context + different request_id + dedup=true.
        let second = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([4; 16], "dedup me", 1, true)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(
            second.was_deduplicated,
            "second encode hits the fingerprint"
        );
        assert_eq!(first.memory_id, second.memory_id);
    })
}

#[test]
fn dedup_different_context_no_hit() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let ctx_a = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([5; 16], "same text", 1, true)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        let ctx_b = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([6; 16], "same text", 2, true)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_ne!(
            ctx_a.memory_id, ctx_b.memory_id,
            "different context partitions"
        );
        assert!(!ctx_a.was_deduplicated);
        assert!(
            !ctx_b.was_deduplicated,
            "ctx 2 must not hit ctx 1's fingerprint"
        );
    })
}

#[test]
fn dedup_off_then_on_still_misses() {
    run_in_glommio(|| async {
        // The dedup-off encode does NOT populate the fingerprint table.
        // A later dedup-on encode of the same text must miss.
        let fix = build_fixture();
        let _bare = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([7; 16], "wash me", 1, false)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        let dedup_attempt = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([8; 16], "wash me", 1, true)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(
            !dedup_attempt.was_deduplicated,
            "dedup-off encode does not populate the index; dedup-on must miss",
        );
    })
}

#[test]
fn dedup_after_forget_evicts_and_misses() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let first = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([9; 16], "evict me", 1, true)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(!first.was_deduplicated);

        // Forget the dedup-indexed memory.
        let forget = brain_protocol::request::ForgetRequest {
            memory_id: first.memory_id,
            mode: brain_protocol::request::ForgetMode::Soft,
            request_id: [0xAA; 16],
            txn_id: None,
        };
        dispatch(
            RequestBody::Forget(forget),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap();

        // Re-encode the same text with dedup. The fingerprint entry
        // was evicted in the same txn as the tombstone — must miss.
        let after = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([10; 16], "evict me", 1, true)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(
            !after.was_deduplicated,
            "fingerprint was evicted on FORGET → dedup must miss",
        );
        assert_ne!(first.memory_id, after.memory_id, "fresh slot allocated");
    })
}

// ---------------------------------------------------------------------------
// 6. Live writer persists text to TEXTS_TABLE.
// ---------------------------------------------------------------------------

#[test]
fn encode_persists_text_to_texts_table_atomically() {
    use brain_metadata::tables::text::TEXTS_TABLE;
    run_in_glommio(|| async {
        let fix = build_fixture();
        let text = "exact-bytes-we-encoded";
        let resp = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req([0x60; 16], text)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );

        // Reach into the metadata DB and assert the texts row exists
        // and equals exactly what we encoded. Same redb file the live
        // writer just wrote.
        let memory_id_bytes = resp.memory_id.to_be_bytes();
        let metadata = fix.ctx.executor.metadata.clone();
        let guard = metadata.lock();
        let rtxn = guard.read_txn().expect("read_txn");
        let table = rtxn.open_table(TEXTS_TABLE).expect("texts table exists");
        let row = table
            .get(memory_id_bytes)
            .expect("get ok")
            .expect("texts row present");
        assert_eq!(row.value(), text.as_bytes());
    })
}

/// Read the stored text for `memory_id` from the fixture's metadata.
/// Returns `None` if the row is absent and panics on any other error
/// (open / get / borrow) — tests use this on the happy path only.
fn read_text(fix: &Fixture, memory_id: u128) -> Option<Vec<u8>> {
    use brain_metadata::tables::text::TEXTS_TABLE;
    let guard = fix.ctx.executor.metadata.lock();
    let rtxn = guard.read_txn().expect("read_txn");
    let table = rtxn.open_table(TEXTS_TABLE).expect("texts table exists");
    table
        .get(memory_id.to_be_bytes())
        .expect("texts get")
        .map(|g| g.value().to_vec())
}

#[test]
fn encode_empty_text_is_rejected_at_planner() {
    use brain_ops::ErrorCode;
    run_in_glommio(|| async {
        let fix = build_fixture();
        let err = dispatch(
            RequestBody::Encode(encode_req([0x61; 16], "")),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .expect_err("empty text must be rejected upstream of the writer");
        assert_eq!(err.error_code(), ErrorCode::InvalidRequest);
        let msg = format!("{err:?}");
        assert!(
            msg.contains("text") && (msg.contains("empty") || msg.contains("non-empty")),
            "error should mention text/empty, got: {msg}",
        );
    })
}

#[test]
fn encode_unicode_text_round_trips_byte_for_byte() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        // Mixture of ASCII, multibyte UTF-8, emoji (4-byte UTF-8),
        // and a combining mark.
        let text = "héllo 🌍 — naïve café é\u{0301}";
        let resp = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req([0x62; 16], text)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        let got = read_text(&fix, resp.memory_id).expect("row present");
        assert_eq!(got, text.as_bytes(), "byte-for-byte round trip");
    })
}

#[test]
fn encode_large_text_round_trips() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        // 64 KiB — well under the wire frame cap, well above redb's
        // internal page size so it exercises the variable-length value
        // path.
        let text: String = "lorem ipsum ".repeat(5500);
        let resp = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req([0x63; 16], &text)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        let got = read_text(&fix, resp.memory_id).expect("row present");
        assert_eq!(got, text.as_bytes());
        assert!(got.len() >= 64_000);
    })
}

#[test]
fn encode_idempotent_retry_keeps_single_text_row_unchanged() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let text = "idempotent-text-payload";
        let req = encode_req([0x64; 16], text);

        let first = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(req.clone()),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        let second = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );

        // Idempotency replay must return the same memory_id and leave
        // the texts row unchanged (and untwinned).
        assert_eq!(first.memory_id, second.memory_id);
        assert_eq!(
            read_text(&fix, first.memory_id),
            Some(text.as_bytes().to_vec())
        );
    })
}

#[test]
fn encode_dedup_hit_does_not_clobber_original_text_row() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let original_text = "dedup-original-text";

        let first = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([0x70; 16], original_text, 9, true)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(!first.was_deduplicated);
        assert_eq!(
            read_text(&fix, first.memory_id),
            Some(original_text.as_bytes().to_vec())
        );

        // Same text + context + dedup=true under a different request_id:
        // server returns the existing memory_id and must NOT mutate the
        // original text row.
        let second = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([0x71; 16], original_text, 9, true)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(second.was_deduplicated);
        assert_eq!(first.memory_id, second.memory_id);
        assert_eq!(
            read_text(&fix, first.memory_id),
            Some(original_text.as_bytes().to_vec()),
            "dedup hit must not rewrite the texts row",
        );
    })
}

// ---------------------------------------------------------------------------
// 7. Real-embedder gated test. Skips when env var is unset.
// ---------------------------------------------------------------------------

#[test]
fn encode_with_real_embedder_end_to_end() {
    run_in_glommio(|| async {
        let Ok(model_dir) = std::env::var("BRAIN_EMBED_MODEL_DIR") else {
            eprintln!("BRAIN_EMBED_MODEL_DIR unset; skipping BGE end-to-end test");
            return;
        };

        let model_dir = std::path::PathBuf::from(model_dir);
        let handle = brain_embed::ModelHandle::load(&brain_embed::EmbedderConfig::new(model_dir))
            .expect("BGE model loads");
        let dispatcher = brain_embed::CpuDispatcher::new(handle);
        let fix = build_fixture_with_embedder(Arc::new(dispatcher) as Arc<dyn Dispatcher>);

        let req = encode_req([0x7E; 16], "the real embedder is plumbed end-to-end");
        let resp = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_ne!(resp.memory_id, 0);
        assert!(!resp.was_deduplicated);
    })
}
