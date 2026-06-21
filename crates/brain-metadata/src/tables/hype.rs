//! HyPE family — 1 table.
//!
//! - [`HYPE_QUESTION_VECTORS_TABLE`] — durable hypothetical-question
//!   embeddings, the source from which the in-RAM HyPE HNSW is rebuilt
//!   on boot.
//!
//! HyPE ("Hypothetical Prompt Embeddings") is a write-time bridge for
//! the query↔memory phrasing gap: an LLM generates several questions
//! whose answer is a memory, each is embedded, and the vectors live
//! here. A read probes the derived HNSW with the user's query vector and
//! a hit maps back to the owning memory.
//!
//! The key is `MemoryId.to_be_bytes() ++ [question_index]` so a memory
//! owns a contiguous run of rows: rebuild range-scans the whole table,
//! and a FORGET cascade can range-delete a single memory's questions by
//! its 16-byte prefix. A `u8` index caps a memory at 256 questions,
//! far above the ~5–8 generated.

use redb::TableDefinition;

/// Bytes per persisted question vector — 384 f32 components × 4 bytes,
/// pinned to BGE-small. Identical layout to the entity-vector table.
pub const HYPE_VECTOR_BYTES: usize = 384 * 4;

/// `MemoryId.to_be_bytes() ++ [question_index]` (17 bytes) →
/// little-endian `[f32; 384]` byte image. One row per generated
/// question; several rows share a memory's 16-byte prefix.
pub const HYPE_QUESTION_VECTORS_TABLE: TableDefinition<'static, [u8; 17], [u8; HYPE_VECTOR_BYTES]> =
    TableDefinition::new("hype_question_vectors");

/// `MemoryId.to_be_bytes()` (16 bytes) → the blake3 hash of the
/// `(text, neighborhood)` pair that produced this memory's current HyPE
/// questions. The write-time refresh worker reads it to decide whether a
/// memory needs re-generation: when a memory's typed-graph neighborhood
/// grows (a later memory adds an edge to one of its entities), the recomputed
/// hash diverges from the stored one and the worker regenerates the
/// questions — the bridge questions a multi-hop read needs only become
/// writable once the connecting facts exist. A matching hash means the
/// neighborhood is unchanged since the last generation, so the memory is
/// skipped (no LLM call). Absent row ⇒ never generated under the graph-aware
/// path ⇒ eligible.
pub const HYPE_NEIGHBORHOOD_HASH_TABLE: TableDefinition<'static, [u8; 16], [u8; 32]> =
    TableDefinition::new("hype_neighborhood_hash");
