//! `Extractor` trait + execution context + result types.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use brain_core::ExtractorId;
use brain_core::ExtractorKind;
use brain_core::Memory;
use brain_core::MemoryId;

use crate::framework::item::ExtractedItem;
use crate::framework::registry::ExtractorRegistry;

// ---------------------------------------------------------------------------
// Bounded LLM context.
// ---------------------------------------------------------------------------

/// Bounded inferential context handed to the LLM extractor before each
/// call. Without it the LLM only sees the memory currently being
/// extracted and can't anchor predicates like "Alice mentioned earlier"
/// against any history. With it the prompt grows by at most a few
/// thousand tokens (top-m semantic neighbors + an optional rolling
/// summary), which is bounded so the cost-per-extraction stays
/// predictable even on long-running deployments.
///
/// Always construct via [`ExtractorContext::empty`] when no real
/// context is available — the LLM extractor's prompt builder handles
/// the empty case without emitting the context sections.
#[derive(Debug, Clone, Default)]
pub struct ExtractorContext {
    /// Most-similar prior memories from the same scope, ranked by
    /// descending cosine similarity. Capped at the caller-chosen
    /// `top_m` (10 by default) so the prompt budget can't grow
    /// unbounded.
    pub neighbors: Vec<NeighborMemory>,
    /// Optional rolling summary of recent activity in the same
    /// context. `None` when no summarizer is wired (v1 default);
    /// `Some(s)` when a future summarizer worker provides one.
    pub summary: Option<String>,
}

impl ExtractorContext {
    /// The zero-value: no neighbors, no summary. Callers pass this
    /// when they explicitly want to skip bounded-context injection
    /// (tests that compare against the no-context baseline, fallback
    /// paths after a context-fetch failure).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// True when this context has no usable signal — the prompt
    /// builder uses this to short-circuit the section rendering.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.neighbors.is_empty() && self.summary.is_none()
    }
}

/// One bounded-context neighbor entry — the text content (truncated to
/// keep the prompt budget predictable), the similarity score it earned
/// against the cue, and its wall-clock creation time so the prompt can
/// render a "T-3h" recency hint.
#[derive(Debug, Clone)]
pub struct NeighborMemory {
    pub memory_id: MemoryId,
    pub text: String,
    pub similarity_score: f32,
    pub created_at_unix_nanos: u64,
}

// ---------------------------------------------------------------------------
// Trait.
// ---------------------------------------------------------------------------

/// Boxed-future return type for [`Extractor::run`]. Pattern +
/// classifier impls wrap their sync bodies via
/// `Box::pin(async move { ... })`; LLM impls use the async path
/// natively for HTTP calls.
pub type ExtractionFuture<'a> = Pin<Box<dyn Future<Output = ExtractionResult> + Send + 'a>>;

/// Object-safe extractor interface. Pattern / classifier / LLM
/// impls live in `pattern.rs`, `classifier.rs`, and `llm.rs`.
/// The registry stores `Arc<dyn Extractor>`.
///
/// `run` returns a boxed future to let LLM extractors call out
/// to HTTP providers without blocking the executor thread. Sync
/// impls trivially wrap their bodies in `Box::pin(async move
/// { ... })`.
pub trait Extractor: Send + Sync {
    fn id(&self) -> ExtractorId;
    fn kind(&self) -> ExtractorKind;
    /// Canonical qname, e.g. `"acme:person_mentions"`.
    fn name(&self) -> &str;
    fn extractor_version(&self) -> u32;
    /// Run over `mem`. Returns a populated [`ExtractionResult`]
    /// including `started_at` / `completed_at` timestamps; the
    /// caller writes the audit row from these.
    fn run<'a>(&'a self, ctx: &'a ExtractionContext<'a>, mem: &'a Memory) -> ExtractionFuture<'a>;

    /// Run this extractor over a batch of memories in one logical
    /// invocation. The classifier tier overrides this to amortise the
    /// model forward pass across rows (single-input GLiNER inference
    /// is ~4s on CPU; a batched backbone pass over 8 memories
    /// completes in ~1-2x the single-input cost). Pattern + LLM
    /// extractors don't benefit from batching, so they fall through
    /// to the default impl that sequentially calls [`run`].
    ///
    /// Output is aligned to input order: `result[i]` is the result for
    /// `mems[i]`. An empty `mems` returns an empty `Vec`.
    fn run_batch<'a>(
        &'a self,
        ctx: &'a ExtractionContext<'a>,
        mems: &'a [Memory],
    ) -> Pin<Box<dyn Future<Output = Vec<ExtractionResult>> + Send + 'a>> {
        Box::pin(async move {
            let mut out = Vec::with_capacity(mems.len());
            for m in mems {
                out.push(self.run(ctx, m).await);
            }
            out
        })
    }

    /// True iff the underlying capability is actually configured.
    /// Pattern + fully-wired classifier/LLM extractors return true;
    /// the `degraded` variants (missing API key, missing model
    /// files, schema compile failure, …) return false. Used by the
    /// encode response so the renderer can tell operators when 0
    /// statements is a "set BRAIN__LLM__API_KEY" condition versus a
    /// content-coverage condition.
    fn is_wired(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Context.
// ---------------------------------------------------------------------------

/// Read-only context handed to every `run` call. Carries the
/// schema version stamped onto outputs and a registry reference
/// for dep lookups.
pub struct ExtractionContext<'a> {
    pub schema_version: u32,
    pub now_unix_nanos: u64,
    pub registry: &'a ExtractorRegistry,
    /// Items produced by earlier tiers in this cycle, keyed by
    /// memory id. The classifier tier (GLiNER) populates this so
    /// the LLM tier can reference canonical entity names in its
    /// prompt without re-extracting. `None` for the first tier in
    /// the pipeline (pattern) and for callers that don't run a
    /// multi-tier pipeline.
    pub prior_tier_items: Option<&'a HashMap<MemoryId, Vec<ExtractedItem>>>,
    /// Bounded inferential context (top-m similar memories + optional
    /// rolling summary) keyed by the memory being extracted. The LLM
    /// tier reads from this map and injects the per-memory context
    /// into its prompt; absent (or absent for a given memory id)
    /// means context-free extraction. Pattern + classifier tiers
    /// ignore this field.
    pub extractor_context: Option<&'a HashMap<MemoryId, ExtractorContext>>,
    /// The active schema's declared predicates, pre-rendered as a prompt
    /// block (`brain_metadata::render_declared_predicates_block`). The LLM
    /// tier substitutes this into its `{DECLARED_PREDICATES}` placeholder so
    /// its closed vocabulary tracks the active schema (system core + user
    /// `SCHEMA_UPLOAD`) at runtime. `None` = no block injected (the
    /// placeholder renders empty); pattern + classifier tiers ignore it.
    pub declared_predicates: Option<&'a str>,
    /// The active schema's declared statement kinds, pre-rendered as a
    /// prompt block (`brain_metadata::render_declared_kinds_block`): the
    /// six builtin kinds plus any user-declared ones. The LLM tier
    /// substitutes this into its `{DECLARED_KINDS}` placeholder so its
    /// kind taxonomy tracks the active schema at runtime. `None` = no
    /// block injected (the placeholder renders empty); pattern +
    /// classifier tiers ignore it.
    pub declared_kinds: Option<&'a str>,
    /// The active schema's entity-type label set (`brain:<Name>`, id-order),
    /// read fresh each drain cycle. The classifier (GLiNER) tier uses this as
    /// its zero-shot label set so a user's `SCHEMA_UPLOAD` adding entity types
    /// reaches the classifier on the next batch — no shard restart. `None`
    /// falls back to the labels baked in at construction; pattern + LLM tiers
    /// ignore it.
    pub entity_type_labels: Option<&'a [String]>,
}

// ---------------------------------------------------------------------------
// Result.
// ---------------------------------------------------------------------------

/// Whether a `Failure` result is worth retrying. Tier-agnostic so any
/// tier can classify, though only the LLM tier sets it meaningfully today
/// (pattern/classifier failures are terminal regardless — their rows
/// already committed). The worker reads this to decide a failed memory's
/// fate: a `Transient` (or unclassified) LLM failure keeps the memory
/// queued for a backoff retry so a passing provider outage never
/// permanently drops its grounding; a `Permanent` one is terminal so a
/// bad key / no balance fails loudly instead of looping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExtractionFailureClass {
    /// Not a failure, or a failure carrying no retry signal — treated as
    /// retryable by the worker (retry-with-backoff is bounded and cheap;
    /// dropping a memory's grounding on an unclassified blip is not).
    #[default]
    Unclassified,
    /// The same call could succeed later — retry with backoff.
    Transient,
    /// The call can't succeed as-is — terminal; surface to the operator.
    Permanent,
}

/// One extractor invocation's full output. Whether `items` is
/// populated depends on `status`: `Success` always carries items
/// (possibly empty); the various `Skipped*` and `Failure` variants
/// carry an empty `items` vec.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractionResult {
    pub items: Vec<ExtractedItem>,
    pub status: ExtractionStatus,
    pub status_reason: String,
    /// Set on `Failure` results to drive the worker's retry decision;
    /// `Unclassified` for success / skip / unclassified failures.
    pub failure_class: ExtractionFailureClass,
    pub started_at_unix_nanos: u64,
    pub completed_at_unix_nanos: u64,
    /// Provider cost this run actually incurred, in micro-USD. Non-zero
    /// only for LLM-tier runs that made a paid call (cache hits, budget
    /// skips, and the free pattern/classifier tiers are `0`). The worker
    /// sums this into the per-cycle spend gate and the
    /// `brain_extractor_llm_micro_usd_spent_total` metric.
    pub cost_micro_usd: u64,
}

impl ExtractionResult {
    /// Convenience: an empty result with `Success`. Wall time is
    /// expected to be filled in by callers that wrap `run`. Cost
    /// defaults to `0`; LLM callers chain [`with_cost`](Self::with_cost).
    pub fn success(items: Vec<ExtractedItem>, started_at: u64, completed_at: u64) -> Self {
        Self {
            items,
            status: ExtractionStatus::Success,
            status_reason: String::new(),
            failure_class: ExtractionFailureClass::Unclassified,
            started_at_unix_nanos: started_at,
            completed_at_unix_nanos: completed_at,
            cost_micro_usd: 0,
        }
    }

    pub fn skipped(status: ExtractionStatus, reason: impl Into<String>, at: u64) -> Self {
        Self {
            items: Vec::new(),
            status,
            status_reason: reason.into(),
            failure_class: ExtractionFailureClass::Unclassified,
            started_at_unix_nanos: at,
            completed_at_unix_nanos: at,
            cost_micro_usd: 0,
        }
    }

    pub fn failure(reason: impl Into<String>, started_at: u64, completed_at: u64) -> Self {
        Self {
            items: Vec::new(),
            status: ExtractionStatus::Failure,
            status_reason: reason.into(),
            failure_class: ExtractionFailureClass::Unclassified,
            started_at_unix_nanos: started_at,
            completed_at_unix_nanos: completed_at,
            cost_micro_usd: 0,
        }
    }

    /// Attach the provider cost (micro-USD) this run incurred.
    #[must_use]
    pub fn with_cost(mut self, cost_micro_usd: u64) -> Self {
        self.cost_micro_usd = cost_micro_usd;
        self
    }

    /// Tag a `Failure` with whether it's worth retrying. The LLM tier sets
    /// this from [`brain_llm::LlmError::failure_class`] so the worker can
    /// keep transient failures queued (backoff retry) and terminate
    /// permanent ones (bad key / no balance) loudly.
    #[must_use]
    pub fn with_failure_class(mut self, class: ExtractionFailureClass) -> Self {
        self.failure_class = class;
        self
    }
}

// ---------------------------------------------------------------------------
// Status.
// ---------------------------------------------------------------------------

/// Audit-row status discriminant. Bytes match — never
/// reassigned; new variants append.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ExtractionStatus {
    Success = 1,
    Failure = 2,
    SkippedBudget = 3,
    SkippedFilter = 4,
    SkippedDuplicate = 5,
    SkippedDisabled = 6,
}

impl ExtractionStatus {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            1 => Self::Success,
            2 => Self::Failure,
            3 => Self::SkippedBudget,
            4 => Self::SkippedFilter,
            5 => Self::SkippedDuplicate,
            6 => Self::SkippedDisabled,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum ExtractorError {
    #[error("regex compilation failed at index {index}: {message}")]
    RegexCompile { index: usize, message: String },

    #[error("resource limit exceeded at pattern {index}: {limit}")]
    ResourceLimit { index: usize, limit: &'static str },

    #[error("extractor declared `pattern` kind but has no patterns")]
    EmptyPatterns,

    #[error("classifier model not found: {id:?}")]
    ModelNotFound { id: String },

    #[error("feature extraction failed: {reason}")]
    FeatureExtractionFailed { reason: String },

    #[error("inference failed: {reason}")]
    InferenceFailed { reason: String },

    #[error("output decode failed: {reason}")]
    OutputDecodeFailed { reason: String },

    #[error("trigger evaluation error: {reason}")]
    TriggerEval { reason: String },
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_from_u8_round_trip() {
        for s in [
            ExtractionStatus::Success,
            ExtractionStatus::Failure,
            ExtractionStatus::SkippedBudget,
            ExtractionStatus::SkippedFilter,
            ExtractionStatus::SkippedDuplicate,
            ExtractionStatus::SkippedDisabled,
        ] {
            assert_eq!(ExtractionStatus::from_u8(s.as_u8()), Some(s));
        }
        assert_eq!(ExtractionStatus::from_u8(0), None);
        assert_eq!(ExtractionStatus::from_u8(255), None);
    }

    #[test]
    fn extraction_result_success_builder() {
        let r = ExtractionResult::success(Vec::new(), 10, 20);
        assert_eq!(r.status, ExtractionStatus::Success);
        assert!(r.items.is_empty());
        assert_eq!(r.completed_at_unix_nanos - r.started_at_unix_nanos, 10);
    }

    #[test]
    fn extraction_result_skipped_filter_builder() {
        let r = ExtractionResult::skipped(ExtractionStatus::SkippedFilter, "no match", 42);
        assert_eq!(r.status, ExtractionStatus::SkippedFilter);
        assert_eq!(r.status_reason, "no match");
        assert_eq!(r.started_at_unix_nanos, r.completed_at_unix_nanos);
    }

    #[test]
    fn extraction_result_failure_builder() {
        let r = ExtractionResult::failure("inference oom", 5, 7);
        assert_eq!(r.status, ExtractionStatus::Failure);
        assert!(r.items.is_empty());
    }

    #[test]
    fn extractor_error_messages_carry_position() {
        let e = ExtractorError::RegexCompile {
            index: 2,
            message: "bad".into(),
        };
        let s = e.to_string();
        assert!(s.contains("index 2"));
        assert!(s.contains("bad"));
    }
}
