//! Extractor framework: the trait every concrete extractor implements,
//! the records they emit, the in-memory registry, and the per-extractor
//! run options.

pub mod extractor;
pub mod item;
pub mod options;
pub mod registry;

pub use extractor::{
    ExtractionContext, ExtractionFuture, ExtractionResult, ExtractionStatus, Extractor,
    ExtractorContext, ExtractorError, NeighborMemory,
};
pub use item::{EntityMention, ExtractedItem, RelationMention, StatementMention};
pub use options::ExtractorRunOptions;
pub use registry::{ExtractorRegistry, TierGate, TierState};
