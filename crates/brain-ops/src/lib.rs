//! # brain-ops
//!
//! The five cognitive primitives plus LINK / UNLINK and transactions.
//! Wires together the planner, storage, metadata, embedder, and
//! index. Idempotency lives at this layer.
//!
//! See `spec/09_cognitive_operations/` for the authoritative design.
//!
//! ## Sub-task 7.1 surface
//!
//! - [`OpsContext`] — handle bag (currently a thin wrapper over
//!   `brain_planner::ExecutorContext`; later sub-tasks add fields).
//! - [`OpError`] + [`ErrorCode`] — spec §09/01 §12 error taxonomy
//!   with `error_code()` + `retryable()` mappings.
//! - [`dispatch`] — top-level async entry; exhaustive `match` over
//!   `RequestBody`.
//!
//! Handler bodies (sub-tasks 7.3–7.10) are stubs returning
//! `OpError::NotYetImplemented`.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod access_buffer;
pub mod context;
pub mod dispatch;
pub mod error;
pub mod idempotency;
pub mod txn_lens;
pub mod writer;

// Per-op handler modules. 7.1 ships stubs; 7.3-7.10 replace each.
pub mod encode;
pub mod forget;
pub mod link;
pub mod plan;
pub mod reason;
pub mod recall;
pub mod subscribe;
pub mod txn;

pub use access_buffer::{AccessBuffer, DEFAULT_ACCESS_BUFFER_CAPACITY};
pub use brain_planner::PlannerContext;
pub use context::OpsContext;
pub use dispatch::dispatch;
pub use error::{ErrorCode, OpError};
pub use subscribe::{
    EventBus, EventEnvelope, LsnAllocator, ParsedFilter, SubscriptionHandle, SubscriptionRegistry,
    DEFAULT_EVENT_CHANNEL_CAPACITY,
};
pub use txn::{TxnState, TxnStore};
pub use writer::RealWriterHandle;

#[cfg(test)]
mod tests {
    use super::*;
    use brain_planner::{ExecError, PlanError, WriterError};

    // -----------------------------------------------------------------
    // OpError + ErrorCode mapping.
    // -----------------------------------------------------------------

    #[test]
    fn error_code_maps_each_variant() {
        let cases: Vec<(OpError, ErrorCode)> = vec![
            (
                OpError::InvalidRequest("bad".into()),
                ErrorCode::InvalidRequest,
            ),
            (
                OpError::NotFound {
                    what: "memory",
                    detail: "nope".into(),
                },
                ErrorCode::NotFound,
            ),
            (OpError::Conflict("dup".into()), ErrorCode::Conflict),
            (
                OpError::QuotaExceeded("limit".into()),
                ErrorCode::QuotaExceeded,
            ),
            (
                OpError::Unauthorized("nope".into()),
                ErrorCode::Unauthorized,
            ),
            (OpError::Overloaded("busy".into()), ErrorCode::Overloaded),
            (OpError::TooManyMemories, ErrorCode::InvalidRequest),
            (OpError::TxnExpired, ErrorCode::Conflict),
            (
                OpError::NotYetImplemented("anything"),
                ErrorCode::InternalError,
            ),
            (OpError::Internal("oops".into()), ErrorCode::InternalError),
            (
                OpError::PlanError(PlanError::QueryTooExpensive {
                    estimated_ms: 2000.0,
                    budget_ms: 1000.0,
                }),
                ErrorCode::InvalidRequest,
            ),
            (
                OpError::PlanError(PlanError::Unsupported("xshard")),
                ErrorCode::InternalError,
            ),
            (
                OpError::ExecError(ExecError::WriterFailed(WriterError::Overloaded)),
                ErrorCode::Overloaded,
            ),
            (
                OpError::ExecError(ExecError::MemoryNotFound {
                    memory_id: brain_core::MemoryId::from(7u128),
                }),
                ErrorCode::NotFound,
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.error_code(), expected, "for {err:?}");
        }
    }

    #[test]
    fn retryable_only_for_overloaded() {
        assert!(OpError::Overloaded("busy".into()).retryable());
        assert!(OpError::ExecError(ExecError::WriterFailed(WriterError::Overloaded)).retryable());

        assert!(!OpError::InvalidRequest("bad".into()).retryable());
        assert!(!OpError::Internal("oops".into()).retryable());
        assert!(!OpError::NotYetImplemented("X").retryable());
        assert!(!OpError::TxnExpired.retryable());
    }

    #[test]
    fn op_error_displays_readably() {
        let e = OpError::Conflict("request_id replay with different params".into());
        let s = format!("{e}");
        assert!(s.contains("conflict"));
        assert!(s.contains("request_id replay"));
    }

    // -----------------------------------------------------------------
    // Dispatcher behaviour (with stub handlers).
    // -----------------------------------------------------------------

    /// Build a minimal `OpsContext` for dispatcher tests. We don't
    /// actually need a live executor — every handler returns
    /// `NotYetImplemented` before touching `ctx.executor`.
    fn fake_context() -> OpsContext {
        use std::sync::Arc;

        use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
        use brain_index::{IndexParams, SharedHnsw};
        use brain_planner::{
            EncodeAck, EncodeOp, ExecutorContext, ForgetAck, ForgetOp, SharedMetadataDb,
            WriterError as PlannerWriterError, WriterHandle,
        };

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

        struct NopWriter;
        impl WriterHandle for NopWriter {
            fn submit_encode<'a>(
                &'a self,
                _op: EncodeOp,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<EncodeAck, PlannerWriterError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(
                    async move { Err(PlannerWriterError::Internal("unused in 7.1 tests".into())) },
                )
            }
            fn submit_forget<'a>(
                &'a self,
                _op: ForgetOp,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<ForgetAck, PlannerWriterError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(
                    async move { Err(PlannerWriterError::Internal("unused in 7.1 tests".into())) },
                )
            }
            fn submit_link<'a>(
                &'a self,
                _: brain_planner::LinkOp,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<brain_planner::LinkAck, PlannerWriterError>,
                        > + Send
                        + 'a,
                >,
            > {
                Box::pin(
                    async move { Err(PlannerWriterError::Internal("unused in 7.1 tests".into())) },
                )
            }
            fn submit_unlink<'a>(
                &'a self,
                _: brain_planner::UnlinkOp,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<brain_planner::UnlinkAck, PlannerWriterError>,
                        > + Send
                        + 'a,
                >,
            > {
                Box::pin(
                    async move { Err(PlannerWriterError::Internal("unused in 7.1 tests".into())) },
                )
            }
            fn reserve_memory_id<'a>(
                &'a self,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<brain_core::MemoryId, PlannerWriterError>,
                        > + Send
                        + 'a,
                >,
            > {
                Box::pin(
                    async move { Err(PlannerWriterError::Internal("unused in 7.1 tests".into())) },
                )
            }
            fn submit_batch<'a>(
                &'a self,
                _: brain_planner::TxnBatch,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<brain_planner::TxnBatchAck, PlannerWriterError>,
                        > + Send
                        + 'a,
                >,
            > {
                Box::pin(
                    async move { Err(PlannerWriterError::Internal("unused in 7.1 tests".into())) },
                )
            }
        }

        let tempdir = tempfile::tempdir().unwrap();
        let db_path = tempdir.path().join("metadata.redb");
        let metadata: SharedMetadataDb = Arc::new(parking_lot::Mutex::new(
            brain_metadata::MetadataDb::open(&db_path).unwrap(),
        ));
        let (shared, _writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
        let executor = ExecutorContext::new(
            Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
            shared,
            metadata,
            Arc::new(NopWriter) as Arc<dyn WriterHandle>,
        );
        // Leak the tempdir so the DB file stays alive for the test
        // duration. The OnceLock pattern would be cleaner, but we
        // construct a fresh context per test — leaking the dir for a
        // few µs is fine.
        std::mem::forget(tempdir);
        OpsContext::new(executor)
    }

    fn encode_req() -> brain_protocol::request::EncodeRequest {
        brain_protocol::request::EncodeRequest {
            text: "hi".into(),
            context_id: 1,
            kind: brain_protocol::request::MemoryKindWire::Episodic,
            salience_hint: 0.5,
            edges: vec![],
            request_id: [1; 16],
            txn_id: None,
            deduplicate: false,
        }
    }

    #[tokio::test]
    async fn dispatch_encode_routes_to_handler() {
        // 7.3 wired the real ENCODE handler. The `NopWriter` returns
        // `WriterError::Internal`, so the handler propagates an
        // `ExecError::WriterFailed` — which is enough to prove the
        // dispatcher reaches `handle_encode` rather than the stub.
        let ctx = fake_context();
        let req = brain_protocol::request::RequestBody::Encode(encode_req());
        match dispatch(req, &ctx).await {
            Err(OpError::ExecError(_)) => {}
            other => panic!("expected ExecError from NopWriter, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_admin_variant_returns_not_yet_implemented() {
        let ctx = fake_context();
        let req = brain_protocol::request::RequestBody::AdminStats(
            brain_protocol::request::AdminStatsRequest {
                detail: brain_protocol::request::StatsDetail::Summary,
            },
        );
        match dispatch(req, &ctx).await {
            Err(OpError::NotYetImplemented(msg)) => assert!(msg.contains("admin")),
            other => panic!("expected NotYetImplemented, got {other:?}"),
        }
    }

    #[test]
    fn ops_context_is_send_sync() {
        fn require<T: Send + Sync>() {}
        require::<OpsContext>();
    }
}
