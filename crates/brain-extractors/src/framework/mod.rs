//! Extractor framework: the trait every concrete extractor implements,
//! the records they emit, and the in-memory registry.

pub mod extractor;
pub mod item;
pub mod registry;
pub mod trigger;

pub use extractor::{
    ExtractionContext, ExtractionFailureClass, ExtractionFuture, ExtractionResult,
    ExtractionStatus, Extractor, ExtractorContext, ExtractorError, NeighborMemory,
    SYSTEM_NAMESPACE,
};
pub use item::{EntityMention, ExtractedItem, RelationMention, StatementMention};
pub use registry::ExtractorRegistry;
pub use trigger::{evaluate_trigger_on_encode, TriggerDecision};
