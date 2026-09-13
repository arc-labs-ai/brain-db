//! Statement question-bridge persistence: durable per-statement question
//! vectors.
//!
//! The in-RAM statement-question HNSW
//! (`brain_index::StatementQuestionHnswIndex`) is derived from these rows
//! and rebuilt on boot, so a restart never re-runs the embedder — it
//! range-scans the table and re-inserts. The questions themselves are
//! templated (zero-LLM), so even first ingestion is embedder-only.

pub mod ops;
