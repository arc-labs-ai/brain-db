//! Fluent hybrid query builder + result projections. Phase 23.10.
//!
//! Public surface: one verb on [`crate::Client`] — [`Client::query`] —
//! returns a [`QueryBuilder`] with three modifier verbs:
//!
//! - `.execute()` → `QueryResult` (the hits + diagnostics).
//! - `.explain()` → `ExplainResult` (planner output, no execution).
//! - `.trace()`   → `TraceResult`   (planner + per-retriever execution
//!   metrics).
//!
//! See `spec/29_knowledge_sdk/00_purpose.md` §"Fluent query builder"
//! for the target ergonomics and
//! `spec/24_hybrid_query/00_purpose.md` for the server-side pipeline
//! this builder drives.
//!
//! ```no_run
//! # use brain_sdk_rust::{Client, ClientError, StatementKind, ItemRef, TimeRange};
//! # async fn ex(client: Client, priya: brain_sdk_rust::EntityId) -> Result<(), ClientError> {
//! let results = client.query()
//!     .text("budget pushback from leadership")
//!     .with_entity(priya)
//!     .of_kinds([StatementKind::Fact, StatementKind::Event])
//!     .where_time(TimeRange::last_days(30))
//!     .with_min_confidence(0.6)
//!     .limit(20)
//!     .execute()
//!     .await?;
//!
//! for hit in &results.items {
//!     match &hit.id {
//!         ItemRef::Memory(_id)    => { /* ... */ }
//!         ItemRef::Statement(_id) => { /* ... */ }
//!         ItemRef::Entity(_id)    => { /* ... */ }
//!         ItemRef::Relation(_id)  => { /* ... */ }
//!     }
//! }
//! # Ok(()) }
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::knowledge::StatementKind;
use brain_core::{EntityId, MemoryId, RelationId, StatementId};
use brain_protocol::error::ProtocolError;
use brain_protocol::knowledge::{
    FusionConfigWire, ItemIdWire, QueryExplainRequest as WireExplainReq,
    QueryExplainResponse as WireExplainResp, QueryRequest as WireQueryRequest, QueryResponse,
    QueryResultItem as WireQueryResultItem, QueryTraceRequest as WireTraceReq,
    QueryTraceResponse as WireTraceResp, RetrieverContributionWire,
    RetrieverOutcomeWire as WireRetrieverOutcome, RetrieverSelectionWire, RetrieverWire,
    TimeRangeWire,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::request::WireUuid;
use brain_protocol::{RequestBody, ResponseBody};

use crate::client::Client;
use crate::error::ClientError;

// ---------------------------------------------------------------------------
// Limits — must match the server-side caps in
// `brain-ops::ops::knowledge_query` so the SDK rejects bad input
// before the round-trip.
// ---------------------------------------------------------------------------

/// Max bytes of `text` accepted by [`QueryBuilder::execute`]. Matches the
/// server-side `MAX_QUERY_TEXT_BYTES` (16 KiB).
pub const MAX_QUERY_TEXT_BYTES: usize = 16 * 1024;

/// Max retrievers in an explicit selection. Matches the server's
/// `MAX_EXPLICIT_RETRIEVERS` and the router's `MAX_RETRIEVERS`.
pub const MAX_EXPLICIT_RETRIEVERS: usize = 3;

// ---------------------------------------------------------------------------
// Domain enums.
// ---------------------------------------------------------------------------

/// A retriever family. The hybrid engine runs zero or more of these
/// in parallel and fuses their ranks (`spec/23_retrievers/00_purpose.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Retriever {
    Semantic,
    Lexical,
    Graph,
}

impl Retriever {
    /// Display name used in EXPLAIN / TRACE / log lines.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Semantic => "semantic",
            Self::Lexical => "lexical",
            Self::Graph => "graph",
        }
    }

    /// `true` if the retriever requires query text to do anything
    /// useful. Lexical and Semantic need a cue; Graph walks the
    /// relation graph from an anchor.
    #[must_use]
    pub fn needs_text(self) -> bool {
        matches!(self, Self::Semantic | Self::Lexical)
    }

    /// `true` if the retriever requires an entity anchor.
    #[must_use]
    pub fn needs_anchor(self) -> bool {
        matches!(self, Self::Graph)
    }
}

impl From<RetrieverWire> for Retriever {
    fn from(w: RetrieverWire) -> Self {
        match w {
            RetrieverWire::Semantic => Self::Semantic,
            RetrieverWire::Lexical => Self::Lexical,
            RetrieverWire::Graph => Self::Graph,
        }
    }
}

impl From<Retriever> for RetrieverWire {
    fn from(r: Retriever) -> Self {
        match r {
            Retriever::Semantic => Self::Semantic,
            Retriever::Lexical => Self::Lexical,
            Retriever::Graph => Self::Graph,
        }
    }
}

impl From<brain_protocol::responses::types::RetrieverNameWire> for Retriever {
    fn from(w: brain_protocol::responses::types::RetrieverNameWire) -> Self {
        use brain_protocol::responses::types::RetrieverNameWire as W;
        match w {
            W::Semantic => Self::Semantic,
            W::Lexical => Self::Lexical,
            W::Graph => Self::Graph,
        }
    }
}

/// Which retrievers the planner is allowed to run. [`Self::auto`] lets
/// the router pick from the rules in `§24/00 §"Routing rules"`.
/// [`Self::explicit`] forces a specific set (validated at construction).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RetrieverSelection {
    #[default]
    Auto,
    Explicit(Vec<Retriever>),
}

impl RetrieverSelection {
    /// Let the router decide. Equivalent to `Self::Auto`.
    #[must_use]
    pub fn auto() -> Self {
        Self::Auto
    }

    /// Force a specific retriever list. Rejects:
    /// - an empty list (the planner would treat it as no-signal);
    /// - more than [`MAX_EXPLICIT_RETRIEVERS`] entries counted
    ///   pre-deduplication (matches the server's wire-side cap).
    ///
    /// Surviving duplicates are removed while preserving caller
    /// order before storage.
    ///
    /// # Errors
    /// Returns [`QueryBuilderError::EmptyExplicitRetrievers`] if
    /// `picks` yields nothing, or
    /// [`QueryBuilderError::TooManyExplicitRetrievers`] if the input
    /// exceeds [`MAX_EXPLICIT_RETRIEVERS`].
    pub fn explicit(picks: impl IntoIterator<Item = Retriever>) -> Result<Self, QueryBuilderError> {
        let raw: Vec<Retriever> = picks.into_iter().collect();
        if raw.is_empty() {
            return Err(QueryBuilderError::EmptyExplicitRetrievers);
        }
        if raw.len() > MAX_EXPLICIT_RETRIEVERS {
            return Err(QueryBuilderError::TooManyExplicitRetrievers {
                got: raw.len(),
                max: MAX_EXPLICIT_RETRIEVERS,
            });
        }
        let mut out: Vec<Retriever> = Vec::with_capacity(raw.len());
        for r in raw {
            if !out.contains(&r) {
                out.push(r);
            }
        }
        Ok(Self::Explicit(out))
    }
}

impl From<RetrieverSelection> for RetrieverSelectionWire {
    fn from(s: RetrieverSelection) -> Self {
        match s {
            RetrieverSelection::Auto => Self::Auto,
            RetrieverSelection::Explicit(list) => {
                Self::Explicit(list.into_iter().map(RetrieverWire::from).collect())
            }
        }
    }
}

impl From<RetrieverSelectionWire> for RetrieverSelection {
    fn from(w: RetrieverSelectionWire) -> Self {
        match w {
            RetrieverSelectionWire::Auto => Self::Auto,
            RetrieverSelectionWire::Explicit(list) => {
                Self::Explicit(list.into_iter().map(Retriever::from).collect())
            }
        }
    }
}

/// Per-retriever execution outcome. Collapses the wire's
/// `(status: u8, message: String)` pair into a single
/// pattern-matchable enum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetrieverOutcomeStatus {
    /// Retriever returned results within its timeout.
    Success,
    /// Retriever skipped this query because the request lacked the
    /// signal it needs (e.g. graph without an anchor). Other
    /// retrievers still contribute. `reason` is a short
    /// human-readable string from the server.
    Skipped { reason: String },
    /// Retriever exceeded its soft timeout. Results from it are
    /// included if any were produced before the deadline.
    Timeout,
    /// Retriever returned `Err(...)`. Other retrievers still
    /// contribute. `message` is the server-side error text.
    Failure { message: String },
}

impl RetrieverOutcomeStatus {
    /// `true` only for [`Self::Success`].
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    /// `true` for `Failure`; `false` for `Skipped` / `Timeout`
    /// (which still let other retrievers contribute to the fused
    /// result).
    #[must_use]
    pub fn is_terminal_failure(&self) -> bool {
        matches!(self, Self::Failure { .. })
    }

    fn from_wire(status: u8, message: String) -> Result<Self, ClientError> {
        match status {
            0 => Ok(Self::Success),
            1 => Ok(Self::Skipped { reason: message }),
            2 => Ok(Self::Timeout),
            3 => Ok(Self::Failure { message }),
            other => Err(ClientError::Protocol(ProtocolError::BadFrame(format!(
                "unknown retriever outcome status byte: {other}",
            )))),
        }
    }
}

/// A reference to one of the four item kinds the hybrid engine can
/// surface. Use [`Self::kind`] when you only need the discriminant;
/// use the `as_*` accessors when you want the typed ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemRef {
    Memory(MemoryId),
    Statement(StatementId),
    Entity(EntityId),
    Relation(RelationId),
}

impl ItemRef {
    /// The kind discriminant, payload-free.
    #[must_use]
    pub fn kind(self) -> ItemKind {
        match self {
            Self::Memory(_) => ItemKind::Memory,
            Self::Statement(_) => ItemKind::Statement,
            Self::Entity(_) => ItemKind::Entity,
            Self::Relation(_) => ItemKind::Relation,
        }
    }

    #[must_use]
    pub fn as_memory(self) -> Option<MemoryId> {
        if let Self::Memory(id) = self {
            Some(id)
        } else {
            None
        }
    }

    #[must_use]
    pub fn as_statement(self) -> Option<StatementId> {
        if let Self::Statement(id) = self {
            Some(id)
        } else {
            None
        }
    }

    #[must_use]
    pub fn as_entity(self) -> Option<EntityId> {
        if let Self::Entity(id) = self {
            Some(id)
        } else {
            None
        }
    }

    #[must_use]
    pub fn as_relation(self) -> Option<RelationId> {
        if let Self::Relation(id) = self {
            Some(id)
        } else {
            None
        }
    }

    fn from_wire(w: ItemIdWire) -> Result<Self, ClientError> {
        match w.kind {
            0 => Ok(Self::Memory(MemoryId::from_raw(u128::from_be_bytes(
                w.bytes,
            )))),
            1 => Ok(Self::Statement(StatementId::from_bytes(w.bytes))),
            2 => Ok(Self::Entity(EntityId::from_bytes(w.bytes))),
            3 => Ok(Self::Relation(RelationId::from_bytes(w.bytes))),
            other => Err(ClientError::Protocol(ProtocolError::BadFrame(format!(
                "unknown ItemRef kind byte: {other}",
            )))),
        }
    }
}

/// Payload-free discriminant of [`ItemRef`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Memory,
    Statement,
    Entity,
    Relation,
}

// ---------------------------------------------------------------------------
// Domain structs (config / inputs).
// ---------------------------------------------------------------------------

/// Reciprocal Rank Fusion knobs. Per-query override of the planner's
/// defaults (`k = 60`, all weights `1.0`).
#[derive(Clone, Debug, PartialEq)]
pub struct FusionConfig {
    pub k: u32,
    pub semantic_weight: f32,
    pub lexical_weight: f32,
    pub graph_weight: f32,
}

impl FusionConfig {
    /// New config with default `1.0` weights and the given `k`.
    #[must_use]
    pub fn new(k: u32) -> Self {
        Self {
            k,
            semantic_weight: 1.0,
            lexical_weight: 1.0,
            graph_weight: 1.0,
        }
    }

    /// Chainable weight setter.
    #[must_use]
    pub fn weights(mut self, semantic: f32, lexical: f32, graph: f32) -> Self {
        self.semantic_weight = semantic;
        self.lexical_weight = lexical;
        self.graph_weight = graph;
        self
    }

    /// Check that `k > 0` and every weight is finite and `>= 0`.
    pub fn validate(&self) -> Result<(), QueryBuilderError> {
        if self.k == 0 {
            return Err(QueryBuilderError::InvalidFusionK);
        }
        for (label, w) in [
            ("semantic_weight", self.semantic_weight),
            ("lexical_weight", self.lexical_weight),
            ("graph_weight", self.graph_weight),
        ] {
            if !w.is_finite() || w < 0.0 {
                return Err(QueryBuilderError::InvalidFusionWeight {
                    field: label,
                    got: w,
                });
            }
        }
        Ok(())
    }
}

impl Default for FusionConfig {
    fn default() -> Self {
        Self::new(60)
    }
}

impl From<FusionConfig> for FusionConfigWire {
    fn from(c: FusionConfig) -> Self {
        Self {
            k: c.k,
            semantic_weight: c.semantic_weight,
            lexical_weight: c.lexical_weight,
            graph_weight: c.graph_weight,
        }
    }
}

impl From<FusionConfigWire> for FusionConfig {
    fn from(w: FusionConfigWire) -> Self {
        Self {
            k: w.k,
            semantic_weight: w.semantic_weight,
            lexical_weight: w.lexical_weight,
            graph_weight: w.graph_weight,
        }
    }
}

/// Half-open / closed unix-millisecond time window. `None` bounds
/// mean open-ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TimeRange {
    pub from_unix_ms: Option<u64>,
    pub to_unix_ms: Option<u64>,
}

impl TimeRange {
    /// Closed range. Rejects `from > to`.
    pub fn from_to(from_unix_ms: u64, to_unix_ms: u64) -> Result<Self, QueryBuilderError> {
        if from_unix_ms > to_unix_ms {
            return Err(QueryBuilderError::TimeRangeInverted {
                from_unix_ms,
                to_unix_ms,
            });
        }
        Ok(Self {
            from_unix_ms: Some(from_unix_ms),
            to_unix_ms: Some(to_unix_ms),
        })
    }

    /// Half-open: `from .. ∞`.
    #[must_use]
    pub fn since(from_unix_ms: u64) -> Self {
        Self {
            from_unix_ms: Some(from_unix_ms),
            to_unix_ms: None,
        }
    }

    /// Half-open: `-∞ .. to`.
    #[must_use]
    pub fn until(to_unix_ms: u64) -> Self {
        Self {
            from_unix_ms: None,
            to_unix_ms: Some(to_unix_ms),
        }
    }

    /// Convenience: the last `n` days, computed via `SystemTime::now`.
    ///
    /// Tests that need determinism should use [`Self::from_to`].
    #[must_use]
    pub fn last_days(n: u32) -> Self {
        Self::last_millis(u64::from(n) * 24 * 60 * 60 * 1000)
    }

    /// Convenience: the last `n` hours.
    #[must_use]
    pub fn last_hours(n: u32) -> Self {
        Self::last_millis(u64::from(n) * 60 * 60 * 1000)
    }

    fn last_millis(span_ms: u64) -> Self {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        Self {
            from_unix_ms: Some(now_ms.saturating_sub(span_ms)),
            to_unix_ms: Some(now_ms),
        }
    }

    /// Open on both ends — pass-through filter.
    #[must_use]
    pub fn open_ended() -> Self {
        Self::default()
    }

    /// `true` if `unix_ms` falls in this range (inclusive both ends).
    /// Open bounds always pass on that side.
    #[must_use]
    pub fn contains(&self, unix_ms: u64) -> bool {
        let lo_ok = self.from_unix_ms.is_none_or(|lo| unix_ms >= lo);
        let hi_ok = self.to_unix_ms.is_none_or(|hi| unix_ms <= hi);
        lo_ok && hi_ok
    }
}

impl From<TimeRange> for TimeRangeWire {
    fn from(r: TimeRange) -> Self {
        Self {
            from_unix_ms: r.from_unix_ms,
            to_unix_ms: r.to_unix_ms,
        }
    }
}

impl From<TimeRangeWire> for TimeRange {
    fn from(w: TimeRangeWire) -> Self {
        Self {
            from_unix_ms: w.from_unix_ms,
            to_unix_ms: w.to_unix_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// Domain structs (results).
// ---------------------------------------------------------------------------

/// One retriever's contribution to a fused item: which retriever
/// surfaced it, what rank it had locally, and the raw similarity /
/// BM25 / graph score.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetrieverContribution {
    pub retriever: Retriever,
    pub rank: u32,
    pub raw_score: f32,
}

impl From<RetrieverContributionWire> for RetrieverContribution {
    fn from(w: RetrieverContributionWire) -> Self {
        Self {
            retriever: w.retriever.into(),
            rank: w.rank,
            raw_score: w.raw_score,
        }
    }
}

/// Per-retriever execution outcome. Use [`Self::status`] to see why
/// the retriever didn't return.
#[derive(Clone, Debug, PartialEq)]
pub struct RetrieverOutcome {
    pub retriever: Retriever,
    pub status: RetrieverOutcomeStatus,
    pub latency_ms: f64,
    pub result_count: u32,
}

impl RetrieverOutcome {
    fn from_wire(w: WireRetrieverOutcome) -> Result<Self, ClientError> {
        Ok(Self {
            retriever: w.retriever.into(),
            status: RetrieverOutcomeStatus::from_wire(w.status, w.message)?,
            latency_ms: w.latency_ms,
            result_count: w.result_count,
        })
    }
}

/// One fused hit: a typed item reference plus its fused score and the
/// per-retriever contributions that produced it.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryHit {
    pub id: ItemRef,
    pub fused_score: f64,
    pub contributing: Vec<RetrieverContribution>,
}

impl QueryHit {
    /// `true` if `retriever` is among the contributors.
    #[must_use]
    pub fn contributed_by(&self, retriever: Retriever) -> bool {
        self.contributing.iter().any(|c| c.retriever == retriever)
    }

    /// Local rank within `retriever`, if present.
    #[must_use]
    pub fn rank_in(&self, retriever: Retriever) -> Option<u32> {
        self.contributing
            .iter()
            .find(|c| c.retriever == retriever)
            .map(|c| c.rank)
    }

    fn from_wire(w: WireQueryResultItem) -> Result<Self, ClientError> {
        Ok(Self {
            id: ItemRef::from_wire(w.id)?,
            fused_score: w.fused_score,
            contributing: w
                .contributing
                .into_iter()
                .map(RetrieverContribution::from)
                .collect(),
        })
    }
}

/// Full result of [`QueryBuilder::execute`].
#[derive(Clone, Debug, PartialEq)]
pub struct QueryResult {
    pub items: Vec<QueryHit>,
    pub total_latency_ms: f64,
    pub retriever_outcomes: Vec<RetrieverOutcome>,
}

impl QueryResult {
    /// Look up the outcome for a specific retriever.
    #[must_use]
    pub fn outcome(&self, retriever: Retriever) -> Option<&RetrieverOutcome> {
        self.retriever_outcomes
            .iter()
            .find(|o| o.retriever == retriever)
    }

    /// `true` if any retriever reported a terminal failure.
    #[must_use]
    pub fn any_failure(&self) -> bool {
        self.retriever_outcomes
            .iter()
            .any(|o| o.status.is_terminal_failure())
    }
}

/// Result of [`QueryBuilder::explain`] — the planner's plan as
/// rendered text plus the estimated execution cost.
#[derive(Clone, Debug, PartialEq)]
pub struct ExplainResult {
    pub plan_text: String,
    pub estimated_cost_ms: f32,
}

impl From<WireExplainResp> for ExplainResult {
    fn from(w: WireExplainResp) -> Self {
        Self {
            plan_text: w.plan_text,
            estimated_cost_ms: w.estimated_cost_ms,
        }
    }
}

/// Result of [`QueryBuilder::trace`] — plan + execution metrics in
/// one rendered text block, with the observed total latency.
#[derive(Clone, Debug, PartialEq)]
pub struct TraceResult {
    pub trace_text: String,
    pub total_latency_ms: f64,
}

impl From<WireTraceResp> for TraceResult {
    fn from(w: WireTraceResp) -> Self {
        Self {
            trace_text: w.trace_text,
            total_latency_ms: w.total_latency_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// Builder-side errors.
// ---------------------------------------------------------------------------

/// Validation failures detected before the round-trip.
#[derive(Debug, thiserror::Error)]
pub enum QueryBuilderError {
    #[error(
        "query has neither text nor entity anchor; at least one is required to produce results"
    )]
    NoSignal,

    #[error("query text exceeds {max} bytes (got {got})")]
    TextTooLong { got: usize, max: usize },

    #[error("explicit retriever list is empty; use RetrieverSelection::auto() instead")]
    EmptyExplicitRetrievers,

    #[error("explicit retriever list has {got} entries; cap is {max}")]
    TooManyExplicitRetrievers { got: usize, max: usize },

    #[error("invalid fusion k=0; must be positive")]
    InvalidFusionK,

    #[error("invalid fusion {field}={got}; must be finite and non-negative")]
    InvalidFusionWeight { field: &'static str, got: f32 },

    #[error("time range is inverted: from={from_unix_ms} > to={to_unix_ms}")]
    TimeRangeInverted { from_unix_ms: u64, to_unix_ms: u64 },
}

impl From<QueryBuilderError> for ClientError {
    fn from(e: QueryBuilderError) -> Self {
        Self::Internal(format!("query builder: {e}"))
    }
}

// ---------------------------------------------------------------------------
// The builder itself.
// ---------------------------------------------------------------------------

/// Fluent builder for hybrid queries. Construct via [`Client::query`].
///
/// All setters are infallible and chainable. Validation runs once
/// inside the terminal verb (`.execute()` / `.explain()` / `.trace()`);
/// invalid combinations (empty text + no anchor, etc.) are reported
/// as [`QueryBuilderError`] via the surrounding [`ClientError`].
pub struct QueryBuilder<'a> {
    client: &'a Client,
    text: Option<String>,
    entity_anchor: Option<EntityId>,
    kind_filter: Vec<StatementKind>,
    predicate_filter: Vec<String>,
    time_filter: Option<TimeRange>,
    confidence_min: Option<f32>,
    include_tombstoned: bool,
    include_superseded: bool,
    limit: u32,
    retrievers: RetrieverSelection,
    fusion_config: Option<FusionConfig>,
    request_id: Option<WireUuid>,
}

impl<'a> QueryBuilder<'a> {
    pub(crate) fn new(client: &'a Client) -> Self {
        Self {
            client,
            text: None,
            entity_anchor: None,
            kind_filter: Vec::new(),
            predicate_filter: Vec::new(),
            time_filter: None,
            confidence_min: None,
            include_tombstoned: false,
            include_superseded: false,
            limit: 0,
            retrievers: RetrieverSelection::Auto,
            fusion_config: None,
            request_id: None,
        }
    }

    /// Set the cue text. Empty strings are treated as "no text".
    #[must_use]
    pub fn text(mut self, text: impl Into<String>) -> Self {
        let s = text.into();
        self.text = if s.is_empty() { None } else { Some(s) };
        self
    }

    /// Anchor the query at an entity — enables the Graph retriever.
    #[must_use]
    pub fn with_entity(mut self, id: EntityId) -> Self {
        self.entity_anchor = Some(id);
        self
    }

    /// Filter by one or more statement kinds.
    #[must_use]
    pub fn of_kinds(mut self, kinds: impl IntoIterator<Item = StatementKind>) -> Self {
        self.kind_filter = kinds.into_iter().collect();
        self
    }

    /// Filter by a single statement kind.
    #[must_use]
    pub fn of_kind(self, kind: StatementKind) -> Self {
        self.of_kinds([kind])
    }

    /// Filter by predicate qnames (canonical `"namespace:name"`).
    ///
    /// Schemaless clients pass qnames directly — they don't carry
    /// PredicateIds. Schema-strict deployments still resolve through
    /// the same wire path; an unknown qname surfaces as
    /// `PredicateNotInSchema` from the server.
    #[must_use]
    pub fn predicates<S: Into<String>>(mut self, predicates: impl IntoIterator<Item = S>) -> Self {
        self.predicate_filter = predicates.into_iter().map(Into::into).collect();
        self
    }

    /// Restrict to a time window (memory `created_at`, statement
    /// `event_at` / `valid_from..valid_to`).
    #[must_use]
    pub fn where_time(mut self, range: TimeRange) -> Self {
        self.time_filter = Some(range);
        self
    }

    /// Drop items below `min_confidence`. Memory hits use salience
    /// as the substrate analog; statements / relations use their
    /// own confidence fields.
    #[must_use]
    pub fn with_min_confidence(mut self, min: f32) -> Self {
        self.confidence_min = Some(min);
        self
    }

    /// Include tombstoned rows in the result. Default: exclude.
    #[must_use]
    pub fn include_tombstoned(mut self, include: bool) -> Self {
        self.include_tombstoned = include;
        self
    }

    /// Include superseded statements / relations. Default: exclude.
    #[must_use]
    pub fn include_superseded(mut self, include: bool) -> Self {
        self.include_superseded = include;
        self
    }

    /// Cap on returned hits. `0` (the default) means "use the
    /// planner's default" (20).
    #[must_use]
    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }

    /// Override the router. Use [`RetrieverSelection::auto`] (the
    /// default) to let the engine choose.
    #[must_use]
    pub fn retrievers(mut self, selection: RetrieverSelection) -> Self {
        self.retrievers = selection;
        self
    }

    /// Override the planner's fusion knobs.
    #[must_use]
    pub fn fusion(mut self, config: FusionConfig) -> Self {
        self.fusion_config = Some(config);
        self
    }

    /// Pin the wire `request_id` (defaults to a fresh v7 UUID per
    /// call). Useful for idempotency tracing.
    #[must_use]
    pub fn request_id(mut self, id: WireUuid) -> Self {
        self.request_id = Some(id);
        self
    }

    /// Terminal verb: run the query and return the hits +
    /// diagnostics.
    pub async fn execute(self) -> Result<QueryResult, ClientError> {
        let wire = self.to_wire()?;
        let resp = self
            .client_ref()
            .send_knowledge_request(
                RequestBody::Query(wire),
                Opcode::QueryReq,
                Opcode::QueryResp,
            )
            .await?;
        match resp {
            ResponseBody::Query(r) => project_query_response(r),
            other => Err(unexpected("QueryResp", &other)),
        }
    }

    /// Terminal verb: ask the planner for its plan as rendered text,
    /// without execution.
    pub async fn explain(self) -> Result<ExplainResult, ClientError> {
        let wire = self.to_wire()?;
        let resp = self
            .client_ref()
            .send_knowledge_request(
                RequestBody::QueryExplain(WireExplainReq { query: wire }),
                Opcode::QueryExplainReq,
                Opcode::QueryExplainResp,
            )
            .await?;
        match resp {
            ResponseBody::QueryExplain(r) => Ok(r.into()),
            other => Err(unexpected("QueryExplainResp", &other)),
        }
    }

    /// Terminal verb: run the query and return the planner's plan
    /// concatenated with the per-retriever execution metrics, all
    /// as rendered text.
    pub async fn trace(self) -> Result<TraceResult, ClientError> {
        let wire = self.to_wire()?;
        let resp = self
            .client_ref()
            .send_knowledge_request(
                RequestBody::QueryTrace(WireTraceReq { query: wire }),
                Opcode::QueryTraceReq,
                Opcode::QueryTraceResp,
            )
            .await?;
        match resp {
            ResponseBody::QueryTrace(r) => Ok(r.into()),
            other => Err(unexpected("QueryTraceResp", &other)),
        }
    }

    /// Borrow the client. `self.client` is captured into `to_wire`
    /// via the consuming `self`, so we need a single helper that
    /// borrows after the move-of-`self`-into-`to_wire`.
    fn client_ref(&self) -> &'a Client {
        self.client
    }

    /// Validate state and produce a wire-shape request. Consumes
    /// the builder.
    fn to_wire(&self) -> Result<WireQueryRequest, ClientError> {
        // No-signal guard: empty text + no anchor → reject.
        if self.text.is_none() && self.entity_anchor.is_none() {
            return Err(QueryBuilderError::NoSignal.into());
        }
        let text = self.text.clone().unwrap_or_default();
        if text.len() > MAX_QUERY_TEXT_BYTES {
            return Err(QueryBuilderError::TextTooLong {
                got: text.len(),
                max: MAX_QUERY_TEXT_BYTES,
            }
            .into());
        }
        if let Some(cfg) = &self.fusion_config {
            cfg.validate()?;
        }
        if let RetrieverSelection::Explicit(list) = &self.retrievers {
            if list.is_empty() {
                return Err(QueryBuilderError::EmptyExplicitRetrievers.into());
            }
            if list.len() > MAX_EXPLICIT_RETRIEVERS {
                return Err(QueryBuilderError::TooManyExplicitRetrievers {
                    got: list.len(),
                    max: MAX_EXPLICIT_RETRIEVERS,
                }
                .into());
            }
        }

        let kind_filter = self
            .kind_filter
            .iter()
            .copied()
            .map(statement_kind_to_byte)
            .collect();
        let predicate_filter = self.predicate_filter.clone();
        let entity_anchor = self.entity_anchor.map(EntityId::to_bytes);
        let time_filter = self.time_filter.map(TimeRangeWire::from);
        let retrievers = self.retrievers.clone().into();
        let fusion_config = self.fusion_config.clone().map(FusionConfigWire::from);
        let request_id = self
            .request_id
            .unwrap_or_else(|| *uuid::Uuid::now_v7().as_bytes());

        Ok(WireQueryRequest {
            text,
            entity_anchor,
            kind_filter,
            predicate_filter,
            time_filter,
            confidence_min: self.confidence_min,
            include_tombstoned: self.include_tombstoned,
            include_superseded: self.include_superseded,
            limit: self.limit,
            retrievers,
            fusion_config,
            request_id,
        })
    }
}

// ---------------------------------------------------------------------------
// `Client` entry point.
// ---------------------------------------------------------------------------

impl Client {
    /// Start a fluent hybrid query builder. Spec §29 §"Fluent
    /// query builder".
    ///
    /// The builder validates its inputs once, inside the terminal
    /// verb (`.execute()` / `.explain()` / `.trace()`); invalid
    /// combinations (empty text + no anchor, malformed fusion
    /// config, oversized explicit retriever list, etc.) fail
    /// before the round-trip.
    ///
    /// Use [`Self::recall`] when you only want similar memories by
    /// cue text. Use `query` when you need filters, an entity
    /// anchor, or heterogeneous results (memories + statements +
    /// entities + relations).
    #[must_use]
    pub fn query(&self) -> QueryBuilder<'_> {
        QueryBuilder::new(self)
    }
}

// ---------------------------------------------------------------------------
// Response projection helpers.
// ---------------------------------------------------------------------------

fn project_query_response(r: QueryResponse) -> Result<QueryResult, ClientError> {
    let items = r
        .items
        .into_iter()
        .map(QueryHit::from_wire)
        .collect::<Result<Vec<_>, _>>()?;
    let retriever_outcomes = r
        .retriever_outcomes
        .into_iter()
        .map(RetrieverOutcome::from_wire)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(QueryResult {
        items,
        total_latency_ms: r.total_latency_ms,
        retriever_outcomes,
    })
}

fn statement_kind_to_byte(k: StatementKind) -> u8 {
    match k {
        StatementKind::Fact => 0,
        StatementKind::Preference => 1,
        StatementKind::Event => 2,
    }
}

fn unexpected(expected: &str, got: &ResponseBody) -> ClientError {
    ClientError::Protocol(ProtocolError::BadFrame(format!(
        "expected {expected}, got {:?}",
        std::mem::discriminant(got),
    )))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retriever_predicates() {
        assert!(Retriever::Semantic.needs_text());
        assert!(Retriever::Lexical.needs_text());
        assert!(!Retriever::Graph.needs_text());
        assert!(!Retriever::Semantic.needs_anchor());
        assert!(Retriever::Graph.needs_anchor());
        assert_eq!(Retriever::Semantic.name(), "semantic");
    }

    #[test]
    fn retriever_selection_explicit_rejects_empty() {
        let err = RetrieverSelection::explicit([]).unwrap_err();
        assert!(matches!(err, QueryBuilderError::EmptyExplicitRetrievers));
    }

    #[test]
    fn retriever_selection_explicit_rejects_overflow_pre_dedup() {
        // 5 entries (some duplicate) — input length exceeds the cap
        // even though dedup would have shrunk it.
        let err = RetrieverSelection::explicit([
            Retriever::Semantic,
            Retriever::Lexical,
            Retriever::Graph,
            Retriever::Semantic,
            Retriever::Lexical,
        ])
        .unwrap_err();
        assert!(matches!(
            err,
            QueryBuilderError::TooManyExplicitRetrievers {
                got: 5,
                max: MAX_EXPLICIT_RETRIEVERS,
            }
        ));
    }

    #[test]
    fn retriever_selection_explicit_accepts_three_unique() {
        let s = RetrieverSelection::explicit([
            Retriever::Semantic,
            Retriever::Lexical,
            Retriever::Graph,
        ])
        .unwrap();
        match s {
            RetrieverSelection::Explicit(list) => {
                assert_eq!(list.len(), 3);
            }
            other => panic!("expected Explicit, got {other:?}"),
        }
    }

    #[test]
    fn retriever_selection_explicit_dedups_preserving_order() {
        let s = RetrieverSelection::explicit([
            Retriever::Lexical,
            Retriever::Semantic,
            Retriever::Lexical,
        ])
        .unwrap();
        match s {
            RetrieverSelection::Explicit(list) => {
                assert_eq!(list, vec![Retriever::Lexical, Retriever::Semantic]);
            }
            other => panic!("expected Explicit, got {other:?}"),
        }
    }

    #[test]
    fn fusion_config_validate_rejects_zero_k() {
        let mut c = FusionConfig::new(0);
        c.semantic_weight = 1.0;
        assert!(matches!(
            c.validate(),
            Err(QueryBuilderError::InvalidFusionK)
        ));
    }

    #[test]
    fn fusion_config_validate_rejects_negative_weight() {
        let c = FusionConfig::new(60).weights(-0.1, 1.0, 1.0);
        assert!(matches!(
            c.validate(),
            Err(QueryBuilderError::InvalidFusionWeight {
                field: "semantic_weight",
                ..
            })
        ));
    }

    #[test]
    fn fusion_config_round_trips_through_wire() {
        let original = FusionConfig::new(30).weights(1.5, 0.5, 2.0);
        let wire: FusionConfigWire = original.clone().into();
        let back: FusionConfig = wire.into();
        assert_eq!(original, back);
    }

    #[test]
    fn time_range_from_to_rejects_inverted() {
        let err = TimeRange::from_to(100, 99).unwrap_err();
        assert!(matches!(err, QueryBuilderError::TimeRangeInverted { .. }));
    }

    #[test]
    fn time_range_contains_open_bounds() {
        let r = TimeRange::since(100);
        assert!(r.contains(100));
        assert!(r.contains(u64::MAX));
        assert!(!r.contains(99));

        let r = TimeRange::until(100);
        assert!(r.contains(0));
        assert!(r.contains(100));
        assert!(!r.contains(101));

        let r = TimeRange::open_ended();
        assert!(r.contains(0));
        assert!(r.contains(u64::MAX));
    }

    #[test]
    fn time_range_last_hours_uses_now() {
        let r = TimeRange::last_hours(1);
        let from = r.from_unix_ms.unwrap();
        let to = r.to_unix_ms.unwrap();
        assert!(to >= from);
        // The window must be exactly 1h wide modulo saturation at 0.
        let span = to.saturating_sub(from);
        assert_eq!(span, 60 * 60 * 1000);
    }

    #[test]
    fn item_ref_kind_and_accessors() {
        let m = MemoryId::from_raw(7);
        let r = ItemRef::Memory(m);
        assert_eq!(r.kind(), ItemKind::Memory);
        assert_eq!(r.as_memory(), Some(m));
        assert_eq!(r.as_entity(), None);
    }

    #[test]
    fn item_ref_from_wire_each_kind() {
        let bytes = [7u8; 16];
        for (kind, expected_disc) in [
            (0u8, ItemKind::Memory),
            (1, ItemKind::Statement),
            (2, ItemKind::Entity),
            (3, ItemKind::Relation),
        ] {
            let w = ItemIdWire { kind, bytes };
            let r = ItemRef::from_wire(w).expect("kind decode");
            assert_eq!(r.kind(), expected_disc);
        }
    }

    #[test]
    fn item_ref_from_wire_unknown_kind_errors() {
        let err = ItemRef::from_wire(ItemIdWire {
            kind: 9,
            bytes: [0u8; 16],
        })
        .unwrap_err();
        assert!(matches!(err, ClientError::Protocol(_)));
    }

    #[test]
    fn item_ref_memory_round_trips_be_bytes() {
        let raw: u128 = 0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10;
        let w = ItemIdWire {
            kind: 0,
            bytes: raw.to_be_bytes(),
        };
        let r = ItemRef::from_wire(w).unwrap();
        let m = r.as_memory().unwrap();
        assert_eq!(m.raw(), raw);
    }

    #[test]
    fn retriever_outcome_status_from_wire_each_byte() {
        assert_eq!(
            RetrieverOutcomeStatus::from_wire(0, String::new()).unwrap(),
            RetrieverOutcomeStatus::Success
        );
        assert_eq!(
            RetrieverOutcomeStatus::from_wire(1, "no anchor".into()).unwrap(),
            RetrieverOutcomeStatus::Skipped {
                reason: "no anchor".into()
            }
        );
        assert_eq!(
            RetrieverOutcomeStatus::from_wire(2, String::new()).unwrap(),
            RetrieverOutcomeStatus::Timeout
        );
        assert_eq!(
            RetrieverOutcomeStatus::from_wire(3, "blew up".into()).unwrap(),
            RetrieverOutcomeStatus::Failure {
                message: "blew up".into()
            }
        );
        let err = RetrieverOutcomeStatus::from_wire(9, String::new()).unwrap_err();
        assert!(matches!(err, ClientError::Protocol(_)));
    }

    #[test]
    fn retriever_outcome_status_helpers() {
        let s = RetrieverOutcomeStatus::Success;
        assert!(s.is_success());
        assert!(!s.is_terminal_failure());

        let f = RetrieverOutcomeStatus::Failure {
            message: "x".into(),
        };
        assert!(!f.is_success());
        assert!(f.is_terminal_failure());

        let skipped = RetrieverOutcomeStatus::Skipped {
            reason: "no anchor".into(),
        };
        assert!(!skipped.is_terminal_failure());
    }

    #[test]
    fn statement_kind_to_byte_matches_wire() {
        assert_eq!(statement_kind_to_byte(StatementKind::Fact), 0);
        assert_eq!(statement_kind_to_byte(StatementKind::Preference), 1);
        assert_eq!(statement_kind_to_byte(StatementKind::Event), 2);
    }
}
