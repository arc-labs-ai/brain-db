#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! Dispatch-level tests for the `ADMIN_BACKFILL` / `ADMIN_BACKFILL_CANCEL`
//! wire ops.
//!
//! These pin the wiring the resumable-backfill change added: the dispatch
//! arms convert the wire request, drive the `BackfillControl` handle on the
//! executor context, and build the wire progress response — and, when no
//! handle is provisioned, return a clean structured error instead of
//! panicking.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use brain_core::{BackfillId, BackfillProgress as CoreProgress, BackfillRequest, MemoryId};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::error::OpError;
use brain_ops::test_support::run_in_glommio;
use brain_ops::{dispatch, DispatchOutcome, OpsContext, RealWriterHandle};
use brain_planner::{BackfillControl, ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{
    AdminBackfillCancelRequest, AdminBackfillRequest, BackfillScope, RequestBody,
};
use brain_protocol::envelope::response::ResponseBody;

struct NopDispatcher;
impl Dispatcher for NopDispatcher {
    fn embed(&self, _: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok([0.0; VECTOR_DIM])
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0; 16]
    }
}

/// Records the calls the dispatch path makes so the test can assert the
/// handle was actually driven.
#[derive(Default)]
struct MockBackfill {
    submits: AtomicU64,
    cancels: AtomicU64,
    last_cancel_matched: AtomicBool,
}

impl BackfillControl for MockBackfill {
    fn submit(&self, request: BackfillRequest) -> BackfillId {
        self.submits.fetch_add(1, Ordering::Relaxed);
        // Mirror the real worker: submit returns the request's id (the
        // run handle / idempotency key).
        request.request_id
    }
    fn cancel(&self, _request_id: BackfillId) -> bool {
        self.cancels.fetch_add(1, Ordering::Relaxed);
        self.last_cancel_matched.store(true, Ordering::Relaxed);
        true
    }
    fn progress(&self) -> CoreProgress {
        CoreProgress {
            request_id: None,
            completed: 7,
            failed: 1,
            skipped_already_completed: 2,
            last_processed_memory_id: Some(MemoryId::from_raw(99)),
            running: true,
            eta: None,
        }
    }
}

fn build_ctx(handle: Option<Arc<dyn BackfillControl>>) -> (OpsContext, tempfile::TempDir) {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let mut executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );
    if let Some(h) = handle {
        executor = executor.with_backfill_handle(h);
    }
    let ctx = brain_ops::test_support::ops_context_for_tests(executor, tempdir.path());
    (ctx, tempdir)
}

fn single(outcome: DispatchOutcome) -> ResponseBody {
    match outcome {
        DispatchOutcome::Single(b) => b,
        DispatchOutcome::Stream(_) => panic!("expected Single"),
    }
}

#[test]
fn admin_backfill_submits_and_returns_progress() {
    run_in_glommio(|| async {
        let handle = Arc::new(MockBackfill::default());
        let (ctx, _dir) = build_ctx(Some(handle.clone() as Arc<dyn BackfillControl>));

        let req = AdminBackfillRequest {
            scope: BackfillScope::All,
            extractor_ids: vec![1, 2],
            dry_run: false,
            request_id: [7u8; 16],
        };
        let out = dispatch(
            RequestBody::AdminBackfill(req),
            brain_ops::RequestCaller::for_tests(),
            &ctx,
        )
        .await
        .expect("admin backfill dispatch");

        match single(out) {
            ResponseBody::AdminBackfill(resp) => {
                // The worker request id is derived from the wire request id.
                assert_eq!(
                    resp.backfill_id,
                    BackfillId::from_bytes([7u8; 16]).to_bytes()
                );
                assert_eq!(resp.progress.completed, 7);
                assert_eq!(resp.progress.failed, 1);
                assert_eq!(resp.progress.skipped_already_completed, 2);
                assert!(resp.progress.running);
                assert!(resp.progress.last_processed_memory_id_present);
                assert_eq!(resp.progress.last_processed_memory_id, 99);
            }
            other => panic!("expected AdminBackfill, got {other:?}"),
        }
        assert_eq!(
            handle.submits.load(Ordering::Relaxed),
            1,
            "submit driven once"
        );
    });
}

#[test]
fn admin_backfill_cancel_drives_handle() {
    run_in_glommio(|| async {
        let handle = Arc::new(MockBackfill::default());
        let (ctx, _dir) = build_ctx(Some(handle.clone() as Arc<dyn BackfillControl>));

        let req = AdminBackfillCancelRequest {
            backfill_id: [3u8; 16],
            request_id: [4u8; 16],
        };
        let out = dispatch(
            RequestBody::AdminBackfillCancel(req),
            brain_ops::RequestCaller::for_tests(),
            &ctx,
        )
        .await
        .expect("admin backfill cancel dispatch");

        match single(out) {
            ResponseBody::AdminBackfillCancel(resp) => {
                assert_eq!(resp.backfill_id, [3u8; 16]);
                assert!(resp.cancelled, "handle returned cancelled = true");
            }
            other => panic!("expected AdminBackfillCancel, got {other:?}"),
        }
        assert_eq!(
            handle.cancels.load(Ordering::Relaxed),
            1,
            "cancel driven once"
        );
    });
}

#[test]
fn admin_backfill_invalid_extractor_count_rejected() {
    run_in_glommio(|| async {
        let handle = Arc::new(MockBackfill::default());
        let (ctx, _dir) = build_ctx(Some(handle.clone() as Arc<dyn BackfillControl>));

        // Empty extractor list fails conversion -> InvalidRequest, and the
        // handle is never driven.
        let req = AdminBackfillRequest {
            scope: BackfillScope::All,
            extractor_ids: vec![],
            dry_run: false,
            request_id: [1u8; 16],
        };
        let err = dispatch(
            RequestBody::AdminBackfill(req),
            brain_ops::RequestCaller::for_tests(),
            &ctx,
        )
        .await
        .expect_err("empty extractor list must be rejected");
        assert!(matches!(err, OpError::InvalidRequest(_)), "got {err:?}");
        assert_eq!(
            handle.submits.load(Ordering::Relaxed),
            0,
            "handle not driven"
        );
    });
}

#[test]
fn admin_backfill_not_provisioned_is_clean_error() {
    run_in_glommio(|| async {
        // No handle threaded onto the context.
        let (ctx, _dir) = build_ctx(None);

        let req = AdminBackfillRequest {
            scope: BackfillScope::All,
            extractor_ids: vec![1],
            dry_run: false,
            request_id: [9u8; 16],
        };
        let err = dispatch(
            RequestBody::AdminBackfill(req),
            brain_ops::RequestCaller::for_tests(),
            &ctx,
        )
        .await
        .expect_err("not-provisioned backfill must error, not panic");
        // Deployment/wiring gap -> Internal (500), with a descriptive message.
        match err {
            OpError::Internal(msg) => assert!(msg.contains("not provisioned"), "msg: {msg}"),
            other => panic!("expected Internal, got {other:?}"),
        }
    });
}
