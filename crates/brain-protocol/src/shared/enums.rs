//! Foundational wire-domain enums for response bodies plus the
//! ErrorCategoryWire / ErrorCodeWire vocabularies for the open integer
//! classification fields in ERROR frames and mirrors of `crate::error`'s
//! `#[non_exhaustive]` types.

use crate::error::{ErrorCategory, ErrorCode};

/// — `PlanResponseFrame::TransitionKind`.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TransitionKind {
    Initial,
    Causal,
    Temporal,
    Similarity,
    Other(String),
}

// Bridge from the typed-graph namespace's retrieval-query retriever
// enum to the cognitive RECALL response field. Lives here so the
// cognitive response module stays free of typed-graph imports beyond
// this single conversion.
impl From<crate::ops::query::RetrieverWire> for RetrieverNameWire {
    fn from(w: crate::ops::query::RetrieverWire) -> Self {
        match w {
            crate::ops::query::RetrieverWire::Semantic => Self::Semantic,
            crate::ops::query::RetrieverWire::Lexical => Self::Lexical,
            crate::ops::query::RetrieverWire::Graph => Self::Graph,
        }
    }
}

/// Names the retriever family that surfaced a memory in a
/// `MemoryResult`. Populated when `RECALL_REQ` routes through the
/// retrieval query engine (schema-declared deployments).
///
/// This is the cognitive-side wire enum. The typed-graph namespace
/// has its own `RetrieverWire` for retrieval-query opcodes;
/// `From<RetrieverWire>` bridges the two so the cognitive response
/// type doesn't depend on the typed-graph namespace.
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    Hash,
    serde_repr::Serialize_repr,
    serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum RetrieverNameWire {
    Semantic = 0,
    Lexical = 1,
    Graph = 2,
    /// A retriever family this build cannot name reads as unknown; the
    /// surrounding response still decodes, so a peer one version ahead can
    /// describe a retriever this build does not know.
    #[serde(other)]
    Unknown = 255,
}

/// — `PlanResponseFrame::PlanStatus` (set on the final frame
/// only).
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum PlanStatus {
    GoalReached = 0,
    BudgetExhausted = 1,
    NoPathFound = 2,
    Cancelled = 3,
    /// A plan result this build cannot name reads as unknown; the surrounding
    /// response still decodes, so a peer one version ahead can report a result
    /// this build does not know.
    #[serde(other)]
    Unknown = 255,
}

/// — `ReasonResponseFrame::InferenceKind`.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum InferenceKind {
    CausalExplanation,
    EvidenceAccumulation,
    AnalogicalInference,
    Other(String),
}

/// — `ReasonResponseFrame::ReasonStatus`.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum ReasonStatus {
    Complete = 0,
    BudgetExhausted = 1,
    DepthLimitReached = 2,
    Cancelled = 3,
    /// A reason result this build cannot name reads as unknown; the surrounding
    /// response still decodes, allowing a peer one version ahead to report it.
    #[serde(other)]
    Unknown = 255,
}

/// `SubscriptionEvent::EventType`.
///
/// Carries 14 typed-graph event variants ([`Self::EntityCreated`]
/// through [`Self::SchemaUpdated`]). For typed-graph events the
/// cognitive fields on `SubscriptionEvent` (`memory_id`, `session_id`,
/// `kind`, `salience`, `text`) are zero-filled and `graph_payload`
/// carries the typed body.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum EventType {
    // Cognitive events.
    Encoded = 0,
    Forgotten = 1,
    Reclaimed = 2,
    KindChanged = 3,

    // Typed-graph events. graph_payload is populated.
    EntityCreated = 16,
    EntityUpdated = 17,
    EntityRenamed = 18,
    EntityMerged = 19,
    EntityUnmerged = 20,
    EntityTombstoned = 21,
    StatementCreated = 22,
    StatementSuperseded = 23,
    StatementTombstoned = 24,
    RelationCreated = 25,
    RelationSuperseded = 26,
    /// One *stage* of a write's pipeline completed. The same envelope
    /// is published by every background worker (auto-edge, temporal-
    /// edge, extractor, HyPE) once it has committed its derived phases
    /// for a memory. Subscribers waiting on a write's completion count
    /// down their `pending_stages` checklist as `StageCompleted`
    /// events arrive. `stage_payload` carries the per-stage detail
    /// (extractor counts + audit status, edge stages: the count of
    /// edges written, etc.).
    StageCompleted = 27,
    SchemaUpdated = 29,
    /// Appended after `SchemaUpdated` to preserve the stable
    /// discriminants of prior variants.
    RelationTombstoned = 30,

    // Unified-edge change feed. Cognitive Link / Unlink and typed-
    // relation create / supersede / tombstone all flow through these
    // three variants; the per-event `edge_payload` sidecar carries
    // `from`, `to`, kind discriminator, relation id when applicable.
    EdgeAdded = 31,
    EdgeRemoved = 32,
    EdgeSuperseded = 33,
    /// An event kind this build cannot name reads as unknown; the surrounding
    /// subscription frame still decodes, allowing a peer one version ahead to
    /// publish an event this build does not know.
    #[serde(other)]
    Unknown = 255,
}

/// Background stage a write triggers. A write submission runs N
/// foreground phases inside `submit()`'s wtxn (caller blocks) plus M
/// background stages that workers drain (caller doesn't block, but
/// may opt in via `--wait`). The ack lists the stages this write
/// queued so a client knows which `StageCompleted` events to wait
/// for.
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    Hash,
    serde_repr::Serialize_repr,
    serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum StageKind {
    /// `SimilarTo` edges derived from HNSW k-NN of the new memory.
    AutoEdge = 0,
    /// `FollowedBy` edges derived from session adjacency.
    TemporalEdge = 1,
    /// Entities, statements, relations extracted from memory text via
    /// the three-tier pipeline (pattern → classifier → LLM).
    Extractor = 2,
    /// Hypothetical questions generated for the new memory (write-time
    /// HyPE), embedded and inserted into the memory HNSW index.
    Hype = 3,
}

/// Verdict of a completed stage. Carried on every `StageCompleted`
/// event so a client can distinguish "ran and produced output" from
/// "ran but had nothing to produce" from "failed."
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum StageOutcome {
    /// Stage produced at least one derived item.
    Ok = 0,
    /// Stage ran cleanly but produced nothing (e.g. zero-vector input
    /// for auto-edge, no extractable content for extractor). Distinct
    /// from `Ok` so wait helpers can show "completed, nothing to add."
    Empty = 1,
    /// Stage errored. The wait helper still unblocks; the failure
    /// reason lives on the per-stage payload (`StagePayload::*`).
    Failed = 2,
    /// A stage verdict this build cannot name reads as unknown; the surrounding
    /// completion event still decodes, so a peer one version ahead can report
    /// a verdict this build does not know.
    #[serde(other)]
    Unknown = 255,
}

/// Per-stage detail sidecar on `StageCompleted` events. Discriminated
/// by [`StageKind`] but kept as a flat enum so subscribers can
/// destructure without first reading the parent stage_kind byte.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StagePayload {
    AutoEdge(StageAutoEdgePayload),
    TemporalEdge(StageTemporalEdgePayload),
    Extractor(StageExtractorPayload),
    Hype(StageHypePayload),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StageAutoEdgePayload {
    /// How many `SimilarTo` rows the worker wrote.
    pub edges_written: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StageTemporalEdgePayload {
    /// How many `FollowedBy` rows the worker wrote.
    pub edges_written: u32,
}

#[cfg(test)]
mod stage_extractor_payload_wire_keys {
    use super::{StageAuditStatus, StageExtractorPayload};

    /// The keys this struct emits are a cross-repo contract: `brain-db-sdk`
    /// carries its own copy, nothing compiles both, and each derives its
    /// serde keys from its field names. A spelling that drifts in either
    /// copy compiles and passes clippy.
    ///
    /// This asserts the spellings the engine emits; the SDK decodes the same
    /// keys written longhand in `brain-sdk/rust/tests/stage_payload/main.rs`.
    /// Both ends are needed to pin the name.
    #[test]
    fn the_yield_counts_are_emitted_under_their_documented_keys() {
        let p = StageExtractorPayload {
            entity_count: 11,
            statement_count: 22,
            relation_count: 33,
            dropped_count: 44,
            contradiction_count: 55,
            undeclared_entity_count: 66,
            has_active_schema: true,
            audit_status: StageAuditStatus::Succeeded,
            error_message: String::new(),
        };
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&p, &mut bytes).expect("encode");
        let decoded: ciborium::value::Value =
            ciborium::de::from_reader(bytes.as_slice()).expect("decode");
        let ciborium::value::Value::Map(entries) = decoded else {
            panic!("the payload must encode as a CBOR map, not {decoded:?}");
        };
        let keys: Vec<String> = entries
            .iter()
            .filter_map(|(k, _)| match k {
                ciborium::value::Value::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();

        for want in [
            "entity_count",
            "statement_count",
            "relation_count",
            "dropped_count",
            "contradiction_count",
            "undeclared_entity_count",
            "has_active_schema",
            "audit_status",
            "error_message",
        ] {
            assert!(
                keys.iter().any(|k| k == want),
                "`{want}` is not on the wire; keys were {keys:?}"
            );
        }
        assert_eq!(keys.len(), 9, "an unlisted key appeared: {keys:?}");
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StageHypePayload {
    /// How many hypothetical questions were embedded, persisted, and
    /// inserted into the memory HNSW index.
    pub questions_written: u32,
    /// LLM micro-USD spent generating the questions (`0` on a cache
    /// hit).
    pub cost_micro_usd: u64,
    /// Why the stage failed. Populated whenever the parent outcome is
    /// [`StageOutcome::Failed`], empty otherwise. HyPE either produced
    /// questions or could not run, so it has no partial state of the kind
    /// [`StageExtractorPayload::error_message`] also reports on.
    ///
    /// Without this a failed HyPE stage could not explain itself. The
    /// outcome byte said `Failed` and the two counters said zero, which is
    /// also what a namespace with no provider key, a budget too small for a
    /// reasoning model, and a dead embedder all look like. The reason
    /// existed — every failure path logs it — but it reached only the
    /// server's own stdout, so an operator reading the stage record saw
    /// that something broke and had nothing to act on.
    pub error_message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StageExtractorPayload {
    pub entity_count: u32,
    pub statement_count: u32,
    pub relation_count: u32,
    /// Candidates the apply pass discarded before commit.
    ///
    /// The three counts above are survivors, so this is the denominator a
    /// yield rate needs: two statements out of two candidates and two out of
    /// nine are different outcomes. A total rather than a per-reason
    /// breakdown — the worker's fourteen discard reasons stay in the
    /// shard-level metrics, which is where an operator debugging one shard
    /// wants them.
    pub dropped_count: u32,
    /// Committed statements that landed in a Fact-vs-Fact contradiction.
    ///
    /// Read from the durable `statement_contradiction_audit` row rather than
    /// inferred from the write: a contradicting insert proceeds, because
    /// coexisting Facts are legal and the conflict is filed for
    /// reconciliation, so a committed statement is no evidence either way.
    ///
    /// `0` means "no contradiction", not "not measured" — the audit lookup
    /// runs for every entity-subject Fact the apply commits.
    pub contradiction_count: u32,
    /// Entities whose type no schema declares — neither the system's nor the
    /// writing namespace's.
    ///
    /// Not a discard: the resolver interns an undeclared type into the
    /// tenant's namespace, so the entity commits and `dropped_count` never
    /// sees it. This is the entity half of a junk rate, where
    /// `dropped_count` is the triple half.
    ///
    /// Only meaningful alongside `has_active_schema`. A schema is an optional
    /// add-on: with none declared there is no vocabulary to be outside of,
    /// every coined type is ordinary discovery, and a rate built from this
    /// would report a schemaless tenant as entirely junk.
    pub undeclared_entity_count: u32,
    /// Whether the writing namespace had an active schema when the apply ran.
    ///
    /// The denominator for `undeclared_entity_count`. Without it the two
    /// states that produce a high undeclared count are indistinguishable:
    /// a tenant whose schema the extractor went outside of, which is worth
    /// acting on, and a tenant who declared no schema at all, which is the
    /// product working as intended.
    pub has_active_schema: bool,
    pub audit_status: StageAuditStatus,
    /// Why extraction failed, or partly failed. Populated whenever
    /// `audit_status` is `Failed` or `PartiallyApplied`, empty on
    /// `Succeeded` and `Skipped`.
    ///
    /// `PartiallyApplied` is included deliberately. It is the common
    /// failure — one tier of three fell over and the rest still wrote a
    /// graph — and it reaches a caller as `StageOutcome::Ok`, so the
    /// counts alone look like an ordinary small extraction. `audit_status`
    /// distinguishes the two cases; this says which tier and why.
    pub error_message: String,
}

/// Audit verdict for an extractor stage. Mirrors the worker's
/// internal `pipeline_status` byte. Distinct from [`StageOutcome`]
/// because the extractor pipeline has more granularity than the
/// generic three-state outcome — `PartiallyApplied` is a per-tier
/// concern that doesn't make sense for the edge stages.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum StageAuditStatus {
    /// Every enabled tier ran cleanly and writes committed.
    Succeeded = 0,
    /// One or more tiers failed; surviving tiers still committed
    /// their items. Counts reflect what landed.
    PartiallyApplied = 1,
    /// All tiers failed or the apply path errored; nothing landed.
    Failed = 2,
    /// Worker ran but had nothing to commit (no extractors
    /// registered, or every tier returned zero items).
    Skipped = 3,
}

/// — `IntegrityIssue::IntegrityIssueType`.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum IntegrityIssueType {
    VectorCorruption,
    TextCorruption,
    StaleEdge,
    OrphanIndex,
    SchemaVersionMismatch,
    Other(String),
}

/// — `AdminMigrateEmbeddingsResponseFrame::MigrationStatus`.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MigrationStatus {
    InProgress,
    Completed,
    Failed(String),
    Cancelled,
}

/// Wire-encoded mirror of [`crate::error::ErrorCategory`]. The
/// canonical type is intentionally `#[non_exhaustive]` for forward-
/// compatibility, so the wire needs a stable repr while received integers
/// remain open to values introduced by newer peers.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum ErrorCategoryWire {
    Protocol = 0,
    Authentication = 1,
    Authorization = 2,
    Validation = 3,
    NotFound = 4,
    Conflict = 5,
    ResourceExhausted = 6,
    Internal = 7,
    Unavailable = 8,
    OperatorIntervention = 9,
}

impl From<ErrorCategory> for ErrorCategoryWire {
    fn from(c: ErrorCategory) -> Self {
        match c {
            ErrorCategory::Protocol => Self::Protocol,
            ErrorCategory::Authentication => Self::Authentication,
            ErrorCategory::Authorization => Self::Authorization,
            ErrorCategory::Validation => Self::Validation,
            ErrorCategory::NotFound => Self::NotFound,
            ErrorCategory::Conflict => Self::Conflict,
            ErrorCategory::ResourceExhausted => Self::ResourceExhausted,
            ErrorCategory::Internal => Self::Internal,
            ErrorCategory::Unavailable => Self::Unavailable,
            ErrorCategory::OperatorIntervention => Self::OperatorIntervention,
        }
    }
}

impl From<ErrorCategoryWire> for ErrorCategory {
    fn from(c: ErrorCategoryWire) -> Self {
        match c {
            ErrorCategoryWire::Protocol => Self::Protocol,
            ErrorCategoryWire::Authentication => Self::Authentication,
            ErrorCategoryWire::Authorization => Self::Authorization,
            ErrorCategoryWire::Validation => Self::Validation,
            ErrorCategoryWire::NotFound => Self::NotFound,
            ErrorCategoryWire::Conflict => Self::Conflict,
            ErrorCategoryWire::ResourceExhausted => Self::ResourceExhausted,
            ErrorCategoryWire::Internal => Self::Internal,
            ErrorCategoryWire::Unavailable => Self::Unavailable,
            ErrorCategoryWire::OperatorIntervention => Self::OperatorIntervention,
        }
    }
}

/// Match an open wire category onto the vocabulary this build knows. An
/// unfamiliar category must cost one field rather than the whole ERROR frame.
impl TryFrom<u8> for ErrorCategoryWire {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Protocol),
            1 => Ok(Self::Authentication),
            2 => Ok(Self::Authorization),
            3 => Ok(Self::Validation),
            4 => Ok(Self::NotFound),
            5 => Ok(Self::Conflict),
            6 => Ok(Self::ResourceExhausted),
            7 => Ok(Self::Internal),
            8 => Ok(Self::Unavailable),
            9 => Ok(Self::OperatorIntervention),
            value => Err(value),
        }
    }
}

/// The wire representation has a stable numeric repr, while received
/// integers remain open so an unfamiliar code does not destroy its ERROR
/// frame. This enum is the vocabulary this build constructs and matches
/// received integers back onto.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u16)]
pub enum ErrorCodeWire {
    // Protocol
    BadMagic = 0x0001,
    BadHeaderCrc = 0x0002,
    BadPayloadCrc = 0x0003,
    BadOpcode = 0x0004,
    BadVersion = 0x0005,
    BadFrame = 0x0006,
    OversizePayload = 0x0007,
    ReservedFieldNonZero = 0x0008,
    MalformedPayload = 0x000A,
    // Connection / handshake
    VersionNotSupported = 0x0020,
    NoSuchAuthMethod = 0x0021,
    Unauthenticated = 0x0022,
    NotAuthenticated = 0x0023,
    AuthBackendUnavailable = 0x0024,
    SessionExpired = 0x0025,
    // Authorization
    PermissionDenied = 0x0030,
    AdminPermissionRequired = 0x0031,
    WrongShard = 0x0032,
    ActAsDenied = 0x0033,
    // Validation
    InvalidArgument = 0x0040,
    BadSessionId = 0x0044,
    PredicateNotInSchema = 0x004B,
    RelationTypeNotInSchema = 0x004C,
    // Not found
    MemoryNotFound = 0x0050,
    ContextNotFound = 0x0051,
    SubscriptionNotFound = 0x0052,
    SnapshotNotFound = 0x0053,
    TxnNotFound = 0x0054,
    // Conflict
    IdempotencyConflict = 0x0060,
    TransactionConflict = 0x0061,
    TransactionTimeout = 0x0062,
    StreamIdInUse = 0x0063,
    SubscriptionLsnTooOld = 0x0064,
    CardinalityViolation = 0x0065,
    LlmCredentialMissing = 0x0066,
    // Resource exhausted
    OutOfSlots = 0x0070,
    OutOfDisk = 0x0071,
    OutOfMemory = 0x0072,
    RateLimited = 0x0073,
    StreamLimitExceeded = 0x0074,
    TransactionLimitExceeded = 0x0076,
    TransactionTooLarge = 0x0077,
    // Internal
    Internal = 0x0080,
    StorageError = 0x0081,
    IndexError = 0x0082,
    EmbeddingError = 0x0083,
    MetadataError = 0x0084,
    Cancelled = 0x0085,
    // Unavailable
    ShardUnavailable = 0x0090,
    Overloaded = 0x0091,
    Restarting = 0x0092,
    Maintenance = 0x0093,
    RetrieverDegraded = 0x0094,
    LlmCredentialUnavailable = 0x0095,

    // Operator intervention
    ShardStorageFaulted = 0x00A0,
    UnrecoverableInternal = 0x00A1,

    // Typed-graph error codes (0x01xx namespace; low-byte family mirrors
    // the typed-graph opcode ranges).
    SchemaMigrationRequired = 0x0121,
    EntityNotFound = 0x0130,
    EntityTypeMismatch = 0x0131,
    EntityAmbiguous = 0x0132,
    EntityMergeConflict = 0x0133,
    StatementNotFound = 0x0140,
    StatementObjectTypeMismatch = 0x0141,
    StatementContradictsExisting = 0x0142,
    QueryTimeout = 0x0160,
    QueryOverBudget = 0x0161,
    ExtractorDisabled = 0x0170,
    ExtractorBudgetExceeded = 0x0171,
    ExtractionFailed = 0x0172,
}

impl From<ErrorCode> for ErrorCodeWire {
    #[allow(clippy::too_many_lines)]
    fn from(c: ErrorCode) -> Self {
        match c {
            ErrorCode::BadMagic => Self::BadMagic,
            ErrorCode::BadHeaderCrc => Self::BadHeaderCrc,
            ErrorCode::BadPayloadCrc => Self::BadPayloadCrc,
            ErrorCode::BadOpcode => Self::BadOpcode,
            ErrorCode::BadVersion => Self::BadVersion,
            ErrorCode::BadFrame => Self::BadFrame,
            ErrorCode::OversizePayload => Self::OversizePayload,
            ErrorCode::ReservedFieldNonZero => Self::ReservedFieldNonZero,
            ErrorCode::MalformedPayload => Self::MalformedPayload,
            ErrorCode::VersionNotSupported => Self::VersionNotSupported,
            ErrorCode::NoSuchAuthMethod => Self::NoSuchAuthMethod,
            ErrorCode::Unauthenticated => Self::Unauthenticated,
            ErrorCode::NotAuthenticated => Self::NotAuthenticated,
            ErrorCode::AuthBackendUnavailable => Self::AuthBackendUnavailable,
            ErrorCode::SessionExpired => Self::SessionExpired,
            ErrorCode::PermissionDenied => Self::PermissionDenied,
            ErrorCode::AdminPermissionRequired => Self::AdminPermissionRequired,
            ErrorCode::WrongShard => Self::WrongShard,
            ErrorCode::ActAsDenied => Self::ActAsDenied,
            ErrorCode::InvalidArgument => Self::InvalidArgument,
            ErrorCode::BadSessionId => Self::BadSessionId,
            ErrorCode::PredicateNotInSchema => Self::PredicateNotInSchema,
            ErrorCode::RelationTypeNotInSchema => Self::RelationTypeNotInSchema,
            ErrorCode::MemoryNotFound => Self::MemoryNotFound,
            ErrorCode::ContextNotFound => Self::ContextNotFound,
            ErrorCode::SubscriptionNotFound => Self::SubscriptionNotFound,
            ErrorCode::SnapshotNotFound => Self::SnapshotNotFound,
            ErrorCode::TxnNotFound => Self::TxnNotFound,
            ErrorCode::IdempotencyConflict => Self::IdempotencyConflict,
            ErrorCode::TransactionConflict => Self::TransactionConflict,
            ErrorCode::TransactionTimeout => Self::TransactionTimeout,
            ErrorCode::StreamIdInUse => Self::StreamIdInUse,
            ErrorCode::SubscriptionLsnTooOld => Self::SubscriptionLsnTooOld,
            ErrorCode::CardinalityViolation => Self::CardinalityViolation,
            ErrorCode::LlmCredentialMissing => Self::LlmCredentialMissing,
            ErrorCode::OutOfSlots => Self::OutOfSlots,
            ErrorCode::OutOfDisk => Self::OutOfDisk,
            ErrorCode::OutOfMemory => Self::OutOfMemory,
            ErrorCode::RateLimited => Self::RateLimited,
            ErrorCode::StreamLimitExceeded => Self::StreamLimitExceeded,
            ErrorCode::TransactionLimitExceeded => Self::TransactionLimitExceeded,
            ErrorCode::TransactionTooLarge => Self::TransactionTooLarge,
            ErrorCode::Internal => Self::Internal,
            ErrorCode::StorageError => Self::StorageError,
            ErrorCode::IndexError => Self::IndexError,
            ErrorCode::EmbeddingError => Self::EmbeddingError,
            ErrorCode::MetadataError => Self::MetadataError,
            ErrorCode::Cancelled => Self::Cancelled,
            ErrorCode::ShardUnavailable => Self::ShardUnavailable,
            ErrorCode::Overloaded => Self::Overloaded,
            ErrorCode::Restarting => Self::Restarting,
            ErrorCode::Maintenance => Self::Maintenance,
            ErrorCode::RetrieverDegraded => Self::RetrieverDegraded,
            ErrorCode::LlmCredentialUnavailable => Self::LlmCredentialUnavailable,
            ErrorCode::ShardStorageFaulted => Self::ShardStorageFaulted,
            ErrorCode::UnrecoverableInternal => Self::UnrecoverableInternal,
            // Typed-graph codes (0x01xx).
            ErrorCode::SchemaMigrationRequired => Self::SchemaMigrationRequired,
            ErrorCode::EntityNotFound => Self::EntityNotFound,
            ErrorCode::EntityTypeMismatch => Self::EntityTypeMismatch,
            ErrorCode::EntityAmbiguous => Self::EntityAmbiguous,
            ErrorCode::EntityMergeConflict => Self::EntityMergeConflict,
            ErrorCode::StatementNotFound => Self::StatementNotFound,
            ErrorCode::StatementObjectTypeMismatch => Self::StatementObjectTypeMismatch,
            ErrorCode::StatementContradictsExisting => Self::StatementContradictsExisting,
            ErrorCode::QueryTimeout => Self::QueryTimeout,
            ErrorCode::QueryOverBudget => Self::QueryOverBudget,
            ErrorCode::ExtractorDisabled => Self::ExtractorDisabled,
            ErrorCode::ExtractorBudgetExceeded => Self::ExtractorBudgetExceeded,
            ErrorCode::ExtractionFailed => Self::ExtractionFailed,
        }
    }
}

impl From<ErrorCodeWire> for ErrorCode {
    #[allow(clippy::too_many_lines)]
    fn from(c: ErrorCodeWire) -> Self {
        match c {
            ErrorCodeWire::BadMagic => Self::BadMagic,
            ErrorCodeWire::BadHeaderCrc => Self::BadHeaderCrc,
            ErrorCodeWire::BadPayloadCrc => Self::BadPayloadCrc,
            ErrorCodeWire::BadOpcode => Self::BadOpcode,
            ErrorCodeWire::BadVersion => Self::BadVersion,
            ErrorCodeWire::BadFrame => Self::BadFrame,
            ErrorCodeWire::OversizePayload => Self::OversizePayload,
            ErrorCodeWire::ReservedFieldNonZero => Self::ReservedFieldNonZero,
            ErrorCodeWire::MalformedPayload => Self::MalformedPayload,
            ErrorCodeWire::VersionNotSupported => Self::VersionNotSupported,
            ErrorCodeWire::NoSuchAuthMethod => Self::NoSuchAuthMethod,
            ErrorCodeWire::Unauthenticated => Self::Unauthenticated,
            ErrorCodeWire::NotAuthenticated => Self::NotAuthenticated,
            ErrorCodeWire::AuthBackendUnavailable => Self::AuthBackendUnavailable,
            ErrorCodeWire::SessionExpired => Self::SessionExpired,
            ErrorCodeWire::PermissionDenied => Self::PermissionDenied,
            ErrorCodeWire::AdminPermissionRequired => Self::AdminPermissionRequired,
            ErrorCodeWire::WrongShard => Self::WrongShard,
            ErrorCodeWire::ActAsDenied => Self::ActAsDenied,
            ErrorCodeWire::InvalidArgument => Self::InvalidArgument,
            ErrorCodeWire::BadSessionId => Self::BadSessionId,
            ErrorCodeWire::PredicateNotInSchema => Self::PredicateNotInSchema,
            ErrorCodeWire::RelationTypeNotInSchema => Self::RelationTypeNotInSchema,
            ErrorCodeWire::MemoryNotFound => Self::MemoryNotFound,
            ErrorCodeWire::ContextNotFound => Self::ContextNotFound,
            ErrorCodeWire::SubscriptionNotFound => Self::SubscriptionNotFound,
            ErrorCodeWire::SnapshotNotFound => Self::SnapshotNotFound,
            ErrorCodeWire::TxnNotFound => Self::TxnNotFound,
            ErrorCodeWire::IdempotencyConflict => Self::IdempotencyConflict,
            ErrorCodeWire::TransactionConflict => Self::TransactionConflict,
            ErrorCodeWire::TransactionTimeout => Self::TransactionTimeout,
            ErrorCodeWire::StreamIdInUse => Self::StreamIdInUse,
            ErrorCodeWire::SubscriptionLsnTooOld => Self::SubscriptionLsnTooOld,
            ErrorCodeWire::CardinalityViolation => Self::CardinalityViolation,
            ErrorCodeWire::LlmCredentialMissing => Self::LlmCredentialMissing,
            ErrorCodeWire::OutOfSlots => Self::OutOfSlots,
            ErrorCodeWire::OutOfDisk => Self::OutOfDisk,
            ErrorCodeWire::OutOfMemory => Self::OutOfMemory,
            ErrorCodeWire::RateLimited => Self::RateLimited,
            ErrorCodeWire::StreamLimitExceeded => Self::StreamLimitExceeded,
            ErrorCodeWire::TransactionLimitExceeded => Self::TransactionLimitExceeded,
            ErrorCodeWire::TransactionTooLarge => Self::TransactionTooLarge,
            ErrorCodeWire::Internal => Self::Internal,
            ErrorCodeWire::StorageError => Self::StorageError,
            ErrorCodeWire::IndexError => Self::IndexError,
            ErrorCodeWire::EmbeddingError => Self::EmbeddingError,
            ErrorCodeWire::MetadataError => Self::MetadataError,
            ErrorCodeWire::Cancelled => Self::Cancelled,
            ErrorCodeWire::ShardUnavailable => Self::ShardUnavailable,
            ErrorCodeWire::Overloaded => Self::Overloaded,
            ErrorCodeWire::Restarting => Self::Restarting,
            ErrorCodeWire::Maintenance => Self::Maintenance,
            ErrorCodeWire::RetrieverDegraded => Self::RetrieverDegraded,
            ErrorCodeWire::LlmCredentialUnavailable => Self::LlmCredentialUnavailable,
            ErrorCodeWire::ShardStorageFaulted => Self::ShardStorageFaulted,
            ErrorCodeWire::UnrecoverableInternal => Self::UnrecoverableInternal,
            // Typed-graph codes (0x01xx).
            ErrorCodeWire::SchemaMigrationRequired => Self::SchemaMigrationRequired,
            ErrorCodeWire::EntityNotFound => Self::EntityNotFound,
            ErrorCodeWire::EntityTypeMismatch => Self::EntityTypeMismatch,
            ErrorCodeWire::EntityAmbiguous => Self::EntityAmbiguous,
            ErrorCodeWire::EntityMergeConflict => Self::EntityMergeConflict,
            ErrorCodeWire::StatementNotFound => Self::StatementNotFound,
            ErrorCodeWire::StatementObjectTypeMismatch => Self::StatementObjectTypeMismatch,
            ErrorCodeWire::StatementContradictsExisting => Self::StatementContradictsExisting,
            ErrorCodeWire::QueryTimeout => Self::QueryTimeout,
            ErrorCodeWire::QueryOverBudget => Self::QueryOverBudget,
            ErrorCodeWire::ExtractorDisabled => Self::ExtractorDisabled,
            ErrorCodeWire::ExtractorBudgetExceeded => Self::ExtractorBudgetExceeded,
            ErrorCodeWire::ExtractionFailed => Self::ExtractionFailed,
        }
    }
}

/// Match an open wire code onto the vocabulary this build knows. An
/// unfamiliar code must cost one field rather than the whole ERROR frame.
impl TryFrom<u16> for ErrorCodeWire {
    type Error = u16;

    #[allow(clippy::too_many_lines)]
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0x0001 => Ok(Self::BadMagic),
            0x0002 => Ok(Self::BadHeaderCrc),
            0x0003 => Ok(Self::BadPayloadCrc),
            0x0004 => Ok(Self::BadOpcode),
            0x0005 => Ok(Self::BadVersion),
            0x0006 => Ok(Self::BadFrame),
            0x0007 => Ok(Self::OversizePayload),
            0x0008 => Ok(Self::ReservedFieldNonZero),
            0x000A => Ok(Self::MalformedPayload),
            0x0020 => Ok(Self::VersionNotSupported),
            0x0021 => Ok(Self::NoSuchAuthMethod),
            0x0022 => Ok(Self::Unauthenticated),
            0x0023 => Ok(Self::NotAuthenticated),
            0x0024 => Ok(Self::AuthBackendUnavailable),
            0x0025 => Ok(Self::SessionExpired),
            0x0030 => Ok(Self::PermissionDenied),
            0x0031 => Ok(Self::AdminPermissionRequired),
            0x0032 => Ok(Self::WrongShard),
            0x0033 => Ok(Self::ActAsDenied),
            0x0040 => Ok(Self::InvalidArgument),
            0x0044 => Ok(Self::BadSessionId),
            0x004B => Ok(Self::PredicateNotInSchema),
            0x004C => Ok(Self::RelationTypeNotInSchema),
            0x0050 => Ok(Self::MemoryNotFound),
            0x0051 => Ok(Self::ContextNotFound),
            0x0052 => Ok(Self::SubscriptionNotFound),
            0x0053 => Ok(Self::SnapshotNotFound),
            0x0054 => Ok(Self::TxnNotFound),
            0x0060 => Ok(Self::IdempotencyConflict),
            0x0061 => Ok(Self::TransactionConflict),
            0x0062 => Ok(Self::TransactionTimeout),
            0x0063 => Ok(Self::StreamIdInUse),
            0x0064 => Ok(Self::SubscriptionLsnTooOld),
            0x0065 => Ok(Self::CardinalityViolation),
            0x0066 => Ok(Self::LlmCredentialMissing),
            0x0070 => Ok(Self::OutOfSlots),
            0x0071 => Ok(Self::OutOfDisk),
            0x0072 => Ok(Self::OutOfMemory),
            0x0073 => Ok(Self::RateLimited),
            0x0074 => Ok(Self::StreamLimitExceeded),
            0x0076 => Ok(Self::TransactionLimitExceeded),
            0x0077 => Ok(Self::TransactionTooLarge),
            0x0080 => Ok(Self::Internal),
            0x0081 => Ok(Self::StorageError),
            0x0082 => Ok(Self::IndexError),
            0x0083 => Ok(Self::EmbeddingError),
            0x0084 => Ok(Self::MetadataError),
            0x0085 => Ok(Self::Cancelled),
            0x0090 => Ok(Self::ShardUnavailable),
            0x0091 => Ok(Self::Overloaded),
            0x0092 => Ok(Self::Restarting),
            0x0093 => Ok(Self::Maintenance),
            0x0094 => Ok(Self::RetrieverDegraded),
            0x0095 => Ok(Self::LlmCredentialUnavailable),
            0x00A0 => Ok(Self::ShardStorageFaulted),
            0x00A1 => Ok(Self::UnrecoverableInternal),
            0x0121 => Ok(Self::SchemaMigrationRequired),
            0x0130 => Ok(Self::EntityNotFound),
            0x0131 => Ok(Self::EntityTypeMismatch),
            0x0132 => Ok(Self::EntityAmbiguous),
            0x0133 => Ok(Self::EntityMergeConflict),
            0x0140 => Ok(Self::StatementNotFound),
            0x0141 => Ok(Self::StatementObjectTypeMismatch),
            0x0142 => Ok(Self::StatementContradictsExisting),
            0x0160 => Ok(Self::QueryTimeout),
            0x0161 => Ok(Self::QueryOverBudget),
            0x0170 => Ok(Self::ExtractorDisabled),
            0x0171 => Ok(Self::ExtractorBudgetExceeded),
            0x0172 => Ok(Self::ExtractionFailed),
            value => Err(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::cbor::{from_cbor_bytes, to_cbor_bytes};
    use crate::connection::handshake::AuthMethod;
    use crate::error::ErrorCode;
    use crate::ops::entity::ResolutionOutcomeWire;
    use crate::ops::graph::{GraphEdgeKindWire, GraphNodeKindWire};
    use crate::ops::memory::{
        EdgeDirectionWire, EncodeTraceStageStatus, PlanTraceDirection, RankedItemKindWire,
        RecallCandidateKind, RecallTraceRetrieverStatus, WaitMode,
    };
    use crate::shared::primitives::{EdgeKindWire, ForgetMode, MemoryKindWire};

    /// The BYOK contract, pinned.
    ///
    /// `LlmCredentialMissing` must be a NON-retryable category and
    /// `LlmCredentialUnavailable` a retryable one. Every published SDK decides
    /// whether to retry from this classification, so swapping them would make
    /// a tenant with no provider key retry forever (or an unreadable
    /// credential store fail a write permanently).
    #[test]
    fn byok_credential_codes_carry_the_right_retry_semantics() {
        use crate::error::ErrorCategory;
        assert_eq!(
            ErrorCode::LlmCredentialMissing.category(),
            ErrorCategory::Conflict,
            "a missing provider key is terminal — it must not be retried"
        );
        assert_eq!(
            ErrorCode::LlmCredentialUnavailable.category(),
            ErrorCategory::Unavailable,
            "an unreadable credential store is transient — it must be retried"
        );
        assert_ne!(
            ErrorCode::LlmCredentialMissing.category(),
            ErrorCode::LlmCredentialUnavailable.category(),
            "collapsing these two is the bug this split exists to prevent"
        );
    }

    /// Category discriminants are wire values, so moving one changes what
    /// peers decode. Add a category only when no existing one has the right
    /// retry semantics; `OperatorIntervention` is the highest discriminant
    /// until that bar is cleared again.
    #[test]
    fn wire_categories_end_at_operator_intervention() {
        assert_eq!(
            ErrorCategoryWire::OperatorIntervention as u8,
            9,
            "OperatorIntervention must remain the highest category discriminant"
        );
    }

    /// Response enums only describe something the server observed, so an
    /// unrecognised value costs one field while refusing it costs the whole
    /// frame.
    #[test]
    fn response_enums_decode_unknown_values() {
        let bytes = to_cbor_bytes(&200u8);
        assert_eq!(
            from_cbor_bytes::<RetrieverNameWire>(&bytes).unwrap(),
            RetrieverNameWire::Unknown,
            "an unfamiliar retriever must not discard its response frame"
        );
        assert_eq!(
            from_cbor_bytes::<PlanStatus>(&bytes).unwrap(),
            PlanStatus::Unknown,
            "an unfamiliar plan status must not discard its response frame"
        );
        assert_eq!(
            from_cbor_bytes::<ReasonStatus>(&bytes).unwrap(),
            ReasonStatus::Unknown,
            "an unfamiliar reason status must not discard its response frame"
        );
        assert_eq!(
            from_cbor_bytes::<EventType>(&bytes).unwrap(),
            EventType::Unknown,
            "an unfamiliar event type must not discard its subscription frame"
        );
        assert_eq!(
            from_cbor_bytes::<StageOutcome>(&bytes).unwrap(),
            StageOutcome::Unknown,
            "an unfamiliar stage outcome must not discard its event frame"
        );
        assert_eq!(
            from_cbor_bytes::<ResolutionOutcomeWire>(&bytes).unwrap(),
            ResolutionOutcomeWire::Unknown,
            "an unfamiliar resolution outcome must not discard its response frame"
        );
        assert_eq!(
            from_cbor_bytes::<GraphNodeKindWire>(&bytes).unwrap(),
            GraphNodeKindWire::Unknown,
            "an unfamiliar graph node kind must not discard its response frame"
        );
        assert_eq!(
            from_cbor_bytes::<GraphEdgeKindWire>(&bytes).unwrap(),
            GraphEdgeKindWire::Unknown,
            "an unfamiliar graph edge kind must not discard its response frame"
        );
        assert_eq!(
            from_cbor_bytes::<EdgeDirectionWire>(&bytes).unwrap(),
            EdgeDirectionWire::Unknown,
            "an unfamiliar edge direction must not discard its inspection frame"
        );
        assert_eq!(
            from_cbor_bytes::<EncodeTraceStageStatus>(&bytes).unwrap(),
            EncodeTraceStageStatus::Unknown,
            "an unfamiliar trace status must not discard its response frame"
        );
        assert_eq!(
            from_cbor_bytes::<RecallCandidateKind>(&bytes).unwrap(),
            RecallCandidateKind::Unknown,
            "an unfamiliar candidate kind must not discard its trace frame"
        );
        assert_eq!(
            from_cbor_bytes::<RecallTraceRetrieverStatus>(&bytes).unwrap(),
            RecallTraceRetrieverStatus::Unknown,
            "an unfamiliar retriever status must not discard its trace frame"
        );
        assert_eq!(
            from_cbor_bytes::<RankedItemKindWire>(&bytes).unwrap(),
            RankedItemKindWire::Unknown,
            "an unfamiliar ranked-item kind must not discard its trace frame"
        );
        assert_eq!(
            from_cbor_bytes::<PlanTraceDirection>(&bytes).unwrap(),
            PlanTraceDirection::Unknown,
            "an unfamiliar plan direction must not discard its trace frame"
        );
        assert_eq!(
            EventType::Unknown as u8,
            255,
            "255 must remain reserved for the unknown event fallback"
        );
    }

    /// These values direct the server, so an unrecognised directive means the
    /// caller requested behaviour this build does not have. Answering with a
    /// different behaviour is worse than refusing: `ForgetMode` selects what
    /// gets destroyed and `AuthMethod` decides how identity is proven.
    #[test]
    fn request_enums_reject_unknown_values() {
        let bytes = to_cbor_bytes(&200u8);
        assert!(
            from_cbor_bytes::<ForgetMode>(&bytes).is_err(),
            "unknown forget modes must not select what gets destroyed"
        );
        assert!(
            from_cbor_bytes::<MemoryKindWire>(&bytes).is_err(),
            "unknown memory kinds must not select request semantics"
        );
        assert!(
            from_cbor_bytes::<EdgeKindWire>(&bytes).is_err(),
            "unknown edge kinds must not select a graph operation"
        );
        assert!(
            from_cbor_bytes::<WaitMode>(&bytes).is_err(),
            "unknown wait modes must not select server behaviour"
        );
        assert!(
            from_cbor_bytes::<AuthMethod>(&bytes).is_err(),
            "unknown auth methods must not change how identity is proven"
        );
    }

    /// Spec-pinned numeric values for the new codes. If these change, the
    /// wire format changed and this test catches it.
    #[test]
    fn new_codes_have_spec_numeric_values() {
        assert_eq!(ErrorCodeWire::Cancelled as u16, 0x0085);
        assert_eq!(ErrorCodeWire::RetrieverDegraded as u16, 0x0094);
        // BYOK credential codes. Their numeric BLOCK is what assigns their
        // category (0x006x = Conflict, 0x009x = Unavailable), so moving one
        // silently changes whether clients retry it.
        assert_eq!(ErrorCodeWire::LlmCredentialMissing as u16, 0x0066);
        assert_eq!(ErrorCodeWire::LlmCredentialUnavailable as u16, 0x0095);
        // A faulted shard's code sits in its own 0x00Ax block because the
        // numeric block is what assigns the category; moving it would make
        // clients retry a dead disk again.
        assert_eq!(ErrorCodeWire::ShardStorageFaulted as u16, 0x00A0);
        assert_eq!(ErrorCodeWire::UnrecoverableInternal as u16, 0x00A1);
        assert_eq!(ErrorCodeWire::SchemaMigrationRequired as u16, 0x0121);
        assert_eq!(ErrorCodeWire::EntityNotFound as u16, 0x0130);
        assert_eq!(ErrorCodeWire::EntityTypeMismatch as u16, 0x0131);
        assert_eq!(ErrorCodeWire::EntityAmbiguous as u16, 0x0132);
        assert_eq!(ErrorCodeWire::EntityMergeConflict as u16, 0x0133);
        assert_eq!(ErrorCodeWire::StatementNotFound as u16, 0x0140);
        assert_eq!(ErrorCodeWire::StatementObjectTypeMismatch as u16, 0x0141);
        assert_eq!(ErrorCodeWire::StatementContradictsExisting as u16, 0x0142);
        assert_eq!(ErrorCodeWire::QueryTimeout as u16, 0x0160);
        assert_eq!(ErrorCodeWire::QueryOverBudget as u16, 0x0161);
        assert_eq!(ErrorCodeWire::ExtractorDisabled as u16, 0x0170);
        assert_eq!(ErrorCodeWire::ExtractorBudgetExceeded as u16, 0x0171);
        assert_eq!(ErrorCodeWire::ExtractionFailed as u16, 0x0172);
    }

    /// Round-trip every new code through the ErrorCode ↔ ErrorCodeWire
    /// conversion. If a From impl forgets to wire up a variant, this
    /// fails.
    #[test]
    fn new_codes_round_trip_through_wire_and_back() {
        let codes = [
            ErrorCode::Cancelled,
            ErrorCode::RetrieverDegraded,
            ErrorCode::LlmCredentialMissing,
            ErrorCode::LlmCredentialUnavailable,
            ErrorCode::ShardStorageFaulted,
            ErrorCode::UnrecoverableInternal,
            ErrorCode::SchemaMigrationRequired,
            ErrorCode::EntityNotFound,
            ErrorCode::EntityTypeMismatch,
            ErrorCode::EntityAmbiguous,
            ErrorCode::EntityMergeConflict,
            ErrorCode::StatementNotFound,
            ErrorCode::StatementObjectTypeMismatch,
            ErrorCode::StatementContradictsExisting,
            ErrorCode::QueryTimeout,
            ErrorCode::QueryOverBudget,
            ErrorCode::ExtractorDisabled,
            ErrorCode::ExtractorBudgetExceeded,
            ErrorCode::ExtractionFailed,
        ];
        for code in codes {
            let wire: ErrorCodeWire = code.into();
            let back: ErrorCode = wire.into();
            assert_eq!(code, back, "round-trip failed for {code:?}");
        }
    }

    /// The point of an ERROR frame is that a peer learns what failed, so an
    /// unfamiliar classification must cost one field rather than the whole answer.
    #[test]
    fn unknown_error_classifications_preserve_the_frame() {
        let original = crate::envelope::error::ErrorResponse {
            code: 0xFFFE,
            category: 250,
            message: "future failure".into(),
            details: Some(crate::envelope::error::ErrorDetails {
                field: Some("id".into()),
                expected: None,
                actual: Some("bad".into()),
            }),
            retry_after_ms: Some(321),
        };
        let bytes = to_cbor_bytes(&original);
        let decoded: crate::envelope::error::ErrorResponse = from_cbor_bytes(&bytes).unwrap();
        assert_eq!(decoded.code, 0xFFFE);
        assert_eq!(decoded.category, 250);
        assert_eq!(decoded.message, original.message);
        assert_eq!(decoded.details, original.details);
        assert_eq!(decoded.retry_after_ms, Some(321));
    }

    #[test]
    fn error_wire_try_from_preserves_unknown_values() {
        assert_eq!(
            ErrorCodeWire::try_from(ErrorCodeWire::Internal as u16),
            Ok(ErrorCodeWire::Internal)
        );
        assert_eq!(ErrorCodeWire::try_from(0xFFFE), Err(0xFFFE));
        assert_eq!(
            ErrorCategoryWire::try_from(ErrorCategoryWire::Internal as u8),
            Ok(ErrorCategoryWire::Internal)
        );
        assert_eq!(ErrorCategoryWire::try_from(250), Err(250));
    }

    /// The committed conformance corpus pins these bytes, so changing the
    /// field's Rust type must not change the encoded integer.
    #[test]
    fn known_error_code_integer_keeps_cbor_bytes() {
        assert_eq!(
            to_cbor_bytes(&(ErrorCodeWire::Internal as u16)),
            to_cbor_bytes(&ErrorCodeWire::Internal)
        );
    }
}
