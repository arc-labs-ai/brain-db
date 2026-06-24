//! Integration tests for `handle_encode`.
//!
//! Drives the full pipeline:
//!   dispatcher → handle_encode → plan_encode_inner →
//!   RealWriterHandle::submit(Write) → metadata + HNSW
//!
//! Embedder is a deterministic mock for offline runs. One test
//! exercises the real BGE dispatcher when `BRAIN_EMBED_MODEL_DIR` is
//! set.

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::test_support::{run_in_glommio, single_body};
use brain_ops::{dispatch, DispatchOutcome, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{EncodeRequest, RequestBody};
use brain_protocol::envelope::response::{EncodeResponse, ResponseBody};

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
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());

    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));

    let executor =
        ExecutorContext::new(embedder, shared, metadata, writer as Arc<dyn WriterHandle>);

    Fixture {
        ctx: brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor),
        _tempdir: tempdir,
    }
}

fn encode_req(request_id: [u8; 16], text: &str) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        context_id: 42,
        request_id,
        txn_id: None,
        occurred_at_unix_nanos: None,
    }
}

fn unwrap_encode_resp(outcome: DispatchOutcome) -> EncodeResponse {
    match single_body(outcome) {
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
        assert_eq!(enc.salience, 0.5, "salience is the router default");
        assert_eq!(enc.auto_edges_added, 0);
    })
}

// ---------------------------------------------------------------------------
// 2. Idempotency replay is transparent.
// ---------------------------------------------------------------------------
//
// same `RequestId` retried returns the original
// responsea: idempotency replay does NOT set
// `was_deduplicated` — that flag is for fingerprint dedup
// (`deduplicate = true`) only. The two mechanisms
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
// 4. Memory kind is router-decided.
// ---------------------------------------------------------------------------
//
// The client can no longer choose the memory kind (the `kind` field is
// gone from ENCODE); the write router files every text encode as
// Episodic. The old "Consolidated is rejected" test exercised a
// capability that no longer exists on this path.

// ---------------------------------------------------------------------------
// 5b. Fingerprint dedup (a).
// ---------------------------------------------------------------------------
//
// Dedup is now a DB policy — always on for text ENCODE, scoped per
// `(shard, agent_id, context_id)`, tombstone-aware. The client can no
// longer toggle it, so the builder always produces a router-default
// (dedup-on) encode; only the request_id / text / context vary.

fn encode_req_with_dedup(request_id: [u8; 16], text: &str, context_id: u64) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        context_id,
        request_id,
        txn_id: None,
        occurred_at_unix_nanos: None,
    }
}

#[test]
fn same_text_distinct_request_ids_create_two_memories() {
    // The write path stores faithfully: two separate observations of the
    // same text under different request_ids are two distinct memories, and
    // neither reports a dedup hit. Collapsing genuine near-duplicates is an
    // asynchronous consolidation concern, never a synchronous write-time
    // drop — the client cannot lose a write to an implicit dedup.
    run_in_glommio(|| async {
        let fix = build_fixture();
        let first = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([3; 16], "dedup me", 1)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(!first.was_deduplicated);

        // Same text + same context, different request_id.
        let second = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([4; 16], "dedup me", 1)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(!second.was_deduplicated);
        assert_ne!(
            first.memory_id, second.memory_id,
            "same text under a new request_id is a distinct memory",
        );
    })
}

#[test]
fn dedup_different_context_no_hit() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let ctx_a = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([5; 16], "same text", 1)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        let ctx_b = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([6; 16], "same text", 2)),
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
fn dedup_after_forget_evicts_and_misses() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let first = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([9; 16], "evict me", 1)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(!first.was_deduplicated);

        // Forget the dedup-indexed memory.
        let forget = brain_protocol::envelope::request::ForgetRequest {
            memory_id: first.memory_id,
            mode: brain_protocol::envelope::request::ForgetMode::Soft,
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
                RequestBody::Encode(encode_req_with_dedup([10; 16], "evict me", 1)),
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
        let rtxn = fix.ctx.executor.metadata.read_txn().expect("read_txn");
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
    let rtxn = fix.ctx.executor.metadata.read_txn().expect("read_txn");
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
fn same_text_second_encode_leaves_first_text_row_intact() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let original_text = "dedup-original-text";

        let first = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([0x70; 16], original_text, 9)),
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

        // Same text + context under a different request_id: the write path
        // stores it faithfully as a second, independent memory — the first
        // memory's text row is never mutated or collapsed.
        let second = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(encode_req_with_dedup([0x71; 16], original_text, 9)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(!second.was_deduplicated);
        assert_ne!(first.memory_id, second.memory_id);
        assert_eq!(
            read_text(&fix, first.memory_id),
            Some(original_text.as_bytes().to_vec()),
            "the first text row is untouched by the second encode",
        );
        assert_eq!(
            read_text(&fix, second.memory_id),
            Some(original_text.as_bytes().to_vec()),
            "the second encode stores its own faithful text row",
        );
    })
}

#[test]
fn encode_fresh_then_replay_returns_was_deduplicated_false() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let req = encode_req([0x82; 16], "fresh-then-replay");

        let first = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(req.clone()),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(!first.was_deduplicated);

        let second = unwrap_encode_resp(
            dispatch(
                RequestBody::Encode(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(first.memory_id, second.memory_id);
        assert!(
            !second.was_deduplicated,
            "fresh-write replay must surface was_deduplicated=false",
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
