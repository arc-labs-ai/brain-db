//! error taxonomy.
//!
//! `OpError` is brain-ops's runtime error type. Every variant maps to a
//! stable wire `ErrorCode` through `error_code()`, and that mapping is the
//! single classification this crate owns. Retryability is not decided
//! here: the dispatcher turns the `ErrorCode` into a `brain_protocol::ErrorCode`,
//! and clients read it from that code's `ErrorCategory`.
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

    /// — an invalid request whose offending input is known by name.
    ///
    /// Carries the same client-facing sentence as `InvalidRequest` plus the
    /// parts the wire `ErrorDetails` block is made of, so a client can branch
    /// on which input it got wrong instead of parsing the message. `expected`
    /// and `actual` hold the constraint and the value that broke it when the
    /// check knows them; a check that only knows the field leaves them unset.
    #[error("invalid request: {reason}")]
    InvalidField {
        field: String,
        expected: Option<String>,
        actual: Option<String>,
        reason: String,
    },

    /// — referenced entity does not exist.
    #[error("{what} not found: {detail}")]
    NotFound { what: &'static str, detail: String },

    /// — a supersession chain does not terminate.
    ///
    /// A read that follows `superseded_by` gives up after a bounded number of
    /// hops. Only corrupt or mis-replayed state can produce a chain that long,
    /// so this is the substrate's fault, not the caller's: the walk stops and
    /// says so rather than spinning inside a read that never yields.
    #[error("{what} supersession chain does not terminate: {detail}")]
    SupersessionChainCycle { what: &'static str, detail: String },

    /// — the write's namespace has no usable LLM provider credential (BYOK).
    ///
    /// Terminal: retrying will not conjure a key. Raised synchronously on
    /// ENCODE rather than letting the write ack and derivation fail quietly,
    /// which would store the memory ungrounded with nothing telling the
    /// caller why recall degraded.
    #[error("no LLM provider credential for namespace {namespace}: {detail}")]
    LlmCredentialMissing { namespace: String, detail: String },

    /// This shard's write-ahead log is terminally broken. Terminal for the
    /// life of the process: the fault latches and nothing clears it, so the
    /// client must stop rather than retry. `detail` holds the underlying
    /// failure for the server log and stays out of the `Display` that reaches
    /// the client.
    #[error("shard storage is terminally faulted: this shard's write-ahead log can no longer be written, so every write will fail until an operator restarts the shard process; retrying will not help")]
    ShardStorageFaulted { detail: String },

    /// The request conflicts with committed state. Two causes: an
    /// idempotency mismatch on a duplicate `request_id` ("same RequestId
    /// returns same response within 24h; different params → Conflict"), and a
    /// write refused by a uniqueness invariant such as a duplicate canonical
    /// name. Neither succeeds on a retry.
    ///
    /// The display stays neutral so the second cause is not mislabelled as
    /// the first; the detail each call site passes names it.
    #[error("conflict: {0}")]
    Conflict(String),

    /// — space limits exceeded.
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),

    /// — credentials don't allow this operation.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// — maps to wire `Overloaded` (`Unavailable`), which clients retry.
    #[error("overloaded: {0}")]
    Overloaded(String),

    /// — single FORGET targets > 100 000 memories.
    #[error("too many memories targeted by one request")]
    TooManyMemories,

    /// Transaction buffer would exceed the per-transaction op cap.
    /// The cap is fixed at 1000 buffered ops (ENCODE +
    /// FORGET + LINK + UNLINK). Surfaced at append-time so an space
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

    /// Diagnostic-only: a retrieval retriever degraded after the shard
    /// spawned (tantivy segment corruption, an HNSW reader going
    /// stale, etc.). Surfaced only by admin / health surfaces
    /// (`/health`, `ADMIN_STATS`) so operators learn about the
    /// degradation; never returned from `handle_recall` in v1,
    /// because RECALL is a single verb whose path the server picks
    /// and whose required sinks shard spawn guarantees.
    #[error("retrieval unavailable on this shard: {0}")]
    RetrievalUnavailable(String),

    /// Client requested a capability the operator explicitly turned
    /// off in config (`rerank.enabled = false`, an extractor tier
    /// disabled, etc.). Distinct from `RetrievalUnavailable`: that one is
    /// a *runtime degradation* of a required capability; this one is a
    /// *deployment choice*. The client can either drop the opt-in flag
    /// (e.g. set `rerank = false` on the recall request) or talk to a
    /// shard where the capability is enabled.
    #[error("capability \"{capability}\" is not enabled on this shard")]
    CapabilityNotEnabled { capability: &'static str },

    /// Catch-all for internal bookkeeping: maps to wire `InternalError`, whose
    /// protocol category is `Internal`, so clients may retry it. A shard whose
    /// write-ahead log is terminally broken is routed to
    /// `ShardStorageFaulted`, because an operator restart is what recovers it;
    /// a broken engine invariant or unreadable stored row is routed to
    /// `PermanentFault`, because no restart touches that.
    #[error("internal error: {0}")]
    Internal(String),

    /// A stored row or engine invariant is permanently broken; retrying
    /// cannot change the fault and it needs investigation.
    #[error("permanent fault: the request cannot succeed on a retry; the fault needs investigation: {0}")]
    PermanentFault(String),
}

/// — stable wire error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidRequest,
    NotFound,
    /// A typed-graph entity (or a statement's subject entity) was not
    /// found. Split from the generic `NotFound` so the wire surfaces the
    /// precise `EntityNotFound` code instead of `MemoryNotFound`.
    EntityNotFound,
    /// A typed-graph statement was not found. Surfaces the wire
    /// `StatementNotFound` code instead of the generic `MemoryNotFound`.
    StatementNotFound,
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
    /// The write's namespace has no usable LLM provider credential (BYOK).
    /// Maps to wire `LlmCredentialMissing` (0x0066, category Conflict).
    /// Terminal — clients must not retry it.
    LlmCredentialMissing,
    Overloaded,
    /// Retrieval is unavailable on this shard. Wire code
    /// reserved for admin / health diagnostics only; a normal
    /// client RECALL never sees this — the server picks the path
    /// and shard spawn guarantees the required sinks are wired.
    RetrievalUnavailable,
    /// This shard's WAL is terminally broken. Maps to wire
    /// `ShardStorageFaulted` (0x00A0, category `OperatorIntervention`).
    /// Terminal — clients must not retry it; an operator must restart the shard.
    ShardStorageFaulted,
    /// Engine state is permanently inconsistent and a retry cannot clear it.
    PermanentFault,
    InternalError,
}

impl OpError {
    /// Map this error to its wire `ErrorCode`.
    #[must_use]
    pub fn error_code(&self) -> ErrorCode {
        match self {
            Self::InvalidRequest(_)
            | Self::InvalidField { .. }
            | Self::TooManyMemories
            | Self::SchemaConflict { .. } => ErrorCode::InvalidRequest,
            // Route the typed-graph NotFound cases to their precise wire
            // codes. The `what` tag is set at construction (here and in the
            // `From<*OpError>` conversions); anything else stays generic.
            Self::NotFound { what, .. } => match *what {
                "entity" | "subject entity" => ErrorCode::EntityNotFound,
                "statement" => ErrorCode::StatementNotFound,
                _ => ErrorCode::NotFound,
            },
            Self::Conflict(_) => ErrorCode::Conflict,
            Self::LlmCredentialMissing { .. } => ErrorCode::LlmCredentialMissing,
            Self::ShardStorageFaulted { .. } => ErrorCode::ShardStorageFaulted,
            Self::TxnExpired => ErrorCode::TxnExpired,
            Self::TxnNotFound => ErrorCode::TxnNotFound,
            Self::TransactionTooLarge { .. } => ErrorCode::TransactionTooLarge,
            Self::PredicateNotInSchema { .. } => ErrorCode::PredicateNotInSchema,
            Self::RelationTypeNotInSchema { .. } => ErrorCode::RelationTypeNotInSchema,
            Self::CardinalityViolation { .. } => ErrorCode::CardinalityViolation,
            Self::QuotaExceeded(_) => ErrorCode::QuotaExceeded,
            Self::Unauthorized(_) => ErrorCode::Unauthorized,
            Self::Overloaded(_) => ErrorCode::Overloaded,
            Self::RetrievalUnavailable(_) => ErrorCode::RetrievalUnavailable,
            // Operator opted out of this capability — surfaces as an
            // invalid request because the client can fix it without
            // server-side intervention by dropping the opt-in flag.
            Self::CapabilityNotEnabled { .. } => ErrorCode::InvalidRequest,
            Self::NotYetImplemented(_) | Self::Internal(_) => ErrorCode::InternalError,
            Self::PermanentFault(_) | Self::SupersessionChainCycle { .. } => {
                ErrorCode::PermanentFault
            }
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
                ExecError::WriterFailed(WriterError::NotFound { what, .. }) => match *what {
                    "entity" | "subject entity" => ErrorCode::EntityNotFound,
                    "statement" => ErrorCode::StatementNotFound,
                    _ => ErrorCode::NotFound,
                },
                ExecError::WriterFailed(WriterError::Conflict(_)) => ErrorCode::Conflict,
                ExecError::WriterFailed(WriterError::InvalidRequest(_)) => {
                    ErrorCode::InvalidRequest
                }
                ExecError::WriterFailed(WriterError::Internal(_)) => ErrorCode::InternalError,
                ExecError::WriterFailed(WriterError::StorageFaulted { .. }) => {
                    ErrorCode::ShardStorageFaulted
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_planner::ExecError;

    /// An apply-side refusal of the caller's input must keep its
    /// invalid-request classification all the way to the wire. Collapsing it
    /// into `InternalError` puts it in a category clients retry, so a client
    /// spins against a write the engine refuses every time.
    #[test]
    fn a_writer_invalid_request_reaches_the_wire_as_an_invalid_request() {
        let err = OpError::ExecError(ExecError::WriterFailed(WriterError::InvalidRequest(
            "relation_create: entity type violation on from".into(),
        )));
        assert_eq!(err.error_code(), ErrorCode::InvalidRequest);
    }

    /// The writer's three caller-facing classifications must stay apart: a
    /// client routes on the difference between "fix the request", "the row is
    /// gone" and "you raced committed state".
    #[test]
    fn writer_faults_keep_their_classifications_apart() {
        let invalid = OpError::ExecError(ExecError::WriterFailed(WriterError::InvalidRequest(
            "bad endpoint".into(),
        )));
        let conflict =
            OpError::ExecError(ExecError::WriterFailed(WriterError::Conflict("dup".into())));
        let internal =
            OpError::ExecError(ExecError::WriterFailed(WriterError::Internal("io".into())));
        assert_eq!(invalid.error_code(), ErrorCode::InvalidRequest);
        assert_eq!(conflict.error_code(), ErrorCode::Conflict);
        assert_eq!(internal.error_code(), ErrorCode::InternalError);
    }
}

// ---------------------------------------------------------------------------
// Domain-error → OpError conversions.
//
// One place each metadata error type is classified into the wire taxonomy,
// so handlers `?` them through. Per-handler `map_*_op_error` copies diverge:
// a copy that silently downgrades everything to `Internal` (500) loses the
// NotFound/Conflict classification a client routes on, and nothing catches it
// because each copy is only ever read on its own.
// ---------------------------------------------------------------------------

/// redb already records whether a failure is deterministic; stringifying it
/// before classification loses the only evidence of whether a retry can work.
fn storage_op_error<E: crate::apply::RedbFault>(e: &E, message: String) -> OpError {
    if e.storage_fault().is_deterministic() {
        OpError::PermanentFault(message)
    } else {
        OpError::Internal(message)
    }
}

impl From<brain_metadata::schema::predicate::PredicateOpError> for OpError {
    fn from(err: brain_metadata::schema::predicate::PredicateOpError) -> Self {
        use brain_metadata::schema::predicate::PredicateOpError as E;
        match err {
            E::InvalidIdentifier { reason } => {
                OpError::InvalidRequest(format!("predicate identifier: {reason}"))
            }
            E::AlreadyExists { qname, existing_id } => OpError::Conflict(format!(
                "predicate {qname:?} already exists with id {existing_id:?}"
            )),
            E::NotFound(id) => OpError::NotFound {
                what: "predicate",
                detail: format!("{id:?}"),
            },
            E::Storage(e) => storage_op_error(&e, format!("redb storage: {e}")),
            E::Table(e) => storage_op_error(&e, format!("redb table: {e}")),
            // A neighbouring subsystem failed underneath a predicate op; the
            // caller did nothing wrong, so this is ours, not theirs.
            E::Dependency(detail) => OpError::Internal(format!("predicate dependency: {detail}")),
        }
    }
}

impl From<brain_metadata::statement::StatementOpError> for OpError {
    fn from(err: brain_metadata::statement::StatementOpError) -> Self {
        use brain_metadata::statement::StatementOpError as E;
        match err {
            E::NotFound(id) => OpError::NotFound {
                what: "statement",
                detail: format!("{id:?}"),
            },
            E::AlreadyExists(id) => OpError::Conflict(format!("statement {id:?} already exists")),
            E::UnknownPredicate(p) => OpError::NotFound {
                what: "predicate",
                detail: format!("id={p}"),
            },
            E::UnknownSubject(id) => OpError::NotFound {
                what: "subject entity",
                detail: format!("{id:?}"),
            },
            E::UnknownObjectEntity(id) => OpError::NotFound {
                what: "entity",
                detail: format!("{id:?}"),
            },
            // A declared `object: Entity<Type>` range the write
            // violated — the client can fix it by pointing at an
            // entity of the declared type.
            e @ E::ObjectEntityTypeMismatch { .. } => OpError::InvalidRequest(e.to_string()),
            // Same, on the subject side: the predicate declares
            // `subject: Entity<Type>` and the write hung it off a
            // different kind of thing. Client-fixable, so InvalidRequest
            // rather than a server fault.
            e @ E::SubjectEntityTypeMismatch { .. } => OpError::InvalidRequest(e.to_string()),
            E::InvalidArgument(s) => OpError::InvalidRequest(s.to_string()),
            E::AlreadySuperseded(id, by) => {
                OpError::Conflict(format!("statement {id:?} already superseded by {by:?}"))
            }
            E::AlreadyTombstoned(id) => {
                OpError::Conflict(format!("statement {id:?} is tombstoned"))
            }
            E::EventCannotSupersede => OpError::Conflict("events cannot be superseded".into()),
            E::KindMismatch { old, new } => OpError::InvalidRequest(format!(
                "kind mismatch on supersede: old={old:?} new={new:?}"
            )),
            E::SubjectMismatch => OpError::InvalidRequest("subject must match on supersede".into()),
            E::PredicateMismatch => {
                OpError::InvalidRequest("predicate must match on supersede".into())
            }
            E::DecodeFailed => {
                OpError::PermanentFault("statement row decode failed — possible corruption".into())
            }
            E::Storage(e) => storage_op_error(&e, format!("redb storage: {e}")),
            E::Table(e) => storage_op_error(&e, format!("redb table: {e}")),
            E::Kind(e) => OpError::Internal(format!("kind registry: {e}")),
            E::PredicateOp(e) => OpError::from(e),
            E::EntityOp(e) => {
                OpError::Internal(format!("entity op forwarded from statement_ops: {e}"))
            }
        }
    }
}

impl From<brain_metadata::entity::ops::EntityOpError> for OpError {
    fn from(err: brain_metadata::entity::ops::EntityOpError) -> Self {
        use brain_metadata::entity::ops::EntityOpError as E;
        match err {
            E::NotFound(id) => OpError::NotFound {
                what: "entity",
                detail: format!("{id:?}"),
            },
            E::UnknownEntityType(t) => {
                OpError::InvalidRequest(format!("unknown entity_type {t:?}"))
            }
            E::DuplicateCanonicalName {
                type_id,
                name,
                existing,
            } => OpError::Conflict(format!(
                "canonical_name {name:?} already exists for type {type_id:?}: {existing:?}"
            )),
            E::Storage(e) => storage_op_error(&e, format!("redb storage: {e}")),
            E::Table(e) => storage_op_error(&e, format!("redb table: {e}")),
            E::TrigramOp(e) => OpError::Internal(format!("trigram op: {e}")),
            E::MergeRedirectCycle(id) => {
                OpError::PermanentFault(format!("entity {id:?} merge-redirect chain is a cycle"))
            }
        }
    }
}

impl From<brain_metadata::relation::types::RelationTypeOpError> for OpError {
    fn from(err: brain_metadata::relation::types::RelationTypeOpError) -> Self {
        use brain_metadata::relation::types::RelationTypeOpError as E;
        match err {
            E::InvalidIdentifier { reason } => {
                OpError::InvalidRequest(format!("relation_type identifier: {reason}"))
            }
            E::AlreadyExists { qname, existing_id } => OpError::Conflict(format!(
                "relation_type {qname:?} already exists with id {existing_id:?}"
            )),
            E::Storage(e) => storage_op_error(&e, format!("redb storage: {e}")),
            E::Table(e) => storage_op_error(&e, format!("redb table: {e}")),
        }
    }
}

/// Decode a tombstone reason byte that arrived on a request.
///
/// The range check lives on
/// [`brain_core::TombstoneReason::from_caller_byte`] — the two server-stamped
/// reasons mean "reclaim this row", so a request may not name them. This
/// wrapper only turns the rejection into the wire error, and is shared so the
/// statement and relation tombstone paths cannot drift into accepting
/// different sets.
pub fn caller_tombstone_reason(byte: u8) -> Result<brain_core::TombstoneReason, OpError> {
    brain_core::TombstoneReason::from_caller_byte(byte).ok_or_else(|| {
        OpError::InvalidRequest(format!(
            "unknown tombstone_reason byte {byte}; expected 1..=4"
        ))
    })
}

#[cfg(test)]
mod permanent_fault_tests {
    use super::*;

    /// A permanent fault must remain non-retryable through the protocol
    /// category selected for it.
    #[test]
    fn permanent_fault_is_non_retryable_end_to_end() {
        let error = OpError::PermanentFault("chain dangling at 7".into());
        assert_eq!(error.error_code(), ErrorCode::PermanentFault);
        assert_eq!(
            brain_protocol::error::ErrorCode::UnrecoverableInternal.category(),
            brain_protocol::error::ErrorCategory::OperatorIntervention
        );
        assert!(!brain_protocol::error::ErrorCategory::OperatorIntervention.is_retryable());
    }

    /// Collapsing the two directions is the bug this split prevents: a
    /// permanent fault makes a client spin, while a transient one must not
    /// abandon work that could succeed.
    #[test]
    fn internal_fault_remains_retryable() {
        assert_eq!(
            OpError::Internal("busy".into()).error_code(),
            ErrorCode::InternalError
        );
        assert_eq!(
            brain_protocol::error::ErrorCode::Internal.category(),
            brain_protocol::error::ErrorCategory::Internal
        );
    }

    /// A restart recovers a faulted WAL and does nothing for a corrupt row, so
    /// borrowing that instruction would send an operator down the wrong path.
    #[test]
    fn permanent_fault_message_does_not_instruct_restart() {
        assert!(!OpError::PermanentFault("corrupt row".into())
            .to_string()
            .contains("restart"));
    }

    #[test]
    fn deterministic_redb_failure_becomes_permanent() {
        let error = brain_metadata::statement::StatementOpError::Table(
            redb::TableError::TableDoesNotExist("x".into()),
        );
        assert!(matches!(OpError::from(error), OpError::PermanentFault(_)));
    }
}
