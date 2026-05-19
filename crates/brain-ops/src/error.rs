//! Spec §09/01 §12 error taxonomy.
//!
//! `OpError` is brain-ops's runtime error type. Each variant maps to
//! a stable wire `ErrorCode` (`error_code()`) and carries a
//! `retryable` flag (`retryable()`) — both surfaced to clients per
//! spec §12.
//!
//! `#[from]` conversions wrap `brain_planner::PlanError` and
//! `brain_planner::ExecError` so handlers can `?` upstream errors
//! through without manual mapping. The `error_code()` mapping
//! collapses the inner variants to the right wire code.

use thiserror::Error;

use brain_planner::{ExecError, PlanError, WriterError};

#[derive(Debug, Error)]
pub enum OpError {
    /// Spec §09/01 §12 — malformed or invalid request.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Spec §09/01 §12 — referenced entity does not exist.
    #[error("{what} not found: {detail}")]
    NotFound { what: &'static str, detail: String },

    /// Spec §09/01 §12 — idempotency mismatch on duplicate
    /// `request_id`. Spec §09/02 §4: "same RequestId returns same
    /// response within 24h; different params → Conflict".
    #[error("idempotency conflict: {0}")]
    Conflict(String),

    /// Spec §09/01 §12 — agent limits exceeded.
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),

    /// Spec §09/01 §12 — credentials don't allow this operation.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// Spec §09/01 §12 — substrate is shedding load. Retryable.
    #[error("overloaded: {0}")]
    Overloaded(String),

    /// Spec §08/06 §6 — single FORGET targets > 100 000 memories.
    #[error("too many memories targeted by one request")]
    TooManyMemories,

    /// Schema-strict mode: STATEMENT_CREATE / QUERY referenced a
    /// predicate qname that the active schema version doesn't
    /// declare. Schemaless deployments never raise this — unknown
    /// qnames are interned on first use. Maps to wire
    /// `PredicateNotInSchema`.
    #[error(
        "predicate {predicate:?} is not declared in schema namespace {namespace:?} v{version}"
    )]
    PredicateNotInSchema {
        predicate: String,
        namespace: String,
        version: u32,
    },

    /// Schema-strict mode: RELATION_CREATE referenced a relation type
    /// qname that the active schema version doesn't declare. Maps to
    /// wire `RelationTypeNotInSchema`.
    #[error(
        "relation type {type_name:?} is not declared in schema namespace {namespace:?} v{version}"
    )]
    RelationTypeNotInSchema {
        type_name: String,
        namespace: String,
        version: u32,
    },

    /// Schema-strict mode: RELATION_CREATE would have exceeded the
    /// declared cardinality (OneToOne / OneToMany / ManyToOne).
    /// Maps to wire `CardinalityViolation`. Implicit-from-write
    /// relation types behave as ManyToMany and never trigger this.
    #[error(
        "cardinality {kind} on relation_type {relation_type:?} violated: {existing} existing current row(s) exceed limit {limit}"
    )]
    CardinalityViolation {
        relation_type: String,
        kind: &'static str,
        existing: u32,
        limit: u32,
    },

    /// Transaction was Active and either ran past its deadline (the
    /// sweeper marked it Expired), or has already moved past Active
    /// (Committed / Aborted). Distinct from `TxnNotFound` — the id
    /// was real at some point.
    #[error("transaction expired")]
    TxnExpired,

    /// The supplied transaction id has never existed on this server.
    /// Distinct from `TxnExpired` so clients can tell a typo from a
    /// timed-out txn and recover accordingly.
    #[error("transaction not found")]
    TxnNotFound,

    /// Sub-task placeholder. 7.3–7.10 replace each stub handler;
    /// while in flight, the dispatcher returns this for handlers
    /// not yet implemented.
    #[error("not yet implemented: {0}")]
    NotYetImplemented(&'static str),

    /// Planner-side failure (plan validation, query-too-expensive,
    /// unsupported request shape). `error_code()` maps each inner
    /// variant to the right wire code.
    #[error(transparent)]
    PlanError(#[from] PlanError),

    /// Executor-side failure (embed, index, metadata read, missing
    /// memory, writer error). `error_code()` collapses.
    #[error(transparent)]
    ExecError(#[from] ExecError),

    /// Caller requested `RecallStrategy::HybridOnly` but the shard
    /// is missing a retriever slot (semantic / lexical) needed for
    /// the hybrid path. We surface this loud instead of silently
    /// falling back so latency-critical clients that depend on
    /// hybrid signal-fusion notice the degradation immediately.
    #[error("hybrid retrieval unavailable on this shard: {0}")]
    HybridUnavailable(String),

    /// Catch-all for internal bookkeeping. Spec §09/01 §12: maps to
    /// wire `InternalError`. Not retryable.
    #[error("internal error: {0}")]
    Internal(String),
}

/// Spec §09/01 §12 — stable wire error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidRequest,
    NotFound,
    QuotaExceeded,
    Unauthorized,
    Conflict,
    /// Txn was real at some point but is no longer Active (timed out,
    /// committed, or aborted). Split from `Conflict` so the
    /// dispatcher maps it to the right wire code and the SDK can
    /// detect it programmatically.
    TxnExpired,
    /// Txn id never existed on this server.
    TxnNotFound,
    /// Schema-strict mode rejected the request because the predicate
    /// qname isn't in the active schema's vocabulary.
    PredicateNotInSchema,
    /// Schema-strict mode rejected the request because the relation
    /// type qname isn't in the active schema's vocabulary.
    RelationTypeNotInSchema,
    /// Schema-declared cardinality constraint would be violated.
    /// Distinct from generic `Conflict` so SDK clients can recognise
    /// the constraint failure and surface a domain-specific message.
    CardinalityViolation,
    Overloaded,
    /// Hybrid retrieval is unavailable on this shard. Returned
    /// only when the client explicitly requested
    /// `RecallStrategy::HybridOnly`; auto-strategy callers fall
    /// back transparently and never see this code.
    HybridUnavailable,
    InternalError,
}

impl OpError {
    /// Map this error to its spec §12 wire `ErrorCode`.
    #[must_use]
    pub fn error_code(&self) -> ErrorCode {
        match self {
            Self::InvalidRequest(_) | Self::TooManyMemories => ErrorCode::InvalidRequest,
            Self::NotFound { .. } => ErrorCode::NotFound,
            Self::Conflict(_) => ErrorCode::Conflict,
            Self::TxnExpired => ErrorCode::TxnExpired,
            Self::TxnNotFound => ErrorCode::TxnNotFound,
            Self::PredicateNotInSchema { .. } => ErrorCode::PredicateNotInSchema,
            Self::RelationTypeNotInSchema { .. } => ErrorCode::RelationTypeNotInSchema,
            Self::CardinalityViolation { .. } => ErrorCode::CardinalityViolation,
            Self::QuotaExceeded(_) => ErrorCode::QuotaExceeded,
            Self::Unauthorized(_) => ErrorCode::Unauthorized,
            Self::Overloaded(_) => ErrorCode::Overloaded,
            Self::HybridUnavailable(_) => ErrorCode::HybridUnavailable,
            Self::NotYetImplemented(_) | Self::Internal(_) => ErrorCode::InternalError,
            Self::PlanError(p) => match p {
                PlanError::QueryTooExpensive { .. } | PlanError::InvalidParameters { .. } => {
                    ErrorCode::InvalidRequest
                }
                PlanError::Unsupported(_) => ErrorCode::InternalError,
            },
            Self::ExecError(e) => match e {
                ExecError::EmbedFailed(_)
                | ExecError::IndexSearchFailed(_)
                | ExecError::MetadataReadFailed(_)
                | ExecError::Unsupported(_)
                | ExecError::Internal(_) => ErrorCode::InternalError,
                ExecError::MemoryNotFound { .. } => ErrorCode::NotFound,
                ExecError::WriterFailed(WriterError::Overloaded) => ErrorCode::Overloaded,
                ExecError::WriterFailed(WriterError::Conflict(_)) => ErrorCode::Conflict,
                ExecError::WriterFailed(WriterError::Internal(_)) => ErrorCode::InternalError,
            },
        }
    }

    /// Spec §09/01 §12: clients see a `retryable` flag. Only
    /// `Overloaded` (and the same condition surfacing from the
    /// writer) is retryable; everything else needs operator
    /// investigation or is a client-side bug.
    #[must_use]
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Overloaded(_) | Self::ExecError(ExecError::WriterFailed(WriterError::Overloaded))
        )
    }
}
