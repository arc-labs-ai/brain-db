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

pub mod extractor;
pub mod idempotency;
pub mod item;
pub mod options;
pub mod registry;

pub use extractor::{
    ExtractionContext, ExtractionResult, ExtractionStatus, Extractor, ExtractorError,
};
pub use idempotency::{hash_memory_text, IdempotencyKey};
pub use item::{EntityMention, ExtractedItem, RelationMention, StatementMention};
pub use options::ExtractorRunOptions;
pub use registry::ExtractorRegistry;
