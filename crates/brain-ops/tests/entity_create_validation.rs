//! Regression: an entity type id no registry row answers to is a bad
//! argument, not an engine fault.
//!
//! The write path has always refused it, but only inside the writer, where
//! `entity_put` fails and the rejection is wrapped as a writer-internal
//! error. That category maps to `502 engine.error` at the HTTP edge, so a
//! caller who passed a wrong number was told the memory engine had broken.
//!
//! The handler now answers the question before submitting, on a read, which
//! is where every other argument on this request is already checked.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::test_support::run_in_glommio;
use brain_ops::{dispatch, ErrorCode, OpError, OpsContext, RealWriterHandle, RequestCaller};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::EntityCreateRequest;

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

fn build_ctx() -> OpsContext {
    let tempdir = tempfile::tempdir().unwrap();
    let metadata: SharedMetadataDb =
        Arc::new(MetadataDb::open(tempdir.path().join("metadata.redb")).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let embedder: Arc<dyn Dispatcher> = Arc::new(MockDispatcher);
    let executor =
        ExecutorContext::new(embedder, shared, metadata, writer as Arc<dyn WriterHandle>);
    let ctx = brain_ops::test_support::ops_context_for_tests(executor, tempdir.path());
    std::mem::forget(tempdir);
    ctx
}

fn caller() -> RequestCaller {
    RequestCaller::from_scope(
        brain_core::SpaceId(uuid::Uuid::from_bytes([0xC1; 16])),
        [0u8; 16],
        [0u8; 16],
        "acme".to_string(),
        brain_metadata::api_keys::bits::FULL,
    )
}

fn create_request(entity_type_id: u32) -> EntityCreateRequest {
    EntityCreateRequest {
        entity_type_id,
        canonical_name: "Ghost Type Probe".to_string(),
        aliases: Vec::new(),
        attributes_blob: Vec::new(),
        session_id: 0,
        request_id: [0x5A; 16],
        act_as: None,
    }
}

#[test]
fn unknown_entity_type_is_an_invalid_argument_not_an_engine_error() {
    run_in_glommio(|| async {
        let ctx = build_ctx();
        let err = dispatch(
            RequestBody::EntityCreate(create_request(99_999)),
            caller(),
            &ctx,
        )
        .await
        .expect_err("an unregistered entity type must be refused");

        assert!(
            matches!(err, OpError::InvalidRequest(_)),
            "expected InvalidRequest, got {err:?}"
        );
        assert_eq!(
            err.error_code(),
            ErrorCode::InvalidRequest,
            "the wire code decides the HTTP status; anything else reaches the caller as a 5xx"
        );
        assert!(
            err.to_string().contains("99999"),
            "the message must name the id the caller sent: {err}"
        );
    });
}

/// The seeded `Person` type is id 1, so the same path accepts a real one.
#[test]
fn a_registered_entity_type_is_accepted() {
    run_in_glommio(|| async {
        let ctx = build_ctx();
        dispatch(RequestBody::EntityCreate(create_request(1)), caller(), &ctx)
            .await
            .expect("the seeded Person type must still create");
    });
}
