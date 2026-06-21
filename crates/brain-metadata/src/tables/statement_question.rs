//! Statement question-bridge family — 1 table.
//!
//! - [`STATEMENT_QUESTION_VECTORS_TABLE`] — durable embeddings of the
//!   templated questions a statement answers ("what is {subject}'s
//!   {predicate}?"), the source from which the in-RAM statement-question
//!   HNSW is rebuilt on boot.
//!
//! This is the per-statement analogue of the per-memory HyPE bridge
//! ([`crate::tables::hype`]): the write path turns each current statement
//! into a few full questions whose answer is that statement, embeds them,
//! and stores the vectors here. A read probes the derived HNSW with the
//! user's query vector and a hit maps back to the owning statement (whose
//! evidence memory is the answer). Embedding a FULL QUESTION — not a bare
//! predicate name — is what keeps this off the confident-wrong-answer trap
//! that a short predicate-name cosine would hit.
//!
//! The key is `StatementId.to_bytes() ++ [question_index]` so a statement
//! owns a contiguous run of rows: rebuild range-scans the whole table, and
//! a FORGET / supersession cascade range-deletes a single statement's
//! questions by its 16-byte prefix. A `u8` index caps a statement at 256
//! questions, far above the ~3–5 generated.

use redb::TableDefinition;

/// Bytes per persisted question vector — 384 f32 components × 4 bytes,
/// pinned to BGE-small. Identical layout to the HyPE / entity vectors.
pub const STATEMENT_QUESTION_VECTOR_BYTES: usize = 384 * 4;

/// `StatementId.to_bytes() ++ [question_index]` (17 bytes) →
/// little-endian `[f32; 384]` byte image. One row per generated question;
/// several rows share a statement's 16-byte prefix.
pub const STATEMENT_QUESTION_VECTORS_TABLE: TableDefinition<
    'static,
    [u8; 17],
    [u8; STATEMENT_QUESTION_VECTOR_BYTES],
> = TableDefinition::new("statement_question_vectors");
