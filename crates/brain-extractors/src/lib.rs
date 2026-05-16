//! # brain-extractors
//!
//! Extractor framework (pattern / classifier / LLM) for the Brain
//! knowledge layer. Phase 20.1 lands the trait + registry + output
//! types; pattern / classifier impls follow in 20.2 / 20.3.
//!
//! See `spec/22_extractors/` for the authoritative design.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

mod candle_runtime;
pub mod classifier;
pub mod extractor;
pub mod idempotency;
pub mod item;
pub mod labels;
pub mod materialize;
pub mod options;
pub mod pattern;
pub mod registry;

pub use classifier::{
    BertTokenClassifier, ClassifierConfig, ClassifierExtractor, ClassifierModel,
    TokenClassification,
};
pub use extractor::{
    ExtractionContext, ExtractionResult, ExtractionStatus, Extractor, ExtractorError,
};
pub use idempotency::{hash_memory_text, IdempotencyKey};
pub use item::{EntityMention, ExtractedItem, RelationMention, StatementMention};
pub use labels::{decode_bio, load_labels_file, BioSpan};
pub use materialize::{
    build_registry_from_definitions, materialize_classifier_extractor,
    materialize_pattern_extractor,
};
pub use options::ExtractorRunOptions;
pub use pattern::{CompiledRegex, PatternExtractor};
pub use registry::ExtractorRegistry;
