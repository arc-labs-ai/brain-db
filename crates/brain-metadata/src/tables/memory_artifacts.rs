//! `memory_artifacts` table — the durable per-memory write-artifact bundle.
//!
//! When a memory is written with introspection enabled, the concrete outputs of
//! each write stage — the persisted metadata row, the write-time HyPE
//! questions, the analyzed keyword terms, and the extracted knowledge graph —
//! are captured and stored here so **any** memory can be inspected later (the
//! `MEMORY_INSPECT` op), not only in the transient ENCODE trace. This is the
//! only place the HyPE question *text* and the analyzed keyword terms are
//! persisted (elsewhere only their embeddings survive).
//!
//! The embedding itself is **not** here: it lives once, in
//! [`memory_vectors`](super::memory_vector), as a flat byte run. A copy in
//! this bundle would put a 384-float JSON array on the write path for a
//! reader that is better served by a point lookup. The bundle uses
//! `brain_protocol::EncodeStageArtifact` as its serialized shape.
//!
//! ## Key
//!
//! `MemoryId.to_be_bytes()` (16 bytes) — one row per memory. A FORGET cascade
//! deletes the row by this exact key when the memory is hard-forgotten.
//!
//! ## Value
//!
//! The bundle serialized as **JSON text** (`&str`). Stored as text on purpose:
//! it is a human-readable, presentation-shaped record meant to be rendered in a
//! friendly per-memory view, while this table remains opaque to its readers.
//!
//! ## Population — incremental
//!
//! The row fills in over the write's own sync→async timeline: the synchronous
//! phases write the record + analyzed terms at persist; the async workers
//! (extractor, HyPE) read-merge-write their outputs as they settle. A reader
//! inspecting a just-written memory therefore sees it filling in, matching the
//! real pipeline.

use redb::TableDefinition;

/// `MemoryId.to_be_bytes()` (16 bytes) → the memory's write-artifact bundle,
/// serialized as JSON text. See the module docs for the shape and lifecycle.
pub const MEMORY_ARTIFACTS_TABLE: TableDefinition<'static, [u8; 16], &str> =
    TableDefinition::new("memory_artifacts");
