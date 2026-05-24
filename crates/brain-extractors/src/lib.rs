//! # brain-extractors
//!
//! Three-tier extractor pipeline (pattern → classifier → LLM) for the
//! Brain typed-graph layer.
//!
//! ## Module map
//!
//! - [`framework`] — the `Extractor` trait, registry, output items
//!   (`EntityMention` / `StatementMention` / `RelationMention`), and
//!   per-extractor run options.
//! - [`pattern`] — Tier 1: regex-driven extraction.
//! - [`classifier`] — Tier 2: GLiNER zero-shot NER + statement-kind
//!   pattern matcher.
//! - [`llm`] — Tier 3: Anthropic / OpenAI LLM extraction with cost
//!   budgeting, schema validation, retries, and an idempotency cache.
//! - [`resolver`] — entity-resolution gauntlet (exact / alias /
//!   fuzzy trigram / embedding HNSW / create) used by the worker.
//! - [`materialize`] — bridge from schema definitions to in-memory
//!   `Extractor` instances; produces the registry the worker dispatches
//!   through.
//! - [`idempotency`] — text-hash keys for extractor caching.
//! - [`enricher_hook`] — `EnricherPlugin` dispatch seam (lives here to
//!   avoid a circular dep with `brain-plugins`).
//! - [`supersede_source`] — adapter that exposes the statement HNSW as
//!   a nearest-neighbour source for the W2.1 supersession judge.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod classifier;
pub mod enricher_hook;
pub mod framework;
pub mod idempotency;
pub mod llm;
pub mod materialize;
pub mod pattern;
pub mod resolver;
pub mod resolver_llm;
pub mod supersede_source;

pub use classifier::{
    classify_statement_kind_pattern, default_xdg_model_dir, ClassifiedSpan, ClassifierConfig,
    ClassifierExtractor, ClassifierModel, GlinerClassifier, GlinerSpan, NER_MODEL_DIR_NAME,
    NER_MODEL_PATH_ENV, NER_MODEL_REQUIRED_FILES, STATEMENT_KIND_PATTERN_THRESHOLD,
};
pub use enricher_hook::{run_pipeline_enrichers, EnricherHook, EnricherHookOutcome};
pub use framework::{
    EntityMention, ExtractedItem, ExtractionContext, ExtractionFuture, ExtractionResult,
    ExtractionStatus, Extractor, ExtractorContext, ExtractorError, ExtractorRegistry,
    ExtractorRunOptions, NeighborMemory, RelationMention, StatementMention, TierGate, TierState,
};
pub use idempotency::{hash_memory_text, IdempotencyKey};
pub use llm::{estimate_cost, CostBudget, LlmExtractor, LlmExtractorInner, Pricing};
pub use materialize::{
    build_registry_from_definitions, build_registry_with_gate, materialize_classifier_extractor,
    materialize_llm_extractor, materialize_pattern_extractor, MaterializeDeps,
};
pub use pattern::{CompiledRegex, PatternExtractor};
pub use resolver::{
    resolve_or_create, EntityDisambiguator, MatchVerdict, Resolution, ResolutionTier,
    ResolverError, DEFAULT_DISAMBIGUATOR_MIN_CONFIDENCE,
};
pub use resolver_llm::{BrainLlmDisambiguator, LlmCandidateView};
pub use supersede_source::StatementHnswSource;
