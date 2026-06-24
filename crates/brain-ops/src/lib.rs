//! # brain-ops
//!
//! The five cognitive primitives plus LINK / UNLINK and transactions.
//! Wires together the planner, storage, metadata, embedder, and
//! index. Idempotency lives at this layer.
//!
//! ## Surface
//!
//! - [`OpsContext`] — handle bag (currently a thin wrapper over
//!   `brain_planner::ExecutorContext`; later work adds fields).
//! - [`OpError`] + [`ErrorCode`] error taxonomy
//!   with `error_code()` + `retryable()` mappings.
//! - [`dispatch()`] — top-level async entry; exhaustive `match` over
//!   `RequestBody`.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod apply;
pub mod clock;
pub mod context;
pub mod dispatch;
pub mod error;
pub mod grounded;
pub mod handlers;
pub mod index;
pub mod metrics;
pub mod state;
#[doc(hidden)]
pub mod test_support;
pub mod write;
pub mod writer;

// Module-level re-exports so external callers (brain-server, brain-planner)
// can write `brain_ops::encode::*` rather than `brain_ops::handlers::encode::*`.
pub use handlers::{
    encode, encode_vector_direct, entity, extractor_admin, forget, link, plan, query, reason,
    recall, relation, schema, statement, subscribe, txn,
};

pub use brain_planner::PlannerContext;
pub use context::{CrossEncoderSlot, OpsContext};
pub use dispatch::{dispatch, DispatchOutcome, RequestCaller};
pub use error::{ErrorCode, OpError};
pub use handlers::subscribe::{
    parse_filter, EventBus, EventEnvelope, LsnAllocator, ParsedFilter, SubscriptionHandle,
    SubscriptionRegistry, DEFAULT_EVENT_CHANNEL_CAPACITY,
};
pub use handlers::txn::{TxnId, TxnState, TxnStore};
pub use metrics::{
    AmbiguityResolverMetrics, AmbiguityResolverMetricsSnapshot, ApplyErrorSnapshot,
    AutoEdgeMetrics, AutoEdgeMetricsSnapshot, CausalEdgeMetrics, CausalEdgeMetricsSnapshot,
    CausalSkipReason, ConfidenceSweepMetrics, ConfidenceSweepMetricsSnapshot, ExtractorItemKind,
    ExtractorMetrics, ExtractorMetricsSnapshot, ForgetCascadeMetrics, ForgetCascadeMetricsSnapshot,
    IdempotencyOutcome, LlmCacheMetrics, LlmCacheMetricsSnapshot, LlmCacheModelCounts,
    LlmCacheSweepMetrics, LlmCacheSweepMetricsSnapshot, PerPhaseSnapshot, ResolverOutcome,
    SchemaMigrationMetrics, SchemaMigrationMetricsSnapshot, StatementEmbedMetrics,
    StatementEmbedMetricsSnapshot, SubmitOutcome, TemporalEdgeMetrics, TemporalEdgeMetricsSnapshot,
    TemporalSkipReason, TierKind, TierStatus, WorkerBucketSnapshot, WorkerHistogram,
    WorkerHistogramSnapshot, WriterMetrics, WriterMetricsSnapshot, ITEM_KIND_LABELS,
    RESOLVER_OUTCOME_LABELS, TIER_LABELS, TIER_STATUS_LABELS,
};
pub use state::access_buffer::{AccessBuffer, DEFAULT_ACCESS_BUFFER_CAPACITY};
pub use write::{
    AllocatedId, EvidenceRefPhase, IdKind, Phase, PhaseAck, SupersedeReplacement,
    SupersedeReplacementId, SupersedeTarget, TombstoneTarget, Write, WriteAck, WriteId,
};
pub use writer::{
    AutoEdgeEnqueue, CausalEdgeEnqueue, ExtractorEnqueue, ForgetCascadeJob, ForgetCascadeKind,
    ForgetCascadeMode, RealWriterHandle, SchemaFlagSweepJob, TemporalEdgeEnqueue,
};

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
            (OpError::TxnExpired, ErrorCode::TxnExpired),
            (OpError::TxnNotFound, ErrorCode::TxnNotFound),
            (
                OpError::PredicateNotInSchema {
                    predicate: "acme:x".into(),
                    namespace: "acme".into(),
                    version: 1,
                },
                ErrorCode::PredicateNotInSchema,
            ),
            (
                OpError::RelationTypeNotInSchema {
                    type_name: "acme:knows".into(),
                    namespace: "acme".into(),
                    version: 1,
                },
                ErrorCode::RelationTypeNotInSchema,
            ),
            (
                OpError::CardinalityViolation {
                    relation_type: "acme:knows".into(),
                    kind: "OneToOne",
                    existing: 2,
                    limit: 1,
                },
                ErrorCode::CardinalityViolation,
            ),
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
        assert!(!OpError::TxnNotFound.retryable());
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
            ExecutorContext, SharedMetadataDb, WriterError as PlannerWriterError, WriterHandle,
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
            fn reserve_memory_id<'a>(
                &'a self,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<brain_core::MemoryId, PlannerWriterError>,
                        > + 'a,
                >,
            > {
                Box::pin(
                    async move { Err(PlannerWriterError::Internal("unused in 7.1 tests".into())) },
                )
            }
        }

        let tempdir = tempfile::tempdir().unwrap();
        let db_path = tempdir.path().join("metadata.redb");
        let metadata: SharedMetadataDb =
            Arc::new(brain_metadata::MetadataDb::open(&db_path).unwrap());
        let (shared, _writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
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
        crate::test_support::ops_context_for_tests_owning_tempdir(executor)
    }

    fn encode_req() -> brain_protocol::envelope::request::EncodeRequest {
        brain_protocol::envelope::request::EncodeRequest {
            text: "hi".into(),
            context_id: 1,
            request_id: [1; 16],
            txn_id: None,
            occurred_at_unix_nanos: None,
        }
    }

    #[test]
    fn dispatch_encode_routes_to_handler() {
        use crate::test_support::run_in_glommio;
        run_in_glommio(|| async {
            // The real ENCODE handler runs here. The unified path needs
            // a `RealWriterHandle`; the `NopWriter` fixture fails the
            // downcast with `OpError::Internal`, which is sufficient to
            // prove the dispatcher reaches `handle_encode` rather than
            // a stub. Either of those error shapes confirms routing.
            let ctx = fake_context();
            let req = brain_protocol::envelope::request::RequestBody::Encode(encode_req());
            match dispatch(req, RequestCaller::anonymous(), &ctx).await {
                Err(OpError::ExecError(_)) | Err(OpError::Internal(_)) => {}
                other => panic!("expected ExecError or Internal from NopWriter, got {other:?}"),
            }
        })
    }

    #[test]
    fn dispatch_substrate_admin_variant_rejected_as_http_only() {
        use crate::test_support::run_in_glommio;
        run_in_glommio(|| async {
            let ctx = fake_context();
            let req = brain_protocol::envelope::request::RequestBody::AdminStats(
                brain_protocol::envelope::request::AdminStatsRequest {
                    detail: brain_protocol::envelope::request::StatsDetail::Summary,
                },
            );
            // Substrate admin ops are served over the HTTP admin listener, not
            // the wire — dispatch rejects them with a clear InvalidRequest.
            match dispatch(req, RequestCaller::anonymous(), &ctx).await {
                Err(OpError::InvalidRequest(msg)) => assert!(msg.contains("admin")),
                other => panic!("expected InvalidRequest (admin is HTTP-only), got {other:?}"),
            }

            // Backfill control is part of the same HTTP-only admin plane
            // (`POST /v1/extract/backfill`); the wire opcode must reject the
            // same way — NOT dangle on `NotYetImplemented`. Pinned explicitly
            // so a future split of the shared match arm can't silently
            // regress it back to "coming later".
            let backfill = brain_protocol::envelope::request::RequestBody::AdminBackfill(
                brain_protocol::envelope::request::AdminBackfillRequest {
                    scope: brain_protocol::envelope::request::BackfillScope::All,
                    extractor_ids: vec![1],
                    dry_run: true,
                    request_id: [0u8; 16],
                },
            );
            match dispatch(backfill, RequestCaller::anonymous(), &ctx).await {
                Err(OpError::InvalidRequest(msg)) => assert!(msg.contains("admin")),
                other => {
                    panic!("expected InvalidRequest for AdminBackfill (HTTP-only), got {other:?}")
                }
            }
        })
    }
}
