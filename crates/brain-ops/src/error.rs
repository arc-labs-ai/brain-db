//! error taxonomy.
//!
//! `OpError` is brain-ops's runtime error type. Each variant maps to
//! a stable wire `ErrorCode` (`error_code()`) and carries a
//! `retryable` flag (`retryable()`) — both surfaced to clients per
//!
//! `#[from]` conversions wrap `brain_planner::PlanError` and
//! `brain_planner::ExecError` so handlers can `?` upstream errors
//! through without manual mapping. The `error_code()` mapping
//! collapses the inner variants to the right wire code.

use thiserror::Error;

use brain_planner::{ExecError, PlanError, WriterError};

#[derive(Debug, Error)]
pub enum OpError {
    /// — malformed or invalid request.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// — referenced entity does not exist.
    #[error("{what} not found: {detail}")]
    NotFound { what: &'static str, detail: String },

    /// — idempotency mismatch on duplicate
    /// `request_id`: "same RequestId returns same
    /// response within 24h; different params → Conflict".
    #[error("idempotency conflict: {0}")]
    Conflict(String),

    /// — agent limits exceeded.
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),

    /// — credentials don't allow this operation.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// — substrate is shedding load. Retryable.
    #[error("overloaded: {0}")]
    Overloaded(String),

    /// — single FORGET targets > 100 000 memories.
    #[error("too many memories targeted by one request")]
    TooManyMemories,

    /// Transaction buffer would exceed the per-transaction op cap.
    /// The cap is fixed at 1000 buffered ops (ENCODE +
    /// FORGET + LINK + UNLINK). Surfaced at append-time so an agent
    /// learns immediately when the 1001st op is buffered, and again at
    /// commit-time as defense-in-depth. The client should split the
    /// work into multiple transactions.
    #[error("transaction too large: {ops} ops exceeds cap of {cap}")]
    TransactionTooLarge { ops: u32, cap: u32 },

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

    /// `SCHEMA_UPLOAD` carried a declaration that conflicts with an
    /// already-active row for the same name in the same namespace —
    /// e.g. a `predicate` whose `kind` constraint differs from the
    /// existing row, or a `relation_type` whose cardinality changed.
    /// `kind` names the schema item kind (`"entity_type"`,
    /// `"predicate"`, `"relation_type"`, `"extractor"`); `conflict`
    /// is a human-readable summary of which fields diverged. The
    /// whole upload is aborted — no half-merged state lands.
    ///
    /// Maps to wire `InvalidRequest`: existing wire codes don't have
    /// a precise slot for "schema merge would conflict," and adding
    /// new codes is out of scope. Clients distinguish this from a
    /// parse / validate failure by inspecting the error message.
    #[error("schema conflict: {kind} {name:?} in namespace {namespace:?}: {conflict}")]
    SchemaConflict {
        kind: &'static str,
        name: String,
        namespace: String,
        conflict: String,
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

    /// Placeholder for stub handlers: while a handler is in flight,
    /// the dispatcher returns this for ops not yet implemented.
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

    /// Diagnostic-only: a hybrid retriever degraded after the shard
    /// spawned (tantivy segment corruption, an HNSW reader going
    /// stale, etc.). Surfaced only by admin / health surfaces
    /// (`/health`, `ADMIN_STATUS`) so operators learn about the
    /// degradation; never returned from `handle_recall` in v1,
    /// because RECALL is a single verb whose path the server picks
    /// and whose required sinks shard spawn guarantees.
    #[error("hybrid retrieval unavailable on this shard: {0}")]
    HybridUnavailable(String),

    /// Client requested a capability the operator explicitly turned
    /// off in config (`rerank.enabled = false`, an extractor tier
    /// disabled, etc.). Distinct from `HybridUnavailable`: that one is
    /// a *runtime degradation* of a required capability; this one is a
    /// *deployment choice*. The client can either drop the opt-in flag
    /// (e.g. set `rerank = false` on the recall request) or talk to a
    /// shard where the capability is enabled.
    #[error("capability \"{capability}\" is not enabled on this shard")]
    CapabilityNotEnabled { capability: &'static str },

    /// Catch-all for internal bookkeeping: maps to
    /// wire `InternalError`. Not retryable.
    #[error("internal error: {0}")]
    Internal(String),
}

/// — stable wire error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidRequest,
    NotFound,
    QuotaExceeded,
    Unauthorized,
    Conflict,
    /// Txn was real at some point but is no longer Active (timed out,
    /// committed, or aborted). Split from `Conflict` so the
    /// dispatcher maps it to the right wire code and the client can
    /// detect it programmatically.
    TxnExpired,
    /// Txn id never existed on this server.
    TxnNotFound,
    /// Buffered transaction would exceed the per-transaction op cap
    /// (1000 ops). Distinct from `Conflict` so the
    /// client can report a domain-specific recovery hint ("split into
    /// multiple transactions").
    TransactionTooLarge,
    /// Schema-strict mode rejected the request because the predicate
    /// qname isn't in the active schema's vocabulary.
    PredicateNotInSchema,
    /// Schema-strict mode rejected the request because the relation
    /// type qname isn't in the active schema's vocabulary.
    RelationTypeNotInSchema,
    /// Schema-declared cardinality constraint would be violated.
    /// Distinct from generic `Conflict` so clients can recognise
    /// the constraint failure and surface a domain-specific message.
    CardinalityViolation,
    Overloaded,
    /// Hybrid retrieval is unavailable on this shard. Wire code
    /// reserved for admin / health diagnostics only; a normal
    /// client RECALL never sees this — the server picks the path
    /// and shard spawn guarantees the required sinks are wired.
    HybridUnavailable,
    InternalError,
}

impl OpError {
    /// Map this error to its wire `ErrorCode`.
    #[must_use]
    pub fn error_code(&self) -> ErrorCode {
        match self {
            Self::InvalidRequest(_) | Self::TooManyMemories | Self::SchemaConflict { .. } => {
                ErrorCode::InvalidRequest
            }
            Self::NotFound { .. } => ErrorCode::NotFound,
            Self::Conflict(_) => ErrorCode::Conflict,
            Self::TxnExpired => ErrorCode::TxnExpired,
            Self::TxnNotFound => ErrorCode::TxnNotFound,
            Self::TransactionTooLarge { .. } => ErrorCode::TransactionTooLarge,
            Self::PredicateNotInSchema { .. } => ErrorCode::PredicateNotInSchema,
            Self::RelationTypeNotInSchema { .. } => ErrorCode::RelationTypeNotInSchema,
            Self::CardinalityViolation { .. } => ErrorCode::CardinalityViolation,
            Self::QuotaExceeded(_) => ErrorCode::QuotaExceeded,
            Self::Unauthorized(_) => ErrorCode::Unauthorized,
            Self::Overloaded(_) => ErrorCode::Overloaded,
            Self::HybridUnavailable(_) => ErrorCode::HybridUnavailable,
            // Operator opted out of this capability — surfaces as an
            // invalid request because the client can fix it without
            // server-side intervention by dropping the opt-in flag.
            Self::CapabilityNotEnabled { .. } => ErrorCode::InvalidRequest,
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

    /// clients see a `retryable` flag. Only
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
